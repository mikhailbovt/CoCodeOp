"""Verify task handoffs and access boundaries on the real PostgreSQL/MCP server."""

import json
import tempfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from uuid import uuid4

from workspace_fixture import workspace_fixture


def run(tmp):
    checks = []
    with workspace_fixture(tmp, "handoffs") as office:
        lead_a, alice = office.agent(office.owner_a, "Backend lead", "lead")
        lead_b, bob = office.agent(office.owner_b, "Client lead", "lead")
        other_lead_b, _ = office.agent(office.owner_b, "Other client lead", "lead")
        _, worker_b = office.agent(office.owner_b, "Client worker", "worker")
        target = {"user_id": bob["owner_user_id"], "agent_id": bob["agent_id"]}

        def new_task(**extra):
            return office.tool(lead_a, "office_create_task", idempotency_key=str(uuid4()),
                               objective="Plan the client boundary", acceptance_criteria=["Publish the interface"],
                               **extra)["task"]

        def read(task):
            return office.tool(lead_b, "office_get_task", task_id=task["task_id"])

        def offer(task, recipient=target, key=None):
            return office.tool(lead_a, "office_request_handoff", idempotency_key=key or str(uuid4()),
                               task_id=task["task_id"], expected_revision=task["revision"],
                               recipient=recipient, reason="The client owner coordinates the consumer contract")

        def decide(token, receipt, decision="accept", key=None):
            return office.tool(token, "office_respond_handoff", idempotency_key=key or str(uuid4()),
                               handoff_id=receipt["handoff"]["handoff_id"], decision=decision)

        def change(token, task, op, **change_fields):
            return office.tool(token, "office_change_task", idempotency_key=str(uuid4()),
                               task_id=task["task_id"], expected_revision=task["revision"],
                               change={"operation": op, **change_fields})

        task = new_task()
        secret = new_task(visibility={"kind": "owner_private"})
        assert offer(secret)["code"] == "FORBIDDEN"
        assert offer(task, {"user_id": bob["owner_user_id"], "agent_id": worker_b["agent_id"]})["code"] == "FORBIDDEN"
        assert offer(task, {"user_id": bob["owner_user_id"], "agent_id": alice["agent_id"]})["code"] == "FORBIDDEN"
        assert change(lead_b, task, "prepare")["code"] == "FORBIDDEN"
        checks.append("handoff_cannot_grant_data_access_or_promote_worker")

        key = str(uuid4())
        request = offer(task, key=key)
        assert request == offer(task, key=key)
        assert office.tool(other_lead_b, "office_list_handoffs")["handoffs"] == []
        assert office.cli(office.owner_b, "tasks", "handoffs", "--workspace", office.workspace)["handoffs"] == [
            request["handoff"],
        ]
        assert decide(lead_a, request)["code"] == "FORBIDDEN"
        assert decide(other_lead_b, request)["code"] == "FORBIDDEN"
        notifications = office.tool(lead_b, "office_read_inbox")["events"]
        assert any(event["event_type"] == "handoff.requested" and event["payload"]["handoff_id"] ==
                   request["handoff"]["handoff_id"] for event in notifications)
        assert decide(lead_b, request, "decline")["handoff"]["state"] == "declined"
        assert decide(lead_b, request)["code"] == "INVALID_TRANSITION"
        assert read(task)["task"] == task
        checks.extend(["directed_offer_delivery_and_replay", "only_exact_recipient_or_human_owner", "decline_leaves_task_unchanged"])

        stale = offer(task)
        task = change(lead_a, task, "prepare")["task"]
        reject = decide(lead_b, stale)
        assert reject["code"] == "REVISION_CONFLICT" and reject["current_revision"] == task["revision"]
        task = change(lead_a, task, "assign", agent_id=alice["agent_id"])["task"]
        assert task["status"] == "assigned"
        request = offer(task)
        votes = [(office.owner_b, str(uuid4())), (lead_b, str(uuid4()))]
        with ThreadPoolExecutor(max_workers=2) as pool:
            decisions = list(pool.map(lambda vote: decide(vote[0], request, key=vote[1]), votes))
        winners = [index for index, result in enumerate(decisions) if result.get("handoff", {}).get("state") == "accepted"]
        assert len(winners) == 1, decisions
        winner = winners[0]
        assert decisions[1 - winner]["code"] == "INVALID_TRANSITION"
        assert decisions[winner] == decide(votes[winner][0], request, key=votes[winner][1])
        transferred = read(task)["task"]
        assert transferred["revision"] == task["revision"] + 1 == decisions[winner]["transferred_task_revision"]
        assert transferred["status"] == "ready" and transferred["assignee"] is None
        assert transferred["author"] == task["author"] and transferred["owner_user_id"] == task["owner_user_id"]
        assert transferred["accountable"] == {"kind": "agent", "agent_id": bob["agent_id"]}
        assert change(lead_a, transferred, "cancel", reason="Former accountable lead")["code"] == "FORBIDDEN"
        assert change(office.owner_a, transferred, "cancel", reason="Original author owner")["code"] == "FORBIDDEN"
        assert change(other_lead_b, transferred, "assign", agent_id=worker_b["agent_id"])["code"] == "FORBIDDEN"
        assert change(lead_b, transferred, "assign", agent_id=worker_b["agent_id"])["task"]["status"] == "assigned"
        checks.extend(["handoff_bound_to_task_revision", "single_concurrent_transfer", "accountability_and_assignment_follow_consent"])

        shared = new_task(visibility={"kind": "explicit_acl"}, grants=[dict(target, right="read")])
        restricted_offer = offer(shared)
        assert decide(office.owner_b, restricted_offer)["code"] == "FORBIDDEN"
        assert decide(lead_b, restricted_offer)["handoff"]["state"] == "accepted"
        running = change(lead_a, new_task(), "prepare")["task"]
        running = change(office.owner_a, running, "claim")["task"]
        assert offer(running)["code"] == "INVALID_TRANSITION"
        expired = offer(new_task())
        offer_id = expired["handoff"]["handoff_id"]
        office.sql(f"UPDATE task_handoffs SET created_at=clock_timestamp()-interval '2 days', "
                   f"expires_at=clock_timestamp()-interval '1 day' WHERE id='{offer_id}'")
        assert decide(lead_b, expired)["code"] == "INVALID_TRANSITION"
        withdrawn_sender = offer(new_task())
        office.sql(f"UPDATE agents SET disabled_at=clock_timestamp() WHERE id='{alice['agent_id']}'")
        assert decide(lead_b, withdrawn_sender)["code"] == "FORBIDDEN"
        checks.extend(["consenting_owner_requires_existing_data_access", "running_task_keeps_its_custody",
                       "expired_and_inactive_sender_cannot_transfer"])

        office.restart()
        saved = read(task)["task"]
        assert saved["status"] == "assigned" and saved["accountable"] == transferred["accountable"]
        events = office.tool(lead_b, "office_read_inbox")["events"]
        assert any(event["event_type"] == "task.handed_off" and event["payload"]["task_id"] == task["task_id"]
                   for event in events)
        assert office.sql("SET ROLE cocodeop; SELECT count(*) FROM task_handoffs").splitlines()[-1] == "0"
        checks.append("handoff_history_survives_restart_with_scoped_reads")
    return {"checks": checks}


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="cocodeop-handoffs-") as directory:
        print(json.dumps(run(Path(directory))))
