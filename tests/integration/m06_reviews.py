"""Verify immutable task results, independent review and bounded dispute recovery."""

import argparse
import hashlib
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path
from uuid import uuid4

from m02_bootstrap import call
from m01 import compose_runner, request, wait_for_status
from workspace_fixture import ROOT, WorkspaceFixture, workspace_fixture


def recipient(identity):
    return {"user_id": identity["owner_user_id"], "agent_id": identity["agent_id"]}


def verify(office, tmp):
    lead, lead_id = office.agent(office.owner_a, "Backend lead", "lead")
    worker, worker_id = office.agent(office.owner_a, "Backend implementer", "worker")
    reviewer, reviewer_id = office.agent(office.owner_b, "Independent reviewer", "reviewer")
    outsider, _ = office.agent(office.owner_b, "Unassigned worker", "worker")
    instances = {}
    for token in (lead, worker):
        status, response = call(office.endpoint, token, "tools/call", {"name": "office_attach_instance",
                                "arguments": {"idempotency_key": str(uuid4()), "label": "Review verification"}})
        assert status == 200, response
        instances[token] = response["result"]["structuredContent"]["instance_id"]

    def create(label, review_identity=reviewer_id):
        grants = [{**recipient(identity), "right": "read"} for identity in (worker_id, review_identity)
                  if identity != lead_id]
        return office.tool(lead, "office_create_task", idempotency_key=str(uuid4()), objective=label,
                           acceptance_criteria=["Validate the exact content"],
                           visibility={"kind": "explicit_acl"}, grants=grants,
                           review_policy={"kind": "explicit_reviewer", "reviewer": recipient(review_identity)})["task"]

    def mutate(token, task, operation, **arguments):
        return office.tool(token, "office_change_task", task_id=task["task_id"], expected_revision=task["revision"],
                           idempotency_key=str(uuid4()), change={"operation": operation, **arguments})

    def start(task, executor=worker):
        prepared = mutate(lead, task, "prepare")
        assert prepared.get("task"), prepared
        running = mutate(executor, prepared["task"], "claim", instance_id=instances[executor])
        assert running.get("run"), running
        return running

    def report(body):
        return {"artifacts": [{"name": "contract.json", "format": "json", "content": body}],
                "agent_reported_checks": [{"name": "client CLI tests", "outcome": "passed",
                                          "notes": "The worker reports a successful local check."}]}

    def submit(running, attachments=None, token=worker, key=None):
        return office.tool(token, "office_submit_result", task_id=running["task"]["task_id"],
                           expected_revision=running["task"]["revision"], idempotency_key=key or str(uuid4()),
                           summary="Published the interface", report=attachments)

    def read(task, token=reviewer, revision=None):
        return office.tool(token, "office_get_task_result", task_id=task["task_id"], revision=revision)

    def review(submitted, decision, binding=None, token=reviewer, run_id=None, key=None):
        return office.tool(token, "office_review_task", task_id=submitted["task"]["task_id"],
                           expected_revision=submitted["task"]["revision"], run_id=run_id or submitted["run"]["run_id"],
                           decision=decision, reason="Reviewed the captured artifact", binding=binding,
                           idempotency_key=key or str(uuid4()))

    draft = create("Check an immutable task result")
    running = start(draft)
    assert running["task"]["acceptance_criteria_revision"] == 1
    assert submit(running, report("{broken"))["code"] == "INVALID_INPUT"
    assert read(draft)["code"] == "INVALID_TRANSITION"
    assert office.tool(worker, "office_get_task", task_id=draft["task_id"])["runs"][0]["status"] == "running"
    body = '{ "version": 1, "label": "ёмкость" }\n'
    candidate = report(body)
    forged = {**candidate, "runner_verified": True}
    status, rejection = call(office.endpoint, worker, "tools/call", {"name": "office_submit_result", "arguments": {
        "workspace_id": office.workspace, "task_id": draft["task_id"], "expected_revision": running["task"]["revision"],
        "summary": "Invalid provenance attempt", "report": forged, "idempotency_key": str(uuid4()),
    }})
    assert status == 200 and ("error" in rejection
        or rejection.get("result", {}).get("isError") is True
        or rejection.get("result", {}).get("structuredContent", {}).get("code") == "INVALID_INPUT"), rejection
    assert read(draft)["code"] == "INVALID_TRANSITION"
    checks = ["invalid_artifact_or_runner_provenance_leaves_attempt_running_without_a_result"]

    key = str(uuid4())
    first = submit(running, candidate, key=key)
    assert first.get("task"), first
    replay = submit(running, candidate, key=key)
    assert all(first[k] == replay[k] for k in ("task", "run", "event_cursor"))
    snapshot = read(draft)
    assert snapshot["binding"]["result_revision"] == 1 and snapshot["run_id"] == running["run"]["run_id"]
    artifact = snapshot["content"]["artifacts"][0]
    assert artifact["content"] == body and artifact["content_sha256"] == hashlib.sha256(body.encode()).hexdigest()
    assert snapshot["content"]["agent_reported_checks"] == candidate["agent_reported_checks"]
    assert "runner_verified" not in snapshot["content"] and snapshot["review"] is None
    assert read(draft, outsider)["code"] == "FORBIDDEN"
    assert office.sql("SET ROLE cocodeop; SELECT count(*) FROM task_result_revisions").splitlines()[-1] == "0"
    assert read(draft, worker) == snapshot
    checks.append("snapshot_preserves_artifact_bytes_criteria_and_agent_reported_checks_under_task_acl")

    assert review(first, "accept")["code"] == "INVALID_INPUT"
    assert review(first, "accept", snapshot["binding"], token=lead)["code"] == "FORBIDDEN"
    wrong_digest = {**snapshot["binding"], "result_sha256": "0" * 64}
    assert review(first, "accept", wrong_digest)["code"] == "REVISION_CONFLICT"
    wrong_criteria = {**snapshot["binding"], "criteria_sha256": "f" * 64}
    assert review(first, "accept", wrong_criteria)["code"] == "REVISION_CONFLICT"
    assert mutate(lead, first["task"], "revise_criteria", expected_criteria_revision=1,
                  acceptance_criteria=["Changed mid-review"])["code"] == "INVALID_TRANSITION"
    returned = review(first, "request_changes", snapshot["binding"])
    assert returned["task"]["status"] == "changes_requested"
    original = read(draft, revision=1)
    assert original["review"]["decision"] == "request_changes"
    assert original["review"]["reviewer"] == recipient(reviewer_id)
    checks.append("independent_review_requires_both_the_exact_content_and_the_criteria_digest")

    assert mutate(lead, returned["task"], "revise_criteria", expected_criteria_revision=2,
                  acceptance_criteria=["Use the new criteria"])["code"] == "REVISION_CONFLICT"
    changed = mutate(lead, returned["task"], "revise_criteria", expected_criteria_revision=1,
                     acceptance_criteria=["Validate the amended interface and its encoding"])["task"]
    assert changed["acceptance_criteria_revision"] == 2
    second_run = start(changed)
    second_file = tmp / "submit-second.json"
    second_file.write_text(json.dumps({"workspace_id": office.workspace, "task_id": draft["task_id"],
        "expected_revision": second_run["task"]["revision"], "idempotency_key": str(uuid4()),
        "summary": "Published the revised interface", "report": report('{"version":2}\n')}), encoding="utf-8")
    second = office.cli(worker, "tasks", "submit", "--file", str(second_file))
    after = office.cli(reviewer, "tasks", "result", "--workspace", office.workspace, "--id", draft["task_id"])
    assert after["binding"]["result_revision"] == 2 and after["binding"]["criteria_revision"] == 2
    assert after["binding"]["criteria_sha256"] != snapshot["binding"]["criteria_sha256"]
    assert read(draft, revision=1) == original and after["review"] is None
    assert review(second, "accept", snapshot["binding"])["code"] == "REVISION_CONFLICT"
    assert review(second, "accept", after["binding"], run_id=first["run"]["run_id"])["code"] == "INVALID_TRANSITION"
    review_file = tmp / "review-second.json"
    review_file.write_text(json.dumps({"workspace_id": office.workspace, "task_id": draft["task_id"],
        "expected_revision": second["task"]["revision"], "run_id": second["run"]["run_id"], "idempotency_key": str(uuid4()),
        "decision": "accept", "reason": "Approved only revision two", "binding": after["binding"]}), encoding="utf-8")
    accepted = office.cli(reviewer, "tasks", "review", "--file", str(review_file))
    assert accepted["task"]["status"] == "done"
    checks.append("new_submission_invalidates_old_review_binding_and_retains_old_artifact_and_criteria")

    self_task = create("Self review must be denied", lead_id)
    self_submit = submit(start(self_task, lead), report('{"owner":"same"}'), token=lead)
    assert self_submit["task"]["status"] == "in_review"
    self_binding = read(self_task, lead)["binding"]
    assert review(self_submit, "accept", self_binding, token=lead)["code"] == "FORBIDDEN"
    checks.append("an_agent_cannot_review_its_own_policy_bound_result")

    stalled = create("Escalate a prolonged review")
    for round_number in range(1, 4):
        submitted = submit(start(stalled))
        assert submitted["task"]["status"] == "in_review"
        assert read(stalled)["binding"]["result_revision"] == round_number
        stalled = review(submitted, "request_changes")["task"]
        assert stalled["status"] == ("blocked" if round_number == 3 else "changes_requested")
    assert stalled["block_reason"] == "review_escalation"
    assert mutate(lead, stalled, "prepare")["code"] == "INVALID_TRANSITION"
    assert mutate(lead, stalled, "resolve_review_escalation", reason="Agent would override")["code"] == "FORBIDDEN"
    assert mutate(office.owner_b, stalled, "resolve_review_escalation", reason="Other owner")["code"] == "FORBIDDEN"
    restored = mutate(office.owner_a, stalled, "resolve_review_escalation", reason="Authorize a further review round")["task"]
    assert restored["status"] == "changes_requested" and restored["block_reason"] is None
    fourth = submit(start(restored))
    assert review(fourth, "request_changes")["task"]["status"] == "changes_requested"
    checks.append("three_returns_block_the_task_until_its_human_owner_resolves_the_escalation")

    office.restart()
    final = read(draft)
    assert final["review"]["decision"] == "accept" and final["binding"] == after["binding"]
    assert read(draft, revision=1) == original
    assert read(stalled, revision=3)["review"]["decision"] == "request_changes"
    assert office.sql("SELECT count(*) FROM task_result_reviews WHERE task_id='" + draft["task_id"] + "'") == "2"
    checks.append("review_and_artifact_revisions_survive_restart_and_replayed_commands")
    return checks


def verify_upgrade(tmp, previous_image):
    storage = tmp / "secrets"
    subprocess.run([sys.executable, ROOT / "deploy/init-secrets.py", storage], check=True)
    project = f"cocodeop-review-upgrade-{uuid4().hex[:12]}"
    env = dict(os.environ, COCODEOP_SECRETS_DIR=str(storage), COCODEOP_HTTP_PORT="0")
    override = tmp / "previous-image.json"
    override.write_text(json.dumps({"services": {"server": {"image": previous_image}}}), encoding="utf-8")
    previous = compose_runner(project, env, override)
    updated = compose_runner(project, env)
    operator = tmp / "operator"
    operator.mkdir(mode=0o700)
    try:
        previous("up", "--no-build", "--wait", "--wait-timeout", "90", timeout=120)
        office = WorkspaceFixture(previous, operator)
        assert request(office.endpoint, "/readyz")[2]["result"]["schema_version"] == 31
        task = office.tool(office.owner_a, "office_create_task", objective="Preserve a prior result",
                           acceptance_criteria=["Keep the original criterion"], idempotency_key=str(uuid4()))["task"]
        prepared = office.tool(office.owner_a, "office_prepare_task", task_id=task["task_id"],
                               expected_revision=task["revision"], idempotency_key=str(uuid4()))["task"]
        running = office.tool(office.owner_a, "office_claim_task", task_id=task["task_id"],
                              expected_revision=prepared["revision"], idempotency_key=str(uuid4()))
        payload = {"task_id": task["task_id"], "expected_revision": running["task"]["revision"],
                   "idempotency_key": str(uuid4()), "summary": "Saved on the previous runtime."}
        submitted = office.tool(office.owner_a, "office_submit_result", **payload)
        assert submitted["task"]["status"] == "in_review"

        updated("up", "--build", "--force-recreate", "--wait", "--wait-timeout", "90", "server", timeout=720)
        office.compose = updated
        office.endpoint = updated("port", "server", "8400").stdout.strip()
        wait_for_status(office.endpoint, "/readyz", 200)
        assert request(office.endpoint, "/readyz")[2]["result"]["schema_version"] >= 32
        migrated = office.tool(office.owner_a, "office_get_task_result", task_id=task["task_id"])
        assert migrated["run_id"] == running["run"]["run_id"] and migrated["binding"]["result_revision"] == 1
        assert migrated["content"]["summary"] == payload["summary"]
        assert migrated["content"]["acceptance_criteria"] == task["acceptance_criteria"]
        assert migrated["content"]["artifacts"] == migrated["content"]["agent_reported_checks"] == []
        assert migrated["review"] is None
        replayed = office.tool(office.owner_a, "office_submit_result", **payload)
        assert replayed["run"] == submitted["run"] and replayed["event_cursor"] == submitted["event_cursor"]
        reviewed = office.tool(office.owner_a, "office_review_task", task_id=task["task_id"],
                               expected_revision=submitted["task"]["revision"], run_id=running["run"]["run_id"],
                               idempotency_key=str(uuid4()), decision="accept", reason="Reviewed the original summary.")
        assert reviewed["task"]["status"] == "done"
        rows = office.sql("SET ROLE cocodeop; SELECT count(*) FROM tasks; SELECT count(*) FROM runs; "
                          "SELECT count(*) FROM task_result_revisions").splitlines()
        assert rows[-3:] == ["0", "0", "0"], rows
        return ["existing_pending_review_keeps_attempt_criteria_and_replay_across_real_upgrade"]
    finally:
        updated("down", "--volumes", "--rmi", "local", "--remove-orphans", check=False, timeout=90)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-image", help="Run the upgrade check from a real schema 31 server image.")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="cocodeop-review-") as directory:
        tmp = Path(directory)
        if args.previous_image:
            print(json.dumps({"checks": verify_upgrade(tmp, args.previous_image)}))
        else:
            with workspace_fixture(tmp, "task-review") as office:
                print(json.dumps({"checks": verify(office, tmp)}))
