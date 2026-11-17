"""Check admission, stopping, and reconciliation against the real server and PostgreSQL."""

import json
import secrets
import tempfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from uuid import uuid4

from m02_bootstrap import request
from m07_leases import register_runner
from m07_queue import configure, create_template, enqueue
from m07_start_effects import proof
from workspace_fixture import digest, workspace_fixture


def set_control(office, token, kind, revision, mode, outstanding, concurrent, **scope):
    return request(office.endpoint, "/v1/execution-control/set", token=token, body={
        "workspace_id": office.workspace, "scope": {"kind": kind, **scope},
        "expected_revision": revision, "mode": mode, "reason": "Test operator control",
        "limits": {"max_outstanding": outstanding, "max_concurrent": concurrent},
        "idempotency_key": str(uuid4()),
    })


def set_mode(office, token, mode, revision=0):
    status, reply = set_control(office, token, "owner", revision, mode, 256, 32)
    assert status == 200, reply
    return reply["result"]


def verify(office):
    owners = (office.owner_a, office.owner_b)
    leads = [office.agent(owner, f"Lead {label}", "lead")[0] for owner, label in zip(owners, "AB")]
    workers = [office.agent(owner, f"Worker {label}", "worker") for owner, label in zip(owners, "AB")]
    runners = [register_runner(office, owner, f"Runner {label}")[1] for owner, label in zip(owners, "AB")]
    backups = [register_runner(office, owner, f"Backup {label}")[1] for owner, label in zip(owners, "AB")]
    runtimes = [create_template(office, owner, units=10) for owner in owners]
    teams = []
    for owner, lead, (_, worker), runtime in zip(owners, leads, workers, runtimes):
        assert configure(office, owner, 100).get("budget")
        team = office.tool(lead, "office_create_team", name="Owner team", idempotency_key=str(uuid4()))["team"]
        membership = office.tool(lead, "office_set_team_member", team_id=team["team_id"],
                                 expected_revision=team["revision"], agent_id=worker["agent_id"],
                                 role="worker", idempotency_key=str(uuid4()))
        assert membership.get("team"), membership
        teams.append(team["team_id"])

    def runner(token, route, body, code=None):
        status, result = request(office.endpoint, f"/v1/runner-scheduler/{route}", token=token, body=body)
        if code:
            assert result.get("error", {}).get("code") == code, (status, result)
            return
        assert status == 200, (status, result)
        return result["result"]

    for primary, backup, runtime in zip(runners, backups, runtimes):
        for token in (primary, backup):
            runner(token, "profile", {"expected_revision": 0, "capacity": 8, "runtime_template_ids": [runtime],
                                      "capabilities": ["linux"], "ttl_seconds": 120})

    def task(index):
        lead, team_id = leads[index], teams[index]
        draft = office.tool(lead, "office_create_task", team_id=team_id, objective="Verify resource allowance",
                            acceptance_criteria=["Record one start"], idempotency_key=str(uuid4()))["task"]
        return office.tool(lead, "office_prepare_task", task_id=draft["task_id"], expected_revision=draft["revision"],
                           idempotency_key=str(uuid4()))["task"]

    def queue(index, ready, key=None):
        return enqueue(office, leads[index], ready, workers[index][1], runtimes[index], key=key)

    def claim(token):
        return runner(token, "claim", {"claim_key": str(uuid4()), "analysis_only": True})

    def status(index, job):
        return office.tool(leads[index], "office_get_execution", job_id=job["job_id"])

    def workspace_control(revision, mode, pending, active):
        code, result = set_control(office, owners[0], "workspace", revision, mode, pending, active)
        assert code == 200, result
        return result["result"]

    for actor in (owners[1], leads[0], workers[0][0]):
        http, _ = set_control(office, actor, "workspace", 0, "stopped", 1, 1)
        assert http in (401, 403)
    http, _ = set_control(office, owners[1], "agent", 0, "running", 1, 1, agent_id=workers[0][1]["agent_id"])
    assert http == 403
    assert workspace_control(0, "running", 1, 1)["revision"] == 1
    prepared = [task(index) for index in range(2)]
    keys = [str(uuid4()), str(uuid4())]
    with ThreadPoolExecutor(max_workers=2) as pool:
        results = list(pool.map(lambda index: queue(index, prepared[index], keys[index]), range(2)))
    first_owner = next(index for index, result in enumerate(results) if "job" in result)
    other_owner = 1 - first_owner
    assert results[other_owner]["code"] == "EXECUTION_CAPACITY_REACHED", results
    assert queue(first_owner, prepared[first_owner], keys[first_owner]) == results[first_owner]
    assert office.sql("SELECT count(*) FROM execution_budget_reservations WHERE released_at IS NULL") == "1"

    workspace_control(1, "running", 2, 1)
    results[other_owner] = queue(other_owner, prepared[other_owner])
    assert results[other_owner].get("job"), results
    with ThreadPoolExecutor(max_workers=2) as pool:
        leases = list(pool.map(claim, runners))
    assert sum(item is not None for item in leases) == 1, leases
    active = next(index for index, lease in enumerate(leases) if lease is not None)
    peer = 1 - active
    current = leases[active]["run"]
    active_job = results[active]["job"]
    peer_job = results[peer]["job"]
    old_proof = proof(current)
    started = runner(runners[active], "start/authorize", {"lease": old_proof, "idempotency_key": str(uuid4())})
    effect = {"effect_id": started["effect"]["effect_id"], "lease": old_proof}
    worker_token = "crun_" + secrets.token_hex(32)
    runner(runners[active], "run-credential", {"effect": effect, "credential_digest": digest(worker_token)})
    external = "test-process:" + uuid4().hex
    runner(runners[active], "start/observe", {"effect": effect, "external_id": external})
    def read_context():
        return request(office.endpoint, "/run/mcp", token=worker_token, rpc={
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "office_run_context", "arguments": {}},
        })
    assert read_context()[1]["result"]["structuredContent"]["run_id"] == current["run_id"]
    assert set_mode(office, owners[active], "paused")["mode"] == "paused"
    assert runner(runners[active], "renew", old_proof)["status"] == "running"
    assert queue(active, task(active))["code"] == "EXECUTION_PAUSED"
    set_mode(office, owners[active], "stopped", 1)
    runner(runners[active], "renew", old_proof, code="RUN_LEASE_STALE")
    http, rejected = read_context()
    assert http == 409 and rejected["error"]["code"] == "RUN_LEASE_STALE", (http, rejected)
    assert claim(backups[active]) is None
    assert status(active, active_job)["state"] == "reconcile_required"
    assert status(peer, peer_job)["state"] == "queued"
    set_mode(office, owners[active], "running", 2)
    runner(runners[active], "renew", old_proof, code="RUN_LEASE_STALE")
    assert claim(runners[peer]) is None
    stopped = runner(runners[active], "start/reconcile", {
        "effect": effect, "observation": {"kind": "stopped", "external_id": external},
    })
    assert stopped["run"]["status"] == "cancelled" and stopped["effect"]["state"] == "stopped", stopped
    assert status(active, active_job)["terminal_reason"] == "emergency_stopped"
    peer_attempt = claim(runners[peer])["run"]
    assert peer_attempt["lease"]["job_id"] == peer_job["job_id"]

    workspace_control(2, "stopped", 2, 1)
    assert claim(backups[peer]) is None
    assert status(peer, peer_job)["terminal_reason"] == "emergency_stopped"
    assert office.sql("SELECT count(*) FROM execution_budget_reservations WHERE released_at IS NULL") == "0"
    workspace_control(3, "running", 4, 2)
    runner(runners[peer], "start/authorize", {"lease": proof(peer_attempt), "idempotency_key": str(uuid4())},
           code="RUN_LEASE_STALE")

    agent_id = workers[0][1]["agent_id"]
    assert set_control(office, owners[0], "agent", 0, "running", 1, 1, agent_id=agent_id)[0] == 200
    agent_job = queue(0, task(0))["job"]
    assert queue(0, task(0))["code"] == "EXECUTION_CAPACITY_REACHED"
    assert set_control(office, owners[0], "agent", 1, "running", 4, 2, agent_id=agent_id)[0] == 200
    assert set_control(office, owners[0], "team", 0, "running", 1, 1, team_id=teams[0])[0] == 200
    assert queue(0, task(0))["code"] == "EXECUTION_CAPACITY_REACHED"
    assert set_control(office, owners[0], "team", 1, "running", 4, 2, team_id=teams[0])[0] == 200
    assert set_control(office, owners[0], "owner", 0 if active != 0 else 3, "running", 1, 1)[0] == 200
    assert queue(0, task(0))["code"] == "EXECUTION_CAPACITY_REACHED"
    assert status(0, agent_job)["state"] == "queued"
    assert queue(1, task(1)).get("job")

    channel = office.tool(leads[0], "office_create_channel", name="Rate-limit test",
                          audience={"kind": "workspace"}, idempotency_key=str(uuid4()))["channel_id"]
    lead_id = office.tool(leads[0], "office_bootstrap")["agent_id"]

    def post(index):
        return office.tool(leads[0], "office_send_message", channel_id=channel, body=f"Message {index}",
                           idempotency_key=str(uuid4()))

    # Exercise the last slot without depending on how many HTTP calls fit in one minute.
    for _ in range(2):
        minute = office.sql(f"INSERT INTO message_rate_windows (workspace_id,kind,subject_id,minute,messages) "
                            f"VALUES ('{office.workspace}','agent','{lead_id}',floor(extract(epoch FROM now())/60),119) "
                            "ON CONFLICT(workspace_id,kind,subject_id) DO UPDATE SET minute=excluded.minute,messages=119 "
                            "RETURNING minute").splitlines()[0]
        with ThreadPoolExecutor(max_workers=2) as pool:
            sends = list(pool.map(post, range(2)))
        if office.sql("SELECT floor(extract(epoch FROM now())/60)::bigint") == minute:
            break
    else:
        raise AssertionError("The rate-window clock changed during both attempts")
    assert sum("message_id" in result for result in sends) == 1, sends
    assert sum(result.get("code") == "MESSAGE_RATE_LIMITED" for result in sends) == 1, sends

    return {
        "schema": int(office.sql("SELECT max(version) FROM _sqlx_migrations")),
        "stopped_run_id": current["run_id"], "stop_effect_id": effect["effect_id"],
        "pending_then_stopped": stopped["effect"]["state"],
        "checks": [
            "workspace_controls_require_an_human_admin_and_team_agent_controls_are_owner_scoped",
            "concurrent_cross_owner_queue_and_start_respect_the_same_atomic_workspace_limits",
            "agent_team_and_owner_outstanding_caps_overlay_workspace_capacity",
            "pausing_blocks_new_work_and_keeps_a_running_lease",
            "emergency_stop_revokes_worker_credentials_and_holds_the_slot_until_stop_is_observed",
            "a_stopped_generation_cannot_restart_after_owner_or_workspace_resume",
            "concurrent_messages_share_the_last_agent_rate_slot",
        ],
    }


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="cocodeop-controls-") as directory:
        with workspace_fixture(Path(directory), "execution-controls") as office:
            print(json.dumps(verify(office)))
