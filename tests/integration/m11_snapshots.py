"""Seal the approved checkout on its Linux host and verify independent Git storage."""

import argparse
import hashlib
import json
import os
import tempfile
from pathlib import Path
from uuid import uuid4

from m05a_registry import MANIFEST, RunnerHost
from m07_leases import register_runner
from m09_native_checkout import checkout_service
from m10_activation import wait_for
from workspace_fixture import ROOT, workspace_fixture


def verify(office, host):
    runner, token = register_runner(office, office.owner_a, "Snapshot checkout host")
    lead, _ = office.agent(office.owner_a, "Snapshot requester", "lead")
    peer, _ = office.agent(office.owner_b, "Other checkout owner", "lead")
    host.setup({"checkout": token})
    root = f"{host.root}/tree/code"
    state = f"{host.root}/controller"
    host.invoke(["rm", "--", f"{root}/.git/config"], check=True)
    host.invoke(["rmdir", "--", f"{root}/.git"], check=True)

    def git(*arguments):
        return host.invoke(["/usr/bin/git", "-C", root, *arguments], check=True).stdout.strip()

    def checkpoint(*arguments):
        return host.invoke(["/usr/bin/git", f"--git-dir={state}/checkpoints.git", *arguments], check=True).stdout.strip()

    git("init", "--quiet", "--initial-branch=main")
    git("add", "--", "src/app.rs")
    git("-c", "user.name=Fixture", "-c", "user.email=fixture@localhost", "commit", "--quiet", "-m", "User baseline")
    original_head = git("rev-parse", "HEAD")
    host.invoke(["chmod", "644", f"{root}/src/app.rs"], check=True)
    dirty = "pub fn ping() { println!(\"external staged baseline\"); }\n"
    host.replace("tree/code/src/app.rs", dirty)
    git("add", "--", "src/app.rs")
    original_index = host.invoke(["sha256sum", f"{root}/.git/index"], check=True).stdout.split()[0]
    assert git("show", "HEAD:src/app.rs") != dirty.strip()

    project = office.tool(office.owner_a, "office_create_project", name="Immutable source",
                          visibility={"kind": "workspace"}, idempotency_key=str(uuid4()))["project"]
    checkout = office.tool(office.owner_a, "office_request_checkout", project_id=project["project_id"],
                           host_runner_id=runner, manifest=MANIFEST, idempotency_key=str(uuid4()))["checkout"]
    enrolled = host.enroll(office.endpoint, checkout, "checkout", "tree/code")
    assert enrolled.returncode == 0, enrolled.stderr
    checkout_id = checkout["checkout_id"]
    with checkout_service(host, office, checkout):
        head = office.tool(lead, "office_get_checkout_head", checkout_id=checkout_id)
        assert head.get("checkout_revision") == 0 and len(head["source_sha256"]) == 64, head
        assert office.tool(peer, "office_get_checkout_head", checkout_id=checkout_id)["code"] == "FORBIDDEN"
        command = {"checkout_id": checkout_id, "expected_checkout_revision": 0, "idempotency_key": str(uuid4())}
        assert office.tool(peer, "office_create_checkout_snapshot", **command)["code"] == "FORBIDDEN"
        conflict = office.tool(lead, "office_create_checkout_snapshot", **dict(command, expected_checkout_revision=1))
        assert conflict["code"] == "REVISION_CONFLICT", conflict
        queued = office.tool(lead, "office_create_checkout_snapshot", **command)
        assert queued.get("state") == "queued", queued
        snapshot = queued["snapshot_id"]
        assert office.tool(lead, "office_create_checkout_snapshot", **command) == queued
        assert office.tool(peer, "office_get_checkout_snapshot", checkout_id=checkout_id, snapshot_id=snapshot)["code"] == "FORBIDDEN"

        def ready():
            result = office.tool(lead, "office_get_checkout_snapshot", checkout_id=checkout_id, snapshot_id=snapshot)
            return result if result.get("state") != "queued" else None

        sealed = wait_for(ready, 25)
        assert sealed["state"] == "ready", (sealed, host.invoke(["cat", f"{state}/stderr"]).stdout)
        proof = sealed["proof"]
        assert proof["baseline"]["manifest_sha256"] == head["source_sha256"]
        assert sealed == office.cli(office.owner_a, "checkout", "snapshot", "--workspace", office.workspace,
                                   "--id", checkout_id, "--snapshot", snapshot)
        ref = f"refs/cocodeop/wip/{snapshot}"
        assert checkpoint("rev-parse", ref) == proof["git_commit_sha256"]
        assert checkpoint("rev-parse", f"{ref}^{{tree}}") == proof["git_tree_sha256"]
        paths = checkpoint("ls-tree", "-r", "--name-only", ref).splitlines()
        assert paths == [".env.example", "nested/lib.rs", "src/app.rs"], paths
        assert checkpoint("show", f"{ref}:src/app.rs") == dirty.strip()
        assert checkpoint("fsck", "--strict") == ""
        assert git("rev-parse", "HEAD") == original_head
        assert host.invoke(["sha256sum", f"{root}/.git/index"], check=True).stdout.split()[0] == original_index
        assert git("show", ":src/app.rs") == dirty.strip()
        host.replace("tree/code/src/app.rs", "pub fn changed_later() {}\n")
        assert checkpoint("show", f"{ref}:src/app.rs") == dirty.strip()
        captured = host.invoke(["cat", f"{state}/snapshots/{snapshot}/source/src/app.rs"], check=True).stdout
        assert hashlib.sha256(captured.encode()).hexdigest() == hashlib.sha256(dirty.encode()).hexdigest()
        host.replace("tree/code/src/app.rs", dirty)
        office.restart()
        assert ready() == sealed
        assert office.sql(f"SELECT count(*) FROM events WHERE event_type='checkout.snapshot_finished' AND payload->>'snapshot_id'='{snapshot}'") == "1"
        assert office.sql("SET ROLE cocodeop; SELECT count(*) FROM checkout_snapshots").splitlines()[-1] == "0"

    return {"snapshot": sealed, "manifest_paths": paths, "git_version": git("--version"),
            "schema": int(office.sql("SELECT max(version) FROM _sqlx_migrations")),
            "linux_execution": "debian13_qemu_guest" if host.guest else "native_linux",
            "checks": ["only_checkout_owner_and_its_lead_request_and_read_capture", "request_binds_content_revision",
                       "private_git_tree_uses_only_the_approved_source_manifest", "dirty_user_baseline_is_preserved",
                       "user_head_and_index_are_unchanged", "snapshot_stays_immutable_when_source_changes",
                       "exact_capture_receipt_survives_server_restart"]}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--linux-vm", action="store_true")
    args = parser.parse_args()
    assert args.linux_vm or os.name != "nt", "Use --linux-vm on Windows"
    binary = ROOT / (".cocodeop-test/linux-bin/office-runner" if args.linux_vm else "target/release/office-runner")
    with tempfile.TemporaryDirectory(prefix="cocodeop-snapshot-") as directory:
        tmp = Path(directory)
        with workspace_fixture(tmp, "snapshot") as office:
            host = RunnerHost(tmp, args.linux_vm, binary)
            try:
                print(json.dumps(verify(office, host)))
            except Exception:
                import sys
                print(office.compose("logs", "--no-color", "--tail", "14", "db").stdout[-4000:], file=sys.stderr)
                raise
            finally:
                host.close()
