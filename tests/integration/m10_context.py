"""Check actor context and durable addressed-event delivery via MCP and CLI."""

import argparse
import json
import os
import subprocess
import sys
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from uuid import uuid4

from m05a_claims import recipient
from m01 import compose_runner, wait_for_status
from workspace_fixture import ROOT, WorkspaceFixture, workspace_fixture


def verify(office):
    lead_a, a = office.agent(office.owner_a, "Planning lead", "lead")
    lead_b, b = office.agent(office.owner_b, "Delivery lead", "lead")
    reviewer, _ = office.agent(office.owner_b, "Context reviewer", "reviewer")

    def create(lead, objective):
        return office.tool(lead, "office_create_task", objective=objective, acceptance_criteria=["Record the result"],
                           visibility={"kind": "owner_private"}, idempotency_key=str(uuid4()))["task"]

    sender_task = create(lead_a, "Private producer scope")
    receiver_tasks = [create(lead_b, text) for text in ("Define a consumer scope", "Review a consumer scope")]

    def context(credential, **inputs):
        return office.tool(credential, "office_get_context", **inputs)

    sender = context(lead_a)
    assert sender["actor"] == recipient(a) and sender["role"] == "lead"
    assert [item["task_id"] for item in sender["tasks"]["items"]] == [sender_task["task_id"]]
    assert all(task["objective"] not in json.dumps(sender) for task in receiver_tasks)
    first = context(lead_b, inbox_cursor=sender["inbox"]["next_cursor"], limit=1)
    assert first["actor"] == recipient(b) and first["inbox"] is None
    assert first["diagnostics"][0]["code"] == "CURSOR_EXPIRED" and first["diagnostics"][0]["next_action"]
    assert len(first["tasks"]["items"]) == 1 and first["tasks"]["next_after"]
    second = context(lead_b, tasks_after=first["tasks"]["next_after"], limit=1)
    assert second["tasks"]["next_after"] is None
    assert {page["tasks"]["items"][0]["task_id"] for page in (first, second)} == {task["task_id"] for task in receiver_tasks}
    assert context(lead_b, limit=26)["code"] == "INVALID_INPUT"
    assert context(reviewer)["role"] == "reviewer"
    captured = office.cli(lead_b, "context", "--workspace", office.workspace, "--limit", "1")
    assert captured == context(lead_b, limit=1)

    channel = office.tool(lead_a, "office_create_channel", name="Coordination", audience={"kind": "workspace"},
                          idempotency_key=str(uuid4()))
    message = office.tool(lead_a, "office_send_message", channel_id=channel["channel_id"],
                          body="What revision is the consumer ready to use?", mentions=[recipient(b)],
                          idempotency_key=str(uuid4()))
    sender_inbox = context(lead_a, inbox_cursor=sender["inbox"]["next_cursor"])["inbox"]
    addressed = next(event for event in sender_inbox["events"] if event["event_type"] == "message.mentioned")
    event_id = addressed["event_id"]
    assert addressed["payload"]["message_id"] == message["message_id"]

    def delivery(credential):
        return office.tool(credential, "office_inbox_delivery", event_id=event_id)

    waiting = delivery(lead_a)
    assert waiting == {"event_id": event_id, "state": "waiting_for_agent", "delivered_at": None, "acknowledged_at": None}
    assert delivery(reviewer)["code"] == "FORBIDDEN"
    assert office.tool(lead_a, "office_ack_inbox", event_id=event_id)["event_id"] == event_id
    assert delivery(lead_a) == waiting
    receiver_inbox = context(lead_b)["inbox"]
    assert any(event["event_id"] == event_id for event in receiver_inbox["events"])
    delivered = delivery(lead_a)
    assert delivered["state"] == "delivered" and delivered["delivered_at"] and delivered["acknowledged_at"] is None
    assert delivery(lead_b) == delivered
    office.restart()
    assert delivery(lead_a) == delivered
    receipt = office.tool(lead_b, "office_ack_inbox", event_id=event_id)
    assert receipt == office.tool(lead_b, "office_ack_inbox", event_id=event_id)
    cli = office.cli(lead_a, "inbox", "delivery", "--workspace", office.workspace, "--event", event_id)
    assert cli == delivery(lead_a) and cli == {
        **delivered, "state": "acknowledged", "acknowledged_at": receipt["acknowledged_at"],
    }
    return {
        "checks": ["workspace_context_paginates_the_callers_own_tasks", "expired_or_foreign_cursor_only_blocks_inbox",
                   "mcp_and_cli_context_views_match_for_agent_credentials", "addressed_delivery_is_visible_only_to_exact_parties",
                   "sender_reads_and_acknowledgements_do_not_advance_the_recipient", "recipient_delivery_survives_restart",
                   "explicit_ack_is_idempotent_and_visible_to_sender"],
        "addressed_delivery": [waiting, delivered, cli],
    }


def verify_questions(office):
    lead_a, a = office.agent(office.owner_a, "Integration lead", "lead")
    lead_b, b = office.agent(office.owner_b, "Service lead", "lead")
    sibling, peer = office.agent(office.owner_b, "Another teammate", "lead")
    channel = office.tool(lead_a, "office_create_channel", name="Contract clarification",
                          audience={"kind": "direct", "recipient": recipient(b)}, idempotency_key=str(uuid4()))
    channel_id = channel["channel_id"]
    payload = {"channel_id": channel_id, "recipient_user_id": b["owner_user_id"], "recipient_agent_id": b["agent_id"],
               "idempotency_key": str(uuid4()), "body": "Which contract revision can the integration use?"}
    inaccessible = {**payload, "recipient_agent_id": peer["agent_id"], "idempotency_key": str(uuid4())}
    assert office.tool(lead_a, "office_ask_question", **inaccessible)["code"] == "FORBIDDEN"
    asked = office.tool(lead_a, "office_ask_question", **payload)
    assert asked["asker"] == recipient(a) and asked["recipient"] == recipient(b)
    assert office.tool(lead_a, "office_ask_question", **payload) == asked
    event = asked["question_event_id"]
    assert office.tool(lead_a, "office_inbox_delivery", event_id=event)["state"] == "waiting_for_agent"
    identity = {"question_id": asked["question_id"]}
    assert office.tool(sibling, "office_get_question", **identity)["code"] == "FORBIDDEN"
    assert office.tool(lead_b, "office_get_question", **identity) == asked
    assert office.cli(lead_b, "messages", "question", "--workspace", office.workspace,
                      "--question", identity["question_id"]) == asked
    answer = {**identity, "body": "Use revision 2 and check the published manifest."}
    for wrong_credential in (lead_a, sibling, office.owner_b):
        denied = office.tool(wrong_credential, "office_answer_question", **answer, idempotency_key=str(uuid4()))
        assert denied["code"] == "FORBIDDEN", denied
    office.tool(lead_b, "office_send_message", channel_id=channel_id, body="I will check.",
                reply_to=asked["question_id"], idempotency_key=str(uuid4()))
    assert office.tool(lead_a, "office_get_question", **identity)["answer_id"] is None

    def vote(key):
        return office.tool(lead_b, "office_answer_question", **answer, idempotency_key=key)

    keys = [str(uuid4()), str(uuid4())]
    with ThreadPoolExecutor(max_workers=2) as pool:
        results = list(pool.map(vote, keys))
    assert sum(bool(res.get("answer_id")) for res in results) == 1, results
    assert sum(res.get("code") == "INVALID_TRANSITION" for res in results) == 1, results
    index, saved = next((i, res) for i, res in enumerate(results) if res.get("answer_id"))
    assert vote(keys[index]) == saved
    assert office.tool(lead_b, "office_inbox_delivery", event_id=saved["answer_event_id"])["state"] == "waiting_for_agent"
    reply = office.tool(lead_a, "office_read_thread", channel_id=channel_id, thread_id=asked["question_id"])
    assert len(reply["messages"]) == 3, reply
    assert reply["messages"][-1]["message_id"] == saved["answer_id"]
    assert reply["messages"][-1]["reply_to"] == asked["question_id"]
    assert reply["messages"][-1]["author"]["agent_id"] == b["agent_id"]

    expires = office.tool(lead_a, "office_ask_question", **{**payload, "idempotency_key": str(uuid4()),
                                                          "expires_in_seconds": 5})
    time.sleep(5.15)
    late = office.tool(lead_b, "office_answer_question", body="This answer is late", question_id=expires["question_id"],
                       idempotency_key=str(uuid4()))
    assert late["code"] == "INVALID_TRANSITION"
    assert len(office.tool(lead_a, "office_read_thread", channel_id=channel_id,
                           thread_id=expires["question_id"])["messages"]) == 1
    assert office.sql("SET ROLE cocodeop; SELECT count(*) FROM thread_questions").splitlines()[-1] == "0"
    return {"question": saved, "checks": ["only_exact_recipient_can_answer", "question_requires_readable_channel",
        "ordinary_replies_leave_question_open", "one_concurrent_answer_links_reply_and_addressed_event",
        "retry_preserves_question_and_answer_ids", "expired_answer_does_not_write_a_message"]}


def verify_upgrade(tmp, previous_image):
    storage = tmp / "secrets"
    subprocess.run([sys.executable, ROOT / "deploy/init-secrets.py", storage], check=True)
    project = f"cocodeop-inbox-upgrade-{uuid4().hex[:12]}"
    env = dict(os.environ, COCODEOP_SECRETS_DIR=str(storage), COCODEOP_HTTP_PORT="0")
    override = tmp / "previous-image.json"
    override.write_text(json.dumps({"services": {"server": {"image": previous_image}}}), encoding="utf-8")
    previous = compose_runner(project, env, override)
    current = compose_runner(project, env)
    operator = tmp / "operator"
    operator.mkdir(mode=0o700)
    try:
        previous("up", "--no-build", "--wait", "--wait-timeout", "90", timeout=180)
        office = WorkspaceFixture(previous, operator)
        original = office.sql("SELECT max(version) FROM _sqlx_migrations")
        assert original == "46", original
        lead_a, _ = office.agent(office.owner_a, "Existing sender", "lead")
        lead_b, b = office.agent(office.owner_b, "Existing recipient", "lead")
        channel = office.tool(lead_a, "office_create_channel", name="Existing direct channel",
                              audience={"kind": "direct", "recipient": recipient(b)}, idempotency_key=str(uuid4()))
        office.tool(lead_a, "office_send_message", channel_id=channel["channel_id"], body="Previously acknowledged notice",
                    mentions=[recipient(b)], idempotency_key=str(uuid4()))
        inbox = office.tool(lead_b, "office_read_inbox")
        event = next(event for event in inbox["events"] if event["event_type"] == "message.mentioned")["event_id"]
        receipt = office.tool(lead_b, "office_ack_inbox", event_id=event)

        current("up", "--build", "--force-recreate", "--wait", "--wait-timeout", "90", "server", timeout=720)
        office.compose = current
        office.endpoint = current("port", "server", "8400").stdout.strip()
        wait_for_status(office.endpoint, "/readyz", 200)
        delivered = office.tool(lead_a, "office_inbox_delivery", event_id=event)
        assert delivered == {"event_id": event, "state": "acknowledged",
                             "delivered_at": receipt["acknowledged_at"], "acknowledged_at": receipt["acknowledged_at"]}
        assert office.tool(lead_b, "office_ack_inbox", event_id=event) == receipt
        migrated = int(office.sql("SELECT max(version) FROM _sqlx_migrations"))
        assert migrated >= 48
        assert office.sql("SET ROLE cocodeop; SELECT count(*) FROM inbox_receipts").splitlines()[-1] == "0"
        return {"previous_schema": 46, "upgraded_schema": migrated, "delivery": delivered,
                "check": "existing_acknowledgement_survives_schema_upgrade_with_the_original_timestamp"}
    finally:
        current("down", "--volumes", "--rmi", "local", "--remove-orphans", check=False, timeout=90)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-image", help="Verify an upgrade from an existing schema 46 installation")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="cocodeop-context-") as directory:
        root = Path(directory)
        if args.previous_image:
            print(json.dumps(verify_upgrade(root, args.previous_image)))
        else:
            with workspace_fixture(root, "context") as office:
                evidence = verify(office)
                evidence["questions"] = verify_questions(office)
                print(json.dumps(evidence))
