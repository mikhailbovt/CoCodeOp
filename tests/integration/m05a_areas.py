"""Verify owned and shared area barriers for an enrolled checkout."""

import json
from datetime import datetime, timedelta, timezone
from uuid import uuid4


def verify(office, host, checkout, lead_a, lead_b, tmp):
    checks = []
    worker_a, identity_a = office.agent(office.owner_a, "Backend file worker", "worker")
    worker_b, identity_b = office.agent(office.owner_b, "Client file worker", "worker")

    def recipient(identity):
        return {"user_id": identity["owner_user_id"], "agent_id": identity["agent_id"]}

    def directory(path):
        return {"kind": "directory", "path": path}

    def properties(path, deny=None):
        return {"read": {"allow": [directory(path)]},
                "write": {"allow": [directory(path)], "deny": deny or []}, "write_operations": ["create", "replace"]}

    def definition(path, policy, area_id=None, expected_area_revision=None):
        return {"checkout_id": checkout["checkout_id"], "expected_checkout_revision": checkout["revision"],
                "idempotency_key": str(uuid4()), "name": f"Policy for {path}", "paths": directory(path),
                "policy": policy, "area_id": area_id, "expected_area_revision": expected_area_revision}

    def define(token, path, policy, **revision):
        return office.tool(token, "office_define_area", **definition(path, policy, **revision))

    def envelope(lead, permissions):
        return office.tool(office.owner_a, "office_issue_checkout_grant", checkout_id=checkout["checkout_id"],
                           expected_revision=checkout["revision"], idempotency_key=str(uuid4()), recipient=recipient(lead),
                           expires_at=(datetime.now(timezone.utc) + timedelta(hours=8)).isoformat(),
                           permissions=permissions)["grant"]

    def task(token, title):
        return office.tool(token, "office_create_task", objective=title, acceptance_criteria=["Save evidence"],
                           execution={"kind": "coding"}, idempotency_key=str(uuid4()))["task"]

    def scope(token, task, worker, grant, permissions, expected=None):
        return office.tool(token, "office_set_task_scope", task_id=task["task_id"], checkout_id=checkout["checkout_id"],
                           expected_task_revision=task["revision"], expected_scope_revision=expected,
                           idempotency_key=str(uuid4()), grant_id=grant["grant_id"], worker=recipient(worker),
                           expires_at=(datetime.now(timezone.utc) + timedelta(hours=4)).isoformat(), permissions=permissions)

    own = {"kind": "owned", "lead": recipient(lead_a[1])}
    shared = {"kind": "shared", "approvers": [recipient(lead_a[1]), recipient(lead_b[1])]}
    assert define(lead_a[0], "src", own)["code"] == "FORBIDDEN"
    assert define(lead_b[0], "src", own)["code"] == "FORBIDDEN"
    assert define(office.owner_b, "src", own)["code"] == "FORBIDDEN"
    assert define(office.owner_a, "src", {"kind": "owned", "lead": recipient(identity_a)})["code"] == "FORBIDDEN"
    assert define(office.owner_a, "local", shared)["code"] == "ACCESS_REQUEST_REQUIRED"
    payload = tmp / "owned-area.json"
    payload.write_text(json.dumps({"workspace_id": office.workspace, **definition("src", own)}), encoding="utf-8")
    owned = office.cli(office.owner_a, "scope", "define-area", "--file", str(payload))["area"]
    assert owned["policy"] == own
    assert define(office.owner_a, "src/backend", own)["code"] == "INVALID_INPUT"
    assert office.tool(lead_b[0], "office_get_area", area_id=owned["area_id"])["code"] == "FORBIDDEN"
    checks.append("area_configuration_requires_owner_and_nonoverlapping_manifest_paths")

    grant_a = envelope(lead_a[1], properties("src"))
    grant_b_at_a = envelope(lead_b[1], properties("src"))
    grant_b = envelope(lead_b[1], properties("nested"))
    draft_a = task(lead_a[0], "Update backend source")
    draft_b = task(lead_b[0], "Update shared contracts")
    assert scope(lead_b[0], draft_b, identity_b, grant_b_at_a, properties("src"))["code"] == "ACCESS_REQUEST_REQUIRED"
    pending_a = scope(lead_a[0], draft_a, identity_a, grant_a, properties("src"))
    assert pending_a["scope"]["state"] == "pending_activation"
    pending_b = scope(lead_b[0], draft_b, identity_b, grant_b, properties("nested"))
    assert pending_b["scope"]["state"] == "pending_activation"
    checks.append("recipient_envelope_does_not_override_another_leads_area")

    rule = define(office.owner_a, "nested", shared)["area"]
    assert define(office.owner_a, "nested", shared)["code"] == "INVALID_INPUT"
    takeover = {"kind": "owned", "lead": recipient(lead_b[1])}
    assert define(lead_b[0], "nested", takeover, area_id=rule["area_id"], expected_area_revision=1)["code"] == "FORBIDDEN"
    assert define(office.owner_b, "nested", takeover, area_id=rule["area_id"], expected_area_revision=1)["code"] == "FORBIDDEN"
    assert office.tool(office.owner_a, "office_get_area", area_id=rule["area_id"]) == rule
    checks.append("equal_leads_cannot_rewrite_shared_approval_policy")

    state_path = "area-state"
    host.invoke(["mkdir", "-m", "700", "--", f"{host.root}/{state_path}"], check=True)
    adoption = host.sync_scopes(office.endpoint, checkout, state=state_path)
    assert adoption.returncode == 0, adoption.stderr
    changes = json.loads(adoption.stdout)
    assert [(change["definition"]["task_id"], change["state"]) for change in changes] == [
        (draft_a["task_id"], "active"), (draft_b["task_id"], "rejected"),
    ]
    draft_b["revision"] = pending_b["task_revision"]
    assert scope(lead_b[0], draft_b, identity_b, grant_b, properties("nested"), expected=1)["code"] == "ACCESS_REQUEST_REQUIRED"
    readable = properties("nested", deny=[directory("nested")])
    assert scope(lead_b[0], draft_b, identity_b, grant_b, readable, expected=1)["scope"]["state"] == "pending_activation"
    read_activation = host.sync_scopes(office.endpoint, checkout, state=state_path)
    assert read_activation.returncode == 0, read_activation.stderr
    assert json.loads(read_activation.stdout)[0]["state"] == "active"
    checks.extend(["host_revalidates_pending_scope_against_new_area_policy", "read_only_scope_can_use_shared_paths"])

    amended = define(office.owner_a, "nested", shared, area_id=rule["area_id"], expected_area_revision=1)["area"]
    assert amended["revision"] == 2
    assert define(office.owner_a, "nested", shared, area_id=rule["area_id"], expected_area_revision=1)["code"] == "REVISION_CONFLICT"
    office.restart()
    assert office.cli(office.owner_a, "scope", "area", "--workspace", office.workspace, "--id", rule["area_id"]) == amended
    assert office.sql("SET ROLE cocodeop; SELECT count(*) FROM checkout_areas").splitlines()[-1] == "0"
    checks.append("area_revision_survives_restart_with_isolated_reads")
    return checks, amended
