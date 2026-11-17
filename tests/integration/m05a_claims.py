"""Exercise path claim admission, races, fencing and duration with PostgreSQL and the real checkout host."""

import json
import time
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timedelta, timezone
from itertools import product
from uuid import uuid4

from m02_bootstrap import call


def file(path):
    return {"kind": "file", "path": path}


def directory(path):
    return {"kind": "directory", "path": path}


def recipient(identity):
    return {"user_id": identity["owner_user_id"], "agent_id": identity["agent_id"]}


def hours(count):
    return (datetime.now(timezone.utc) + timedelta(hours=count)).isoformat()


def check_index(office):
    def covers(left, right):
        same = left["path"] == right["path"]
        if left["kind"] == "file":
            return same and right["kind"] == "file"
        return same or right["path"].startswith(left["path"] + "/")

    def encoded(value):
        return "'" + json.dumps([value], ensure_ascii=False).replace("'", "''") + "'::jsonb"

    names = ["a", "ab", "a-b", "a/child", "a/child/é🌍", "a/x", "a/x0", "à", "ą",
             "a/%_b", "a/[]'\"", "a/_b", "a/x/save.rs", "a/x/deep/component", "a/b.c"]
    rules = [kind(name) for name, kind in product(names, (file, directory))]
    cases = [f"({encoded(left)}, {encoded(right)}, {str(covers(left, right) or covers(right, left)).lower()})"
             for left, right in product(rules, repeat=2)]
    for offset in range(0, len(cases), 100):
        values = ",\n".join(cases[offset:offset + 100])
        mismatches = office.sql(f"SELECT count(*) FROM (VALUES {values}) pair(a,b,expected) "
                                "WHERE (office_claim_ranges(a) && office_claim_ranges(b)) IS DISTINCT FROM expected")
        assert mismatches.splitlines()[-1] == "0", mismatches


class Participant:
    def __init__(self, office, checkout, lead, owner, label, permissions=None,
                 *, objective=None, acceptance=None, envelope=None, required_read_paths=None):
        self.office = office
        self.checkout = checkout
        self.lead, identity = lead
        self.worker, self.identity = office.agent(owner, f"{label} worker", "worker")
        team = office.tool(self.lead, "office_create_team", name=f"{label} claims", idempotency_key=str(uuid4()))["team"]
        self.team_id = team["team_id"]
        office.tool(self.lead, "office_set_team_member", team_id=team["team_id"], agent_id=self.identity["agent_id"],
                    role="worker", expected_revision=team["revision"], idempotency_key=str(uuid4()))
        paths = [directory("components"), directory("parts")]
        self.permissions = permissions or {
            "read": {"allow": paths}, "write": {"allow": paths, "deny": [directory("components/hidden")]},
            "write_operations": ["create", "replace"],
        }
        grant = office.tool(office.owner_a, "office_issue_checkout_grant", checkout_id=checkout["checkout_id"],
                            expected_revision=checkout["revision"], idempotency_key=str(uuid4()), recipient=recipient(identity),
                            expires_at=hours(8), permissions=envelope or self.permissions)
        assert grant.get("grant"), grant
        self.grant = grant["grant"]
        draft = office.tool(self.lead, "office_create_task", objective=objective or f"Work on the {label} modules",
                            acceptance_criteria=acceptance or ["Coordinate exact paths and components"],
                            team_id=team["team_id"], visibility={"kind": "team", "team_id": team["team_id"]},
                            execution={"kind": "coding"}, idempotency_key=str(uuid4()))["task"]
        self.task = draft["task_id"]
        staged = self.propose_scope(draft["revision"], required_read_paths=required_read_paths)
        assert staged.get("scope"), staged
        self.task_revision = staged["task_revision"]
        self.scope_revision = 1
        status, attached = call(office.endpoint, self.worker, "tools/call", {"name": "office_attach_instance",
                                "arguments": {"idempotency_key": str(uuid4()), "label": f"{label} claims session"}})
        assert status == 200, attached
        self.instance = attached["result"]["structuredContent"]["instance_id"]
        self.run = None

    def start(self):
        prepared = self.office.tool(self.lead, "office_prepare_task", task_id=self.task,
                                    expected_revision=self.task_revision, idempotency_key=str(uuid4()))
        assert prepared.get("task"), prepared
        running = self.office.tool(self.worker, "office_claim_task", task_id=self.task, instance_id=self.instance,
                                   expected_revision=prepared["task"]["revision"], idempotency_key=str(uuid4()))
        assert running.get("run"), running
        self.run = running["run"]["run_id"]
        self.task_revision = running["task"]["revision"]

    def propose_scope(self, task_revision, expected_scope_revision=None, permissions=None, required_read_paths=None):
        return self.office.tool(self.lead, "office_set_task_scope", task_id=self.task,
                                checkout_id=self.checkout["checkout_id"], worker=recipient(self.identity),
                                grant_id=self.grant["grant_id"], expected_task_revision=task_revision,
                                expected_scope_revision=expected_scope_revision, idempotency_key=str(uuid4()),
                                expires_at=hours(4), permissions=permissions or self.permissions,
                                required_read_paths=required_read_paths)

    def payload(self, paths, expected=None, ttl=60, key=None, revision=None, run=None):
        return {"task_id": self.task, "run_id": run or self.run, "scope_revision": revision or self.scope_revision,
                "paths": paths, "expected_fence": expected, "ttl_seconds": ttl, "idempotency_key": key or str(uuid4())}

    def claim(self, paths, expected=None, ttl=60, token=None, key=None, revision=None, run=None):
        return self.office.tool(token or self.worker, "office_claim_paths",
                                **self.payload(paths, expected, ttl, key, revision, run))

    def renew(self, fence, seconds=100):
        return self.office.tool(self.worker, "office_renew_claim", task_id=self.task, run_id=self.run,
                                claim_fence=fence, ttl_seconds=seconds, idempotency_key=str(uuid4()))

    def release(self, fence, key=None):
        return self.office.tool(self.worker, "office_release_claim", task_id=self.task, run_id=self.run,
                                claim_fence=fence, idempotency_key=key or str(uuid4()))

    def history(self, token=None, **page):
        return self.office.tool(token or self.worker, "office_list_claims", checkout_id=self.checkout["checkout_id"],
                                task_id=self.task, **page)


def verify(office, host, checkout, first, second, tmp):
    check_index(office)
    checks = ["postgres_conflict_index_matches_exact_prefix_contract_in_900_cases"]
    a = Participant(office, checkout, first, office.owner_a, "Backend")
    b = Participant(office, checkout, second, office.owner_b, "Client")
    state = "claims-state"
    host.invoke(["mkdir", "-m", "700", "--", f"{host.root}/{state}"], check=True)

    def sync():
        applied = host.sync_scopes(office.endpoint, checkout, state=state)
        assert applied.returncode == 0, applied.stderr
        return json.loads(applied.stdout)

    assert [item["state"] for item in sync()] == ["active", "active"]
    a.start()
    b.start()

    common = [file("components/a/common.rs")]
    assert a.claim(common, token=a.lead)["code"] == "FORBIDDEN"
    assert a.claim(common, run=b.run)["code"] == "FORBIDDEN"
    assert a.claim(common, revision=2)["code"] == "REVISION_CONFLICT"
    for forbidden in (file("src/app.rs"), directory("components"), file("components/hidden/token.rs"),
                      file("parts/.git/config"), file("parts/.env.example")):
        assert a.claim([forbidden])["code"] == "ACCESS_REQUEST_REQUIRED", forbidden
    assert a.claim(common, ttl=121)["code"] == "INVALID_INPUT"
    assert a.claim([{"kind": "tree"}])["code"] == "INVALID_INPUT"
    checks.append("claim_requires_current_worker_run_and_uninterrupted_write_permission")

    keys = [str(uuid4()), str(uuid4())]
    with ThreadPoolExecutor(max_workers=2) as pool:
        futures = [pool.submit(worker.claim, common, key=key) for worker, key in zip((a, b), keys)]
        results = [future.result() for future in futures]
    assert sum("claim" in result for result in results) == 1 and sum(
        result.get("code") == "PATH_CLAIMED" for result in results) == 1, results
    winner_index = next(index for index, result in enumerate(results) if "claim" in result)
    winner, peer = (a, b) if winner_index == 0 else (b, a)
    claimed = results[winner_index]
    first_fence = claimed["claim"]["claim_fence"]
    assert winner.claim(common, key=keys[winner_index]) == claimed
    assert winner.history(peer.worker)["claims"] == []
    assert winner.history(office.owner_a)["claims"] == [claimed["claim"]]
    assert office.sql("SET ROLE cocodeop; SELECT count(*) FROM path_claims").splitlines()[-1] == "0"
    checks.append("two_distinct_workers_racing_for_one_file_get_one_fence_and_no_private_metadata_leak")

    prefix = [directory("components/a")]
    current = winner.claim(prefix, expected=first_fence)["claim"]
    assert current["claim_fence"] > first_fence and current["started_at"] == claimed["claim"]["started_at"]
    assert winner.release(first_fence)["code"] == "CLAIM_FENCE_STALE"
    adjacent = peer.claim([file("components/ab/common.rs")])["claim"]
    free = file("parts/available.rs")
    blocked = peer.claim([file("components/a/other.rs"), free], expected=adjacent["claim_fence"])
    assert blocked == {"code": "PATH_CLAIMED", "retryable": False}, blocked
    assert peer.history()["claims"] == [adjacent]
    replaced = winner.claim([*prefix, free], expected=current["claim_fence"])["claim"]
    assert replaced["claim_fence"] > current["claim_fence"]
    assert peer.claim([directory("components/a/child")], expected=adjacent["claim_fence"])["code"] == "PATH_CLAIMED"
    checks.append("conflicting_set_replacement_rolls_back_all_paths_and_preserves_the_old_fence")

    first_renewed = winner.renew(replaced["claim_fence"], 95)["claim"]
    renewed = winner.renew(replaced["claim_fence"], 120)["claim"]
    assert renewed["claim_fence"] == replaced["claim_fence"] and renewed["renewals"] == 2
    assert renewed["expires_at"] > first_renewed["expires_at"] > replaced["expires_at"]
    assert winner.renew(renewed["claim_fence"], 120)["code"] == "CLAIM_RENEWAL_LIMIT"
    lease = office.cli(winner.worker, "claims", "list", "--workspace", office.workspace,
                       "--checkout", checkout["checkout_id"], "--task", winner.task, "--limit", "1")
    assert lease["claims"] == [renewed] and lease["next_before_fence"] == renewed["claim_fence"]
    assert winner.history(before_fence=lease["next_before_fence"])["claims"][0]["state"] == "fenced"
    checks.append("renewal_retains_the_fence_and_history_paginates_with_the_workers_cli")

    office.restart()
    assert winner.history(limit=1)["claims"] == [renewed]
    assert winner.claim(common, key=keys[winner_index]) == claimed
    release_key = str(uuid4())
    released = winner.release(renewed["claim_fence"], release_key)
    assert released["claim"]["state"] == "released" and winner.release(renewed["claim_fence"], release_key) == released
    notices = office.tool(winner.worker, "office_read_inbox", limit=100)["events"]
    assert sum(event["event_type"] == "path.released" and event["payload"]["claim_fence"] == renewed["claim_fence"]
               for event in notices) == 1
    checks.append("restart_and_retries_preserve_claim_history_without_duplicate_events")

    assert peer.release(adjacent["claim_fence"])["claim"]["state"] == "released"
    expiring = winner.claim(common, ttl=5)["claim"]
    assert peer.claim(common)["code"] == "PATH_CLAIMED"
    time.sleep(5.15)
    assert winner.renew(expiring["claim_fence"])["code"] == "CLAIM_FENCE_STALE"
    fresh = peer.claim(common)["claim"]
    assert fresh["claim_fence"] > expiring["claim_fence"]
    assert winner.history(limit=1)["claims"][0]["state"] == "expired"
    checks.append("expiry_allows_a_different_worker_to_obtain_a_fresh_fence")

    blocked = office.tool(peer.worker, "office_change_task", task_id=peer.task, expected_revision=peer.task_revision,
                          change={"operation": "request_help", "message": "Check the shared assumptions"},
                          idempotency_key=str(uuid4()))
    assert blocked["run"]["status"] == "waiting_input" and blocked["run"]["run_id"] == peer.run
    assert peer.history(limit=1)["claims"][0]["state"] == "fenced"
    assert peer.renew(fresh["claim_fence"])["code"] == "INVALID_TRANSITION"
    request = tmp / "claims-request.json"
    request.write_text(json.dumps({"workspace_id": office.workspace, **winner.payload(common)}), encoding="utf-8")
    cli_claimed = office.cli(winner.worker, "claims", "acquire", "--file", str(request))["claim"]
    assert cli_claimed["claim_fence"] > fresh["claim_fence"]
    checks.append("waiting_run_frees_paths_and_the_peer_can_acquire_them_through_cli")

    narrower = {**winner.permissions, "write_operations": []}
    edit = winner.propose_scope(winner.task_revision, expected_scope_revision=1, permissions=narrower)
    assert edit["scope"]["definition"]["revision"] == 2
    assert winner.history(limit=1)["claims"][0]["state"] == "active"
    assert sync()[0]["state"] == "active"
    assert winner.history(limit=1)["claims"][0]["state"] == "fenced"
    assert winner.claim(common)["code"] == "REVISION_CONFLICT"
    assert winner.claim(common, revision=2)["code"] == "ACCESS_REQUEST_REQUIRED"
    assert winner.release(cli_claimed["claim_fence"])["code"] == "CLAIM_FENCE_STALE"
    checks.append("host_activation_fences_existing_claims_and_read_only_scope_cannot_claim")
    return checks
