"""Verify addressed handoffs between distinct workers on the same verified checkout."""

import json
from concurrent.futures import ThreadPoolExecutor
from uuid import uuid4

from m05a_claims import Participant, directory, file


def verify(office, host, checkout, first, second, tmp):
    holder = Participant(office, checkout, first, office.owner_a, "Owner")
    requester = Participant(office, checkout, second, office.owner_b, "Requester")
    peer = Participant(office, checkout, second, office.owner_b, "Peer")
    state = "handoff-policy"
    host.invoke(["mkdir", "-m", "700", "--", f"{host.root}/{state}"], check=True)

    def synchronize():
        applied = host.sync_scopes(office.endpoint, checkout, state=state)
        assert applied.returncode == 0, applied.stderr
        return json.loads(applied.stdout)

    assert [item["state"] for item in synchronize()] == ["active", "active", "active"]
    for worker in (holder, requester, peer):
        worker.start()

    selected = [file("components/api/types.rs"), file("components/api/values.rs")]
    original = [directory("components/api"), file("parts/unrelated.rs")]

    def ask(worker, paths=selected, key=None, scope_revision=None):
        return office.tool(worker.worker, "office_request_path_handoff", task_id=worker.task, run_id=worker.run,
                           scope_revision=scope_revision or worker.scope_revision, paths=paths,
                           reason="Coordinate the next edit to this interface", idempotency_key=key or str(uuid4()))

    def get(token, ticket):
        return office.tool(token, "office_get_path_handoff", handoff_id=ticket["handoff_id"])

    def respond(token, ticket, decision="offer", key=None):
        return office.tool(token, "office_respond_path_handoff", handoff_id=ticket["handoff_id"],
                           expected_revision=ticket["revision"], decision=decision, idempotency_key=key or str(uuid4()))

    def consume(worker, offer, paths=selected, key=None):
        return office.tool(worker.worker, "office_claim_paths", **worker.payload(paths, key=key),
                           handoff_id=offer["handoff_id"])

    assert ask(requester)["code"] == "CLAIM_FENCE_STALE"
    initial = holder.claim(original)["claim"]
    self_held = requester.claim([file("parts/requester.rs")])["claim"]
    assert ask(requester)["code"] == "INVALID_TRANSITION"
    requester.release(self_held["claim_fence"])
    assert ask(requester, scope_revision=2)["code"] == "REVISION_CONFLICT"
    assert ask(requester, [file("src/app.rs")])["code"] == "ACCESS_REQUEST_REQUIRED"
    assert ask(requester, [*selected, file("parts/not-held.rs")])["code"] == "CLAIM_FENCE_STALE"
    key = str(uuid4())
    requested = ask(requester, key=key)
    assert requested.get("handoff"), requested
    assert requested == ask(requester, key=key)
    ticket = requested["handoff"]
    assert ticket["state"] == "requested" and ticket["paths"] == selected
    assert get(holder.worker, ticket) == ticket and get(peer.worker, ticket)["code"] == "FORBIDDEN"
    assert get(holder.lead, ticket)["code"] == "FORBIDDEN"
    assert office.tool(holder.worker, "office_get_task", task_id=requester.task)["code"] == "FORBIDDEN"
    assert ask(requester)["code"] == "PATH_HANDOFF_PENDING"
    assert requester.claim([file("parts/requester.rs")])["code"] == "PATH_HANDOFF_PENDING"
    notices = office.tool(holder.worker, "office_read_inbox", limit=100)["events"]
    addressed = [event for event in notices if event["event_type"] == "path.handoff_requested"]
    assert len(addressed) == 1 and addressed[0]["payload"]["handoff_id"] == ticket["handoff_id"]
    assert "task_id" not in addressed[0]["payload"]
    assert office.sql("SET ROLE cocodeop; SELECT count(*) FROM path_claim_handoffs").splitlines()[-1] == "0"
    checks = ["request_requires_own_write_authority_no_held_paths_and_private_addressing"]

    waiting_peer = ask(peer)["handoff"]
    assert holder.renew(initial["claim_fence"], 120)["code"] == "PATH_HANDOFF_PENDING"
    assert holder.claim(original, expected=initial["claim_fence"], ttl=120)["code"] == "PATH_HANDOFF_PENDING"
    assert respond(requester.worker, ticket)["code"] == "FORBIDDEN"
    assert respond(office.owner_a, ticket)["code"] == "FORBIDDEN"
    approval_key = str(uuid4())
    offer = respond(holder.worker, ticket, key=approval_key)
    assert offer.get("handoff"), offer
    assert offer == respond(holder.worker, ticket, key=approval_key)
    offered = offer["handoff"]
    assert offered["state"] == "offered" and offered["revision"] == 2
    assert get(peer.worker, waiting_peer)["state"] == "expired"
    assert holder.history(limit=1)["claims"][0]["state"] == "released"
    assert holder.release(initial["claim_fence"])["code"] == "CLAIM_FENCE_STALE"
    assert holder.claim(selected)["code"] == "PATH_CLAIMED"
    assert consume(peer, offered)["code"] == "FORBIDDEN"
    assert consume(requester, offered, [*selected, file("parts/unrelated.rs")])["code"] == "INVALID_INPUT"
    alone = peer.claim([file("parts/unrelated.rs")])["claim"]
    peer.release(alone["claim_fence"])
    checks.append("waiting_blocks_renew_and_replacement_then_offer_releases_and_reserves_only_requested_paths")

    office.restart()
    assert office.cli(holder.worker, "claims", "handoff", "--workspace", office.workspace,
                      "--id", offered["handoff_id"]) == offered
    request_file = tmp / "handoff-consume.json"
    request_file.write_text(json.dumps({"workspace_id": office.workspace, **requester.payload(selected),
                                        "handoff_id": offered["handoff_id"]}), encoding="utf-8")
    with ThreadPoolExecutor(max_workers=2) as pool:
        consume_offer = pool.submit(office.cli, requester.worker, "claims", "acquire", "--file", str(request_file))
        outsider = pool.submit(peer.claim, selected)
        acquired = consume_offer.result()
        refused = outsider.result()
    assert refused["code"] == "PATH_CLAIMED", refused
    replay = office.cli(requester.worker, "claims", "acquire", "--file", str(request_file))
    claim = acquired["claim"]
    assert replay == acquired and claim["claim_fence"] > initial["claim_fence"] and claim["run_id"] == requester.run
    assert get(holder.worker, offered)["state"] == "accepted"
    checks.append("reservation_survives_restart_and_grants_one_new_fence_to_the_addressed_run")

    assert requester.release(claim["claim_fence"])["claim"]["state"] == "released"
    current = holder.claim(original)["claim"]
    refused = ask(requester)["handoff"]
    assert respond(holder.worker, refused, "reject")["handoff"]["state"] == "rejected"
    assert holder.renew(current["claim_fence"], 120)["claim"]["claim_fence"] == current["claim_fence"]
    cancelled = ask(requester)["handoff"]
    assert respond(requester.worker, cancelled, "cancel")["handoff"]["state"] == "cancelled"
    assert respond(holder.worker, cancelled)["code"] == "REVISION_CONFLICT"
    expired = ask(requester)["handoff"]
    offer = respond(holder.worker, expired)["handoff"]
    office.sql(f"UPDATE path_claim_handoffs SET expires_at=clock_timestamp()-interval '1 second' WHERE id='{offer['handoff_id']}'")
    assert get(requester.worker, offer)["state"] == "expired"
    assert consume(requester, offer)["code"] == "CLAIM_FENCE_STALE"
    after_timeout = peer.claim(selected)["claim"]
    checks.append("reject_cancel_and_expiry_leave_no_spurious_reservation")

    revocable = ask(requester)["handoff"]
    poised = respond(peer.worker, revocable)["handoff"]
    assert poised["state"] == "offered"
    revoked = office.tool(office.owner_a, "office_revoke_checkout_grant", grant_id=requester.grant["grant_id"],
                          expected_revision=requester.grant["revision"], idempotency_key=str(uuid4()))
    assert revoked.get("grant"), revoked
    assert get(requester.worker, poised)["state"] == "expired"
    assert consume(requester, poised)["code"] == "ACCESS_REQUEST_REQUIRED"
    reclaimed = holder.claim(selected)["claim"]
    assert reclaimed["claim_fence"] > after_timeout["claim_fence"]
    revoke_scope = office.tool(office.owner_a, "office_revoke_scope", task_id=holder.task, expected_scope_revision=1,
                               reason="Close the holder's source permission", idempotency_key=str(uuid4()))
    assert revoke_scope["scope"]["state"] == "revocation_pending"
    assert holder.history(limit=1)["claims"][0]["state"] == "fenced"
    assert synchronize()[0]["state"] == "revoked"
    assert peer.claim(selected)["claim"]["claim_fence"] > reclaimed["claim_fence"]
    checks.append("grant_and_scope_revocation_free_paths_and_invalidate_pending_reservations")
    return checks
