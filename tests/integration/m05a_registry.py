"""Enroll real Linux directories on the live coordinator API, optionally inside the Debian VM."""

import argparse
import hashlib
import json
import os
import secrets
import shlex
import subprocess
import sys
import tempfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from uuid import uuid4

from m02_bootstrap import request
from workspace_fixture import ROOT, digest, workspace_fixture


MANIFEST = {"include": [{"kind": "tree"}, {"kind": "file", "path": ".env.example"}],
            "exclude": [{"kind": "directory", "path": "local"}]}
PREPARE = """
from pathlib import Path
import os, sys
home = Path(sys.argv[1])
root = home / 'tree' / 'code'
for name, body in {
    'tree/code/src/app.rs': 'pub fn ping() {}\\n',
    'tree/code/nested/lib.rs': 'pub const VERSION: u16 = 1;\\n',
    'tree/code/.env.example': 'SERVICE_URL=\\n',
    'tree/code/.env': 'FIXTURE_PRIVATE_ENV_ONLY\\n',
    'tree/code/.git/config': 'FIXTURE_INTERNAL_CONFIG\\n',
    'tree/code/local/private.note': 'FIXTURE_PRIVATE_LOCAL_ONLY\\n',
    'independent/note.txt': 'A separate source directory\\n',
    'unsafe/src': 'An unsupported hardlink fixture\\n',
}.items():
    path = home / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body)
os.link(home / 'unsafe/src', home / 'unsafe/duplicate')
(home / 'alias').symlink_to(root, target_is_directory=True)
"""


class RunnerHost:
    def __init__(self, tmp, guest, binary, vm_state=None, *, ssh_connection=None):
        self.guest = guest
        self.binary = binary
        self.staging = tmp / "host-input"
        self.staging.mkdir(mode=0o700)
        if guest:
            sys.path.insert(0, str(ROOT / "tests/topology"))
            from live_clients import GUEST, connection

            self.target = GUEST
            self.topology, self.ssh, self.scp = ssh_connection or connection(vm_state)
            self.owner = self.topology.get("owner", "owner-b")
            if self.owner not in ("owner-a", "owner-b"):
                raise ValueError("Use an owner account created by the topology harness")
            leaf = f"cocodeop-registry-lab-{uuid4().hex}"
            self.upload = leaf
            self.root = f"/home/{self.owner}/{leaf}"
            self.installed = f"{self.root}/office-runner"
        else:
            self.root = str((tmp / "local-host").resolve())
            self.installed = str(binary)

    def setup(self, tokens):
        manifest = self.staging / "manifest.json"
        manifest.write_text(json.dumps(MANIFEST), encoding="utf-8")
        files = [manifest]
        for label, secret in tokens.items():
            token_file = self.staging / f"{label}.token"
            with os.fdopen(os.open(token_file, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as stream:
                stream.write(secret)
            files.append(token_file)
        if self.guest:
            self.remote(["mkdir", "-m", "700", self.upload])
            self.remote(["sudo", "-n", "install", "-d", "-m", "700", "-o", self.owner, "-g", self.owner, self.root])
            subprocess.run([*self.scp, str(self.binary.resolve()), *map(str, files), f"{self.target}:{self.upload}/"],
                           check=True, timeout=45)
            for file in [self.binary, *files]:
                mode = "700" if file == self.binary else "600"
                self.remote(["sudo", "-n", "install", "-o", self.owner, "-g", self.owner, "-m", mode,
                             f"{self.upload}/{file.name}", f"{self.root}/{file.name}"])
                self.remote(["rm", "--", f"{self.upload}/{file.name}"])
            self.remote(["rmdir", "--", self.upload])
        else:
            Path(self.root).mkdir(mode=0o700)
            for file in files:
                file.rename(Path(self.root) / file.name)
        self.invoke(["python3", "-c", PREPARE, self.root], check=True)

    def remote(self, args, forward=None, check=True, timeout=50):
        forwarding = ["-o", "ExitOnForwardFailure=yes"] if forward else []
        if forward:
            for route in (forward if isinstance(forward, (list, tuple)) else [forward]):
                forwarding.extend(["-R", route])
        return subprocess.run([*self.ssh, *forwarding, self.target, shlex.join(args)], check=check,
                              capture_output=True, encoding="utf-8", timeout=timeout)

    def invoke(self, args, forward=None, check=False, timeout=None):
        if self.guest:
            return self.remote(["sudo", "-n", "-u", self.owner, "-H", "--", *args], forward=forward,
                               check=check, timeout=timeout or 50)
        return subprocess.run(args, check=check, capture_output=True, encoding="utf-8", timeout=timeout or 20)

    def inspect(self, path):
        return self.invoke([self.installed, "checkout", "inspect", "--root", f"{self.root}/{path}",
                            "--manifest", f"{self.root}/manifest.json", "--json"])

    def enroll(self, endpoint, checkout, label, path, expected=None):
        forward, endpoint_for_runner = self.connection_for_runner(endpoint)
        args = [self.installed, "checkout", "enroll", "--checkout", checkout["checkout_id"],
                "--root", f"{self.root}/{path}", "--token-file", f"{self.root}/{label}.token",
                "--url", f"http://{endpoint_for_runner}", "--json"]
        if expected:
            args += ["--expected-source-hash", expected]
        return self.invoke(args, forward=forward)

    def connection_for_runner(self, endpoint):
        forward, endpoint_for_runner = None, endpoint
        if self.guest:
            host, port = endpoint.rsplit(":", 1)
            assert host == "127.0.0.1"
            remote_port = 20000 + secrets.randbelow(40000)
            forward = f"127.0.0.1:{remote_port}:127.0.0.1:{port}"
            endpoint_for_runner = f"127.0.0.1:{remote_port}"
        return forward, endpoint_for_runner

    def sync_scopes(self, endpoint, checkout, label="a", path="tree/code", state="state"):
        forward, endpoint_for_runner = self.connection_for_runner(endpoint)
        return self.invoke([self.installed, "checkout", "sync-scopes", "--checkout", checkout["checkout_id"],
                            "--root", f"{self.root}/{path}", "--state-dir", f"{self.root}/{state}",
                            "--token-file", f"{self.root}/{label}.token", "--url", f"http://{endpoint_for_runner}",
                            "--json"], forward=forward)

    def replace(self, path, content):
        self.invoke(["python3", "-c", "from pathlib import Path; import sys; Path(sys.argv[1]).write_text(sys.argv[2])",
                     f"{self.root}/{path}", content], check=True)

    def close(self):
        if self.guest:
            home = f"/home/{self.owner}"
            assert self.root.startswith(f"{home}/cocodeop-registry-lab-")
            cleanup = "from pathlib import Path; import shutil, sys; p = Path(sys.argv[1]); " \
                      "assert p.resolve() == p and p.parent == Path(sys.argv[2]) and p.name.startswith('cocodeop-registry-lab-'); " \
                      "shutil.rmtree(p, ignore_errors=True)"
            self.invoke(["python3", "-c", cleanup, self.root, home])


def run(tmp, binary, options):
    guest = options.linux_vm
    checks = []
    grant_checks = []
    scope_checks = []
    area_checks = []
    access_checks = []
    revocation_checks = []
    claim_checks = []
    handoff_checks = []
    decision_checks = []
    link_checks = []
    queue_checks = []
    coordinator_checks = []
    gateway_read_checks = []
    with workspace_fixture(tmp, "checkout-registry", server_image=options.server_image) as office:
        lead_a, identity_a = office.agent(office.owner_a, "Backend lead", "lead")
        lead_b, identity_b = office.agent(office.owner_b, "Client lead", "lead")

        def runner(owner, name, workspace=None):
            key = secrets.token_hex(32)
            binding = office.api(f"/v1/workspaces/{workspace or office.workspace}/runners", owner, {
                "idempotency_key": str(uuid4()), "name": name, "credential_digest": digest(key),
            })
            return binding, key

        runner_a, token_a = runner(office.owner_a, "Backend checkout host")
        runner_b, token_b = runner(office.owner_b, "Client checkout host")
        second_workspace = office.api("/v1/workspaces", office.owner_a, {
            "name": "Other workspace", "idempotency_key": str(uuid4()),
        })["workspace_id"]
        runner_c, token_c = runner(office.owner_a, "Other workspace runner", second_workspace)

        def project(token, name, visibility="owner_private"):
            return office.tool(token, "office_create_project", idempotency_key=str(uuid4()),
                               name=name, visibility={"kind": visibility})["project"]

        def ask(token, project, runner, key=None, manifest=MANIFEST):
            return office.tool(token, "office_request_checkout", project_id=project["project_id"],
                               host_runner_id=runner["runner_id"], idempotency_key=key or str(uuid4()), manifest=manifest)

        def checkout(token, record):
            return office.tool(token, "office_get_checkout", checkout_id=record["checkout_id"])

        def reserve(token, name, identity):
            return ask(token, project(token, name), identity)["checkout"]

        shared = project(office.owner_a, "Shared product", "workspace")
        private = project(office.owner_b, "Private client project")
        assert office.tool(lead_a, "office_get_project", project_id=private["project_id"])["code"] == "FORBIDDEN"
        assert office.tool(lead_b, "office_get_project", project_id=shared["project_id"]) == shared
        assert office.tool(lead_a, "office_create_project", name="Agent project", idempotency_key=str(uuid4()))["code"] == "FORBIDDEN"
        assert ask(lead_a, shared, runner_a)["code"] == "FORBIDDEN"
        assert ask(office.owner_b, shared, runner_b)["code"] == "FORBIDDEN"
        assert ask(office.owner_a, shared, runner_b)["code"] == "FORBIDDEN"
        assert ask(office.owner_a, shared, runner_a, manifest={"include": [{"kind": "file", "path": ".env"}]})["code"] == "INVALID_INPUT"
        status, rejected = request(office.endpoint, "/v1/checkouts", token=office.owner_a, body={
            "workspace_id": office.workspace, "project_id": shared["project_id"], "host_runner_id": runner_a["runner_id"],
            "idempotency_key": str(uuid4()), "manifest": {"include": [{"kind": "file", "path": "../bad"}]},
        })
        assert status == 400 and rejected["error"]["code"] == "INVALID_INPUT"
        key = str(uuid4())
        pending = ask(office.owner_a, shared, runner_a, key)
        assert pending == ask(office.owner_a, shared, runner_a, key)
        pending = pending["checkout"]
        assert pending["state"] == "pending_registration" and pending["mode"] == "shared" and pending["write_strategy"] == "mediated"
        assert pending["root_ref"] is None and pending["baseline"] is None
        assert ask(office.owner_a, shared, runner_a)["code"] == "INVALID_TRANSITION"
        assert checkout(lead_b, pending)["manifest"] is None
        host_path = f"/v1/runner-checkouts/{pending['checkout_id']}"
        assert request(office.endpoint, host_path, token=token_b)[0] == 403
        assert request(office.endpoint, host_path, token=office.owner_a)[0] == 401
        checks.extend(["project_and_runner_bound_to_owner", "manifest_policy_rejects_reserved_or_invalid_paths",
                       "one_checkout_per_project_with_idempotent_request", "runner_enrollment_requires_its_exact_credential"])

        host = RunnerHost(tmp, guest, binary)
        try:
            host.setup({"a": token_a, "b": token_b, "c": token_c})
            preview = host.inspect("tree/code")
            assert preview.returncode == 0, preview.stderr
            proof = json.loads(preview.stdout)
            assert [item["path"] for item in proof["entries"]] == [".env.example", "nested/lib.rs", "src/app.rs"]
            app = proof["entries"][-1]
            assert app["sha256"] == hashlib.sha256(b"pub fn ping() {}\n").hexdigest()
            assert "FIXTURE_PRIVATE" not in preview.stdout and proof["baseline"]["file_count"] == 3
            assert host.inspect("alias").returncode != 0
            assert "unsupported" in host.inspect("unsafe").stderr
            if guest:
                alias = f"{host.root}/bind-alias"
                host.invoke(["mkdir", "--", alias], check=True)
                host.remote(["sudo", "-n", "mount", "--bind", f"{host.root}/tree/code/nested", alias])
                try:
                    assert "unsupported ancestry" in host.inspect("bind-alias").stderr
                finally:
                    host.remote(["sudo", "-n", "umount", "--", alias])
                checks.append("guest_bind_mount_of_subtree_is_rejected")
            host.replace("tree/code/.env", "Another excluded fixture value\n")
            rendered_hash = proof["baseline"]["manifest_sha256"]
            checks.extend(["real_source_preview_and_hashes_exclude_secrets_and_metadata", "symlink_roots_and_selected_hardlinks_fail_closed"])

            enrolled = host.enroll(office.endpoint, pending, "a", "tree/code", rendered_hash)
            assert enrolled.returncode == 0, enrolled.stderr
            saved = json.loads(enrolled.stdout)
            assert saved["state"] == "registered" and saved["root_ref"] and saved["baseline"] == proof["baseline"]
            assert saved["revision"] == pending["revision"] + 1
            assert json.loads(host.enroll(office.endpoint, pending, "a", "tree/code").stdout) == saved
            assert office.cli(office.owner_a, "checkout", "get", "--workspace", office.workspace,
                              "--id", pending["checkout_id"]) == saved
            outside = checkout(lead_b, pending)
            assert outside["manifest"] is None and outside["baseline"] is None
            assert outside["root_ref"] == saved["root_ref"]
            assert host.root not in enrolled.stdout and "directory_id" not in enrolled.stdout
            checks.extend(["linux_runner_establishes_registry_and_replays_enrollment", "absolute_paths_and_manifest_not_exposed_to_peers"])

            for path in ("tree/code", "tree/code/nested", "tree"):
                duplicate = reserve(office.owner_b, "Conflicting checkout", runner_b)
                refused = host.enroll(office.endpoint, duplicate, "b", path)
                assert refused.returncode != 0 and "CheckoutRootOverlap" in refused.stderr, refused.stderr
                assert checkout(office.owner_b, duplicate)["state"] == "pending_registration"
            checks.append("exact_and_nested_root_conflicts_detected_across_distinct_owners")

            project_elsewhere = office.tool(office.owner_a, "office_create_project", workspace_id=second_workspace,
                                            name="Other project", idempotency_key=str(uuid4()))["project"]
            checkout_elsewhere = office.tool(office.owner_a, "office_request_checkout", workspace_id=second_workspace,
                                             project_id=project_elsewhere["project_id"], host_runner_id=runner_c["runner_id"],
                                             idempotency_key=str(uuid4()), manifest=MANIFEST)["checkout"]
            assert checkout(lead_b, checkout_elsewhere)["code"] == "FORBIDDEN"
            refused = host.enroll(office.endpoint, checkout_elsewhere, "c", "tree/code")
            assert refused.returncode != 0 and "CheckoutRootOverlap" in refused.stderr, refused.stderr
            checks.append("host_root_reservation_spans_workspaces_without_exposing_objects")

            contenders = [(reserve(office.owner_a, "Concurrent A", runner_a), "a"),
                          (reserve(office.owner_b, "Concurrent B", runner_b), "b")]
            with ThreadPoolExecutor(max_workers=2) as pool:
                results = list(pool.map(lambda contender: host.enroll(office.endpoint, *contender, "independent"), contenders))
            assert sum(result.returncode == 0 for result in results) == 1, [result.stderr for result in results]
            assert sum("CheckoutRootOverlap" in result.stderr for result in results) == 1
            checks.append("concurrent_real_enrollments_reserve_one_root")

            host.replace("tree/code/src/app.rs", "Changed after registration\n")
            stale = host.enroll(office.endpoint, pending, "a", "tree/code")
            assert stale.returncode != 0 and "RevisionConflict" in stale.stderr, stale.stderr
            office.restart()
            assert checkout(office.owner_a, pending) == saved
            events = office.tool(lead_b, "office_read_inbox")["events"]
            assert sum(event["event_type"] == "checkout.registered"
                       and event["payload"]["checkout_id"] == saved["checkout_id"] for event in events) == 1
            assert office.sql("SET ROLE cocodeop; SELECT count(*) FROM checkout_roots").splitlines()[-1] == "0"
            checks.append("registered_baseline_and_exactly_one_event_survive_restart")
            if options.checkout_coordinator:
                from m08a_coordinator import verify
                coordinator_checks = verify(office, host, saved, (lead_b, identity_b), token_a, token_b)
            if options.gateway_read:
                from m08a_gateway_read import verify
                gateway_read_checks = verify(office, host, saved, (lead_b, identity_b), options.worker_image_id)
            if options.gateway_write:
                from m08a_gateway_write import prepare_tree, verify
                write_checkout = ask(office.owner_a, project(office.owner_a, "Shared publication", "workspace"),
                                     runner_a)["checkout"]
                write_root = "publication-source"
                prepare_tree(host, write_root)
                enrolled = host.enroll(office.endpoint, write_checkout, "a", write_root)
                assert enrolled.returncode == 0, enrolled.stderr
                try:
                    gateway_write_checks = verify(office, host, checkout(office.owner_a, write_checkout), write_root,
                        (lead_a, identity_a), (lead_b, identity_b), options.worker_image_id,
                        options.gateway_commands, options.gateway_recovery, options.gateway_resources,
                        options.checkpoint_snapshots, options.compensations)
                except Exception:
                    logs = office.compose("logs", "--no-color", "--tail", "30", "db", check=False)
                    for line in logs.stdout.splitlines():
                        if "ERROR:" in line:
                            print(line, file=sys.stderr)
                    raise
            if options.owner_grants:
                from m05a_owner_grants import verify
                grant_checks = verify(office, saved, (lead_a, identity_a), (lead_b, identity_b), tmp)
            if options.work_scopes:
                from m05a_work_scopes import verify
                scope_checks = verify(office, host, saved, lead_a, (lead_b, identity_b))
            if options.areas or options.access or options.scope_revocation or options.decision_links:
                from m05a_areas import verify
                area_checks, shared_area = verify(office, host, saved, (lead_a, identity_a), (lead_b, identity_b), tmp)
            if options.access or options.scope_revocation or options.decision_links:
                from m05a_access import verify
                access_checks, revocation_checks, link_checks = verify(
                    office, host, saved, (lead_a, identity_a), (lead_b, identity_b), shared_area,
                    options.scope_revocation, options.decision_links)
            if options.path_claims:
                from m05a_claims import verify
                claim_checks = verify(office, host, saved, (lead_a, identity_a), (lead_b, identity_b), tmp)
            if options.path_handoffs:
                from m05a_path_handoffs import verify
                handoff_checks = verify(office, host, saved, (lead_a, identity_a), (lead_b, identity_b), tmp)
            if options.decisions:
                from m06_proposals import verify
                decision_checks = verify(office, saved, (lead_a, identity_a), (lead_b, identity_b), tmp)
            if options.scheduler_queue or options.scheduler_leases:
                from m07_queue import verify_checkout_queue
                queue_checks = verify_checkout_queue(office, host, saved, (lead_b, identity_b),
                                                     leases=options.scheduler_leases)
        finally:
            host.close()
    report = {"checks": checks, "linux_execution": "debian13_qemu_guest" if guest else "native_linux",
              "independent_physical_hosts": False}
    if options.owner_grants:
        report["owner_grant_checks"] = grant_checks
    if options.work_scopes:
        report["work_scope_checks"] = scope_checks
    if options.areas:
        report["area_checks"] = area_checks
    if options.access:
        report["access_checks"] = access_checks
    if options.scope_revocation:
        report["scope_revocation_checks"] = revocation_checks
    if options.path_claims:
        report["path_claim_checks"] = claim_checks
    if options.path_handoffs:
        report["path_handoff_checks"] = handoff_checks
    if options.decisions:
        report["decision_checks"] = decision_checks
    if options.decision_links:
        report["decision_link_checks"] = link_checks
    if options.scheduler_queue or options.scheduler_leases:
        report["checkout_queue_checks"] = queue_checks
    if options.checkout_coordinator:
        report["checkout_coordinator_checks"] = coordinator_checks
    if options.gateway_read:
        report["gateway_read_checks"] = gateway_read_checks
    if options.gateway_write:
        report["gateway_write_checks"] = gateway_write_checks
    return report


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--linux-vm", action="store_true")
    parser.add_argument("--runner", type=Path)
    parser.add_argument("--server-image", help="Use a previously built local Core image by exact ID")
    parser.add_argument("--owner-grants", action="store_true")
    parser.add_argument("--work-scopes", action="store_true")
    parser.add_argument("--areas", action="store_true")
    parser.add_argument("--access", action="store_true")
    parser.add_argument("--scope-revocation", action="store_true")
    parser.add_argument("--path-claims", action="store_true")
    parser.add_argument("--path-handoffs", action="store_true")
    parser.add_argument("--decisions", action="store_true")
    parser.add_argument("--decision-links", action="store_true")
    parser.add_argument("--scheduler-queue", action="store_true")
    parser.add_argument("--scheduler-leases", action="store_true")
    parser.add_argument("--checkout-coordinator", action="store_true")
    parser.add_argument("--gateway-read", action="store_true")
    parser.add_argument("--gateway-write", action="store_true")
    parser.add_argument("--gateway-commands", action="store_true")
    parser.add_argument("--gateway-recovery", action="store_true")
    parser.add_argument("--gateway-resources", action="store_true")
    parser.add_argument("--checkpoint-snapshots", action="store_true")
    parser.add_argument("--compensations", action="store_true")
    parser.add_argument("--worker-image-id")
    args = parser.parse_args()
    if args.gateway_recovery or args.gateway_resources or args.checkpoint_snapshots:
        args.gateway_commands = True
    if args.gateway_commands:
        args.gateway_write = True
    if args.compensations:
        if args.gateway_commands:
            parser.error("Run the compensation and command scenarios separately")
        args.gateway_write = True
    if (args.gateway_read or args.gateway_write) and not args.worker_image_id:
        parser.error("Gateway worker tests require --worker-image-id")
    assert args.linux_vm or os.name != "nt", "Use --linux-vm with the Debian topology on Windows"
    binary = (args.runner or ROOT / (".cocodeop-test/linux-bin/office-runner" if args.linux_vm
                                     else "target/release/office-runner")).resolve()
    assert binary.is_file(), f"Build the Linux runner first: {binary}"
    with tempfile.TemporaryDirectory(prefix="cocodeop-registry-") as directory:
        print(json.dumps(run(Path(directory), binary, args)))
