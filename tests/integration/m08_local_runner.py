"""Exercise the real rootless OCI controller using deterministic test-only workers."""

import argparse
import json
import os
import shlex
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from uuid import uuid4

from m05a_registry import RunnerHost
from m07_leases import register_runner
from m07_queue import configure, enqueue
from m12_execution_controls import set_mode
from workspace_fixture import ROOT, workspace_fixture


ISOLATION = r"""
set -eu
test "$(id -u)" = 1000
test -r "$COCODEOP_TASK"
grep -q 'Verify native isolation' "$COCODEOP_TASK"
test -z "${OFFICE_TOKEN-}"
test -z "${COCODEOP_FIXTURE_PROVIDER_TOKEN-}"
test -z "${COCODEOP_MCP_SOCKET-}"
test ! -e /var/run/docker.sock
test ! -e /run/podman/podman.sock
test ! -r "$1"
test ! -r "$2"
test ! -r "$3"
test "$(awk 'END { print NR }' /proc/net/route)" = 1
if touch /must-stay-read-only 2>/dev/null; then exit 81; fi
if /bin/sh -c 'printf incorrect > "$1"' sh "$COCODEOP_TASK" 2>/dev/null; then exit 82; fi
if /bin/sh -c 'printf incorrect > "$1"' sh "$1" 2>/dev/null; then exit 83; fi
printf '{"kind":"succeeded","summary":"Isolated process validated the real task input and mount boundary"}' > "$COCODEOP_RESULT"
"""

HANG = """
set -eu
sleep 75 &
printf '%s\n' "$!" > /work/child-pid
wait
"""

LAUNCH = """
import json, subprocess, sys
from pathlib import Path
process = subprocess.Popen(sys.argv[2:])
Path(sys.argv[1]).write_text(json.dumps({'pid':process.pid}))
raise SystemExit(process.wait())
"""

KILL_FIXTURE = """
import json, os, signal, sys
from pathlib import Path
pid = json.loads(Path(sys.argv[1]).read_text())['pid']
args = Path(f'/proc/{pid}/cmdline').read_bytes().split(b'\\0')
assert os.fsdecode(args[0]) == sys.argv[2] and os.fsencode(sys.argv[3]) in args
os.kill(pid, signal.SIGKILL)
"""


def verify(office, tmp, guest, binary, image_id):
    lead, _ = office.agent(office.owner_a, "Local execution lead", "lead")
    worker, worker_id = office.agent(office.owner_a, "Local execution worker", "worker")
    assert configure(office, office.owner_a, 160).get("budget")
    runner, token = register_runner(office, office.owner_a, "Isolated OCI controller")
    host = RunnerHost(tmp, guest, binary)
    host.setup({"runner": token})
    state = f"{host.root}/controller"
    host.invoke(["mkdir", "-m", "700", "--", state], check=True)

    def manifest(script, seconds=45, operands=()):
        return {"adapter_id": "oci-test.v1", "image_id": image_id, "entrypoint": "/bin/sh",
                "args": ["-c", script, "worker", *operands], "timeout_seconds": seconds,
                "memory_mib": 128, "cpus": 1, "pids_limit": 64}

    def configure_runtime(policy, approved=None):
        host.replace("runtime-manifest.json", json.dumps(policy))
        host.invoke(["chmod", "600", f"{host.root}/runtime-manifest.json"], check=True)
        digest = host.invoke([host.installed, "runtime-manifest", "--json", "--file",
                              f"{host.root}/runtime-manifest.json"])
        assert digest.returncode == 0, digest.stderr
        fingerprint = json.loads(digest.stdout)["manifest_sha256"]
        runtime = office.tool(office.owner_a, "office_create_runtime_template", name="Pinned OCI fixture",
                              kind="managed", managed={"adapter_id": policy["adapter_id"], "reservation_units": 10,
                                                        "manifest_sha256": approved or fingerprint},
                              idempotency_key=str(uuid4()))
        assert runtime.get("template_id"), runtime
        host.replace("controller.json", json.dumps({"capabilities": ["linux", "oci"], "runtimes": [
            {"template_id": runtime["template_id"], "manifest": policy},
        ]}))
        host.invoke(["chmod", "600", f"{host.root}/controller.json"], check=True)
        return runtime["template_id"]

    def queue(runtime, objective):
        draft = office.tool(lead, "office_create_task", objective=objective,
                            acceptance_criteria=["Persist authentic process outcome"],
                            idempotency_key=str(uuid4()))["task"]
        task = office.tool(lead, "office_prepare_task", task_id=draft["task_id"], expected_revision=draft["revision"],
                           idempotency_key=str(uuid4()))["task"]
        return enqueue(office, lead, task, worker_id, runtime)["job"]

    def controller(*, send_secrets=False, pid_file=None):
        forward, url = host.connection_for_runner(office.endpoint)
        args = [host.installed, "serve", "--once", "--json", "--url", f"http://{url}",
                "--file", f"{host.root}/controller.json", "--state-dir", state,
                "--token-file", f"{host.root}/runner.token"]
        if pid_file:
            args = ["python3", "-c", LAUNCH, pid_file, *args]
        if send_secrets:
            args = ["env", "OFFICE_TOKEN=synthetic-local-test-sentinel", "COCODEOP_FIXTURE_PROVIDER_TOKEN=second-test-sentinel",
                    *args]
        return host.invoke(args, forward=forward)

    def view(job):
        return office.tool(worker, "office_get_task", task_id=job["task_id"])

    def observe_running(job):
        deadline = time.monotonic() + 35
        while time.monotonic() < deadline:
            runs = view(job)["runs"]
            if runs and runs[0]["status"] == "running":
                state_file = host.invoke(["cat", f"{state}/current.json"], check=True)
                pending = json.loads(state_file.stdout)
                assert pending["start_attempted"] is True
                return runs[0], pending["grant"]["effect"]["effect_id"]
            time.sleep(0.25)
        raise AssertionError("The controller did not start the isolated test process")

    def report(reply):
        assert reply.returncode == 0, reply.stderr
        return json.loads(reply.stdout)

    def container_absent(effect):
        if guest:
            args = ["env", "XDG_RUNTIME_DIR=/run/user/1001", "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1001/bus"]
        else:
            args = []
        result = host.invoke([*args, "podman", "--remote=false", "container", "exists", f"cocodeop-{effect}"])
        assert result.returncode == 1, (result.returncode, result.stderr)

    source = f"{host.root}/tree/code/src/app.rs"
    try:
        policy = manifest(ISOLATION, operands=(source, f"{host.root}/runner.token", f"{state}/current.json"))
        runtime = configure_runtime(policy)
        initial = host.invoke(["cat", source], check=True).stdout
        simple = queue(runtime, "Verify native isolation")
        receipt = report(controller(send_secrets=True))["receipt"]
        run = receipt["execution"]["run"]
        assert run["status"] == "succeeded" and receipt["exit_code"] == 0, receipt
        assert run["lease"]["job_id"] == simple["job_id"] and run["lease"]["runner_id"] == runner
        container_absent(receipt["execution"]["effect"]["effect_id"])
        assert host.invoke(["cat", source], check=True).stdout == initial
        result = office.tool(worker, "office_get_task_result", task_id=simple["task_id"])
        assert "real task input and mount boundary" in result["content"]["summary"]
        checks = ["owner_approved_local_podman_executes_actual_task_with_readonly_input_and_no_host_secrets_or_network"]

        runtime = configure_runtime(manifest(HANG))
        cancelled = queue(runtime, "Cancel the isolated process tree")
        with ThreadPoolExecutor(max_workers=1) as pool:
            active = pool.submit(controller)
            current, effect = observe_running(cancelled)
            foreign = controller()
            assert foreign.returncode != 0 and "Another controller" in foreign.stderr, foreign.stderr
            item = view(cancelled)["task"]
            changed = office.tool(lead, "office_change_task", task_id=item["task_id"],
                                  expected_revision=item["revision"], idempotency_key=str(uuid4()),
                                  change={"operation": "cancel", "reason": "Stop the isolated worker and its child"})
            assert changed["run"]["status"] == "cancel_requested" and changed["run"]["run_id"] == current["run_id"]
            ended = report(active.result(timeout=35))["receipt"]
        assert ended["run"]["status"] == "cancelled" and ended["effect"]["state"] == "stopped"
        container_absent(effect)
        child_pid = f"{state}/run-{current['run_id']}/scratch/child-pid"
        assert int(host.invoke(["cat", child_pid], check=True).stdout) > 1
        checks.append("controller_lock_and_cancel_stop_the_actual_container_with_a_descendant")

        stopped = queue(runtime, "Stop the active container through owner policy")
        with ThreadPoolExecutor(max_workers=1) as pool:
            active = pool.submit(controller)
            current, effect = observe_running(stopped)
            policy_mode = set_mode(office, office.owner_a, "stopped")
            ended = report(active.result(timeout=35))["receipt"]
        assert ended["run"]["status"] == "cancelled" and ended["effect"]["state"] == "stopped", ended
        recorded = office.tool(lead, "office_get_execution", job_id=stopped["job_id"])
        assert recorded["terminal_reason"] == "emergency_stopped", recorded
        container_absent(effect)
        set_mode(office, office.owner_a, "running", policy_mode["revision"])
        assert len(view(stopped)["runs"]) == 1
        checks.append("owner_emergency_stop_kills_the_real_container_and_records_a_confirmed_stop")

        interrupted = queue(runtime, "Crash and recover the local controller")
        pid_file = f"{host.root}/fixture-controller.pid"
        with ThreadPoolExecutor(max_workers=1) as pool:
            active = pool.submit(controller, pid_file=pid_file)
            current, effect = observe_running(interrupted)
            host.invoke(["python3", "-c", KILL_FIXTURE, pid_file, host.installed, state], check=True)
            assert active.result(timeout=30).returncode != 0
        recovered = report(controller())["receipt"]
        assert recovered["effect"]["effect_id"] == effect and recovered["effect"]["state"] == "stopped"
        assert recovered["run"]["run_id"] == current["run_id"] and recovered["run"]["status"] == "failed"
        assert len(view(interrupted)["runs"]) == 1
        assert office.sql(f"SELECT count(*) FROM execution_start_effects WHERE run_id='{current['run_id']}'") == "1"
        container_absent(effect)
        checks.append("sigkill_between_start_and_completion_reopens_the_durable_journal_and_stops_without_a_second_launch")

        timed_runtime = configure_runtime(manifest("exec sleep 45", seconds=5))
        expired = queue(timed_runtime, "Bound execution with the Podman watchdog")
        before = time.monotonic()
        execution = controller()
        assert time.monotonic() - before < 30
        outcome = report(execution)
        receipt = outcome["receipt"]
        if outcome["status"] == "reconciled":
            assert "without providing a process exit code" in execution.stderr, execution.stderr
        else:
            assert receipt["exit_code"] != 0
            receipt = receipt["execution"]
        assert receipt["run"]["status"] == "failed" and receipt["effect"]["state"] == "stopped"
        assert receipt["run"]["lease"]["job_id"] == expired["job_id"]
        container_absent(receipt["effect"]["effect_id"])
        checks.append("container_watchdog_ends_a_timed_out_worker_and_records_the_observed_stop")

        wrong = configure_runtime(policy, approved="0" * 64)
        declined = queue(wrong, "Reject an unapproved local executable")
        denied = controller()
        assert denied.returncode != 0 and "owner-approved digest" in denied.stderr, denied.stderr
        rejected = view(declined)
        assert rejected["runs"][0]["status"] == "starting"
        assert office.sql(f"SELECT count(*) FROM execution_start_effects WHERE run_id='{rejected['runs'][0]['run_id']}'") == "0"
        checks.append("local_manifest_mismatch_never_obtains_start_authorization")
        return checks
    finally:
        host.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--linux-vm", action="store_true")
    parser.add_argument("--runner", type=Path)
    parser.add_argument("--image-id", required=True)
    args = parser.parse_args()
    assert args.linux_vm or os.name != "nt", "Use --linux-vm on Windows"
    binary = (args.runner or ROOT / (".cocodeop-test/linux-bin/office-runner" if args.linux_vm
                                     else "target/release/office-runner")).resolve()
    assert binary.is_file(), f"Build the Linux runner: {binary}"
    with tempfile.TemporaryDirectory(prefix="cocodeop-local-runner-") as directory:
        tmp = Path(directory)
        with workspace_fixture(tmp, "local-runner") as office:
            print(json.dumps({"checks": verify(office, tmp, args.linux_vm, binary, args.image_id),
                              "linux_execution": "debian13_qemu_guest" if args.linux_vm else "native_linux",
                              "podman_image_id": args.image_id, "independent_physical_hosts": False}))
