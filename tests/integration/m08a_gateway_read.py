"""Read a registered checkout across owner identities from a real isolated worker."""

import base64
import hashlib
import json
import time
from concurrent.futures import ThreadPoolExecutor
from uuid import uuid4

from m05a_claims import Participant, directory, file
from m07_queue import configure, enqueue
from m08a_coordinator import LAUNCH, SIGNAL


PREPARE_SOURCE = """
import os, sys
from pathlib import Path
root = Path(sys.argv[1])
parts = root / 'parts' / 'gateway-read'
parts.mkdir(parents=True)
with (parts / 'aaa-oversize.dat').open('wb') as file:
    file.truncate(16 * 1024 * 1024 + 1)
(parts / 'binary.dat').write_bytes(bytes([0, 255, 2, 31]))
(parts / 'blocked.txt').write_text('FIXTURE_SCOPE_DENIED')
(parts / 'large.txt').write_bytes(b'x' * (24 * 1024 + 1))
(parts / 'symlink.txt').symlink_to(root / '.env')
os.link(root / '.env', parts / 'hardlink.txt')
"""

WORKER = r"""
set -eu
test ! -r "$3"
test -S "$COCODEOP_MCP_SOCKET"
printf '%s\n' "$1" | /usr/local/bin/office-runner worker-mcp > /work/reads-first.jsonl
touch /work/first-ready
until test -e /work/next; do sleep 0.1; done
printf '%s\n' "$2" | /usr/local/bin/office-runner worker-mcp > /work/reads-second.jsonl
touch /work/second-ready
until test -e /work/finish; do sleep 0.1; done
printf '{"kind":"succeeded","summary":"Read the resource from the checkout host over scoped MCP"}' > "$COCODEOP_RESULT"
"""


def read(path, scope_revision=1, key=None):
    return {"name": "office_read_file", "arguments": {"path": path, "scope_revision": scope_revision,
                                                        "idempotency_key": key or str(uuid4())}}


def scoped_query(name, scope_revision=1, **arguments):
    return {"name": name, "arguments": {"scope_revision": scope_revision, "idempotency_key": str(uuid4()), **arguments}}


def rpc(calls):
    return "\n".join(json.dumps({"jsonrpc": "2.0", "id": seq, "method": "tools/call", "params": method})
                     for seq, method in enumerate(calls, start=1))


def verify(office, host, checkout, owner_b, image_id):
    root = f"{host.root}/tree/code"
    state_a = f"{host.root}/file-gateway"
    state_b = f"{host.root}/gateway-worker"
    for state in (state_a, state_b):
        host.invoke(["mkdir", "-m", "700", "--", state], check=True)
    host.invoke(["python3", "-c", PREPARE_SOURCE, root], check=True)
    prefix = "parts/gateway-read/"
    paths = {"read": {"allow": [file("src/app.rs"), directory(prefix[:-1])], "deny": [file(prefix + "blocked.txt")]},
             "write": {"allow": []}, "write_operations": []}
    participant = Participant(office, checkout, owner_b, office.owner_b, "Gateway read", paths)
    synced = host.sync_scopes(office.endpoint, checkout, state="file-gateway")
    assert synced.returncode == 0, synced.stderr
    active = office.tool(participant.lead, "office_get_work_scope", task_id=participant.task)
    assert active["state"] == "active" and active["definition"]["revision"] == 1
    ready = office.tool(participant.lead, "office_prepare_task", task_id=participant.task,
                        expected_revision=participant.task_revision, idempotency_key=str(uuid4()))["task"]
    assert configure(office, office.owner_b, 30).get("budget")

    source = host.invoke(["cat", f"{root}/src/app.rs"], check=True).stdout
    needle = source.split()[0]
    stable_key = str(uuid4())
    first_queries = [read("src/app.rs", key=stable_key), read("src/app.rs", key=stable_key),
                     read(prefix + "binary.dat", key=stable_key), read(prefix + "binary.dat"), read(".env"),
                     read("nested/lib.rs"), read(prefix + "blocked.txt"), read(prefix + "symlink.txt"),
                     read(prefix + "hardlink.txt"), read(prefix + "missing.txt"), read(prefix + "large.txt")]
    first_queries += [scoped_query("office_search_code", needle=needle),
                      scoped_query("office_search_code", needle="FIXTURE"),
                      scoped_query("office_search_code", needle=needle, under="nested"),
                      scoped_query("office_checkout_status", under=prefix[:-1], limit=1),
                      scoped_query("office_checkout_status", under=prefix[:-1], after=prefix + "aaa-oversize.dat", limit=2),
                      scoped_query("office_checkout_status", after=prefix + "large.txt")]
    second_queries = [read("src/app.rs"), read("src/app.rs", 2), read(prefix + "binary.dat", 2),
                      scoped_query("office_search_code", needle=needle),
                      scoped_query("office_checkout_status", 2)]
    policy = {"adapter_id": "file-reader.v1", "image_id": image_id, "entrypoint": "/bin/sh",
              "args": ["-c", WORKER, "worker", rpc(first_queries), rpc(second_queries), f"{root}/src/app.rs"],
              "timeout_seconds": 45, "memory_mib": 128, "cpus": 1, "pids_limit": 64,
              "run_mcp": True, "checkout_gateway": True}
    host.replace("file-gateway-manifest.json", json.dumps(policy))
    manifest_file = f"{host.root}/file-gateway-manifest.json"
    host.invoke(["chmod", "600", manifest_file], check=True)
    digest = host.invoke([host.installed, "runtime-manifest", "--file", manifest_file, "--json"])
    assert digest.returncode == 0, digest.stderr
    runtime = office.tool(office.owner_b, "office_create_runtime_template", name="Scoped file reader", kind="managed",
                          managed={"adapter_id": policy["adapter_id"], "reservation_units": 15,
                                   "manifest_sha256": json.loads(digest.stdout)["manifest_sha256"]},
                          idempotency_key=str(uuid4()))["template_id"]
    host.replace("file-gateway-worker.json", json.dumps({"capabilities": ["linux"], "runtimes": [
        {"template_id": runtime, "manifest": policy},
    ]}))
    profile_path = f"{host.root}/file-gateway-worker.json"
    host.invoke(["chmod", "600", profile_path], check=True)
    job = enqueue(office, participant.lead, ready, participant.identity, runtime)["job"]

    def checkout_host():
        forward, address = host.connection_for_runner(office.endpoint)
        return host.invoke(["python3", "-c", LAUNCH, f"{state_a}/stdout", f"{state_a}/stderr", f"{state_a}/pid",
                            host.installed, "checkout", "serve", "--checkout", checkout["checkout_id"], "--root", root,
                            "--state-dir", state_a, "--token-file", f"{host.root}/a.token", "--url", f"http://{address}",
                            "--json"], forward=forward)

    def worker_process():
        forward, address = host.connection_for_runner(office.endpoint)
        return host.invoke([host.installed, "serve", "--file", profile_path, "--state-dir", state_b,
                            "--token-file", f"{host.root}/b.token", "--url", f"http://{address}", "--once", "--json"], forward=forward)

    def wait_for(fetch, deadline=20):
        limit = time.monotonic() + deadline
        while time.monotonic() < limit:
            if result := fetch():
                return result
            time.sleep(0.2)
        raise AssertionError("The file gateway did not reach the expected state")

    idle = worker_process()
    assert idle.returncode == 0 and json.loads(idle.stdout)["status"] == "idle", (idle.stdout, idle.stderr)
    assert office.tool(participant.worker, "office_get_task", task_id=participant.task)["runs"] == []
    checks = ["coding_runner_waits_for_the_real_checkout_coordinator_before_process_launch"]

    with ThreadPoolExecutor(max_workers=2) as pool:
        host_future = pool.submit(checkout_host)
        scratch = None
        try:
            def ready_host():
                line = host.invoke(["head", "-n", "1", f"{state_a}/stdout"]).stdout.strip()
                if host_future.done() and not line:
                    raise AssertionError(host.invoke(["head", "-c", "2000", f"{state_a}/stderr"]).stdout)
                return json.loads(line) if line else None

            coordinator = wait_for(ready_host, 12)
            execution = pool.submit(worker_process)

            def first_ready():
                view = office.tool(participant.worker, "office_get_task", task_id=participant.task)
                if execution.done():
                    result = execution.result()
                    raise AssertionError((result.stdout, result.stderr))
                if not view["runs"]:
                    return None
                run_id = view["runs"][0]["run_id"]
                scratch_path = f"{state_b}/run-{run_id}/scratch"
                if host.invoke(["test", "-f", f"{scratch_path}/first-ready"]).returncode == 0:
                    return run_id, scratch_path, view["task"]["revision"]
                return None

            run_id, scratch, current_task_revision = wait_for(first_ready, 28)

            def frames(name):
                result = host.invoke(["cat", f"{scratch}/reads-{name}.jsonl"], check=True).stdout
                assert all(value not in result for value in ("FIXTURE_PRIVATE_ENV_ONLY", "FIXTURE_SCOPE_DENIED", root))
                parsed = [json.loads(line) for line in result.splitlines()]
                return [item.get("result", {}).get("structuredContent") or item.get("error") for item in parsed]

            first = frames("first")
            assert first[0] == first[1] and first[0]["file"]["content"] == source, first
            assert first[0]["file"]["sha256"] == hashlib.sha256(source.encode()).hexdigest()
            assert first[0]["host_runner_id"] == checkout["host_runner_id"]
            assert first[0]["coordinator_epoch"] == coordinator["epoch"]
            assert first[2]["code"] == "IDEMPOTENCY_CONFLICT"
            assert base64.b64decode(first[3]["file"]["content"]) == bytes([0, 255, 2, 31])
            assert [item["code"] for item in first[4:11]] == ["PATH_FORBIDDEN"] * 5 + ["FILE_NOT_FOUND", "INVALID_INPUT"], first
            checks.extend(["real_remote_owner_worker_reads_exact_text_binary_and_hashes_through_the_registered_host",
                           "readonly_gateway_excludes_hidden_ungranted_symlink_and_hardlink_paths_and_bounds_reads"])
            search, denied_text, denied_subtree, status, page, final = first[11:]
            sha = first[0]["file"]["sha256"]
            assert search["matches"] == [{"path": "src/app.rs", "line_number": 1, "file_sha256": sha,
                                          "snippet": source.rstrip("\n")}] and search["truncated"], search
            assert search["coordinator_epoch"] == coordinator["epoch"]
            assert denied_text["matches"] == [] and denied_subtree["code"] == "PATH_FORBIDDEN"
            assert status["files"] == [{"path": prefix + "aaa-oversize.dat", "size_bytes": 16 * 1024 * 1024 + 1,
                                       "sha256": None, "executable": False}], status
            assert status["next_after"] == prefix + "aaa-oversize.dat"
            assert [file["path"] for file in page["files"]] == [prefix + "binary.dat", prefix + "large.txt"]
            assert page["files"][0]["sha256"] == first[3]["file"]["sha256"]
            assert page["next_after"] is None and final["next_after"] is None
            assert [file["path"] for file in final["files"]] == ["src/app.rs"] and final["files"][0]["sha256"] == sha
            checks.extend(["literal_search_respects_the_manifest_path_rules_and_returns_content_provenance",
                           "status_pages_exclude_protected_metadata_and_keep_large_files_visible"])

            narrowed = {"read": {"allow": [file("src/app.rs")]}, "write": {"allow": []}, "write_operations": []}
            revised = participant.propose_scope(current_task_revision, expected_scope_revision=1, permissions=narrowed)
            assert revised["scope"]["definition"]["revision"] == 2

            def read_active():
                scope = office.tool(participant.lead, "office_get_work_scope", task_id=participant.task, revision=2)
                return scope if scope.get("state") == "active" else None

            wait_for(read_active, 10)
            host.invoke(["touch", f"{scratch}/next"], check=True)
            wait_for(lambda: host.invoke(["test", "-f", f"{scratch}/second-ready"]).returncode == 0, 10)
            second = frames("second")
            assert second[0]["code"] == "SCOPE_STALE" and second[1]["scope_revision"] == 2
            assert second[2]["code"] == "PATH_FORBIDDEN"
            assert second[3]["code"] == "SCOPE_STALE"
            assert second[4]["scope_revision"] == 2 and [file["path"] for file in second[4]["files"]] == ["src/app.rs"]
            host.invoke(["touch", f"{scratch}/finish"], check=True)
            result = execution.result(timeout=20)
            assert result.returncode == 0, result.stderr
            run = json.loads(result.stdout)["receipt"]["execution"]["run"]
            assert run["status"] == "succeeded" and run["run_id"] == run_id
            assert run["lease"]["runner_id"] != coordinator["runner_id"] and run["lease"]["job_id"] == job["job_id"]
            assert office.sql("SET ROLE cocodeop; SELECT count(*) FROM checkout_file_reads").splitlines()[-1] == "0"
            checks.append("same_isolated_run_uses_acknowledged_scope_revision_and_loses_narrowed_read_paths")
            return checks
        finally:
            if scratch:
                host.invoke(["touch", f"{scratch}/next", f"{scratch}/finish"])
            if not host_future.done():
                host.invoke(["python3", "-c", SIGNAL, f"{state_a}/pid", host.installed, state_a, "SIGTERM"])
