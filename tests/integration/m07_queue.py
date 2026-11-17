"""Verify durable managed admission, exclusive task ownership and atomic budget reservations."""

import json
import tempfile
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timedelta, timezone
from pathlib import Path
from uuid import uuid4

from m02_bootstrap import call
from m05a_claims import Participant
from workspace_fixture import workspace_fixture


def create_template(office, owner, name="Managed runtime", units=30):
    response = office.tool(owner, "office_create_runtime_template", name=name, kind="managed",
                           managed={"adapter_id": "openai-compat.v1", "reservation_units": units},
                           idempotency_key=str(uuid4()))
    assert response.get("template_id"), response
    return response["template_id"]


def configure(office, owner, limit, revision=0):
    return office.tool(owner, "office_set_execution_budget", expected_revision=revision,
                       limit_units=limit, idempotency_key=str(uuid4()))


def enqueue(office, lead, task, worker, runtime, key=None, revision=None):
    return office.tool(lead, "office_queue_execution", task_id=task["task_id"],
                       expected_task_revision=revision if revision is not None else task["revision"],
                       worker_agent_id=worker["agent_id"], runtime_template_id=runtime, idempotency_key=key or str(uuid4()))


def verify(office, tmp):
    lead_a, identity_a = office.agent(office.owner_a, "Backend lead", "lead")
    lead_b, _ = office.agent(office.owner_b, "Client lead", "lead")
    worker_a, agent_a = office.agent(office.owner_a, "Backend worker", "worker")
    worker_b, agent_b = office.agent(office.owner_b, "Client worker", "worker")
    sibling_a, sibling = office.agent(office.owner_a, "Backend secondary worker", "worker")
    runtime_a = create_template(office, office.owner_a, "Backend runtime")
    runtime_b = create_template(office, office.owner_b, "Client runtime")
    instances = {}
    for token in (worker_a, worker_b):
        status, response = call(office.endpoint, token, "tools/call", {"name": "office_attach_instance", "arguments": {
            "idempotency_key": str(uuid4()), "label": "Interactive precedence check",
        }})
        assert status == 200, response
        instances[token] = response["result"]["structuredContent"]["instance_id"]

    def task(lead=lead_a, prepared=True, **extra):
        draft = office.tool(lead, "office_create_task", objective="Build a bounded managed workflow",
                            acceptance_criteria=["Record an accepted result"], idempotency_key=str(uuid4()), **extra)["task"]
        return office.tool(lead, "office_prepare_task", task_id=draft["task_id"], expected_revision=draft["revision"],
                           idempotency_key=str(uuid4()))["task"] if prepared else draft

    def budget(owner=office.owner_a):
        return office.cli(owner, "scheduler", "budget", "--workspace", office.workspace)

    def read_job(job, token=lead_a):
        return office.tool(token, "office_get_execution", job_id=job["job_id"])

    def cancel(job, token=lead_a, key=None, revision=None):
        return office.tool(token, "office_cancel_execution", job_id=job["job_id"],
                           expected_revision=revision if revision is not None else job["revision"],
                           idempotency_key=key or str(uuid4()), reason="Release queued capacity")

    draft = task(prepared=False)
    ready = task()
    assert enqueue(office, lead_a, draft, agent_a, runtime_a)["code"] == "INVALID_TRANSITION"
    assert enqueue(office, lead_a, ready, agent_a, runtime_a, revision=1)["code"] == "REVISION_CONFLICT"
    assert enqueue(office, lead_a, ready, agent_b, runtime_a)["code"] == "FORBIDDEN"
    assert enqueue(office, lead_b, ready, agent_b, runtime_b)["code"] == "FORBIDDEN"
    assert enqueue(office, lead_a, ready, agent_a, runtime_b)["code"] == "FORBIDDEN"
    hidden = task(visibility={"kind": "owner_private"})
    assert enqueue(office, lead_a, hidden, agent_a, runtime_a)["code"] == "FORBIDDEN"
    assert enqueue(office, lead_a, ready, agent_a, runtime_a)["code"] == "BUDGET_EXHAUSTED"
    assert office.sql("SELECT count(*) FROM execution_jobs") == "0"
    funded_a = configure(office, office.owner_a, 50)["budget"]
    funded_b = configure(office, office.owner_b, 30)["budget"]
    checks = ["admission_requires_ready_revision_exact_owned_worker_template_acl_and_funding"]

    assigned = office.tool(lead_a, "office_change_task", task_id=ready["task_id"], expected_revision=ready["revision"],
                           change={"operation": "assign", "agent_id": agent_a["agent_id"]}, idempotency_key=str(uuid4()))["task"]
    assert enqueue(office, lead_a, assigned, sibling, runtime_a)["code"] == "FORBIDDEN"
    other_ready = task()
    keys = [str(uuid4()), str(uuid4())]
    pair = (assigned, other_ready)
    with ThreadPoolExecutor(max_workers=2) as pool:
        queued = [pool.submit(enqueue, office, lead_a, entry, agent_a, runtime_a, key=key)
                  for entry, key in zip(pair, keys)]
        answers = [item.result() for item in queued]
    assert sum("job" in answer for answer in answers) == 1
    assert sum(answer.get("code") == "BUDGET_EXHAUSTED" for answer in answers) == 1, answers
    winner = next(index for index, answer in enumerate(answers) if "job" in answer)
    receipt = answers[winner]
    owning_task = pair[winner]
    job = receipt["job"]
    assert budget()["reserved_units"] == 30 and budget()["budget_id"] == funded_a["budget_id"]
    assert office.sql("SELECT count(*) FROM execution_jobs") == "1"
    assert office.sql("SELECT count(*) FROM execution_budget_reservations WHERE released_at IS NULL") == "1"
    assert configure(office, office.owner_a, 29, funded_a["revision"])["code"] == "INVALID_TRANSITION"
    assert enqueue(office, lead_a, owning_task, agent_a, runtime_a, key=keys[winner]) == receipt
    assert enqueue(office, lead_a, owning_task, agent_a, runtime_a)["code"] == "INVALID_TRANSITION"
    checks.append("concurrent_jobs_share_one_atomic_budget_reservation_without_partial_rows_or_duplicates")

    contender = task(lead_b)
    client_job = enqueue(office, lead_b, contender, agent_b, runtime_b)["job"]
    assert budget(office.owner_b)["reserved_units"] == 30 and budget(office.owner_b)["budget_id"] == funded_b["budget_id"]
    assert budget()["reserved_units"] == 30
    assert read_job(job, worker_a) == job and read_job(job, worker_b)["code"] == "FORBIDDEN"
    assert read_job(job, sibling_a)["code"] == "FORBIDDEN"
    assert cancel(job, worker_a)["code"] == "FORBIDDEN"
    eligible = office.tool(lead_a, "office_get_task", task_id=owning_task["task_id"])
    assert eligible["runs"] == [] and eligible["task"]["revision"] == owning_task["revision"]
    for token in (worker_a, worker_b):
        claimed = office.tool(token, "office_claim_task", task_id=owning_task["task_id"],
                              expected_revision=owning_task["revision"], instance_id=instances[token],
                              idempotency_key=str(uuid4()))
        assert claimed["code"] in ("FORBIDDEN", "INVALID_TRANSITION"), claimed
    assert office.tool(worker_a, "office_claim_task", task_id=contender["task_id"],
                       expected_revision=contender["revision"], instance_id=instances[worker_a],
                       idempotency_key=str(uuid4()))["code"] == "INVALID_TRANSITION"
    operations = {"operation": "revise_criteria", "expected_criteria_revision": 1,
                  "acceptance_criteria": ["Do different work"]}
    assert office.tool(lead_a, "office_change_task", task_id=owning_task["task_id"],
                       expected_revision=owning_task["revision"], change=operations,
                       idempotency_key=str(uuid4()))["code"] == "INVALID_TRANSITION"
    checks.append("accepted_queue_excludes_interactive_admission_and_cannot_access_or_spend_another_owners_budget")

    office.restart()
    assert office.cli(worker_a, "scheduler", "job", "--workspace", office.workspace, "--id", job["job_id"]) == job
    assert budget()["reserved_units"] == 30
    assert read_job(client_job, lead_b)["state"] == "queued"
    assert cancel(job, revision=0)["code"] == "REVISION_CONFLICT"
    key = str(uuid4())
    cancelled = cancel(job, key=key)
    assert cancelled["job"]["state"] == "cancelled" and cancelled["job"]["revision"] == 2
    assert cancelled == cancel(job, key=key) and budget()["reserved_units"] == 0
    assert budget()["charged_units"] == 0
    assert cancel(job)["code"] == "REVISION_CONFLICT"
    closed = office.tool(lead_b, "office_change_task", task_id=contender["task_id"],
                         expected_revision=contender["revision"], idempotency_key=str(uuid4()),
                         change={"operation": "cancel", "reason": "Close the queued task"})
    assert closed["task"]["status"] == "cancelled" and read_job(client_job, lead_b)["state"] == "cancelled"
    assert budget(office.owner_b)["reserved_units"] == 0
    assert office.sql("SET ROLE cocodeop; SELECT count(*) FROM execution_jobs; "
                      "SELECT count(*) FROM execution_budget_reservations").splitlines()[-2:] == ["0", "0"]
    checks.append("queue_and_budget_survive_restart_and_both_cancellation_paths_release_once")

    new_task = pair[1 - winner]
    params = {"workspace_id": office.workspace, "task_id": new_task["task_id"],
              "expected_task_revision": new_task["revision"], "worker_agent_id": agent_a["agent_id"],
              "runtime_template_id": runtime_a, "idempotency_key": str(uuid4())}
    queue_file = tmp / "queue.json"
    queue_file.write_text(json.dumps(params), encoding="utf-8")
    cli_job = office.cli(lead_a, "scheduler", "queue", "--file", str(queue_file))["job"]
    assert read_job(cli_job) == cli_job and budget()["reserved_units"] == 30
    assert cancel(cli_job)["job"]["state"] == "cancelled"
    checks.append("cli_and_mcp_share_the_same_durable_queue")

    team = office.tool(lead_a, "office_create_team", name="Delegated queue", idempotency_key=str(uuid4()))["team"]
    office.tool(lead_a, "office_set_team_member", team_id=team["team_id"], expected_revision=team["revision"],
                agent_id=agent_a["agent_id"], role="worker", idempotency_key=str(uuid4()))
    parent = task(prepared=False, team_id=team["team_id"])
    limits = {"remaining_depth": 1, "expires_at": (datetime.now(timezone.utc) + timedelta(hours=2)).isoformat(),
              "max_attempts": 2, "max_subtasks": 1, "runtime_template_ids": [runtime_a]}
    root = office.tool(office.owner_a, "office_issue_delegation", task_id=parent["task_id"],
                       expected_revision=parent["revision"], lead_agent_id=identity_a["agent_id"],
                       limits=limits, idempotency_key=str(uuid4()))["grant"]
    child = office.tool(lead_a, "office_delegate_task", grant_id=root["grant_id"], expected_revision=root["revision"],
                        worker_agent_id=agent_a["agent_id"], objective="Queue only the delegated runtime",
                        acceptance_criteria=["Select the expected runtime"], idempotency_key=str(uuid4()),
                        limits={**limits, "remaining_depth": 0, "max_attempts": 1, "max_subtasks": 0})["grant"]
    delegated = office.tool(lead_a, "office_get_task", task_id=child["task_id"])["task"]
    unapproved_runtime = create_template(office, office.owner_a, "Undelegated alternative")
    assert enqueue(office, lead_a, delegated, agent_a, unapproved_runtime)["code"] == "FORBIDDEN"
    queued_child = enqueue(office, lead_a, delegated, agent_a, runtime_a)["job"]
    assert office.tool(lead_a, "office_get_delegation", grant_id=child["grant_id"])["attempts_used"] == 0
    assert cancel(queued_child)["job"]["state"] == "cancelled"
    checks.append("delegated_jobs_check_ancestor_authority_and_defer_attempt_consumption_until_leasing")
    return checks


def verify_checkout_queue(office, host, checkout, second, leases=False):
    owner = office.owner_b
    paths = [{"kind": "directory", "path": "parts/managed"}]
    permissions = {"read": {"allow": paths}, "write": {"allow": paths}, "write_operations": ["create", "replace"]}
    delegated = Participant(office, checkout, second, owner, "Managed coding", permissions)
    runtime = create_template(office, owner)
    assert configure(office, owner, 30).get("budget")
    draft = office.tool(delegated.lead, "office_get_task", task_id=delegated.task)["task"]
    assert enqueue(office, delegated.lead, draft, delegated.identity, runtime)["code"] == "INVALID_TRANSITION"
    state = "queued-coding"
    host.invoke(["mkdir", "-m", "700", "--", f"{host.root}/{state}"], check=True)
    applied = host.sync_scopes(office.endpoint, checkout, state=state)
    assert applied.returncode == 0, applied.stderr
    scope = next(item["definition"] for item in json.loads(applied.stdout)
                 if item["definition"]["task_id"] == delegated.task)
    ready = office.tool(delegated.lead, "office_prepare_task", task_id=delegated.task,
                        expected_revision=delegated.task_revision, idempotency_key=str(uuid4()))["task"]
    queued = enqueue(office, delegated.lead, ready, delegated.identity, runtime)
    assert queued.get("job"), queued
    job = queued["job"]
    assert (job["checkout_id"], job["scope_id"], job["scope_revision"]) == (
        checkout["checkout_id"], scope["scope_id"], scope["revision"],
    )
    assert job["state"] == "queued" and job["owner_user_id"] == delegated.identity["owner_user_id"]
    assert office.tool(delegated.worker, "office_get_task", task_id=delegated.task)["runs"] == []
    checks = ["coding_queue_records_verified_scope_and_separate_checkout_host_and_executor"]
    if leases:
        from m07_leases import verify_checkout_placement
        checks.extend(verify_checkout_placement(office, host, checkout, job, runtime, delegated))
    return checks


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="cocodeop-queue-") as directory:
        tmp = Path(directory)
        with workspace_fixture(tmp, "scheduler-queue") as office:
            print(json.dumps({"checks": verify(office, tmp)}))
