"""Verify safe pre-start retries and dead-lettering with real runner pulls and PostgreSQL."""

import argparse
import json
import os
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime
from pathlib import Path
from uuid import uuid4

from m05a_registry import RunnerHost
from m07_leases import RunnerControl, register_runner
from m07_queue import configure, create_template, enqueue
from workspace_fixture import ROOT, workspace_fixture


def verify(office, tmp, guest, binary):
    lead, _ = office.agent(office.owner_a, "Recovery lead", "lead")
    worker, identity = office.agent(office.owner_a, "Recovery worker", "worker")
    runtime = create_template(office, office.owner_a)
    assert configure(office, office.owner_a, 70).get("budget")

    def create_job():
        task = office.tool(lead, "office_create_task", objective="Recover an expired unstarted execution",
                           acceptance_criteria=["Preserve each attempt"], idempotency_key=str(uuid4()))["task"]
        task = office.tool(lead, "office_prepare_task", task_id=task["task_id"],
                           expected_revision=task["revision"], idempotency_key=str(uuid4()))["task"]
        return enqueue(office, lead, task, identity, runtime)["job"]

    job = create_job()

    def read_job():
        return office.tool(lead, "office_get_execution", job_id=job["job_id"])

    def task_view():
        return office.tool(worker, "office_get_task", task_id=job["task_id"])

    def budget():
        return office.tool(office.owner_a, "office_get_execution_budget")

    def expire(run):
        office.sql(f"UPDATE runs SET lease_until=clock_timestamp()-interval '1 second' WHERE id='{run['run_id']}'")

    _, first_token = register_runner(office, office.owner_a, "Primary recovery runner")
    standby_id, standby_token = register_runner(office, office.owner_a, "Standby runner")
    _, foreign_token = register_runner(office, office.owner_b, "Peer owner runner")
    host = RunnerHost(tmp, guest, binary)
    host.setup({"primary": first_token, "standby": standby_token, "peer": foreign_token})
    ctl = RunnerControl(office, host)
    try:
        ctl.advertise("primary", 1, [runtime])
        ctl.advertise("standby", 1, [runtime])
        ctl.advertise("peer", 1, [])
        key = str(uuid4())
        first = ctl.claim("primary", key)["run"]
        expire(first)
        assert ctl.claim("peer") is None and read_job()["state"] == "leased"
        assert ctl.heartbeat("primary", first, check=False).returncode != 0
        with ThreadPoolExecutor(max_workers=2) as pool:
            assert list(pool.map(ctl.claim, ["standby", "standby"])) == [None, None]
        replay = ctl.claim("primary", key)["run"]
        assert replay["run_id"] == first["run_id"] and replay["status"] == "lost"
        assert budget()["reserved_units"] == 30 and budget()["charged_units"] == 0
        assert task_view()["task"]["status"] == "assigned"
        assert office.sql(f"SELECT count(*) FROM events WHERE scope_id='{job['task_id']}' "
                          "AND event_type='execution.requeued'") == "1"
        checks = ["same_owner_standby_reconciles_one_expiry_and_a_peer_cannot_recover_it"]

        def retry(previous, minimum_delay):
            queued = read_job()
            latest = task_view()["runs"][0]
            assert queued["state"] == "queued" and "current_run_id" not in queued
            assert queued["managed_attempts"] == previous["attempt_number"]
            scheduled = datetime.fromisoformat(queued["retry_not_before"])
            ended = datetime.fromisoformat(latest["ended_at"])
            assert minimum_delay <= (scheduled - ended).total_seconds() < 2 * minimum_delay + 1
            wait = float(office.sql("SELECT extract(epoch FROM retry_not_before-clock_timestamp()) "
                                   f"FROM execution_jobs WHERE id='{job['job_id']}'"))
            if wait > 0:
                time.sleep(wait + 0.05)
            current = ctl.claim("standby")["run"]
            assert current["run_id"] != previous["run_id"]
            assert current["lease"]["runner_id"] == standby_id
            assert current["lease"]["run_fence"] > previous["lease"]["run_fence"]
            assert current["attempt_number"] == previous["attempt_number"] + 1
            return current

        office.restart()
        second = retry(first, 2)
        expire(second)
        assert ctl.claim("standby") is None
        third = retry(second, 4)
        assert ctl.heartbeat("standby", second, check=False).returncode != 0
        checks.append("jittered_backoff_survives_restart_and_retry_gets_a_new_fence_and_attempt")

        expire(third)
        assert ctl.claim("standby") is None
        dead = read_job()
        assert (dead["state"], dead["managed_attempts"], dead["terminal_reason"]) == (
            "dead_letter", 3, "attempt_limit",
        )
        assert dead["current_run_id"] == third["run_id"] and dead["ended_at"]
        view = task_view()
        assert view["task"]["status"] == "failed"
        assert [run["status"] for run in view["runs"]] == ["lost"] * 3
        assert budget()["reserved_units"] == budget()["charged_units"] == 0
        office.restart()
        assert ctl.claim("standby") is None and read_job() == dead
        assert office.sql(f"SELECT count(*) FROM events WHERE scope_id='{job['task_id']}' "
                          "AND event_type='execution.dead_lettered'") == "1"
        checks.append("third_expiry_dead_letters_and_releases_budget_exactly_once_with_full_history")

        # Compare against a later revision as a separate test of the queue readmit check.
        invalidated = create_job()
        ready = create_job()
        office.sql(f"UPDATE tasks SET revision=revision+1 WHERE id='{invalidated['task_id']}'")
        assert ctl.claim("standby") is None
        stale = office.tool(lead, "office_get_execution", job_id=invalidated["job_id"])
        assert stale["state"] == "dead_letter" and stale["terminal_reason"] == "eligibility_changed"
        assert stale["managed_attempts"] == 0 and budget()["reserved_units"] == 30
        assert ctl.claim("standby")["run"]["lease"]["job_id"] == ready["job_id"]
        checks.append("stale_queue_entry_releases_its_reservation_and_the_next_job_can_start")
        return checks
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
    with tempfile.TemporaryDirectory(prefix="cocodeop-retries-") as directory:
        tmp = Path(directory)
        with workspace_fixture(tmp, "scheduler-retries") as office:
            print(json.dumps({"checks": verify(office, tmp, args.linux_vm, binary),
                              "linux_execution": "debian13_qemu_guest" if args.linux_vm else "native_linux",
                              "independent_physical_hosts": False}))
