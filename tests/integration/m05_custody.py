"""Verify departure, scope revocation, and deliberate custody recovery on PostgreSQL."""

import json
import tempfile
from pathlib import Path
from uuid import uuid4

from m02_bootstrap import call
from workspace_fixture import workspace_fixture


def run(tmp):
    checks = []
    with workspace_fixture(tmp, "custody") as office:
        lead, alice = office.agent(office.owner_a, "Backend lead", "lead")
        worker, worker_id = office.agent(office.owner_a, "Backend worker", "worker")
        reviewer, reviewer_id = office.agent(office.owner_a, "Backend reviewer", "reviewer")
        successor, new_lead = office.agent(office.owner_a, "Successor", "lead")
        stranger, bob = office.agent(office.owner_b, "Client lead", "lead")
        team = office.tool(lead, "office_create_team", name="Backend", idempotency_key=str(uuid4()))["team"]
        team_id = team["team_id"]
        private_scope = {"kind": "team", "team_id": team_id}
        for member, role in [(worker_id, "worker"), (reviewer_id, "reviewer")]:
            team = office.tool(lead, "office_set_team_member", team_id=team_id, agent_id=member["agent_id"],
                               role=role, expected_revision=team["revision"], idempotency_key=str(uuid4()))["team"]

        def update(token, change, key=None, revision=None):
            return office.tool(token, "office_update_team", team_id=team_id,
                               expected_revision=revision or team["revision"], idempotency_key=key or str(uuid4()),
                               change=change)

        def new_task(token, **extra):
            return office.tool(token, "office_create_task", idempotency_key=str(uuid4()), objective="Verify contract",
                               acceptance_criteria=["Record the result"], **extra)

        def change(token, task, op, **fields):
            return office.tool(token, "office_change_task", idempotency_key=str(uuid4()), task_id=task["task_id"],
                               expected_revision=task["revision"], change={"operation": op, **fields})

        def get(token, task):
            return office.tool(token, "office_get_task", task_id=task["task_id"])

        def attach(token, label):
            status, reply = call(office.endpoint, token, "tools/call", {
                "name": "office_attach_instance", "arguments": {"idempotency_key": str(uuid4()), "label": label},
            })
            assert status == 200, reply
            return reply["result"]["structuredContent"]["instance_id"]

        team_note = office.tool(lead, "office_upsert_note", idempotency_key=str(uuid4()), title="Backend draft",
                                body="Current team only", change={"operation": "create", "visibility": private_scope})
        public = new_task(lead, team_id=team_id)["task"]
        public = change(lead, public, "prepare")["task"]
        visible = new_task(lead, visibility=private_scope)["task"]
        private = new_task(lead, team_id=team_id, visibility={"kind": "owner_private"})["task"]
        assert new_task(stranger, team_id=team_id)["code"] == "FORBIDDEN"
        assert new_task(lead, team_id=str(uuid4()), visibility=private_scope)["code"] == "INVALID_INPUT"
        assert public["team_id"] == visible["team_id"] == private["team_id"] == team_id
        assert get(stranger, public)["task"] == public
        assert get(stranger, visible)["code"] == "FORBIDDEN"
        assert change(stranger, public, "claim")["code"] == "FORBIDDEN"
        assert change(reviewer, public, "claim")["code"] == "FORBIDDEN"
        assert change(lead, public, "assign", agent_id=new_lead["agent_id"])["code"] == "FORBIDDEN"
        assert update(stranger, {"operation": "appoint_lead", "agent_id": bob["agent_id"]})["code"] == "FORBIDDEN"
        assert update(lead, {"operation": "appoint_lead", "agent_id": new_lead["agent_id"]})["code"] == "FORBIDDEN"
        checks.extend(["explicit_team_binding_independent_of_visibility", "team_role_limits_task_execution", "appointment_requires_human_owner"])

        pending = new_task(lead, team_id=team_id)["task"]
        pending = change(lead, pending, "prepare")["task"]
        pending = change(lead, pending, "assign", agent_id=alice["agent_id"])["task"]
        still_running = change(worker, public, "claim", instance_id=attach(worker, "Worker"))
        public = still_running["task"]
        former_run = new_task(lead, team_id=team_id)["task"]
        former_run = change(lead, former_run, "prepare")["task"]
        former_run = change(lead, former_run, "claim", instance_id=attach(lead, "Former lead"))

        departure = {"operation": "leave"}
        departed_revision = team["revision"]
        leave_key = str(uuid4())
        team = update(lead, departure, key=leave_key)["team"]
        assert team["state"] == "awaiting_owner" and team["lead_agent_id"] is None
        assert update(lead, departure, key=leave_key, revision=departed_revision)["code"] == "FORBIDDEN"
        assert office.tool(lead, "office_get_note", note_id=team_note["note_id"])["code"] == "FORBIDDEN"
        assert get(lead, visible)["code"] == "FORBIDDEN"
        assert office.tool(lead, "office_get_team", team_id=team_id)["code"] == "FORBIDDEN"
        blocked = get(worker, public)
        assert blocked["task"]["status"] == "blocked" and blocked["task"]["block_reason"] == "awaiting_owner"
        assert blocked["task"]["accountable"] == public["accountable"]
        assert blocked["runs"] == [still_running["run"]]
        assert get(office.owner_a, private)["task"]["status"] == "blocked"
        assert change(worker, blocked["task"], "progress", message="Must wait")["code"] == "INVALID_TRANSITION"
        assert {item["task_id"] for item in office.tool(worker, "office_list_tasks", status="blocked")["tasks"]} == {
            public["task_id"], visible["task_id"], pending["task_id"], former_run["task"]["task_id"],
        }
        waiting_task = new_task(office.owner_a, team_id=team_id)["task"]
        assert waiting_task["block_reason"] == "awaiting_owner"
        checks.extend(["departed_lead_loses_team_data_access_even_as_author", "task_pause_and_lead_departure_commit_together",
                       "active_attempt_is_preserved_without_false_completion"])

        office.restart()
        assert office.tool(worker, "office_get_team", team_id=team_id)["team"] == team
        assert get(worker, public) == blocked
        events = office.tool(worker, "office_read_inbox")["events"]
        assert any(event["event_type"] == "team.awaiting_owner" and event["payload"]["team_id"] == team_id for event in events)
        assert update(office.owner_a, {"operation": "appoint_lead", "agent_id": worker_id["agent_id"]})["code"] == "FORBIDDEN"
        assert update(office.owner_a, {"operation": "appoint_lead", "agent_id": bob["agent_id"]})["code"] == "FORBIDDEN"
        checks.append("paused_custody_survives_restart")

        command = tmp / "appoint.json"
        command.write_text(json.dumps({"workspace_id": office.workspace, "team_id": team_id,
                                       "expected_revision": team["revision"], "idempotency_key": str(uuid4()),
                                       "change": {"operation": "appoint_lead", "agent_id": new_lead["agent_id"]}}), encoding="utf-8")
        team = office.cli(office.owner_a, "teams", "update", "--file", str(command))["team"]
        assert team["state"] == "active" and team["lead_agent_id"] == new_lead["agent_id"]
        current = get(worker, public)["task"]
        assert current["status"] == "in_progress" and current["block_reason"] is None
        assert current["accountable"] == {"kind": "agent", "agent_id": new_lead["agent_id"]}
        assert change(worker, current, "submit", summary="Contract included")["task"]["status"] == "in_review"
        reassign = get(successor, pending)["task"]
        assert reassign["status"] == "ready" and reassign["assignee"] is None
        assert change(successor, reassign, "assign", agent_id=worker_id["agent_id"])["task"]["status"] == "assigned"
        assert get(office.owner_a, waiting_task)["task"]["status"] == "draft"
        assert get(successor, private)["code"] == "FORBIDDEN"
        assert get(office.owner_a, private)["task"]["status"] == "draft"
        assert office.tool(successor, "office_get_note", note_id=team_note["note_id"]) == team_note
        checks.extend(["owner_selects_replacement_and_reopens_work", "private_task_visibility_survives_custody_recovery"])

        interrupted = get(successor, former_run["task"])
        assert interrupted["task"]["block_reason"] == "awaiting_owner"
        assert interrupted["runs"][0]["status"] == "running"
        cancelled = change(successor, interrupted["task"], "cancel", reason="Stop former lead's attempt")
        assert cancelled["run"]["status"] == "cancel_requested"
        acknowledged = change(lead, cancelled["task"], "acknowledge_cancel", run_id=former_run["run"]["run_id"])
        assert acknowledged["run"]["status"] == "cancelled"
        checks.append("departed_executor_must_acknowledge_cancellation")

        team = update(reviewer, {"operation": "leave"})["team"]
        assert team["state"] == "active" and team["lead_agent_id"] == new_lead["agent_id"]
        assert office.tool(reviewer, "office_get_note", note_id=team_note["note_id"])["code"] == "FORBIDDEN"
        team = update(successor, {"operation": "remove_member",
                                  "member": {"user_id": office.alice["user_id"], "agent_id": worker_id["agent_id"]}})["team"]
        assert team["state"] == "active"
        assert change(worker, get(worker, pending)["task"], "claim", instance_id=attach(worker, "Removed"))["code"] == "FORBIDDEN"
        checks.append("ordinary_members_can_leave_or_be_removed_without_pausing_team")
    return {"checks": checks}


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="cocodeop-custody-") as root:
        print(json.dumps(run(Path(root))))
