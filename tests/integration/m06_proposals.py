"""Verify revision-bound consensus and a single effective shared-area decision."""

import hashlib
import json
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timedelta, timezone
from uuid import uuid4

from m05a_claims import file, recipient


def deadline():
    return (datetime.now(timezone.utc) + timedelta(hours=1)).isoformat()


def content(recommendation="structured", title="Shared API envelope"):
    return {"title": title, "problem": "Two clients need to interpret the same response.",
            "options": [{"key": "structured", "description": "Use fields with typed names."},
                        {"key": "plain", "description": "Return a plain text response."}],
            "recommendation": recommendation}


def verify(office, checkout, first, second, tmp):
    lead_a, agent_a = first
    lead_b, agent_b = second
    outsider, _ = office.agent(office.owner_a, "Unappointed lead", "lead")
    candidates = [recipient(agent_a), recipient(agent_b)]
    checkout_id = checkout["checkout_id"]

    def area_change(path, approvers, before=None):
        return office.tool(office.owner_a, "office_define_area", checkout_id=checkout_id,
                           expected_checkout_revision=checkout["revision"], idempotency_key=str(uuid4()),
                           area_id=before["area_id"] if before else None,
                           expected_area_revision=before["revision"] if before else None,
                           name="Shared API", paths=file(path), policy={"kind": "shared", "approvers": approvers})["area"]

    area = area_change("components/api/contract.json", candidates)

    def target(generation=0, policy=None):
        policy = policy or area
        return {"kind": "shared_area", "checkout_id": checkout_id, "area_id": policy["area_id"],
                "expected_area_revision": policy["revision"], "expected_generation": generation}

    def read_area(token=lead_a):
        return office.tool(token, "office_get_area_decision", checkout_id=checkout_id, area_id=area["area_id"])

    def open_proposal(token=lead_a, draft=None, policy=None, key=None):
        return office.tool(token, "office_propose_decision", target=policy or target(),
                           content=draft or content(), deadline=deadline(), idempotency_key=key or str(uuid4()))

    def get(token, proposal, revision=None):
        return office.tool(token, "office_get_proposal", proposal_id=proposal["proposal_id"], revision=revision)

    def read_decision(token, decision):
        return office.tool(token, "office_get_decision", decision_id=decision["decision_id"])

    def respond(token, proposal, decision="approve", key=None):
        return office.tool(token, "office_respond_proposal", proposal_id=proposal["proposal_id"],
                           expected_revision=proposal["revision"], decision=decision,
                           reason="Reviewed that exact version of the interface", idempotency_key=key or str(uuid4()))

    def revise(token, proposal, draft):
        return office.tool(token, "office_revise_proposal", proposal_id=proposal["proposal_id"],
                           expected_revision=proposal["revision"], content=draft, deadline=deadline(),
                           idempotency_key=str(uuid4()))

    no_decision = read_area()
    assert no_decision == {"area_id": area["area_id"], "area_revision": 1, "generation": 0, "decision": None}
    assert read_area(outsider)["code"] == "FORBIDDEN"
    assert open_proposal(outsider)["code"] == "FORBIDDEN"
    assert open_proposal(policy=target(1))["code"] == "REVISION_CONFLICT"
    another, another_id = office.agent(office.owner_a, "Second agent of owner A", "lead")
    same_owner = area_change("parts/single-owner.json", [recipient(agent_a), recipient(another_id)])
    assert open_proposal(another, policy=target(policy=same_owner))["code"] == "INVALID_INPUT"
    checks = ["shared_approval_roster_comes_from_area_and_contains_independent_owners"]

    initial = open_proposal()
    assert initial.get("proposal"), initial
    candidate = initial["proposal"]
    competing = open_proposal(lead_b, content("plain"))["proposal"]
    assert candidate["required_approvers"] == candidates and candidate["state"] == "open"
    assert get(outsider, candidate)["code"] == "FORBIDDEN"
    assert respond(office.owner_a, candidate)["code"] == "FORBIDDEN"
    assert respond(outsider, candidate)["code"] == "FORBIDDEN"
    approved = respond(lead_a, candidate)
    assert approved.get("proposal"), approved
    assert approved["decision"] is None and approved["proposal"]["state"] == "open"
    assert respond(lead_a, candidate)["code"] == "INVALID_TRANSITION"
    assert respond(lead_a, competing)["decision"] is None
    assert read_area() == no_decision
    checks.append("exact_voters_must_explicitly_approve_the_same_revision")

    final_keys = [str(uuid4()), str(uuid4())]
    with ThreadPoolExecutor(max_workers=2) as pool:
        results = [future.result() for future in [pool.submit(respond, lead_b, proposal, key=key)
                   for proposal, key in zip((candidate, competing), final_keys)]]
    assert sum(result.get("decision") is not None for result in results) == 1
    assert sum(result.get("code") == "REVISION_CONFLICT" for result in results) == 1, results
    winner = next(index for index, result in enumerate(results) if result.get("decision"))
    decided = results[winner]
    chosen = decided["decision"]
    assert chosen["generation"] == 1 and chosen["proposal_revision"] == 1
    assert len(decided["proposal"]["responses"]) == 2
    assert {item["responder"]["agent_id"] for item in decided["proposal"]["responses"]} == {
        agent_a["agent_id"], agent_b["agent_id"],
    }
    draft = (content(), content("plain"))[winner]
    assert chosen["selected_option"]["key"] == draft["recommendation"]
    assert chosen["content_sha256"] == hashlib.sha256(json.dumps(draft, separators=(",", ":")).encode()).hexdigest()
    assert respond(lead_b, (candidate, competing)[winner], key=final_keys[winner]) == decided
    assert read_area(lead_b)["decision"] == chosen
    assert read_decision(outsider, chosen)["code"] == "FORBIDDEN"
    assert office.sql("SET ROLE cocodeop; SELECT count(*) FROM decisions").splitlines()[-1] == "0"
    checks.append("concurrent_incompatible_proposals_publish_only_one_generation_and_content_hash")

    next_spec = target(1)
    new = open_proposal(policy=next_spec)["proposal"]
    assert respond(lead_b, new)["decision"] is None
    assert revise(lead_b, new, content("plain"))["code"] == "FORBIDDEN"
    edited = revise(lead_a, new, content("plain", "Revise the shared envelope"))["proposal"]
    assert edited["revision"] == 2 and edited["responses"] == []
    assert get(lead_b, new, revision=1)["state"] == "superseded"
    assert len(get(lead_b, new, revision=1)["responses"]) == 1
    assert respond(lead_b, new)["code"] == "REVISION_CONFLICT"
    assert respond(lead_a, edited)["decision"] is None
    updated = respond(lead_b, edited)["decision"]
    assert updated["generation"] == 2 and updated["proposal_revision"] == 2
    assert updated["selected_option"]["key"] == "plain" and updated["content_sha256"] != chosen["content_sha256"]
    assert read_area()["decision"] == updated
    assert read_decision(lead_b, chosen) == chosen
    checks.append("editing_an_open_proposal_forces_fresh_votes_and_preserves_prior_decisions")

    stale = open_proposal(policy=target(2))["proposal"]
    assert respond(lead_a, stale)["decision"] is None
    changed_area = area_change("components/api/contract.json", candidates, before=area)
    assert respond(lead_b, stale)["code"] == "REVISION_CONFLICT"
    assert read_area()["generation"] == 2 and read_area()["decision"] is None
    assert open_proposal(policy=target(2))["code"] == "REVISION_CONFLICT"
    assert open_proposal(policy=target(2, policy=changed_area))["proposal"]["state"] == "open"
    checks.append("changed_area_policy_fences_inflight_approvals_without_erasing_history")

    general = {"kind": "workspace", "topic": "Engineering workflow", "required_approvers": candidates}
    ordinary = open_proposal(policy=general)["proposal"]
    rejected = respond(lead_b, ordinary, "reject")["proposal"]
    assert rejected["state"] == "rejected" and rejected["decision_id"] is None
    assert revise(lead_a, ordinary, content())["code"] == "INVALID_TRANSITION"
    expired = open_proposal(policy=general)["proposal"]
    office.sql(f"UPDATE proposal_revisions SET deadline=clock_timestamp()-interval '1 second' "
               f"WHERE proposal_id='{expired['proposal_id']}'")
    assert get(lead_a, expired)["state"] == "expired"
    assert respond(lead_b, expired)["code"] == "INVALID_TRANSITION"
    dispute = open_proposal(policy=general)["proposal"]
    for revision in (1, 2):
        round_end = respond(lead_b, dispute, "request_changes")["proposal"]
        assert round_end["revision"] == revision and round_end["state"] == "changes_requested"
        dispute = revise(lead_a, round_end, content(title=f"Review revision {revision + 1}"))["proposal"]
    stalled = respond(lead_b, dispute, "request_changes")["proposal"]
    assert stalled["revision"] == 3 and stalled["state"] == "escalated"
    assert stalled["escalated_to"] == agent_a["owner_user_id"] and not stalled["decision_id"]
    assert revise(lead_a, stalled, content())["code"] == "INVALID_TRANSITION"
    page = office.tool(office.owner_a, "office_read_inbox", limit=100)
    inbox = list(page["events"])
    while page["has_more"]:
        page = office.tool(office.owner_a, "office_read_inbox", cursor=page["next_cursor"], limit=100)
        inbox.extend(page["events"])
    assert any(event["event_type"] == "proposal.escalated"
               and event["payload"]["proposal_id"] == dispute["proposal_id"] for event in inbox)
    checks.append("deadlines_rejection_and_three_rounds_of_dispute_end_with_a_human_escalation")

    office.restart()
    assert office.cli(lead_a, "decisions", "get", "--workspace", office.workspace,
                      "--id", updated["decision_id"]) == updated
    request = tmp / "decision.json"
    request.write_text(json.dumps({"workspace_id": office.workspace, "target": general, "content": content(),
                                   "deadline": deadline(), "idempotency_key": str(uuid4())}), encoding="utf-8")
    cli_opened = office.cli(lead_b, "decisions", "propose", "--file", str(request))["proposal"]
    assert get(lead_a, cli_opened) == cli_opened
    assert get(lead_a, stalled)["state"] == "escalated"
    checks.append("decision_and_escalation_survive_restart_and_cross_mcp_cli")
    return checks
