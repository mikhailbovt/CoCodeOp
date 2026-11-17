"""Exercise typed publications with two isolated local runtimes and a real checkout host."""

import contextlib
import hashlib
import json
import time
from concurrent.futures import ThreadPoolExecutor
from uuid import uuid4

from m05a_claims import Participant, directory, file, hours
from m07_queue import configure, enqueue
from m08a_coordinator import LAUNCH, SIGNAL
from m08a_gateway_read import read, rpc
from publication_faults import publication_proxy


PREPARE = """
import os, sys
from pathlib import Path
os.umask(0o022)
root = Path(sys.argv[1])
(root / 'src').mkdir(parents=True)
(root / 'src' / 'alpha.txt').write_text('alpha before')
(root / 'src' / 'beta.txt').write_text('beta before')
(root / '.env').write_text('FIXTURE_RESTRICTED')
(root / '.env.example').write_text('DEMO=\\n')
"""

WORKER = r"""
set -eu
test ! -r "$3"
printf '%s\n' "$1" | /usr/local/bin/office-runner worker-mcp > /work/first.jsonl
touch /work/first-ready
until test -e /work/next; do sleep 0.1; done
printf '%s\n' "$2" | /usr/local/bin/office-runner worker-mcp > /work/second.jsonl
touch /work/second-ready
until test -e /work/finish; do sleep 0.1; done
printf '{"kind":"succeeded","summary":"Completed scoped publication checks"}' > "$COCODEOP_RESULT"
"""

CALL_SOCKET = """
import os, subprocess, sys
fd = os.open(sys.argv[1], os.O_DIRECTORY)
try:
    subprocess.run([sys.argv[2], "worker-mcp", "--socket", f"/proc/self/fd/{fd}/mcp.sock"],
                   input=sys.argv[3] + "\\n", text=True, check=True, pass_fds=(fd,))
finally:
    os.close(fd)
"""

PORT_GUARD = """
import json, os, socket, sys, time
from pathlib import Path
with socket.socket() as server:
    server.bind(('127.0.0.1', 0))
    server.listen()
    Path(sys.argv[1]).write_text(json.dumps({
        'port': server.getsockname()[1], 'namespace': os.readlink('/proc/self/ns/net'),
    }))
    limit = time.monotonic() + 180
    until = Path(sys.argv[2])
    while not until.exists() and time.monotonic() < limit:
        time.sleep(.1)
"""

ALPHA = "src/alpha.txt"
BETA = "src/beta.txt"
RENAMED = "src/renamed.txt"
CREATED = "src/new.dat"


def prepare_tree(host, relative_root):
    host.invoke(["python3", "-c", PREPARE, f"{host.root}/{relative_root}"], check=True)


def sha(content):
    return hashlib.sha256(content.encode()).hexdigest()


def text_file(content):
    return {"encoding": "utf8", "content": content}


def replacement(path, before, after):
    return {"kind": "replace", "path": path, "before_sha256": sha(before), "file": text_file(after)}


def write(*operations, key=None, fence=None, dependencies=(), scope_revision=1):
    return {"name": "office_apply_changes", "arguments": {
        "scope_revision": scope_revision, "base_checkout_revision": 0, "idempotency_key": key or str(uuid4()),
        "claim_fence": fence, "operations": list(operations), "read_dependencies": list(dependencies),
    }}


def frames(host, scratch, batch):
    raw = host.invoke(["cat", f"{scratch}/{batch}.jsonl"], check=True).stdout
    return [entry.get("result", {}).get("structuredContent") or entry.get("error")
            for entry in map(json.loads, raw.splitlines())]


def verify(office, host, checkout, root_name, lead_a, lead_b, image_id, commands=False, recovery=False, resources=False,
           snapshots=False, compensations=False):
    fault = (
        publication_proxy(office.endpoint, mode="release") if snapshots
        else publication_proxy(office.endpoint) if recovery
        else contextlib.nullcontext()
    )
    port_guard = host_port(host) if resources else contextlib.nullcontext()
    with fault as proxy, port_guard as local_port:
        return exercise(office, host, checkout, root_name, lead_a, lead_b, image_id, commands, proxy, local_port, snapshots,
                        compensations)


@contextlib.contextmanager
def host_port(host):
    ready = f"{host.root}/bound-port.json"
    release = f"{host.root}/release-port"
    with ThreadPoolExecutor(max_workers=1) as pool:
        listener = pool.submit(host.invoke, ["python3", "-c", PORT_GUARD, ready, release], timeout=200)
        try:
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                assert not listener.done()
                result = host.invoke(["cat", ready])
                if result.returncode == 0 and result.stdout:
                    break
                time.sleep(.1)
            else:
                raise AssertionError("The checkout host did not bind the test port")
            yield json.loads(result.stdout)
        finally:
            host.invoke(["touch", release], check=True)
            stopped = listener.result(timeout=12)
            assert stopped.returncode == 0, stopped.stderr


def exercise(office, host, checkout, root_name, lead_a, lead_b, image_id, commands, proxy, local_port, snapshots,
             compensations):
    root = f"{host.root}/{root_name}"
    state = f"{host.root}/publication-coordinator"
    host.invoke(["mkdir", "-m", "700", state], check=True)
    scope = {"read": {"allow": [directory("src")]}, "write": {"allow": [directory("src")]},
             "write_operations": ["create", "replace", "delete", "rename"]}
    if snapshots:
        scope["read"]["allow"] = checkout["manifest"]["include"]
    command_templates = None
    if commands:
        command_templates, config_file = configure_commands(office, host, checkout, image_id, local_port, snapshots)
        scope["command_template_ids"] = list(command_templates.values())
    owner_a = Participant(office, checkout, lead_a, office.owner_a, "Publish A", scope)
    owner_b = Participant(office, checkout, lead_b, office.owner_b, "Publish B", scope)
    scope_receipt = host.sync_scopes(office.endpoint, checkout, path=root_name, state="publication-coordinator")
    assert scope_receipt.returncode == 0, scope_receipt.stderr

    key = str(uuid4())
    change_a = {"name": "office_replace_text_file", "arguments": {
        "scope_revision": 1, "base_checkout_revision": 0, "idempotency_key": key, "path": ALPHA,
        "before_sha256": sha("alpha before"), "content": "alpha published",
    }}
    change_b = write({"kind": "rename", "source": BETA, "target": RENAMED, "before_sha256": sha("beta before")},
                     {"kind": "create", "path": CREATED, "file": {"encoding": "base64", "content": "AP8CHw=="}})
    delete = {"kind": "delete", "path": RENAMED, "before_sha256": sha("beta before")}
    first_a = [change_a, change_a]
    first_b = [change_b, change_b]
    second_a = [write(replacement(ALPHA, "alpha published", "wrong replay"), key=key), read(CREATED)]
    current_dependencies = ({"path": ALPHA, "sha256": sha("alpha published")},)
    second_b = [{"name": "office_run_context", "arguments": {}}, read(ALPHA, 2),
                write(replacement(ALPHA, "alpha before", "would clobber"), scope_revision=2,
                      dependencies=current_dependencies),
                write(delete, fence=7340032, scope_revision=2, dependencies=current_dependencies),
                write({"kind": "create", "path": ".env", "file": text_file("would overwrite")}, scope_revision=2),
                write(delete, scope_revision=2),
                write(delete, scope_revision=2, dependencies=({"path": ALPHA, "sha256": sha("alpha before")},)),
                write(delete, scope_revision=2, dependencies=current_dependencies)]
    runtime_seconds = 180 if snapshots else 90 if commands or compensations else 45
    transport_timeout = runtime_seconds + 15 if commands or compensations else None

    def configure_worker(label, participant, owner, first, second):
        budget = office.tool(owner, "office_get_execution_budget") or {}
        revision = budget.get("revision", 0)
        assert configure(office, owner, 60, revision).get("budget"), budget
        runner_state = f"{host.root}/publication-{label}"
        host.invoke(["mkdir", "-m", "700", runner_state], check=True)
        policy = {"adapter_id": "file-publisher.v1", "image_id": image_id, "entrypoint": "/bin/sh",
                  "args": ["-c", WORKER, "worker", rpc(first), rpc(second), f"{root}/{ALPHA}"],
                  "timeout_seconds": runtime_seconds, "memory_mib": 128, "cpus": 1,
                  "pids_limit": 64,
                  "run_mcp": True, "checkout_gateway": True}
        manifest_name = f"publication-manifest-{label}.json"
        manifest_path = f"{host.root}/{manifest_name}"
        host.replace(manifest_name, json.dumps(policy))
        host.invoke(["chmod", "600", manifest_path], check=True)
        reviewed = host.invoke([host.installed, "runtime-manifest", "--file", manifest_path, "--json"], check=True)
        runtime = office.tool(owner, "office_create_runtime_template", name=f"Publisher {label}", kind="managed",
                              managed={"adapter_id": policy["adapter_id"], "reservation_units": 10,
                                       "manifest_sha256": json.loads(reviewed.stdout)["manifest_sha256"]},
                              idempotency_key=str(uuid4()))
        profile = f"publication-profile-{label}.json"
        host.replace(profile, json.dumps({"capabilities": ["linux"], "runtimes": [
            {"template_id": runtime["template_id"], "manifest": policy}]}))
        host.invoke(["chmod", "600", f"{host.root}/{profile}"], check=True)
        prepared = office.tool(participant.lead, "office_prepare_task", task_id=participant.task,
                               expected_revision=participant.task_revision, idempotency_key=str(uuid4()))["task"]
        job = enqueue(office, participant.lead, prepared, participant.identity, runtime["template_id"])
        assert job.get("job"), job
        return runner_state, profile

    settings_a = configure_worker("a", owner_a, office.owner_a, first_a, second_a)
    settings_b = configure_worker("b", owner_b, office.owner_b, first_b, second_b)

    def coordinator(reconcile=False):
        forward, endpoint = host.connection_for_runner(proxy.endpoint if proxy is not None else office.endpoint)
        configured = ["--commands-file", config_file] if commands else []
        if reconcile:
            configured.append("--reconcile")
        return host.invoke(["python3", "-c", LAUNCH, f"{state}/stdout", f"{state}/stderr", f"{state}/pid",
                            host.installed, "checkout", "serve", "--checkout", checkout["checkout_id"],
                            "--root", root, "--state-dir", state, "--token-file", f"{host.root}/a.token",
                            "--url", f"http://{endpoint}", "--json", *configured], forward=forward,
                           timeout=transport_timeout)

    def execute(label, settings):
        worker_state, profile = settings
        forward, endpoint = host.connection_for_runner(office.endpoint)
        return host.invoke([host.installed, "serve", "--file", f"{host.root}/{profile}",
                            "--state-dir", worker_state, "--token-file", f"{host.root}/{label}.token",
                            "--url", f"http://{endpoint}", "--once", "--json"], forward=forward,
                           timeout=transport_timeout)

    def wait_for(fetch, seconds):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if result := fetch():
                return result
            time.sleep(0.16)
        raise AssertionError("The shared publication did not reach its expected state")

    def service_ready(future, after_epoch=0):
        if future.done():
            raise AssertionError(host.invoke(["cat", f"{state}/stderr"]).stdout)
        line = host.invoke(["head", "-n", "1", f"{state}/stdout"]).stdout.strip()
        if line:
            lease = json.loads(line)
            if lease["epoch"] > after_epoch:
                return lease
        return None

    def signal(name):
        return host.invoke(["python3", "-c", SIGNAL, f"{state}/pid", host.installed, state, name], check=True)

    def get_scratch(participant, worker_state, future):
        if future.done():
            result = future.result()
            raise AssertionError((result.stdout, result.stderr))
        runs = office.tool(participant.worker, "office_get_task", task_id=participant.task)["runs"]
        if not runs:
            return None
        scratch = f"{worker_state}/run-{runs[0]['run_id']}/scratch"
        return scratch if host.invoke(["test", "-f", f"{scratch}/first-ready"]).returncode == 0 else None

    def worker_call(scratch, name, arguments):
        run_path = scratch.rsplit("/", 1)[0]
        wire = rpc([{"name": name, "arguments": arguments}])
        result = host.invoke(["python3", "-c", CALL_SOCKET, run_path, host.installed, wire], check=True)
        response = json.loads(result.stdout)
        assert "result" in response, response
        return response["result"]["structuredContent"]

    def claim_call(scratch, target, paths, revision, ttl=12):
        return worker_call(scratch, "office_claim_paths", {
            **target, "scope_revision": revision, "paths": [{"kind": "file", "path": path} for path in paths],
            "ttl_seconds": ttl, "idempotency_key": str(uuid4()),
        })

    def capture_snapshot(revision):
        queued = office.tool(office.owner_a, "office_create_checkout_snapshot", checkout_id=checkout["checkout_id"],
                             expected_checkout_revision=revision, idempotency_key=str(uuid4()))
        assert queued.get("state") == "queued", queued

        def captured():
            snapshot = office.tool(office.owner_a, "office_get_checkout_snapshot", checkout_id=checkout["checkout_id"],
                                   snapshot_id=queued["snapshot_id"])
            return snapshot if snapshot.get("state") != "queued" else None

        snapshot = wait_for(captured, 25)
        assert snapshot["state"] == "ready", snapshot
        return snapshot["snapshot_id"]

    checks = []
    with ThreadPoolExecutor(max_workers=3) as pool:
        service = pool.submit(coordinator)
        scratch = []
        try:
            lease = wait_for(lambda: service_ready(service), 15)
            sealed_baseline = capture_snapshot(0) if snapshots else None
            running = [pool.submit(execute, label, settings) for label, settings in (("a", settings_a), ("b", settings_b))]
            for participant, settings, future in zip((owner_a, owner_b), (settings_a, settings_b), running):
                scratch.append(wait_for(lambda: get_scratch(participant, settings[0], future), 22))
            first = [frames(host, location, "first") for location in scratch]
            for (receipt, repeated), participant in zip(first, (owner_a, owner_b)):
                assert receipt == repeated and "change_id" in receipt, first
                assert receipt["author"]["agent_id"] == participant.identity["agent_id"]
                assert receipt["coordinator_epoch"] == lease["epoch"]
                assert receipt["host_runner_id"] == lease["runner_id"] == checkout["host_runner_id"]
                assert receipt["claim_fence"] > 0 and receipt["run_fence"] > 0
            assert {batch[0]["checkout_revision"] for batch in first} == {1, 2}, first
            assert first[1][0]["files"][0] == {"path": BETA, "before_sha256": sha("beta before"), "after_sha256": None}
            assert host.invoke(["cat", f"{root}/{RENAMED}"], check=True).stdout == "beta before"
            checks.append("two_owner_runtimes_publish_disjoint_changes_with_exact_hash_and_host_receipts")
            checks.append("replaying_an_accepted_operation_does_not_rewrite_files_or_advance_revision")

            target_a = {"workspace_id": office.workspace, "task_id": owner_a.task, "run_id": first[0][0]["run_id"]}
            target_b = {"workspace_id": office.workspace, "task_id": owner_b.task, "run_id": first[1][0]["run_id"]}
            assert claim_call(scratch[1], target_a, [RENAMED], 1)["code"] == "FORBIDDEN"
            a_claim = claim_call(scratch[0], target_a, [RENAMED], 1, ttl=8)["claim"]
            fence_a = a_claim["claim_fence"]
            assert claim_call(scratch[1], target_b, [RENAMED], 1)["code"] == "PATH_CLAIMED"
            renewed = worker_call(scratch[0], "office_renew_claim", {
                **target_a, "claim_fence": fence_a, "ttl_seconds": 20, "idempotency_key": str(uuid4()),
            })["claim"]
            assert renewed["claim_fence"] == fence_a and renewed["renewals"] == 1
            release_a = {**target_a, "claim_fence": fence_a, "idempotency_key": str(uuid4())}
            assert office.tool(owner_a.worker, "office_release_claim", **release_a)["code"] == "RUN_LEASE_STALE"
            assert worker_call(scratch[0], "office_release_claim", release_a)["claim"]["state"] == "released"
            checks.append("managed_claims_enforce_same_attempt_fences_and_exclusive_paths_between_owners")

            a_claim = claim_call(scratch[0], target_a, [ALPHA], 1, ttl=60)["claim"]
            paths = [file(ALPHA)]
            request = {**target_b, "scope_revision": 1, "paths": paths,
                       "reason": "Publish after the other owner's changes",
                       "idempotency_key": str(uuid4())}
            name = "office_request_path_handoff"
            assert worker_call(scratch[1], name, {**request, **target_a})["code"] == "FORBIDDEN"
            assert claim_call(scratch[1], target_b, [ALPHA], 1)["code"] == "PATH_CLAIMED"
            requested = worker_call(scratch[1], name, request)["handoff"]
            assert requested["state"] == "requested" and requested["paths"] == paths
            assert worker_call(scratch[1], name, request)["handoff"] == requested
            lookup = {"workspace_id": office.workspace, "handoff_id": requested["handoff_id"]}
            assert worker_call(scratch[0], "office_get_path_handoff", lookup) == requested
            assert office.tool(owner_a.lead, "office_get_path_handoff", **lookup)["code"] == "FORBIDDEN"
            inbox = worker_call(scratch[0], "office_run_work_context", {"limit": 25})["inbox"]["events"]
            notices = [event["payload"] for event in inbox if event["event_type"] == "path.handoff_requested"]
            assert len(notices) == 1 and notices[0]["handoff_id"] == requested["handoff_id"], notices
            assert "task_id" not in notices[0] and "run_id" not in notices[0]
            respond = {**lookup, "expected_revision": requested["revision"], "decision": "offer",
                       "idempotency_key": str(uuid4())}
            assert worker_call(scratch[1], "office_respond_path_handoff", respond)["code"] == "FORBIDDEN"
            offered = worker_call(scratch[0], "office_respond_path_handoff", respond)["handoff"]
            assert offered["state"] == "offered"
            assert worker_call(scratch[0], "office_respond_path_handoff", respond)["handoff"] == offered
            assert claim_call(scratch[0], target_a, [ALPHA], 1)["code"] == "PATH_CLAIMED"
            accepted = worker_call(scratch[1], "office_claim_paths", {
                **target_b, "scope_revision": 1, "paths": paths, "ttl_seconds": 30,
                "handoff_id": offered["handoff_id"], "idempotency_key": str(uuid4()),
            })["claim"]
            assert accepted["claim_fence"] > a_claim["claim_fence"] and accepted["run_id"] == target_b["run_id"]
            assert worker_call(scratch[1], "office_get_path_handoff", lookup)["state"] == "accepted"
            stale = {**change_a["arguments"], "idempotency_key": str(uuid4()),
                     "claim_fence": accepted["claim_fence"], "content": "would overwrite"}
            assert worker_call(scratch[1], "office_replace_text_file", stale)["code"] == "CONTENT_CONFLICT"
            fresh = read(ALPHA)
            visible = worker_call(scratch[1], fresh["name"], fresh["arguments"])
            assert visible["file"]["content"] == "alpha published" and visible["file"]["sha256"] == sha("alpha published")
            assert worker_call(scratch[1], "office_release_claim", {
                **target_b, "claim_fence": accepted["claim_fence"], "idempotency_key": str(uuid4()),
            })["claim"]["state"] == "released"
            checks.append("managed_handoff_is_delivered_and_consumed_only_by_the_two_active_attempts")
            checks.append("single_file_mcp_replacement_replays_and_enforces_the_same_before_hash")

            task_b = office.tool(owner_b.worker, "office_get_task", task_id=owner_b.task)["task"]
            changed = owner_b.propose_scope(task_b["revision"], expected_scope_revision=1, required_read_paths=[ALPHA])
            assert changed["scope"]["definition"]["required_read_paths"] == [ALPHA], changed
            def active_required_scope():
                result = office.tool(owner_b.lead, "office_get_work_scope", task_id=owner_b.task, revision=2)
                return result if result.get("state") == "active" else None
            wait_for(active_required_scope, 10)

            for location in scratch:
                host.invoke(["touch", f"{location}/next"], check=True)
            for location in scratch:
                wait_for(lambda: host.invoke(["test", "-f", f"{location}/second-ready"]).returncode == 0, 18)
            a, b = [frames(host, location, "second") for location in scratch]
            assert a[0]["code"] == "IDEMPOTENCY_CONFLICT" and a[1]["file"]["content"] == "AP8CHw==", a
            assert b[0]["scope_revision"] == 2 and b[0]["required_read_paths"] == [ALPHA], b
            assert b[1]["file"]["content"] == "alpha published", b
            assert [entry["code"] for entry in b[2:7]] == ["CONTENT_CONFLICT", "CLAIM_FENCE_STALE", "PATH_FORBIDDEN",
                                                          "CONTEXT_STALE", "CONTEXT_STALE"], b
            assert b[7]["checkout_revision"] == 3 and b[7]["host_runner_id"] == checkout["host_runner_id"], b
            assert b[7]["run_id"] == first[1][0]["run_id"] and b[7]["scope_revision"] == 2
            assert b[7]["read_dependencies"] == list(current_dependencies)
            explicit_path = "src/claimed.txt"
            claim_b = claim_call(scratch[1], target_b, [explicit_path], 2)["claim"]
            explicit = write({"kind": "create", "path": explicit_path, "file": text_file("explicit claim")},
                             fence=claim_b["claim_fence"], scope_revision=2, dependencies=current_dependencies)
            receipt = worker_call(scratch[1], explicit["name"], explicit["arguments"])
            assert receipt["claim_fence"] == claim_b["claim_fence"] and receipt["checkout_revision"] == 4, receipt
            assert host.invoke(["cat", f"{root}/{explicit_path}"], check=True).stdout == "explicit claim"
            wrong_dependency = {
                "scope_revision": 2, "base_checkout_revision": 4, "idempotency_key": str(uuid4()),
                "path": explicit_path, "claim_fence": claim_b["claim_fence"],
                "before_sha256": sha("explicit claim"), "content": "stale dependency",
                "dependency_path": ALPHA, "dependency_sha256": sha("alpha before"),
            }
            assert worker_call(scratch[1], "office_replace_text_file", wrong_dependency)["code"] == "CONTEXT_STALE"
            assert host.invoke(["cat", f"{root}/{explicit_path}"], check=True).stdout == "explicit claim"
            checks.append("single_file_replacement_checks_the_required_dependency_hash")
            assert worker_call(scratch[1], "office_release_claim", {
                **target_b, "claim_fence": claim_b["claim_fence"], "idempotency_key": str(uuid4()),
            })["claim"]["state"] == "released"
            checks.append("publication_honors_a_claim_created_by_its_running_worker")
            if compensations:
                original = first[0][0]["change_id"]
                later = worker_call(scratch[1], "office_replace_text_file", {
                    "scope_revision": 2, "base_checkout_revision": 4, "idempotency_key": str(uuid4()),
                    "path": ALPHA, "before_sha256": sha("alpha published"), "content": "alpha published",
                    "dependency_path": ALPHA, "dependency_sha256": sha("alpha published"),
                })
                assert later.get("checkout_revision") == 5, later
                reverse_a = write(replacement(ALPHA, "alpha published", "alpha before"))
                reverse_a["arguments"]["compensates_change_id"] = original
                response = worker_call(scratch[0], reverse_a["name"], reverse_a["arguments"])
                assert response["code"] == "CONTENT_CONFLICT", response
                rejected = worker_call(scratch[1], reverse_a["name"], dict(
                    reverse_a["arguments"], scope_revision=2, read_dependencies=list(current_dependencies)))
                assert rejected["code"] == "FORBIDDEN", rejected
                reverse_b = write({"kind": "delete", "path": explicit_path, "before_sha256": sha("explicit claim")},
                                  scope_revision=2, dependencies=current_dependencies)
                reverse_b["arguments"]["compensates_change_id"] = receipt["change_id"]
                undo = worker_call(scratch[1], reverse_b["name"], reverse_b["arguments"])
                assert undo.get("compensates_change_id") == receipt["change_id"] and undo["checkout_revision"] == 6, undo
                assert worker_call(scratch[1], reverse_b["name"], reverse_b["arguments"]) == undo
                assert host.invoke(["test", "!", "-e", f"{root}/{explicit_path}"]).returncode == 0
                assert host.invoke(["cat", f"{root}/{ALPHA}"], check=True).stdout == "alpha published"
                checks.extend(["compensation_rejects_a_later_owner_change_even_when_the_content_hash_matches",
                               "only_the_original_actor_can_compensate_that_change",
                               "compensation_is_a_new_scoped_publication_and_preserves_unrelated_files"])
            if command_templates is not None:
                if local_port is not None:
                    verify_resources(lambda name, arguments: worker_call(scratch[0], name, arguments),
                                     lambda name, arguments: worker_call(scratch[1], name, arguments),
                                     wait_for, command_templates["resources"], checkout["host_runner_id"],
                                     local_port["namespace"])
                    checks.append("two_checkout_commands_reuse_the_same_port_and_database_names_with_kernel_and_scratch_isolation")
                checks.extend(
                    verify_commands(lambda name, arguments: worker_call(scratch[1], name, arguments),
                                    wait_for, command_templates, checkout["host_runner_id"], target_b["run_id"],
                                    lease["epoch"], proxy.armed.set if proxy is not None and not snapshots else None,
                                    sealed_baseline, image_id)
                )
                if proxy is not None and not snapshots:
                    assert proxy.dropped.wait(5), "The host received the publication acknowledgement"
                    assert service.result(timeout=12).returncode != 0
                    assert host.invoke(["test", "-f", f"{state}/publication-pending.json"]).returncode == 0
                    proxy.armed.clear()
                    service = pool.submit(coordinator)
                    lease = wait_for(lambda: service_ready(service, lease["epoch"]), 12)
                    assert host.invoke(["test", "!", "-e", f"{state}/publication-pending.json"]).returncode == 0
                    checks.append("lost_http_ack_after_core_commit_recovers_the_same_revision_without_republication")
            sealed_full, checks_by_worker = None, []
            if snapshots:
                sealed_full = capture_snapshot(5)
                for location, revision in zip(scratch, (1, 2)):
                    started = worker_call(location, "office_run_command", {
                        "scope_revision": revision, "template_id": command_templates["contract"],
                        "source_snapshot_id": sealed_full, "idempotency_key": str(uuid4()),
                    })
                    assert started.get("command_id"), started

                    def proof():
                        value = worker_call(location, "office_get_command", {
                            "scope_revision": revision, "command_id": started["command_id"],
                        })
                        return value.get("outcome") if value.get("state") == "finished" else None

                    result = wait_for(proof, 18)
                    assert result.get("exit_code") == 0 and result["stdout"].strip() == "contract passed", result
                    checks_by_worker.append(started["command_id"])
            assert host.invoke(["test", "!", "-e", f"{root}/{RENAMED}"]).returncode == 0
            expected_alpha = "alpha formatted" if commands else "alpha published"
            assert host.invoke(["cat", f"{root}/{ALPHA}"], check=True).stdout == expected_alpha
            assert host.invoke(["test", "!", "-e", f"{root}/src/untracked.txt"]).returncode == 0
            assert host.invoke(["cat", f"{root}/.env"], check=True).stdout == "FIXTURE_RESTRICTED"
            checks.append("same_path_stale_claim_protected_path_and_stale_dependency_are_rejected")
            checks.append("mandatory_read_paths_activate_in_the_running_attempt_and_cannot_be_omitted")
            checks.append("each_worker_reads_the_other_owner_publication_and_older_unrelated_revision_is_admissible")
            current_task = office.tool(owner_b.worker, "office_get_task", task_id=owner_b.task)["task"]
            access_request = {
                "workspace_id": office.workspace, "task_id": owner_b.task,
                "expected_task_revision": current_task["revision"], "expected_scope_revision": 2,
                "idempotency_key": str(uuid4()), "reason": "Restrict the remaining write access to the staged file",
                "permissions": dict(owner_b.permissions, write={"allow": [file(explicit_path)]}, write_operations=["replace"]),
                "expires_at": hours(1), "scope_expires_at": hours(2),
            }
            assert worker_call(scratch[1], "office_request_access", dict(access_request, task_id=owner_a.task))["code"] == "FORBIDDEN"
            created = worker_call(scratch[1], "office_request_access", access_request)["request"]
            assert created["run_id"] == target_b["run_id"] and created["requester"]["agent_id"] == owner_b.identity["agent_id"]
            view_request = {"workspace_id": office.workspace, "access_request_id": created["access_request_id"]}
            assert worker_call(scratch[1], "office_get_access_request", view_request) == created
            denied = worker_call(scratch[0], "office_get_access_request", view_request)
            assert denied["code"] == "FORBIDDEN"
            approval = office.tool(owner_b.lead, "office_respond_access", access_request_id=created["access_request_id"],
                                   expected_revision=created["revision"], idempotency_key=str(uuid4()), decision="approve")
            assert approval["request"]["state"] == "approved_pending_apply"
            def applied_request():
                view = worker_call(scratch[1], "office_get_access_request", view_request)
                return view if view["state"] == "approved" else None

            installed = wait_for(applied_request, 10)
            assert installed["issued_scope_revision"] == 3
            updated_context = worker_call(scratch[1], "office_run_context", {})
            assert updated_context["run_id"] == target_b["run_id"] and updated_context["scope_revision"] == 3
            assert updated_context["required_read_paths"] == [ALPHA]
            checks.append("managed_access_requests_stay_bound_to_their_run_with_approved_scope_activation")
            for location in scratch:
                host.invoke(["touch", f"{location}/finish"], check=True)
            for future in running:
                completed = future.result(timeout=15)
                assert completed.returncode == 0, completed.stderr
                assert json.loads(completed.stdout)["receipt"]["execution"]["run"]["status"] == "succeeded"
            if snapshots:
                def set_release_policy(actor, template):
                    return office.tool(actor, "office_set_checkout_release_policy", checkout_id=checkout["checkout_id"],
                                       expected_revision=0, required_template_ids=[template], idempotency_key=str(uuid4()))

                assert set_release_policy(owner_b.lead, command_templates["contract"])["code"] == "FORBIDDEN"
                assert set_release_policy(owner_a.lead, command_templates["snapshot"])["code"] == "INVALID_INPUT"
                policy = set_release_policy(owner_a.lead, command_templates["contract"])
                assert policy.get("revision") == 1, policy
                checks.append("only_the_checkout_owner_or_their_lead_can_bind_a_full_source_template_to_release")
                release = {"checkout_id": checkout["checkout_id"], "snapshot_id": sealed_full,
                           "expected_policy_revision": 1, "expected_commit_sha256": None,
                           "acknowledge_external_baseline": True, "command_ids": checks_by_worker}

                def request_release(**changes):
                    return office.tool(owner_a.lead, "office_request_checkout_release",
                                       **dict(release, idempotency_key=str(uuid4()), **changes))

                assert request_release()["code"] == "DEPENDENCY_NOT_READY"
                for participant, outsider, own, independent in (
                    (owner_a, owner_b, checks_by_worker[0], checks_by_worker[1]),
                    (owner_b, owner_a, checks_by_worker[1], checks_by_worker[0]),
                ):
                    binding = {"task_id": participant.task, "snapshot_id": sealed_full, "command_ids": [independent]}
                    denied = office.tool(outsider.lead, "office_get_checkout_review", **binding)
                    assert denied["code"] == "FORBIDDEN", denied
                    assert office.tool(participant.lead, "office_get_checkout_review", **dict(
                        binding, command_ids=[own]))["code"] == "INVALID_INPUT"
                    wrong_version = office.tool(participant.lead, "office_get_checkout_review", **dict(
                        binding, snapshot_id=sealed_baseline))
                    assert wrong_version["code"] == "INVALID_TRANSITION", wrong_version
                    reviewed = office.tool(participant.lead, "office_get_checkout_review", **binding)
                    assert reviewed.get("content", {}).get("contributions"), reviewed
                    evidence = reviewed["content"]["checks"][0]
                    assert evidence["command_id"] == independent and evidence["exit_code"] == 0
                    assert evidence["source_sha256"] == reviewed["content"]["head"]["source_sha256"]
                    approval = {**binding, "reason": "The immutable contract verification passed.",
                                "revision_sha256": reviewed["revision_sha256"], "idempotency_key": str(uuid4())}
                    stale = office.tool(participant.lead, "office_approve_checkout_review", **dict(approval, revision_sha256="0" * 64))
                    assert stale["code"] == "REVISION_CONFLICT", stale
                    accepted = office.tool(participant.lead, "office_approve_checkout_review", **approval)
                    assert accepted.get("review") == reviewed, accepted
                    assert office.tool(participant.lead, "office_approve_checkout_review", **approval) == accepted
                    assert office.tool(participant.lead, "office_get_task", task_id=participant.task)["task"]["status"] == "done"
                    if participant is owner_a:
                        assert request_release()["code"] == "DEPENDENCY_NOT_READY"
                checks.append("both_task_reviews_bind_independent_full_snapshot_checks_and_each_attributed_change")
                assert request_release(snapshot_id=sealed_baseline)["code"] == "CONTEXT_STALE"
                assert request_release(expected_commit_sha256="f" * 64)["code"] == "REVISION_CONFLICT"
                assert request_release(acknowledge_external_baseline=False)["code"] == "INVALID_INPUT"
                repo = ["/usr/bin/git", f"--git-dir={state}/checkpoints.git"]
                already = host.invoke([*repo, "rev-parse", f"refs/cocodeop/wip/{sealed_baseline}"], check=True).stdout.strip()
                host.invoke([*repo, "update-ref", "refs/cocodeop/accepted", already, "0" * 64], check=True)

                def released(requested):
                    assert requested.get("state") == "queued", requested
                    item = office.tool(office.owner_a, "office_get_checkout_release", checkout_id=checkout["checkout_id"],
                                       release_id=requested["release_id"])
                    return item if item.get("state") not in ("queued", "applying") else None

                conflicted = request_release()
                assert wait_for(lambda: released(conflicted), 20)["state"] == "ref_conflict"
                assert host.invoke([*repo, "rev-parse", "refs/cocodeop/accepted"], check=True).stdout.strip() == already
                host.invoke([*repo, "update-ref", "-d", "refs/cocodeop/accepted", already], check=True)
                proxy.armed.set()
                submitted = request_release()
                assert submitted.get("state") == "queued", submitted
                assert proxy.dropped.wait(7), "The coordinator did not report the locally applied ref"
                pending = office.tool(office.owner_a, "office_get_checkout_release", checkout_id=checkout["checkout_id"],
                                      release_id=submitted["release_id"])
                assert pending["state"] == "applying", pending
                local_commit = host.invoke([*repo, "rev-parse", "refs/cocodeop/accepted"], check=True).stdout.strip()
                assert len(local_commit) == 64 and local_commit != already
                assert service.result(timeout=12).returncode != 0
                proxy.armed.clear()
                service = pool.submit(coordinator)
                lease = wait_for(lambda: service_ready(service, lease["epoch"]), 18)
                released_head = wait_for(lambda: released(submitted), 20)
                assert released_head.get("state") == "accepted" and released_head["approved_changes"] == 5, released_head
                proof = released_head["outcome"]
                assert proof["commit_sha256"] == local_commit
                assert office.sql("SELECT count(*) FROM events WHERE event_type='checkout.release_finished' "
                                  f"AND payload->>'release_id'='{submitted['release_id']}'") == "1"
                assert host.invoke([*repo, "rev-parse", "refs/cocodeop/accepted"], check=True).stdout.strip() == proof["commit_sha256"]
                assert host.invoke([*repo, "rev-parse", f"refs/cocodeop/wip/{sealed_full}^{{tree}}"], check=True).stdout.strip() == proof["tree_sha256"]
                assert office.tool(office.owner_a, "office_get_checkout_release_policy", checkout_id=checkout["checkout_id"])[
                    "accepted_commit_sha256"] == proof["commit_sha256"]
                checks.extend(["release_stays_closed_with_one_unreviewed_task_or_an_old_source_snapshot",
                               "git_accepted_ref_advances_only_on_expected_old_value",
                               "accepted_checkpoint_contains_all_five_approved_changes_and_the_checked_tree",
                               "restart_after_local_ref_update_preserves_exact_commit_and_reports_once"])
            query = f"WHERE payload->>'checkout_id'='{checkout['checkout_id']}' AND event_type='checkout.changed'"
            expected_changes = "6" if compensations else ("5" if commands else "4")
            assert office.sql(f"SELECT count(*) FROM events {query}").splitlines()[-1] == expected_changes
            page = office.tool(office.owner_a, "office_read_inbox", limit=100)
            notices = page["events"]
            while page["has_more"]:
                page = office.tool(office.owner_a, "office_read_inbox", limit=100, cursor=page["next_cursor"])
                notices.extend(page["events"])
            changes = [event["payload"] for event in notices
                       if event["event_type"] == "checkout.changed" and event["payload"]["checkout_id"] == checkout["checkout_id"]]
            assert {item["checkout_revision"] for item in changes} == {first[0][0]["checkout_revision"], int(expected_changes)}, changes
            assert len(changes) == 2 and all("task_id" not in item and "path" not in item for item in changes), changes
            checks.append("checkout_inbox_coalesces_each_task_and_directory_without_hiding_publication_history")
            assert office.sql("SET ROLE cocodeop; SELECT count(*) FROM checkout_publications").splitlines()[-1] == "0"
            signal("SIGKILL")
            assert service.result(timeout=10).returncode != 0
            service = pool.submit(coordinator)
            recovered = wait_for(lambda: service_ready(service, lease["epoch"]), 12)
            assert recovered["epoch"] > lease["epoch"]
            assert host.invoke(["cat", f"{root}/{ALPHA}"], check=True).stdout == expected_alpha
            assert office.sql(f"SELECT count(*) FROM events {query}").splitlines()[-1] == expected_changes
            checks.append("same_journal_restores_verified_checkout_before_announcing_new_epoch")
            if proxy is not None and not snapshots:
                signal("SIGTERM")
                assert service.result(timeout=12).returncode == 0
                host.replace(f"{root_name}/{ALPHA}", "unexpected local content")
                service = pool.submit(coordinator)
                assert service.result(timeout=15).returncode != 0
                assert host.invoke(["cat", f"{root}/{ALPHA}"], check=True).stdout == "unexpected local content"
                blocked_sql = f"SELECT blocked FROM checkout_content_heads WHERE checkout_id='{checkout['checkout_id']}'"
                assert office.sql(blocked_sql).splitlines()[-1] == "t"
                service = pool.submit(coordinator, True)
                assert service.result(timeout=15).returncode != 0
                assert host.invoke(["cat", f"{root}/{ALPHA}"], check=True).stdout == "unexpected local content"
                host.replace(f"{root_name}/{ALPHA}", expected_alpha)
                service = pool.submit(coordinator)
                assert service.result(timeout=15).returncode != 0
                service = pool.submit(coordinator, True)
                repaired = wait_for(lambda: service_ready(service, recovered["epoch"]), 15)
                assert repaired["epoch"] > recovered["epoch"]
                assert office.sql(blocked_sql).splitlines()[-1] == "f"
                assert office.sql(f"SELECT count(*) FROM events {query}").splitlines()[-1] == expected_changes
                checks.append("out_of_band_edit_is_preserved_and_only_verified_owner_repair_lifts_maintenance")
        finally:
            for location in scratch:
                host.invoke(["touch", f"{location}/next", f"{location}/finish"])
            if not service.done():
                signal("SIGTERM")
    return checks


def configure_commands(office, host, checkout, image_id, local_port, snapshots=False):
    script = (
        "test ! -r .env && test ! -e src/new.dat && test ! -e /run/cocodeop/mcp.sock "
        "&& test -z \"${OFFICE_TOKEN-}\" && test \"$(wc -l < /proc/net/route)\" -eq 1 "
        "&& ! (printf 'forbidden' > src/alpha.txt) 2>/dev/null && cat src/alpha.txt "
        "&& printf '%s' \"$1\" > \"$COCODEOP_OUTPUT/result.txt\" "
        "&& printf '%s' private > \"$COCODEOP_OUTPUT/.env\" && printf '%s' \"$1\" >&2"
    )
    configuration = []
    ids = {}
    format_script = (
        "test \"$(cat /snapshot/src/alpha.txt)\" = 'alpha published' "
        "&& printf 'alpha formatted' > src/alpha.txt && printf 'untracked' > src/untracked.txt"
    )
    templates = [
        ("snapshot", [{"kind": "literal", "value": item} for item in ("-c", script, "snapshot")]
         + [{"kind": "choice", "key": "marker", "values": ["owner-approved"]}], 8, False),
        ("timeout", [{"kind": "literal", "value": item} for item in ("-c", "sleep 45")], 1, False),
        ("formatter", [{"kind": "literal", "value": item} for item in ("-c", format_script)], 8, True),
    ]
    if snapshots:
        command = (
            "test \"$(cat src/alpha.txt)\" = 'alpha formatted' && test ! -e .env "
            "&& test ! -e src/beta.txt && test -r src/claimed.txt && printf 'contract passed\\n'"
        )
        templates.append(("contract", [{"kind": "literal", "value": item} for item in ("-ec", command)], 12, False))
    if local_port is not None:
        resource_script = (
            "import os,socket,sqlite3,time;e=os.environ;"
            "assert os.path.isdir(e['COCODEOP_BUILD_CACHE']) and os.path.isdir(e['COCODEOP_PACKAGE_CACHE']);"
            f"s=socket.socket();s.bind(('127.0.0.1',{local_port['port']}));s.listen();"
            "db=sqlite3.connect(e['COCODEOP_TEST_DB_DIR']+'/test.sqlite');"
            "db.execute('create table trials (id int)');db.execute('insert into trials values (1)');db.commit();"
            "print(db.execute('select count(*) from trials').fetchone()[0],os.readlink('/proc/self/ns/net'));"
            "time.sleep(.5)"
        )
        assert len(resource_script.encode()) <= 512
        templates.append(("resources", [{"kind": "literal", "value": item} for item in ("-c", resource_script)], 12, False))
    for label, arguments, timeout, staging in templates:
        resource_case = label == "resources"
        opts = {"resources": {"build_cache": True, "package_cache": True, "test_database": True}} if resource_case else {}
        template = office.tool(office.owner_a, "office_create_command_template", checkout_id=checkout["checkout_id"],
                               expected_revision=checkout["revision"], idempotency_key=str(uuid4()), template={
                                   "name": f"Verify {label}", "executable": "python3" if resource_case else "sh",
                                   "arguments": arguments,
                                   "source_inputs": checkout["manifest"]["include"] if label == "contract" else [{"kind": "file", "path": ALPHA}],
                                   "scratch_outputs": [{"kind": "file", "path": ALPHA if staging else "result.txt"}],
                                   "timeout_seconds": timeout, "writable_staging": staging, **opts,
                               })
        assert template.get("template_id"), (label, template)
        ids[label] = template["template_id"]
        template_file = f"command-{label}.json"
        host.replace(template_file, json.dumps(template))
        host.invoke(["chmod", "600", f"{host.root}/{template_file}"], check=True)
        reviewed = host.invoke([host.installed, "checkout", "command-manifest", "--file",
                                f"{host.root}/{template_file}", "--json"], check=True)
        configuration.append({"template_id": ids[label], "spec_sha256": json.loads(reviewed.stdout)["spec_sha256"],
                              "image_id": image_id, "entrypoint": "/usr/local/bin/python3" if resource_case else "/bin/sh",
                              "cpus": 1, "pids_limit": 64,
                              "memory_mib": 128})
    host.replace("command-host.json", json.dumps({"templates": configuration}))
    config_file = f"{host.root}/command-host.json"
    host.invoke(["chmod", "600", config_file], check=True)
    return ids, config_file


def verify_resources(call_a, call_b, wait_for, template, runner, host_namespace):
    def execute(call, revision):
        issued = call("office_run_command", {"template_id": template, "scope_revision": revision,
                                             "idempotency_key": str(uuid4()), "arguments": {}})
        assert "command_id" in issued, issued
        def fetch():
            result = call("office_get_command", {"scope_revision": revision, "command_id": issued["command_id"]})
            assert "code" not in result, result
            return result if result["state"] == "finished" else None
        finished = wait_for(fetch, 18)
        result = finished["outcome"]
        assert finished["host_runner_id"] == runner and result["kind"] == "exited" and result["exit_code"] == 0, finished
        count, namespace = result["stdout"].strip().split()
        assert count == "1" and namespace.startswith("net:["), result
        return finished["run_id"], namespace
    with ThreadPoolExecutor(max_workers=2) as pool:
        tasks = [pool.submit(execute, call, revision) for call, revision in [(call_a, 1), (call_b, 2)]]
        result_a, result_b = [task.result(timeout=22) for task in tasks]
    assert result_a[0] != result_b[0] and host_namespace not in (result_a[1], result_b[1])


def verify_commands(call, wait_for, templates, runner, run_id, epoch, before_formatter_publish=None,
                    sealed_baseline=None, image_id=None):
    def start(template, arguments):
        return {"template_id": template, "scope_revision": 2,
                "arguments": arguments, "idempotency_key": str(uuid4())}

    def fetch(command):
        result = call("office_get_command", {"scope_revision": 2, "command_id": command["command_id"]})
        assert "code" not in result, result
        return result if result["state"] == "finished" else None

    invalid = start(templates["snapshot"], {"marker": "anything"})
    assert call("office_run_command", invalid)["code"] == "INVALID_INPUT"
    assert call("office_run_command", start(str(uuid4()), {}))["code"] == "ACCESS_REQUEST_REQUIRED"
    input = start(templates["snapshot"], {"marker": "owner-approved"})
    command = call("office_run_command", input)
    assert "command_id" in command, command
    finished = wait_for(lambda: fetch(command), 14)
    assert finished["run_id"] == run_id and finished["host_runner_id"] == runner and finished["coordinator_epoch"] == epoch
    result = finished["outcome"]
    assert result["kind"] == "exited" and result["exit_code"] == 0, result
    assert result["stdout"].strip() == "alpha published" and result["stderr"].strip() == "owner-approved", result
    assert result["snapshot"]["checkout_revision"] == 4 and result["snapshot"]["file_count"] == 1
    assert result["snapshot"]["total_bytes"] == len("alpha published") and len(result["snapshot"]["sha256"]) == 64
    assert result["outputs"] == [{"path": "result.txt", "file": {
        "encoding": "utf8", "content": "owner-approved", "size_bytes": len("owner-approved"),
        "sha256": sha("owner-approved"),
    }}], result
    assert not result["files_truncated"]
    assert not result["log_truncated"] and call("office_run_command", input) == finished
    checks = []
    if sealed_baseline:
        old = start(templates["snapshot"], {"marker": "owner-approved"})
        wrong = call("office_run_command", dict(old, source_snapshot_id=str(uuid4())))
        assert wrong["code"] == "FORBIDDEN", wrong
        pinned = call("office_run_command", dict(old, source_snapshot_id=sealed_baseline))
        assert pinned.get("source_snapshot_id") == sealed_baseline, pinned
        old_result = wait_for(lambda: fetch(pinned), 18)["outcome"]
        assert old_result["kind"] == "exited" and old_result["exit_code"] == 0, old_result
        assert old_result["stdout"].strip() == "alpha before"
        assert old_result["snapshot"]["source_snapshot_id"] == sealed_baseline
        assert old_result["snapshot"]["checkout_revision"] == 0
        assert old_result["snapshot"]["sha256"] != result["snapshot"]["sha256"]
        assert old_result["snapshot"]["environment"]["image_id"] == image_id
        assert len(old_result["snapshot"]["environment"]["template_sha256"]) == 64
        checks.append("independent_podman_command_verifies_a_pinned_snapshot_after_live_source_advances")
    timer = call("office_run_command", start(templates["timeout"], {}))
    timed_out = wait_for(lambda: fetch(timer), 14)
    assert timed_out["outcome"]["kind"] == "timeout", timed_out
    formatting = call("office_run_command", start(templates["formatter"], {}))
    formatted = wait_for(lambda: fetch(formatting), 14)["outcome"]
    assert formatted["kind"] == "exited" and formatted["exit_code"] == 0, formatted
    assert not formatted["files_truncated"] and len(formatted["staged_changes"]) == 1, formatted
    proposal = formatted["staged_changes"][0]
    assert proposal == {"kind": "replace", "path": ALPHA, "before_sha256": sha("alpha published"),
                        "file": text_file("alpha formatted"), "executable": None}
    before = call("office_read_file", read(ALPHA, 2)["arguments"])
    assert before["file"]["content"] == "alpha published"
    publication = write(proposal, scope_revision=2, dependencies=({"path": ALPHA, "sha256": sha("alpha published")},))
    publication["arguments"]["base_checkout_revision"] = formatted["snapshot"]["checkout_revision"]
    if before_formatter_publish is not None:
        before_formatter_publish()
    accepted = call(publication["name"], publication["arguments"])
    assert accepted["checkout_revision"] == 5 and accepted["host_runner_id"] == runner
    return checks + ["remote_worker_runs_only_reviewed_arguments_on_a_read_only_filtered_host_snapshot",
            "exact_host_command_receipt_replays_and_a_timed_out_container_is_observed_stopped",
            "command_result_returns_only_approved_scratch_files_with_verified_content_hashes",
            "formatter_writes_in_private_staging_and_changes_enter_shared_checkout_only_through_the_gateway"]
