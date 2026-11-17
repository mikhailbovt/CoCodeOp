"""Prove one explicit shared execution grant is reserved atomically by independent owners."""

import json
import tempfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from uuid import uuid4

from m02_bootstrap import request
from m07_queue import configure, create_template, enqueue
from workspace_fixture import workspace_fixture


def verify(office):
    participants = []
    for index, token in enumerate((office.owner_a, office.owner_b)):
        lead, identity = office.agent(token, f"Budget lead {index}", "lead")
        _, worker = office.agent(token, f"Budget worker {index}", "worker")
        assert configure(office, token, 100).get("budget")
        template = create_template(office, token, f"Budget template {index}", units=10)
        draft = office.tool(lead, "office_create_task", objective="Hold one shared allocation",
                            acceptance_criteria=["Reserve one atomic slot"], idempotency_key=str(uuid4()))["task"]
        task = office.tool(lead, "office_prepare_task", task_id=draft["task_id"],
                           expected_revision=draft["revision"], idempotency_key=str(uuid4()))["task"]
        participants.append((lead, identity["owner_user_id"], task, worker, template))
    workspace = office.workspace
    setup = {"workspace_id": workspace, "limit_units": 15, "expected_revision": 0,
             "member_user_ids": [participant[1] for participant in participants], "idempotency_key": str(uuid4())}
    for token, expected in ((office.owner_b, 403), (participants[0][0], 401)):
        status, error = request(office.endpoint, "/v1/workspace-budget/set", token=token, body=setup)
        assert status == expected, error
    configured = office.api("/v1/workspace-budget/set", office.owner_a, setup)
    assert configured["revision"] == 1 and configured["reserved_units"] == 0, configured
    assert office.api("/v1/workspace-budget/set", office.owner_a, setup) == configured
    stranger = dict(setup, expected_revision=1, member_user_ids=[str(uuid4())], idempotency_key=str(uuid4()))
    status, reply = request(office.endpoint, "/v1/workspace-budget/set", token=office.owner_a, body=stranger)
    assert status == 403 and reply["error"]["code"] == "FORBIDDEN", reply

    def launch(index):
        lead, _, task, worker, template = participants[index]
        return enqueue(office, lead, task, worker, template)

    with ThreadPoolExecutor(max_workers=2) as pool:
        results = list(pool.map(launch, range(2)))
    winners = [index for index, value in enumerate(results) if value.get("job")]
    assert len(winners) == 1, results
    winner = winners[0]
    loser = 1 - winner
    assert results[loser]["code"] == "BUDGET_EXHAUSTED", results
    from_a = office.cli(office.owner_a, "scheduler", "workspace-budget", "--workspace", workspace)
    from_b = office.cli(office.owner_b, "scheduler", "workspace-budget", "--workspace", workspace)
    assert from_a == from_b and from_a["reserved_units"] == 10 and from_a["charged_units"] == 0, from_a
    assert office.sql("SELECT count(*) FROM execution_budget_reservations WHERE uses_workspace_budget") == "1"
    held = results[winner]["job"]
    cancelled = office.tool(participants[winner][0], "office_cancel_execution", job_id=held["job_id"],
                            expected_revision=held["revision"], idempotency_key=str(uuid4()), reason="Release common grant")
    assert cancelled["job"]["state"] == "cancelled", cancelled
    assert launch(loser)["job"]["owner_user_id"] == participants[loser][1]
    office.restart()
    restored = office.cli(office.owner_b, "scheduler", "workspace-budget", "--workspace", workspace)
    assert restored["reserved_units"] == 10 and restored["limit_units"] == 15, restored
    return {"checks": ["shared_grant_is_explicit_and_requires_workspace_administrator",
                       "independent_owners_cannot_overallocate_one_shared_grant",
                       "cancellation_releases_shared_capacity_and_restarted_core_preserves_allocations"],
            "workspace_id": workspace, "owner_ids": setup["member_user_ids"], "grant": restored}


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="cocodeop-workspace-budget-") as directory:
        with workspace_fixture(Path(directory), "shared-budget") as office:
            print(json.dumps(verify(office)))
