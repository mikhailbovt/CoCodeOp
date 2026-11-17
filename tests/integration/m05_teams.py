"""Exercise the real team directory, roster changes and team data scopes."""

import json
import tempfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from uuid import uuid4

from workspace_fixture import workspace_fixture


def run(tmp):
    checks = []
    with workspace_fixture(tmp, "teams") as office:
        lead_a, agent_a = office.agent(office.owner_a, "Backend lead", "lead")
        lead_b, agent_b = office.agent(office.owner_b, "Client lead", "lead")
        worker_a, worker_identity = office.agent(office.owner_a, "Backend engineer", "worker")
        sibling_a, _ = office.agent(office.owner_a, "Outside team", "worker")
        worker_b_key, worker_b = office.agent(office.owner_b, "Client engineer", "worker")

        key = str(uuid4())
        team_a = office.tool(lead_a, "office_create_team", name="Backend", idempotency_key=key)
        team_b = office.tool(lead_b, "office_create_team", name="Client QA", idempotency_key=str(uuid4()))
        assert team_a == office.tool(lead_a, "office_create_team", name="Backend", idempotency_key=key)
        assert team_a["team"]["lead_agent_id"] == agent_a["agent_id"]
        assert team_b["team"]["lead_agent_id"] == agent_b["agent_id"]
        assert team_a["team"]["owner_user_id"] == office.alice["user_id"]
        assert team_b["team"]["owner_user_id"] == office.bob["user_id"]
        assert office.tool(worker_a, "office_create_team", name="Silently promoted",
                           idempotency_key=str(uuid4()))["code"] == "FORBIDDEN"
        assert office.tool(lead_a, "office_get_team", team_id=team_b["team"]["team_id"])["code"] == "FORBIDDEN"
        assert office.tool(lead_b, "office_get_team", team_id=team_a["team"]["team_id"])["code"] == "FORBIDDEN"
        assert office.tool(lead_a, "office_list_teams")["teams"] == [team_a["team"]]
        assert office.tool(lead_b, "office_list_teams")["teams"] == [team_b["team"]]
        checks.extend(["independent_teams_with_scoped_leads", "role_denies_team_creation", "team_directory_isolation"])

        def member(token, team, target, role, key=None):
            return office.tool(
                token, "office_set_team_member", team_id=team["team_id"], expected_revision=team["revision"],
                agent_id=target["agent_id"], role=role, idempotency_key=key or str(uuid4()),
            )

        assert member(lead_a, team_a["team"], worker_b, "worker")["code"] == "FORBIDDEN"
        assert member(lead_a, team_b["team"], worker_identity, "worker")["code"] == "FORBIDDEN"
        assert member(sibling_a, team_a["team"], worker_identity, "reviewer")["code"] == "FORBIDDEN"
        assert member(lead_a, team_a["team"], worker_identity, "lead")["code"] == "INVALID_INPUT"
        assignment_key = str(uuid4())
        assigned = member(lead_a, team_a["team"], worker_identity, "worker", assignment_key)
        assert assigned == member(lead_a, team_a["team"], worker_identity, "worker", assignment_key)
        stale = member(lead_a, team_a["team"], worker_identity, "reviewer")
        assert stale["code"] == "REVISION_CONFLICT" and stale["current_revision"] == assigned["team"]["revision"]
        view = office.tool(worker_a, "office_get_team", team_id=assigned["team"]["team_id"])
        assert view["team"] == assigned["team"]
        assert {item["role"] for item in view["members"]} == {"lead", "worker"}
        assert len(view["members"]) == 3
        checks.extend(["cross_owner_roster_mutation_denied", "revision_bound_roster_replay", "active_worker_roster_visibility"])

        reviewer_team = office.cli(office.owner_a, "teams", "create", "--workspace", office.workspace, "--name", "Review")
        reviewer_binding = member(office.owner_a, reviewer_team["team"], worker_identity, "reviewer")
        assert reviewer_binding["member"]["role"] == "reviewer"
        assert {(item["role"], item["member"]["agent_id"]) for item in
                office.tool(worker_a, "office_get_team", team_id=reviewer_team["team"]["team_id"])["members"]} == {
            ("lead", None), ("reviewer", worker_identity["agent_id"]),
        }
        checks.append("same_agent_has_distinct_scoped_roles")

        private_note = office.tool(lead_a, "office_upsert_note", idempotency_key=str(uuid4()),
                                   title="Backend contract", body="Backend-only draft",
                                   change={"operation": "create", "visibility": {
                                       "kind": "team", "team_id": assigned["team"]["team_id"],
                                   }})
        assert office.tool(worker_a, "office_get_note", note_id=private_note["note_id"]) == private_note
        assert office.tool(lead_b, "office_get_note", note_id=private_note["note_id"])["code"] == "FORBIDDEN"
        assert office.tool(sibling_a, "office_get_note", note_id=private_note["note_id"])["code"] == "FORBIDDEN"
        assert office.sql("SET ROLE cocodeop; SELECT (SELECT count(*) FROM teams), "
                          "(SELECT count(*) FROM team_memberships)").splitlines()[-1] == "0|0"
        checks.extend(["real_roster_drives_team_acl", "team_postgres_unscoped_deny"])

        office.restart()
        listing = office.cli(office.owner_a, "teams", "get", "--workspace", office.workspace,
                             "--id", assigned["team"]["team_id"])
        assert listing == view
        assert office.tool(lead_b, "office_list_teams")["teams"] == [team_b["team"]]
        checks.append("team_custody_survives_restart")

        def invite(team, recipient, key=None):
            return office.tool(
                lead_a, "office_invite_member", team_id=team["team_id"], expected_revision=team["revision"],
                idempotency_key=key or str(uuid4()), role="reviewer",
                recipient={"user_id": recipient["owner_user_id"], "agent_id": recipient["agent_id"]},
            )

        def respond(token, offer, decision, key=None):
            return office.tool(token, "office_respond_invitation", invitation_id=offer["invitation"]["invitation_id"],
                               decision=decision, idempotency_key=key or str(uuid4()))

        invitation_key = str(uuid4())
        offer = invite(assigned["team"], worker_b, invitation_key)
        assert offer == invite(assigned["team"], worker_b, invitation_key)
        assert offer["invitation"]["state"] == "pending"
        assert office.tool(lead_b, "office_list_team_invitations")["invitations"] == []
        assert office.tool(worker_b_key, "office_list_team_invitations")["invitations"] == [offer["invitation"]]
        assert office.cli(office.owner_b, "teams", "invitations", "--workspace", office.workspace)["invitations"] == [
            offer["invitation"],
        ]
        assert office.tool(worker_b_key, "office_get_team", team_id=assigned["team"]["team_id"])["code"] == "FORBIDDEN"
        assert respond(lead_b, offer, "accept")["code"] == "FORBIDDEN"
        assert respond(lead_a, offer, "accept")["code"] == "FORBIDDEN"
        events = office.tool(worker_b_key, "office_read_inbox")["events"]
        assert any(event["event_type"] == "team.invited" and event["payload"]["invitation_id"] ==
                   offer["invitation"]["invitation_id"] for event in events)
        checks.extend(["explicit_team_invitation_delivery", "only_recipient_or_its_owner_may_decide"])

        votes = [(office.owner_b, str(uuid4())), (worker_b_key, str(uuid4()))]
        with ThreadPoolExecutor(max_workers=2) as pool:
            decisions = list(pool.map(lambda vote: respond(vote[0], offer, "accept", vote[1]), votes))
        accepted = [index for index, item in enumerate(decisions) if item.get("invitation", {}).get("state") == "accepted"]
        assert len(accepted) == 1
        winner = accepted[0]
        assert decisions[1 - winner]["code"] == "INVALID_TRANSITION"
        assert decisions[winner] == respond(votes[winner][0], offer, "accept", votes[winner][1])
        accepted_view = office.tool(worker_b_key, "office_get_team", team_id=assigned["team"]["team_id"])
        assert accepted_view["team"]["revision"] == assigned["team"]["revision"] + 1
        assert decisions[winner]["joined_team_revision"] == accepted_view["team"]["revision"]
        assert office.tool(worker_b_key, "office_get_note", note_id=private_note["note_id"]) == private_note
        assert office.tool(office.owner_b, "office_get_team", team_id=assigned["team"]["team_id"])["code"] == "FORBIDDEN"
        assert member(worker_b_key, accepted_view["team"], worker_b, "worker")["code"] == "FORBIDDEN"
        checks.extend(["single_decision_under_concurrency", "external_review_role_after_consent", "agent_membership_remains_exact"])

        declined = invite(accepted_view["team"], agent_b)
        reply = respond(lead_b, declined, "decline")
        assert reply["invitation"]["state"] == "declined" and reply["joined_team_revision"] is None
        assert respond(lead_b, declined, "accept")["code"] == "INVALID_TRANSITION"
        expired = invite(accepted_view["team"], agent_b)
        expired_id = expired["invitation"]["invitation_id"]
        office.sql(f"UPDATE team_invitations SET created_at=clock_timestamp()-interval '2 days', "
                   f"expires_at=clock_timestamp()-interval '1 day' WHERE id='{expired_id}'")
        listing = office.tool(lead_b, "office_list_team_invitations")["invitations"]
        assert next(item for item in listing if item["invitation_id"] == expired_id)["state"] == "expired"
        assert respond(lead_b, expired, "accept")["code"] == "INVALID_TRANSITION"
        disabled_issuer = invite(accepted_view["team"], agent_b)
        office.sql(f"UPDATE agents SET disabled_at=clock_timestamp() WHERE id='{agent_a['agent_id']}'")
        assert respond(lead_b, disabled_issuer, "accept")["code"] == "FORBIDDEN"
        assert office.sql("SET ROLE cocodeop; SELECT count(*) FROM team_invitations").splitlines()[-1] == "0"
        checks.extend(["declined_and_expired_invitations_closed", "issuer_liveness_rechecked", "unscoped_invite_read_denied"])
    return {"checks": checks}


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="cocodeop-teams-") as root:
        print(json.dumps(run(Path(root))))
