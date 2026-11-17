"""Exercise typed cross-owner decisions and their real checkout-host activation."""

import json
from datetime import datetime, timedelta, timezone
from uuid import uuid4

from m02_bootstrap import call


def verify(office, host, checkout, first, second, shared_area, scope_revocation=False, decision_links=False):
    checks = []
    revocation_checks = []
    link_checks = []
    lead_a, agent_a = first
    lead_b, agent_b = second
    worker, identity = office.agent(office.owner_b, "Contract implementer", "worker")
    sibling, _ = office.agent(office.owner_b, "Other worker", "worker")
    team = office.tool(lead_b, "office_create_team", name="Contract implementation", idempotency_key=str(uuid4()))["team"]
    team = office.tool(lead_b, "office_set_team_member", team_id=team["team_id"], agent_id=identity["agent_id"], role="worker",
                       expected_revision=team["revision"], idempotency_key=str(uuid4()))["team"]

    def party(record):
        return {"user_id": record["owner_user_id"], "agent_id": record["agent_id"]}

    def directory(path):
        return {"kind": "directory", "path": path}

    def file(path):
        return {"kind": "file", "path": path}

    def hours(count):
        return (datetime.now(timezone.utc) + timedelta(hours=count)).isoformat()

    def issue(permissions):
        answer = office.tool(office.owner_a, "office_issue_checkout_grant", checkout_id=checkout["checkout_id"],
                             expected_revision=checkout["revision"], idempotency_key=str(uuid4()), recipient=party(agent_b),
                             expires_at=hours(8), permissions=permissions)
        assert answer.get("grant"), answer
        return answer["grant"]

    read_only = {"read": {"allow": [directory("nested")]}, "write": {"allow": []}, "write_operations": []}
    writable = {**read_only, "write": {"allow": [file("nested/lib.rs")]}, "write_operations": ["replace"]}
    expanded = {**writable, "read": {"allow": [directory("nested"), file("src/app.rs")]}}
    parent = issue(writable)
    task = office.tool(lead_b, "office_create_task", objective="Adjust the contract", acceptance_criteria=["Approve the update"],
                      team_id=team["team_id"], visibility={"kind": "team", "team_id": team["team_id"]},
                      execution={"kind": "coding"}, idempotency_key=str(uuid4()))["task"]
    task_id = task["task_id"]
    draft = office.tool(lead_b, "office_set_task_scope", task_id=task_id, checkout_id=checkout["checkout_id"], worker=party(identity),
                       grant_id=parent["grant_id"], expected_task_revision=task["revision"], idempotency_key=str(uuid4()),
                       expires_at=hours(4), permissions=read_only)
    assert draft.get("scope"), draft
    state = "access-state"
    host.invoke(["mkdir", "-m", "700", "--", f"{host.root}/{state}"], check=True)

    def sync():
        applied = host.sync_scopes(office.endpoint, checkout, state=state)
        assert applied.returncode == 0, applied.stderr
        return json.loads(applied.stdout)

    assert sync()[0]["state"] == "active"
    prepared = office.tool(lead_b, "office_prepare_task", task_id=task_id, expected_revision=draft["task_revision"],
                           idempotency_key=str(uuid4()))["task"]
    status, attached = call(office.endpoint, worker, "tools/call", {"name": "office_attach_instance", "arguments": {
        "idempotency_key": str(uuid4()), "label": "Contract worker",
    }})
    assert status == 200, attached
    instance = attached["result"]["structuredContent"]["instance_id"]
    claimed = office.tool(worker, "office_claim_task", task_id=task_id, expected_revision=prepared["revision"],
                          idempotency_key=str(uuid4()), instance_id=instance)
    run_id = claimed["run"]["run_id"]

    def view(token=worker):
        return office.tool(token, "office_get_task", task_id=task_id)

    def propose(permissions, token=worker, key=None, until=None, decision_id=None):
        latest = view()["task"]
        return office.tool(token, "office_request_access", task_id=task_id, expected_task_revision=latest["revision"],
                           expected_scope_revision=latest["execution"]["work_scope_revision"], idempotency_key=key or str(uuid4()),
                           reason="Adjust the consumer interface with the other lead's consent", expires_at=until or hours(1),
                           scope_expires_at=hours(3), permissions=permissions, decision_id=decision_id)

    def get(ticket, token=worker):
        return office.tool(token, "office_get_access_request", access_request_id=ticket["access_request_id"])

    def respond(token, ticket, decision="approve", **extra):
        return office.tool(token, "office_respond_access", access_request_id=ticket["access_request_id"],
                           idempotency_key=extra.pop("idempotency_key", str(uuid4())), expected_revision=ticket["revision"],
                           decision=decision, **extra)

    assert view(lead_a)["code"] == "FORBIDDEN"
    rejected_path = {**writable, "read": {"allow": [file("local/private.note")]}}
    assert propose(rejected_path)["code"] == "ACCESS_REQUEST_REQUIRED"
    assert propose(writable, token=sibling)["code"] == "FORBIDDEN"
    opened = propose(writable)
    assert opened.get("request"), opened
    first_request = opened["request"]
    assert first_request["state"] == "requested" and first_request["run_id"] == run_id
    assert set((p["user_id"], p["agent_id"]) for p in first_request["approvers"]) == {
        (agent_a["owner_user_id"], agent_a["agent_id"]), (agent_b["owner_user_id"], agent_b["agent_id"]),
    }
    assert get(first_request, lead_a) == first_request
    assert get(first_request, sibling)["code"] == "FORBIDDEN"
    assert respond(worker, first_request)["code"] == "FORBIDDEN"
    checks.append("addressed_access_request_can_cross_teams_without_sharing_task_data")

    first_vote = respond(lead_b, first_request)["request"]
    assert first_vote["state"] == "requested" and first_vote["issued_scope_revision"] is None
    assert respond(lead_a, first_request)["code"] == "REVISION_CONFLICT"
    late_key = str(uuid4())
    unanimous = respond(lead_a, first_vote, idempotency_key=late_key)
    assert unanimous.get("request"), unanimous
    assert unanimous == respond(lead_a, first_vote, idempotency_key=late_key)
    waiting = unanimous["request"]
    assert waiting["state"] == "approved_pending_apply" and waiting["issued_scope_revision"] == 2
    assert unanimous["work_scope"]["definition"]["access_request_id"] == first_request["access_request_id"]
    assert view()["task"]["execution"]["work_scope_revision"] == 1
    assert sync()[0]["state"] == "active"
    assert get(waiting)["state"] == "approved"
    assert view()["task"]["execution"]["work_scope_revision"] == 2 and view()["runs"][0]["run_id"] == run_id
    assert view(lead_a)["code"] == "FORBIDDEN"
    checks.extend(["unanimous_cross_owner_votes_stage_one_revision", "same_attempt_gains_approved_permission_after_real_host_ack"])

    stale = propose(writable)["request"]
    partial = respond(lead_b, stale)["request"]
    current_area = office.tool(office.owner_a, "office_define_area", checkout_id=checkout["checkout_id"],
                               expected_checkout_revision=checkout["revision"], area_id=shared_area["area_id"],
                               expected_area_revision=shared_area["revision"], idempotency_key=str(uuid4()),
                               name=shared_area["name"], paths=shared_area["paths"], policy=shared_area["policy"])
    assert current_area.get("area"), current_area
    assert respond(lead_a, partial)["code"] == "REVISION_CONFLICT"
    assert get(stale)["issued_scope_revision"] is None and view()["task"]["execution"]["work_scope_revision"] == 2
    checks.append("stale_area_approval_cannot_expand_a_new_policy_revision")

    need_owner = propose(expanded)["request"]
    assert need_owner["needs_owner_grant"] is True
    assert respond(office.owner_a, need_owner)["code"] == "ACCESS_REQUEST_REQUIRED"
    assert respond(lead_b, need_owner, replacement_grant_id=parent["grant_id"])["code"] == "FORBIDDEN"
    replacement = issue(expanded)
    updated = respond(office.owner_a, need_owner, replacement_grant_id=replacement["grant_id"])["request"]
    updated = respond(lead_b, updated)["request"]
    queued = respond(lead_a, updated)
    assert queued.get("request"), queued
    assert queued["request"]["state"] == "approved_pending_apply"
    assert sync()[0]["definition"]["grant_id"] == replacement["grant_id"]
    assert get(need_owner)["state"] == "approved" and view()["task"]["execution"]["work_scope_revision"] == 3
    checks.append("rights_above_original_envelope_require_a_new_owner_grant")

    rejected = propose(expanded)["request"]
    assert respond(lead_a, rejected, "reject")["request"]["state"] == "rejected"
    assert respond(lead_b, get(rejected))["code"] == "INVALID_TRANSITION"
    expired = propose(expanded)["request"]
    office.sql(f"UPDATE access_requests SET expires_at=clock_timestamp()-interval '1 second' WHERE id='{expired['access_request_id']}'")
    assert get(expired)["state"] == "expired"
    assert respond(lead_a, expired)["code"] == "INVALID_TRANSITION"
    cancelled = propose(expanded)["request"]
    partial = respond(lead_b, cancelled)["request"]
    queued = respond(lead_a, partial)["request"]
    assert queued["state"] == "approved_pending_apply"
    assert respond(lead_a, queued, "cancel")["code"] == "FORBIDDEN"
    assert respond(worker, queued, "cancel")["request"]["state"] == "cancelled"
    assert sync()[0]["state"] == "rejected" and get(cancelled)["state"] == "cancelled"
    assert view()["task"]["execution"]["work_scope_revision"] == 3
    checks.append("rejected_expired_and_cancelled_requests_cannot_change_the_current_scope")

    fresh = propose(expanded)["request"]
    partial = respond(office.owner_b, fresh, as_approver=party(agent_b))["request"]
    applied = respond(lead_a, partial)["request"]
    assert applied["issued_scope_revision"] == 5
    assert sync()[0]["state"] == "active"
    office.restart()
    final = office.cli(office.owner_b, "scope", "access-request", "--workspace", office.workspace,
                       "--id", applied["access_request_id"])
    assert final == get(applied) and final["state"] == "approved"
    assert view()["runs"][0]["run_id"] == run_id
    notice = office.tool(lead_a, "office_read_inbox", limit=100)["events"]
    assert any(event["event_type"] == "access.resolved" and event["payload"]["access_request_id"] == applied["access_request_id"]
               and event["payload"]["state"] == "approved" for event in notice)
    assert office.sql("SET ROLE cocodeop; SELECT count(*) FROM access_requests").splitlines()[-1] == "0"
    checks.append("fresh_request_after_rejected_revision_and_restart_preserves_history")
    if scope_revocation:
        def set_scope(current_revision, envelope=replacement):
            return office.tool(lead_b, "office_set_task_scope", task_id=task_id, checkout_id=checkout["checkout_id"],
                               worker=party(identity), grant_id=envelope["grant_id"], expected_task_revision=view()["task"]["revision"],
                               expected_scope_revision=current_revision, idempotency_key=str(uuid4()), expires_at=hours(3), permissions=read_only)

        def revoke(token, version, key=None):
            return office.tool(token, "office_revoke_scope", task_id=task_id, expected_scope_revision=version,
                               idempotency_key=key or str(uuid4()), reason="Close this task's current file access")

        def progress():
            return office.tool(worker, "office_change_task", task_id=task_id, expected_revision=view()["task"]["revision"],
                               idempotency_key=str(uuid4()), change={"operation": "progress", "message": "Continuing"})

        update = set_scope(5)
        assert update["scope"]["definition"]["revision"] == 6
        assert revoke(worker, 6)["code"] == "FORBIDDEN"
        assert revoke(lead_a, 6)["code"] == "FORBIDDEN"
        assert revoke(office.owner_a, 5)["code"] == "REVISION_CONFLICT"
        key = str(uuid4())
        queued = revoke(office.owner_a, 6, key)
        assert queued.get("scope"), queued
        assert queued == revoke(office.owner_a, 6, key)
        assert queued["scope"]["state"] == "revocation_pending" and queued["scope"]["definition"]["operation"] == "revoke"
        assert view()["task"]["execution"]["work_scope_revision"] is None
        assert progress()["code"] == "ACCESS_REQUEST_REQUIRED"
        assert view()["runs"][0]["run_id"] == run_id and view()["runs"][0]["status"] == "running"
        revocation_checks.append("owner_can_block_a_private_task_scope_without_exposing_the_task")

        assert set_scope(7)["code"] == "INVALID_TRANSITION"
        outcomes = sync()
        assert [(result["definition"]["revision"], result["state"]) for result in outcomes] == [(6, "rejected"), (7, "revoked")]
        stored = f"{host.root}/{state}/scope-{queued['scope']['definition']['scope_id']}-7.json"
        deployment = json.loads(host.invoke(["cat", "--", stored], check=True).stdout)
        assert deployment["scope"]["operation"] == "revoke"
        assert deployment["scope"]["permissions"]["write"]["allow"] == []
        revocation_checks.append("confirmed_revocation_cannot_be_overtaken_by_pending_expansion")

        office.restart()
        read_back = office.cli(office.owner_a, "scope", "get", "--workspace", office.workspace, "--task", task_id)
        assert read_back["state"] == "revoked" and read_back["definition"]["revision"] == 7
        assert view(office.owner_a)["code"] == "FORBIDDEN"
        assert progress()["code"] == "ACCESS_REQUEST_REQUIRED"
        assert set_scope(7)["code"] == "ACCESS_REQUEST_REQUIRED"
        renewed = issue(expanded)
        assert set_scope(7, renewed)["scope"]["state"] == "pending_activation"
        assert view()["task"]["execution"]["work_scope_revision"] is None
        assert sync()[0]["state"] == "active"
        assert view()["task"]["execution"]["work_scope_revision"] == 8
        assert progress()["run"]["run_id"] == run_id
        revocation_checks.append("new_deliberate_authorization_recovers_access_at_a_fresh_revision")
    if decision_links:
        from m06_proposals import content, deadline

        def accept_contract(generation):
            opened = office.tool(lead_b, "office_propose_decision", target={
                "kind": "shared_area", "checkout_id": checkout["checkout_id"], "area_id": shared_area["area_id"],
                "expected_area_revision": current_area["area"]["revision"], "expected_generation": generation,
            }, content=content("structured", f"Contract generation {generation + 1}"),
                deadline=deadline(), idempotency_key=str(uuid4()))
            assert opened.get("proposal"), opened
            for token in (lead_a, lead_b):
                accepted = office.tool(token, "office_respond_proposal", proposal_id=opened["proposal"]["proposal_id"],
                                       expected_revision=1, decision="approve", idempotency_key=str(uuid4()),
                                       reason="Agree on the exact contract and its generation")
                assert accepted.get("proposal"), accepted
            return accepted["decision"]

        def request_contract(decision):
            request = propose(expanded, decision_id=decision["decision_id"])
            assert request.get("request"), request
            assert request["request"]["decision_id"] == decision["decision_id"]
            return request["request"]

        def report_progress():
            latest = view()["task"]
            return office.tool(worker, "office_change_task", task_id=task_id, expected_revision=latest["revision"],
                               idempotency_key=str(uuid4()), change={"operation": "progress", "message": "Apply the decided interface"})

        initial = accept_contract(0)
        initial_scope = view()["task"]["execution"]["work_scope_revision"]
        assert office.tool(worker, "office_get_decision", decision_id=initial["decision_id"])["code"] == "FORBIDDEN"
        assert propose(read_only, decision_id=initial["decision_id"])["code"] == "ACCESS_REQUEST_REQUIRED"
        assert propose(expanded, decision_id=str(uuid4()))["code"] == "ACCESS_REQUEST_REQUIRED"
        request = request_contract(initial)
        assert get(request)["decision_id"] == initial["decision_id"]
        assert view()["task"]["execution"]["work_scope_revision"] == initial_scope
        partial = respond(lead_b, request)["request"]
        assert partial["state"] == "requested" and partial["issued_scope_revision"] is None
        link_checks.append("decision_links_require_matching_shared_writes_and_keep_separate_access_votes")

        latest = accept_contract(1)
        assert respond(lead_a, partial)["code"] == "REVISION_CONFLICT"
        assert get(request)["issued_scope_revision"] is None
        assert propose(expanded, decision_id=initial["decision_id"])["code"] == "ACCESS_REQUEST_REQUIRED"
        pending = respond(lead_a, respond(lead_b, request_contract(latest))["request"])["request"]
        assert pending["state"] == "approved_pending_apply"
        current = accept_contract(2)
        assert sync()[0]["state"] == "rejected" and get(pending)["state"] == "rejected"
        assert view()["task"]["execution"]["work_scope_revision"] == initial_scope
        link_checks.append("new_contract_generation_blocks_both_the_final_vote_and_pending_host_activation")

        active = respond(lead_a, respond(lead_b, request_contract(current))["request"])["request"]
        assert office.tool(worker, "office_get_decision", decision_id=current["decision_id"])["code"] == "FORBIDDEN"
        assert sync()[0]["state"] == "active" and get(active)["state"] == "approved"
        assert office.tool(worker, "office_get_decision", decision_id=current["decision_id"]) == current
        assert office.tool(sibling, "office_get_decision", decision_id=current["decision_id"])["code"] == "FORBIDDEN"
        assert report_progress()["run"]["run_id"] == run_id
        office.restart()
        assert report_progress()["run"]["run_id"] == run_id
        link_checks.append("acknowledged_access_shares_only_the_linked_decision_with_the_authorized_worker")

        fresh = accept_contract(3)
        assert report_progress()["code"] == "ACCESS_REQUEST_REQUIRED"
        assert view()["runs"][0]["run_id"] == run_id
        updated = respond(lead_a, respond(lead_b, request_contract(fresh))["request"])["request"]
        assert updated["state"] == "approved_pending_apply" and report_progress()["code"] == "ACCESS_REQUEST_REQUIRED"
        assert sync()[0]["state"] == "active" and get(updated)["state"] == "approved"
        assert report_progress()["run"]["run_id"] == run_id
        link_checks.append("new_consensus_invalidates_linked_permission_until_the_same_run_receives_a_fresh_scope")
    return checks, revocation_checks, link_checks
