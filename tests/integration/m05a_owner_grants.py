"""Checks for owner-controlled permissions on an actually enrolled checkout."""

import json
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timedelta, timezone
from uuid import uuid4


def verify(office, checkout, lead_a, lead_b, tmp):
    checks = []
    worker_b, worker_id = office.agent(office.owner_b, "Client worker", "worker")
    sibling_b, _ = office.agent(office.owner_b, "Other client lead", "lead")

    def directory(path):
        return {"kind": "directory", "path": path}

    def file(path):
        return {"kind": "file", "path": path}

    def template(token, spec, expected_revision=checkout["revision"]):
        return office.tool(token, "office_create_command_template", checkout_id=checkout["checkout_id"],
                           expected_revision=expected_revision, idempotency_key=str(uuid4()), template=spec)

    spec = {"name": "Verify client contract", "executable": "cargo",
            "arguments": [{"kind": "literal", "value": "test"},
                          {"kind": "choice", "key": "suite", "values": ["client", "smoke"]},
                          {"kind": "integer", "key": "test_threads", "minimum": 1, "maximum": 8}],
            "source_inputs": [file("nested/lib.rs")], "scratch_outputs": [directory("target")], "timeout_seconds": 60}
    assert template(lead_a[0], spec)["code"] == "FORBIDDEN"
    assert template(office.owner_b, spec)["code"] == "FORBIDDEN"
    assert template(office.owner_a, {**spec, "executable": "/bin/sh"})["code"] == "INVALID_INPUT"
    assert template(office.owner_a, {**spec, "source_inputs": [file("local/private.note")]})["code"] == "ACCESS_REQUEST_REQUIRED"
    assert template(office.owner_a, {**spec, "arguments": spec["arguments"] + [
        {"kind": "integer", "key": "suite", "minimum": 1, "maximum": 5},
    ]})["code"] == "INVALID_INPUT"
    assert template(office.owner_a, spec, expected_revision=1)["code"] == "REVISION_CONFLICT"
    receipt = tmp / "command.json"
    receipt.write_text(json.dumps({"workspace_id": office.workspace, "checkout_id": checkout["checkout_id"],
                                   "expected_revision": checkout["revision"], "idempotency_key": str(uuid4()),
                                   "template": spec}), encoding="utf-8")
    command = office.cli(office.owner_a, "scope", "create-command", "--file", str(receipt))
    assert command["template"] == spec and command["checkout_id"] == checkout["checkout_id"]
    template_id = command["template_id"]
    checks.append("checkout_owner_registers_typed_command_with_allowed_source_inputs")

    def permissions(path, **overrides):
        return {"read": {"allow": [directory(path)], "deny": [file(f"{path}/private.rs")]},
                "write": {"allow": [directory(path)]}, "write_operations": ["create", "replace"],
                "create_directories": True, "change_executable": False, **overrides}

    expires = (datetime.now(timezone.utc) + timedelta(days=1)).isoformat()

    def issue(token, agent, rights, key=None, end=expires):
        return office.tool(token, "office_issue_checkout_grant", checkout_id=checkout["checkout_id"],
                           expected_revision=checkout["revision"], idempotency_key=key or str(uuid4()),
                           recipient={"user_id": agent["owner_user_id"], "agent_id": agent["agent_id"]},
                           expires_at=end, permissions=rights)

    policy_a = permissions("src")
    policy_b = permissions("nested", command_template_ids=[template_id])
    assert issue(lead_a[0], lead_b[1], policy_b)["code"] == "FORBIDDEN"
    assert issue(office.owner_b, lead_b[1], policy_b)["code"] == "FORBIDDEN"
    assert issue(office.owner_a, worker_id, policy_a)["code"] == "FORBIDDEN"
    assert issue(office.owner_a, lead_b[1], permissions("local"))["code"] == "ACCESS_REQUEST_REQUIRED"
    assert issue(office.owner_a, lead_b[1], permissions("src", command_template_ids=[template_id]))["code"] == "ACCESS_REQUEST_REQUIRED"
    assert issue(office.owner_a, lead_b[1], permissions("nested", command_template_ids=[str(uuid4())]))["code"] == "FORBIDDEN"
    assert issue(office.owner_a, lead_b[1], policy_b, end="2000-01-01T12:00:00Z")["code"] == "INVALID_INPUT"
    assert issue(office.owner_a, lead_b[1], policy_b, end=(datetime.now(timezone.utc) + timedelta(days=40)).isoformat())["code"] == "INVALID_INPUT"
    checks.extend(["actors_and_ttl_limit_checkout_grant_issue", "source_and_command_input_permissions_cannot_be_widened"])

    a = issue(office.owner_a, lead_a[1], policy_a)["grant"]
    key = str(uuid4())
    receipt = issue(office.owner_a, lead_b[1], policy_b, key=key)
    assert receipt == issue(office.owner_a, lead_b[1], policy_b, key=key)
    b = receipt["grant"]

    def read(token, grant):
        return office.tool(token, "office_get_checkout_grant", grant_id=grant["grant_id"])

    assert a["recipient"]["agent_id"] == lead_a[1]["agent_id"] and a["permissions"]["write"]["allow"] == [directory("src")]
    assert b["recipient"]["agent_id"] == lead_b[1]["agent_id"] and b["permissions"]["write"]["allow"] == [directory("nested")]
    assert b["permissions"]["command_template_ids"] == [template_id]
    assert read(lead_a[0], a) == a and read(lead_b[0], b) == b
    assert read(lead_b[0], a)["code"] == "FORBIDDEN"
    assert read(lead_a[0], b)["code"] == "FORBIDDEN"
    assert read(sibling_b, b)["code"] == "FORBIDDEN"
    assert read(worker_b, b)["code"] == "FORBIDDEN"
    assert office.cli(office.owner_b, "scope", "get-grant", "--workspace", office.workspace, "--id", b["grant_id"]) == b
    events = office.tool(lead_b[0], "office_read_inbox")["events"]
    assert any(event["event_type"] == "checkout.grant_issued" and event["payload"]["grant_id"] == b["grant_id"] for event in events)
    assert office.sql("SET ROLE cocodeop; SELECT (SELECT count(*) FROM checkout_grants), "
                      "(SELECT count(*) FROM command_templates)").splitlines()[-1] == "0|0"
    checks.extend(["two_leads_receive_distinct_source_and_command_envelopes", "exact_grant_acl_and_replay_from_mcp_and_cli"])

    def revoke(token, key=None):
        return office.tool(token, "office_revoke_checkout_grant", grant_id=b["grant_id"],
                           expected_revision=b["revision"], idempotency_key=key or str(uuid4()))

    assert revoke(lead_b[0])["code"] == "FORBIDDEN"
    assert revoke(office.owner_b)["code"] == "FORBIDDEN"
    keys = [str(uuid4()), str(uuid4())]
    with ThreadPoolExecutor(max_workers=2) as pool:
        revocations = list(pool.map(lambda key: revoke(office.owner_a, key), keys))
    winners = [index for index, result in enumerate(revocations) if result.get("grant", {}).get("revoked_at")]
    assert len(winners) == 1, revocations
    winner = winners[0]
    assert revocations[1 - winner]["code"] == "REVISION_CONFLICT"
    assert revoke(office.owner_a, keys[winner]) == revocations[winner]
    assert read(lead_b[0], b)["revoked_at"] == revocations[winner]["grant"]["revoked_at"]
    office.restart()
    assert read(lead_a[0], a)["revoked_at"] is None
    assert read(lead_b[0], b) == revocations[winner]["grant"]
    checks.append("owner_revocation_is_atomic_and_persists_across_restart")
    return checks
