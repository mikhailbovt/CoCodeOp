"""Verify fenced execution leases with real PostgreSQL and the native Linux runner."""

import argparse
import json
import os
import secrets
import tempfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from uuid import uuid4

from m05a_registry import RunnerHost
from m07_queue import configure, create_template, enqueue
from workspace_fixture import ROOT, digest, workspace_fixture


def register_runner(office, owner, label):
    bearer = secrets.token_hex(32)
    result = office.api(f"/v1/workspaces/{office.workspace}/runners", owner, {
        "name": label, "idempotency_key": str(uuid4()), "credential_digest": digest(bearer),
    })
    return result["runner_id"], bearer


class RunnerControl:
    def __init__(self, office, host):
        self.office = office
        self.host = host

    def invoke(self, label, operation, *args):
        forward, address = self.host.connection_for_runner(self.office.endpoint)
        path = self.host.root
        return self.host.invoke([self.host.installed, "scheduler", operation, *args, "--json",
                                 "--token-file", f"{path}/{label}.token", "--url", f"http://{address}"], forward=forward)

    def advertise(self, label, capacity, templates, revision=0, checkout_ids=(), check=True):
        return self.send(label, "advertise", {
            "expected_revision": revision, "ttl_seconds": 120, "capacity": capacity,
            "runtime_template_ids": templates, "capabilities": ["linux"], "hosted_checkout_ids": list(checkout_ids),
        }, check=check)

    def claim(self, label, key=None):
        result = self.invoke(label, "claim", "--claim-key", key or str(uuid4()))
        assert result.returncode == 0, result.stderr
        return json.loads(result.stdout)

    def heartbeat(self, label, run, fence=None, check=True):
        return self.send(label, "renew", {"job_id": run["lease"]["job_id"], "run_id": run["run_id"],
            "run_fence": fence if fence is not None else run["lease"]["run_fence"]}, check=check)

    def send(self, label, operation, body, check=True):
        file = f"{operation}-{uuid4().hex}.json"
        self.host.replace(file, json.dumps(body))
        result = self.invoke(label, operation, "--file", f"{self.host.root}/{file}")
        if not check:
            return result
        assert result.returncode == 0, result.stderr
        return json.loads(result.stdout)


def verify(office, tmp, guest, binary):
    a, _ = office.agent(office.owner_a, "Backend planner", "lead")
    b, _ = office.agent(office.owner_b, "Client planner", "lead")
    worker_a, identity_a = office.agent(office.owner_a, "Backend worker", "worker")
    _, identity_b = office.agent(office.owner_b, "Client worker", "worker")
    for owner in (office.owner_a, office.owner_b):
        assert configure(office, owner, 140).get("budget")
    runtime_a = create_template(office, office.owner_a, "Backend managed", 20)
    runtime_b = create_template(office, office.owner_b, "Client managed", 20)

    def team(lead, worker, name):
        item = office.tool(lead, "office_create_team", name=name, idempotency_key=str(uuid4()))["team"]
        office.tool(lead, "office_set_team_member", team_id=item["team_id"], expected_revision=item["revision"],
                    agent_id=worker["agent_id"], role="worker", idempotency_key=str(uuid4()))
        return item["team_id"]

    lane_one = team(a, identity_a, "Core")
    lane_two = team(a, identity_a, "Tools")
    lane_b = team(b, identity_b, "Client")

    def queue(lead, worker, lane, runtime):
        task = office.tool(lead, "office_create_task", team_id=lane, objective="Execute an admitted task",
                           acceptance_criteria=["Preserve run identity"], idempotency_key=str(uuid4()))["task"]
        active = office.tool(lead, "office_prepare_task", task_id=task["task_id"], expected_revision=task["revision"],
                             idempotency_key=str(uuid4()))["task"]
        planned = enqueue(office, lead, active, worker, runtime)
        assert planned.get("job"), planned
        return planned["job"]

    first = queue(a, identity_a, lane_one, runtime_a)
    second = queue(a, identity_a, lane_one, runtime_a)
    independent = queue(a, identity_a, lane_two, runtime_a)
    foreign = queue(b, identity_b, lane_b, runtime_b)

    runner_a, token_a = register_runner(office, office.owner_a, "First backend runner")
    runner_a2, token_a2 = register_runner(office, office.owner_a, "Second backend runner")
    runner_b, token_b = register_runner(office, office.owner_b, "Client runner")
    host = RunnerHost(tmp, guest, binary)
    host.setup({"a": token_a, "a2": token_a2, "b": token_b})
    ctl = RunnerControl(office, host)
    try:
        assert ctl.claim("b") is None
        ctl.advertise("a", 1, [])
        assert ctl.claim("a") is None
        ctl.advertise("a", 1, [runtime_a], revision=1)
        ctl.advertise("a2", 2, [runtime_a])
        ctl.advertise("b", 1, [runtime_b])
        keys = (str(uuid4()), str(uuid4()))
        with ThreadPoolExecutor(max_workers=2) as pool:
            results = list(pool.map(lambda key: ctl.claim("a", key), keys))
        assert sum(result is not None for result in results) == 1, results
        winner = next(index for index, entry in enumerate(results) if entry)
        lease = results[winner]
        run = lease["run"]
        assert run["lease"]["job_id"] == first["job_id"] and run["lease"]["runner_id"] == runner_a
        assert run["status"] == "starting" and run["instance_id"] is None
        assert run["runtime_template_id"] == runtime_a and run["attempt_number"] == 1
        assert lease["managed_runtime"]["adapter_id"] == "openai-compat.v1"
        planned = office.tool(worker_a, "office_get_execution", job_id=first["job_id"])
        assert planned["state"] == "leased" and planned["current_run_id"] == run["run_id"]
        view = office.tool(worker_a, "office_get_task", task_id=first["task_id"])
        assert view["task"]["status"] == "in_progress" and view["runs"] == [run]
        assert office.tool(worker_a, "office_submit_result", task_id=first["task_id"],
                           expected_revision=view["task"]["revision"], idempotency_key=str(uuid4()),
                           summary="Work cannot finish during a starting lease")["code"] == "INVALID_TRANSITION"
        checks = ["real_runner_race_gets_one_starting_attempt_without_claiming_a_process_has_launched"]

        office.restart()
        assert ctl.claim("a", keys[winner]) == lease
        client = ctl.claim("b")["run"]
        assert client["lease"]["job_id"] == foreign["job_id"] and client["lease"]["runner_id"] == runner_b
        assert ctl.claim("b") is None
        next_lane = ctl.claim("a2")["run"]
        same_lane = ctl.claim("a2")["run"]
        assert next_lane["lease"]["job_id"] == independent["job_id"] and next_lane["lease"]["runner_id"] == runner_a2
        assert same_lane["lease"]["job_id"] == second["job_id"]
        assert run["lease"]["run_fence"] < client["lease"]["run_fence"] < next_lane["lease"]["run_fence"] < same_lane["lease"]["run_fence"]
        assert ctl.claim("a2") is None
        checks.append("lease_replay_survives_restart_and_dispatch_alternates_eligible_teams_per_owner")

        bad_fence = ctl.heartbeat("a", run, fence=run["lease"]["run_fence"] + 1, check=False)
        assert bad_fence.returncode != 0 and "RunLeaseStale" in bad_fence.stderr, bad_fence.stderr
        wrong_runner = ctl.heartbeat("a2", run, check=False)
        assert wrong_runner.returncode != 0 and "RunLeaseStale" in wrong_runner.stderr
        assert ctl.heartbeat("b", run, check=False).returncode != 0
        renewed = ctl.heartbeat("a", run)
        assert renewed["run_id"] == run["run_id"] and renewed["lease"]["run_fence"] == run["lease"]["run_fence"]
        assert renewed["lease"]["lease_until"] > run["lease"]["lease_until"]
        office.sql(f"UPDATE runs SET lease_until=clock_timestamp()-interval '1 second' WHERE id='{run['run_id']}'")
        expired = ctl.heartbeat("a", run, check=False)
        assert expired.returncode != 0 and "RunLeaseStale" in expired.stderr
        checks.append("heartbeat_extends_only_the_live_exact_run_fence_of_the_assigned_runner")

        job_view = office.tool(a, "office_get_task", task_id=first["task_id"])
        stopped = office.tool(a, "office_change_task", task_id=first["task_id"], expected_revision=job_view["task"]["revision"],
                              idempotency_key=str(uuid4()), change={"operation": "cancel", "reason": "Cancel the unstarted attempt"})
        assert stopped.get("run"), stopped
        assert stopped["run"]["status"] == "cancelled" and stopped["run"]["run_id"] == run["run_id"]
        assert office.tool(a, "office_get_execution", job_id=first["job_id"])["state"] == "cancelled"
        assert office.tool(office.owner_a, "office_get_execution_budget")["reserved_units"] == 40
        extra = queue(a, identity_a, lane_one, runtime_a)
        replacement = ctl.claim("a")["run"]
        assert replacement["lease"]["job_id"] == extra["job_id"] and replacement["run_id"] != run["run_id"]
        assert replacement["lease"]["run_fence"] > same_lane["lease"]["run_fence"]
        assert ctl.heartbeat("a", run, check=False).returncode != 0
        assert office.sql("SET ROLE cocodeop; SELECT count(*) FROM execution_dispatch_order").splitlines()[-1] == "0"
        checks.append("cancellation_of_an_unstarted_lease_releases_budget_and_slot_without_reusing_a_fence")
        return checks
    finally:
        host.close()


def verify_checkout_placement(office, host, checkout, job, runtime, participant):
    ctl = RunnerControl(office, host)
    runner = ctl.advertise("b", 1, [runtime])
    assert ctl.claim("b") is None
    wrong = ctl.advertise("b", 1, [runtime], revision=1, checkout_ids=[checkout["checkout_id"]], check=False)
    assert wrong.returncode != 0 and "Forbidden" in wrong.stderr, wrong.stderr
    present = ctl.advertise("a", 0, [], checkout_ids=[checkout["checkout_id"]])
    assert present["runner_id"] != runner["runner_id"]
    assert present["hosted_checkout_ids"] == [checkout["checkout_id"]]
    assert ctl.claim("a") is None
    started = ctl.claim("b")
    assert started.get("run"), started
    assert started["run"]["lease"]["job_id"] == job["job_id"] and started["run"]["lease"]["runner_id"] == runner["runner_id"]
    assert started["run"]["executor"]["agent_id"] == participant.identity["agent_id"]
    assert (started["checkout_id"], started["scope_id"], started["scope_revision"]) == (
        job["checkout_id"], job["scope_id"], job["scope_revision"],
    )
    return ["coding_job_waits_for_its_verified_checkout_host_then_leases_on_the_other_owners_runner"]


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--linux-vm", action="store_true")
    parser.add_argument("--runner", type=Path)
    args = parser.parse_args()
    assert args.linux_vm or os.name != "nt", "Use --linux-vm on Windows"
    binary = (args.runner or ROOT / (".cocodeop-test/linux-bin/office-runner" if args.linux_vm
                                     else "target/release/office-runner")).resolve()
    assert binary.is_file(), f"Build the Linux runner: {binary}"
    with tempfile.TemporaryDirectory(prefix="cocodeop-leases-") as directory:
        tmp = Path(directory)
        with workspace_fixture(tmp, "scheduler-leases") as office:
            print(json.dumps({"checks": verify(office, tmp, args.linux_vm, binary),
                              "linux_execution": "debian13_qemu_guest" if args.linux_vm else "native_linux",
                              "independent_physical_hosts": False}))
