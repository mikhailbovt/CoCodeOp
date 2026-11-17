"""Verify delegation limits and run admission against the real store and MCP tools."""

import json
import tempfile
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timedelta, timezone
from pathlib import Path
from uuid import uuid4

from m02_bootstrap import call
from workspace_fixture import workspace_fixture


def run(tmp):
    checks = []
    with workspace_fixture(tmp, "delegation") as office:
        lead, lead_id = office.agent(office.owner_a, "Backend lead", "lead")
        other_lead, other_id = office.agent(office.owner_b, "Client lead", "lead")
        workers = [office.agent(office.owner_a, f"Developer {i}", "worker") for i in range(1, 5)]
        outside = office.agent(office.owner_b, "Client worker", "worker")
        team = office.tool(lead, "office_create_team", name="Backend", idempotency_key=str(uuid4()))["team"]
        other_team = office.tool(other_lead, "office_create_team", name="Client QA", idempotency_key=str(uuid4()))["team"]
        for _, identity in workers:
            response = office.tool(lead, "office_set_team_member", team_id=team["team_id"], role="worker",
                                   expected_revision=team["revision"], agent_id=identity["agent_id"],
                                   idempotency_key=str(uuid4()))
            team = response["team"]

        def template(token, name):
            return office.tool(token, "office_create_runtime_template", name=name, kind="interactive",
                               idempotency_key=str(uuid4()))

        payload = tmp / "template.json"
        payload.write_text(json.dumps({"workspace_id": office.workspace, "name": "Interactive engineering",
                                       "kind": "interactive", "idempotency_key": str(uuid4())}), encoding="utf-8")
        runtime = office.cli(office.owner_a, "delegation", "create-runtime", "--file", str(payload))["template_id"]
        foreign_runtime = template(office.owner_b, "Client interactive")["template_id"]
        spare_runtime = template(office.owner_a, "Office review")["template_id"]
        assert template(lead, "Agent-generated template")["code"] == "FORBIDDEN"

        def task(label):
            return office.tool(lead, "office_create_task", team_id=team["team_id"], objective=label,
                               acceptance_criteria=["Publish a reviewable result"], idempotency_key=str(uuid4()))["task"]

        def get(token, task_id):
            return office.tool(token, "office_get_task", task_id=task_id)

        def limits(depth, attempts, subtasks, **extra):
            return dict({"remaining_depth": depth, "max_attempts": attempts, "max_subtasks": subtasks,
                         "expires_at": (datetime.now(timezone.utc) + timedelta(hours=1)).isoformat(),
                         "runtime_template_ids": [runtime]}, **extra)

        def issue(token, source, parameters):
            return office.tool(token, "office_issue_delegation", task_id=source["task_id"],
                               expected_revision=source["revision"], lead_agent_id=lead_id["agent_id"],
                               limits=parameters, idempotency_key=str(uuid4()))

        def read_grant(token, grant):
            return office.tool(token, "office_get_delegation", grant_id=grant["grant_id"])

        def revoke(token, grant, key=None):
            return office.tool(token, "office_revoke_delegation", grant_id=grant["grant_id"],
                               expected_revision=grant["revision"], idempotency_key=key or str(uuid4()))

        def delegate(token, parent, worker, depth, attempts=1, subtasks=2, key=None, **extra):
            inherited = {"expires_at": parent["limits"]["expires_at"], **extra}
            return office.tool(token, "office_delegate_task", grant_id=parent["grant_id"],
                               expected_revision=parent["revision"], idempotency_key=key or str(uuid4()),
                               worker_agent_id=worker["agent_id"], objective="Implement the next contract slice",
                               acceptance_criteria=["Record its implementation result"],
                               limits=limits(depth, attempts, subtasks, **inherited))

        def attach(token):
            status, reply = call(office.endpoint, token, "tools/call", {
                "name": "office_attach_instance", "arguments": {"idempotency_key": str(uuid4()), "label": "Delegated"},
            })
            assert status == 200, reply
            return reply["result"]["structuredContent"]["instance_id"]

        def claim(token, grant, instance, template_id=runtime, key=None):
            return office.tool(token, "office_claim_task", task_id=grant["task_id"], expected_revision=1,
                               instance_id=instance, runtime_template_id=template_id, idempotency_key=key or str(uuid4()))

        def change(token, current, operation, **fields):
            return office.tool(token, "office_change_task", task_id=current["task_id"], expected_revision=current["revision"],
                               change=dict(operation=operation, **fields), idempotency_key=str(uuid4()))

        main = task("Accountable engineering")
        assert issue(lead, main, limits(3, 2, 6))["code"] == "FORBIDDEN"
        assert issue(office.owner_b, main, limits(3, 2, 6))["code"] == "FORBIDDEN"
        assert issue(office.owner_a, main, limits(3, 2, 6, runtime_template_ids=[foreign_runtime]))["code"] == "FORBIDDEN"
        assert issue(office.owner_a, main, limits(3, 2, 6, expires_at="2020-01-01T00:00:00Z"))["code"] == "INVALID_INPUT"
        root = issue(office.owner_a, main, limits(3, 2, 6))["grant"]
        assert get(lead, main["task_id"])["task"]["delegation_grant_id"] == root["grant_id"]
        assert read_grant(other_lead, root)["code"] == "FORBIDDEN"
        assert revoke(other_lead, root)["code"] == "FORBIDDEN"
        assert delegate(other_lead, root, workers[0][1], 2)["code"] == "FORBIDDEN"
        assert delegate(lead, root, outside[1], 2)["code"] == "FORBIDDEN"
        checks.extend(["root_grant_and_runtime_require_current_human_owner", "cross_owner_work_and_runtime_denied"])

        key = str(uuid4())
        entry = delegate(lead, root, workers[0][1], 2, attempts=2, subtasks=4, key=key)
        assert entry == delegate(lead, root, workers[0][1], 2, attempts=2, subtasks=4, key=key)
        first = entry["grant"]
        assert delegate(workers[1][0], first, workers[2][1], 1)["code"] == "FORBIDDEN"
        assert delegate(workers[0][0], first, workers[1][1], 2)["code"] == "DELEGATION_LIMIT"
        assert delegate(workers[0][0], first, workers[1][1], 1, attempts=3)["code"] == "DELEGATION_LIMIT"
        assert delegate(workers[0][0], first, workers[1][1], 1, expires_at=(datetime.now(timezone.utc) + timedelta(hours=2)).isoformat())["code"] == "DELEGATION_LIMIT"
        assert delegate(workers[0][0], first, workers[1][1], 1, runtime_template_ids=[spare_runtime])["code"] == "DELEGATION_LIMIT"
        second = delegate(workers[0][0], first, workers[1][1], 1)["grant"]
        assert delegate(workers[1][0], second, workers[0][1], 0)["code"] == "DELEGATION_CYCLE"
        third = delegate(workers[1][0], second, workers[2][1], 0)["grant"]
        assert delegate(workers[2][0], third, workers[3][1], 0)["code"] == "DELEGATION_LIMIT"
        third_task = get(lead, third["task_id"])["task"]
        assert third_task["parent_task_id"] == second["task_id"] and third_task["status"] == "assigned"
        assert third_task["accountable"] == {"kind": "agent", "agent_id": lead_id["agent_id"]}
        assert get(other_lead, third["task_id"])["code"] == "FORBIDDEN"
        assert read_grant(lead, root)["subtasks_used"] == 3
        checks.extend(["three_delegation_levels_preserve_task_and_resource_provenance",
                       "reject_depth_ttl_limits_runtime_and_ancestor_cycle", "idempotent_subtask_creates_one_reservation"])

        instance = attach(workers[2][0])
        assert claim(workers[2][0], third, instance, foreign_runtime)["code"] == "FORBIDDEN"
        assert claim(workers[2][0], third, instance, None)["code"] == "INVALID_INPUT"
        assert claim(workers[2][0], third, str(uuid4()))["code"] == "FORBIDDEN"
        assert read_grant(lead, root)["attempts_used"] == 0
        claim_key = str(uuid4())
        entered = claim(workers[2][0], third, instance, key=claim_key)
        replay = claim(workers[2][0], third, instance, key=claim_key)
        assert set(entered) == set(replay)
        assert all(value == replay[key] for key, value in entered.items() if key != "request_id"), (entered, replay)
        assert entered["run"]["runtime_template_id"] == runtime
        assert entered["run"]["executor"]["agent_id"] == workers[2][1]["agent_id"]
        fourth = delegate(lead, root, workers[3][1], 1)["grant"]
        candidates = [(workers[0][0], first, attach(workers[0][0])), (workers[3][0], fourth, attach(workers[3][0]))]
        with ThreadPoolExecutor(max_workers=2) as pool:
            results = list(pool.map(lambda work: claim(work[0], work[1], work[2]), candidates))
        assert sum(result.get("run", {}).get("status") == "running" for result in results) == 1, results
        assert sum(result.get("code") == "DELEGATION_LIMIT" for result in results) == 1, results
        assert read_grant(lead, root)["attempts_used"] == 2
        checks.extend(["exact_runtime_and_instance_required_without_spending_on_failure", "concurrent_branches_share_atomic_attempt_limit"])

        other_team = office.tool(other_lead, "office_set_team_member", team_id=other_team["team_id"], role="worker",
                                 agent_id=outside[1]["agent_id"], expected_revision=other_team["revision"],
                                 idempotency_key=str(uuid4()))["team"]
        other_task = office.tool(other_lead, "office_create_task", team_id=other_team["team_id"], idempotency_key=str(uuid4()),
                                 objective="Check the consumer contract", acceptance_criteria=["Record compatibility"])["task"]
        other_root = office.tool(office.owner_b, "office_issue_delegation", task_id=other_task["task_id"],
                                 expected_revision=other_task["revision"], lead_agent_id=other_id["agent_id"],
                                 idempotency_key=str(uuid4()), limits=limits(1, 1, 1, runtime_template_ids=[foreign_runtime]))["grant"]
        other_child = delegate(other_lead, other_root, outside[1], 0, subtasks=0, runtime_template_ids=[foreign_runtime])["grant"]
        assert claim(outside[0], other_child, attach(outside[0]), foreign_runtime)["run"]["runtime_template_id"] == foreign_runtime
        assert read_grant(other_lead, other_root)["attempts_used"] == 1
        assert read_grant(lead, root)["attempts_used"] == 2
        checks.append("independent_leads_start_attempts_with_own_templates_and_limits")

        assert revoke(workers[1][0], first)["code"] == "FORBIDDEN"
        revocation_key = str(uuid4())
        revoked = revoke(lead, first, revocation_key)
        assert revoked == revoke(lead, first, revocation_key)
        assert revoke(lead, first)["code"] == "REVISION_CONFLICT"
        assert change(workers[2][0], entered["task"], "submit", summary="After revocation")["code"] == "DELEGATION_INACTIVE"
        assert get(lead, third["task_id"])["runs"][0]["status"] == "running"
        assert delegate(workers[0][0], first, workers[3][1], 1)["code"] == "DELEGATION_INACTIVE"
        cancelled = change(lead, entered["task"], "cancel", reason="Close the inactive grant")
        assert cancelled["run"]["status"] == "cancel_requested"
        assert change(workers[2][0], cancelled["task"], "acknowledge_cancel", run_id=entered["run"]["run_id"])["run"]["status"] == "cancelled"
        checks.append("ancestor_revocation_blocks_writes_and_preserves_observed_run_state")

        expired_task = task("Check expiry and construction limits")
        expiring = issue(office.owner_a, expired_task, limits(1, 2, 1))["grant"]
        did_create = delegate(lead, expiring, workers[0][1], 0, subtasks=0)
        assert did_create["grant"]["task_id"]
        assert delegate(lead, expiring, workers[1][1], 0, subtasks=0)["code"] == "DELEGATION_LIMIT"
        office.sql(f"UPDATE delegation_grants SET expires_at=clock_timestamp()-interval '1 second' WHERE id='{expiring['grant_id']}'")
        assert claim(workers[0][0], did_create["grant"], attach(workers[0][0]))["code"] == "DELEGATION_INACTIVE"
        checks.append("parent_expiry_and_aggregate_subtask_limit_are_enforced")

        team = office.tool(lead, "office_update_team", team_id=team["team_id"], expected_revision=team["revision"],
                           idempotency_key=str(uuid4()), change={"operation": "leave"})["team"]
        team = office.tool(office.owner_a, "office_update_team", team_id=team["team_id"], expected_revision=team["revision"],
                           idempotency_key=str(uuid4()), change={"operation": "appoint_lead", "agent_id": lead_id["agent_id"]})["team"]
        assert team["state"] == "active"
        assert delegate(lead, root, workers[3][1], 2)["code"] == "DELEGATION_INACTIVE"
        checks.append("reappointed_lead_requires_new_delegation")

        office.restart()
        from_cli = office.cli(office.owner_a, "delegation", "get", "--workspace", office.workspace, "--id", root["grant_id"])
        assert from_cli == read_grant(lead, root) and from_cli["attempts_used"] == 2 and from_cli["subtasks_used"] == 4
        assert get(lead, third["task_id"])["runs"][0]["status"] == "cancelled"
        assert office.sql("SET ROLE cocodeop; SELECT count(*) FROM delegation_grants").splitlines()[-1] == "0"
        checks.append("delegation_history_and_counters_survive_restart_with_rls")
    return {"checks": checks}


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="cocodeop-delegation-") as root:
        print(json.dumps(run(Path(root))))
