"""Verify the durable start ledger using actual Linux processes and controller disconnects."""

import argparse
import json
import os
import tempfile
from pathlib import Path
from uuid import uuid4

from m05a_registry import RunnerHost
from m07_leases import RunnerControl, register_runner
from m07_queue import configure, create_template, enqueue
from workspace_fixture import ROOT, workspace_fixture


PROCESS = """
import json, os, signal, subprocess, sys, time
from pathlib import Path
def inspect(pid):
    try:
        fields = Path(f'/proc/{pid}/stat').read_text().rsplit(') ', 1)[1].split()
        return fields[0], fields[19]
    except FileNotFoundError:
        return None, None
if sys.argv[1] == 'launch':
    with open(os.devnull, 'r+b') as null:
        process = subprocess.Popen(['/bin/sleep', '120'], stdin=null, stdout=null, stderr=null,
                                   start_new_session=True)
    state, token = inspect(process.pid)
    assert state not in (None, 'Z')
    print(json.dumps({'pid': process.pid, 'token': token, 'id': f'linux:{process.pid}:{token}'}))
else:
    pid, expected = int(sys.argv[2]), sys.argv[3]
    state, token = inspect(pid)
    if state not in (None, 'Z') and token == expected:
        os.kill(pid, signal.SIGTERM)
    for _ in range(100):
        state, token = inspect(pid)
        if state in (None, 'Z') or token != expected:
            break
        time.sleep(0.01)
    else:
        raise RuntimeError('The original fixture process survived SIGTERM')
"""


def proof(run):
    return {"job_id": run["lease"]["job_id"], "run_id": run["run_id"], "run_fence": run["lease"]["run_fence"]}


def verify(office, tmp, guest, binary):
    lead, _ = office.agent(office.owner_a, "Start approval lead", "lead")
    worker, identity = office.agent(office.owner_a, "Managed runtime worker", "worker")
    runtime = create_template(office, office.owner_a, units=10)
    assert configure(office, office.owner_a, 100).get("budget")
    _, primary_token = register_runner(office, office.owner_a, "Primary process runner")
    _, alternate_token = register_runner(office, office.owner_a, "Backup process runner")
    _, foreign_token = register_runner(office, office.owner_b, "Other owner runner")
    host = RunnerHost(tmp, guest, binary)
    host.setup({"primary": primary_token, "backup": alternate_token, "peer": foreign_token})
    ctl = RunnerControl(office, host)
    running_processes = []

    def queue():
        task = office.tool(lead, "office_create_task", objective="Launch a bounded monitored process",
                           acceptance_criteria=["Use a durable start receipt"], idempotency_key=str(uuid4()))["task"]
        ready = office.tool(lead, "office_prepare_task", task_id=task["task_id"], expected_revision=task["revision"],
                           idempotency_key=str(uuid4()))["task"]
        return enqueue(office, lead, ready, identity, runtime)["job"]

    def lease(job):
        run = ctl.claim("primary")["run"]
        assert run["lease"]["job_id"] == job["job_id"]
        return run

    def job_state(job):
        return office.tool(worker, "office_get_execution", job_id=job["job_id"])

    def budget():
        item = office.tool(office.owner_a, "office_get_execution_budget")
        return item["reserved_units"], item["charged_units"]

    def expire(run):
        office.sql(f"UPDATE runs SET lease_until=clock_timestamp()-interval '1 second' WHERE id='{run['run_id']}'")

    def launch():
        result = host.invoke(["python3", "-c", PROCESS, "launch"], check=True)
        process = json.loads(result.stdout)
        running_processes.append(process)
        return process

    def stop(process):
        host.invoke(["python3", "-c", PROCESS, "stop", str(process["pid"]), process["token"]], check=True)
        running_processes.remove(process)

    try:
        ctl.advertise("primary", 1, [runtime])
        ctl.advertise("backup", 1, [runtime])
        ctl.advertise("peer", 1, [])
        first = queue()
        active = lease(first)
        authorize = {"lease": proof(active), "idempotency_key": str(uuid4())}
        assert ctl.send("primary", "read-start", proof(active)) is None
        for label in ("backup", "peer"):
            assert ctl.send(label, "authorize-start", authorize, check=False).returncode != 0
        wrong_fence = {**authorize, "lease": {**proof(active), "run_fence": active["lease"]["run_fence"] + 1}}
        assert ctl.send("primary", "authorize-start", wrong_fence, check=False).returncode != 0
        permit = ctl.send("primary", "authorize-start", authorize)
        effect = {"lease": proof(active), "effect_id": permit["effect"]["effect_id"]}
        assert permit["effect"]["state"] == "authorized" and permit["run"]["status"] == "starting"
        assert permit["effect"]["external_id"] is None and len(permit["effect"]["approval_sha256"]) == 64
        assert ctl.send("primary", "authorize-start", authorize) == permit
        changed_key = {**authorize, "idempotency_key": str(uuid4())}
        assert ctl.send("primary", "authorize-start", changed_key, check=False).returncode != 0
        checks = ["permit_records_one_effect_and_binds_exact_job_run_runner_and_fence"]

        process = launch()
        # The Linux process exists before any acknowledgement reaches the coordinator.
        expire(active)
        office.restart()
        assert ctl.claim("backup") is None and ctl.claim("primary") is None
        pending = job_state(first)
        assert pending["state"] == "reconcile_required" and pending["managed_attempts"] == 1
        after = ctl.send("primary", "read-start", proof(active))
        assert after["run"]["status"] == "start_unknown" and after["effect"]["state"] == "unknown"
        assert after["effect"]["effect_id"] == effect["effect_id"] and budget() == (10, 0)
        reconcile = {"effect": effect, "observation": {"kind": "stopped", "external_id": process["id"]}}
        assert ctl.send("backup", "reconcile-start", reconcile, check=False).returncode != 0
        assert ctl.send("primary", "observe-start", {"effect": effect, "external_id": process["id"]},
                        check=False).returncode != 0
        stop(process)
        receipt = ctl.send("primary", "reconcile-start", reconcile)
        assert receipt["effect"]["state"] == "stopped" and receipt["run"]["status"] == "failed"
        assert receipt["effect"]["external_id"] == process["id"] and receipt["effect"]["reconciled_at"]
        assert ctl.send("primary", "reconcile-start", reconcile) == receipt
        assert job_state(first)["terminal_reason"] == "start_interrupted" and budget() == (0, 10)
        assert ctl.claim("backup") is None
        checks.append("process_started_before_lost_ack_requires_origin_runner_reconciliation_after_restart")

        second = queue()
        active = lease(second)
        authorize = {"lease": proof(active), "idempotency_key": str(uuid4())}
        permit = ctl.send("primary", "authorize-start", authorize)
        process = launch()
        effect = {"lease": proof(active), "effect_id": permit["effect"]["effect_id"]}
        observation = {"effect": effect, "external_id": process["id"]}
        started = ctl.send("primary", "observe-start", observation)
        assert started["effect"]["confirmed_at"] and started["run"]["status"] == "running"
        assert ctl.send("primary", "observe-start", observation) == started
        assert ctl.send("primary", "observe-start", {**observation, "external_id": "other"}, check=False).returncode != 0
        assert ctl.heartbeat("primary", active)["status"] == "running"
        current = office.tool(worker, "office_get_task", task_id=second["task_id"])["task"]
        assert office.tool(worker, "office_submit_result", task_id=second["task_id"], expected_revision=current["revision"],
                           idempotency_key=str(uuid4()), summary="Attempt through a long-lived credential")["code"] == "RUN_LEASE_STALE"
        stopped = office.tool(lead, "office_change_task", task_id=second["task_id"], expected_revision=current["revision"],
                              idempotency_key=str(uuid4()), change={"operation": "cancel", "reason": "Stop the live process"})
        assert stopped["run"]["status"] == "cancel_requested" and budget() == (10, 10)
        assert ctl.heartbeat("primary", active, check=False).returncode != 0
        assert ctl.claim("primary") is None
        stop(process)
        reconcile = {"effect": effect, "observation": {"kind": "stopped", "external_id": process["id"]}}
        receipt = ctl.send("primary", "reconcile-start", reconcile)
        assert receipt["run"]["status"] == "cancelled" and job_state(second)["state"] == "cancelled"
        assert ctl.send("primary", "reconcile-start", reconcile) == receipt and budget() == (0, 20)
        checks.append("confirmed_process_uses_live_lease_and_cancel_waits_for_observed_stop")

        third = queue()
        active = lease(third)
        permit = ctl.send("primary", "authorize-start", {"lease": proof(active), "idempotency_key": str(uuid4())})
        expire(active)
        assert ctl.claim("backup") is None
        assert job_state(third)["state"] == "reconcile_required" and budget() == (10, 20)
        effect = {"lease": proof(active), "effect_id": permit["effect"]["effect_id"]}
        reconcile = {"effect": effect, "observation": {"kind": "never_started"}}
        assert ctl.send("primary", "reconcile-start", reconcile)["effect"]["state"] == "never_started"
        assert job_state(third)["state"] == "dead_letter" and budget() == (0, 20)
        assert ctl.claim("backup") is None
        assert office.sql("SET ROLE cocodeop; SELECT count(*) FROM execution_start_effects").splitlines()[-1] == "0"
        checks.append("confirmed_absence_releases_reserve_without_spend_or_a_second_start")
        return checks
    finally:
        try:
            for process in running_processes[:]:
                stop(process)
        finally:
            host.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--linux-vm", action="store_true")
    parser.add_argument("--runner", type=Path)
    args = parser.parse_args()
    assert args.linux_vm or os.name != "nt", "Use --linux-vm on Windows"
    binary = (args.runner or ROOT / (".cocodeop-test/linux-bin/office-runner" if args.linux_vm
                                     else "target/release/office-runner")).resolve()
    assert binary.is_file(), f"Build the Linux runner: {binary}"
    with tempfile.TemporaryDirectory(prefix="cocodeop-effects-") as directory:
        tmp = Path(directory)
        with workspace_fixture(tmp, "scheduler-effects") as office:
            print(json.dumps({"checks": verify(office, tmp, args.linux_vm, binary),
                              "linux_execution": "debian13_qemu_guest" if args.linux_vm else "native_linux",
                              "independent_physical_hosts": False}))
