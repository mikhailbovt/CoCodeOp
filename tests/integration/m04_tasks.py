"""Test typed task transitions, dependency races and separate Run history."""

import hashlib
import json
import os
import secrets
import subprocess
import sys
import tempfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from uuid import UUID, uuid4

from m01 import ROOT, compose_runner, wait_for_status
from m02_bootstrap import call, request


def run(tmp):
    storage = tmp / "secrets"
    subprocess.run([sys.executable, ROOT / "deploy/init-secrets.py", storage], check=True)
    compose = compose_runner(f"cocodeop-tasks-{uuid4().hex[:12]}", dict(
        os.environ, COCODEOP_SECRETS_DIR=str(storage), COCODEOP_HTTP_PORT="0",
    ))
    office = ROOT / "target" / "release" / ("office.exe" if os.name == "nt" else "office")
    operator = tmp / "operator"
    operator.mkdir(mode=0o700)

    def api(path, token, body):
        status, reply = request(endpoint, path, token=token, body=body)
        assert status == 200, (status, reply)
        return reply["result"]

    def mcp(token, name, arguments):
        status, reply = call(endpoint, token, "tools/call", {"name": name, "arguments": arguments})
        assert status == 200, (status, reply)
        return reply["result"]["structuredContent"]

    def digest(value):
        return hashlib.sha256(value.encode()).hexdigest()

    def create_agent(token, name, role):
        key = secrets.token_hex(32)
        identity = api(f"/v1/workspaces/{workspace}/agents", token, {
            "idempotency_key": str(uuid4()), "display_name": name, "role": role, "credential_digest": digest(key),
        })
        instance = mcp(key, "office_attach_instance", {"idempotency_key": str(uuid4()), "label": name})
        return key, identity, instance["instance_id"]

    def new_task(actor, objective, **options):
        payload = {
            "workspace_id": workspace, "idempotency_key": str(uuid4()), "objective": objective,
            "acceptance_criteria": ["Record an explicit, reviewable result"], **options,
        }
        return payload, mcp(actor[0], "office_create_task", payload)

    def change(actor, current, operation, **options):
        row = current["task"]
        payload = {
            "workspace_id": workspace, "idempotency_key": str(uuid4()), "task_id": row["task_id"],
            "expected_revision": row["revision"], "change": {"operation": operation, **options},
        }
        return payload, mcp(actor[0], "office_change_task", payload)

    def get(actor, task, **options):
        return mcp(actor[0], "office_get_task", {"workspace_id": workspace, "task_id": task["task"]["task_id"], **options})

    def same_receipt(a, b):
        for result in (a, b):
            assert UUID(result["request_id"])
        assert a["request_id"] != b["request_id"]
        assert {k: v for k, v in a.items() if k != "request_id"} == {
            k: v for k, v in b.items() if k != "request_id"
        }

    def sql(query):
        return compose("exec", "-T", "db", "psql", "-U", "postgres", "-d", "cocodeop", "-XAt",
                       "-v", "ON_ERROR_STOP=1", "-c", query).stdout.strip()

    checks = []
    try:
        compose("up", "--build", "--wait", "--wait-timeout", "90", timeout=720)
        endpoint = compose("port", "server", "8400").stdout.strip()
        user_flag = ["--user", f"{os.getuid()}:{os.getgid()}"] if hasattr(os, "getuid") else []
        owner = json.loads(compose(
            "run", "--rm", "--no-deps", "-T", *user_flag, "--entrypoint", "/usr/local/bin/office", "-v",
            f"{operator}:/operator", "server", "bootstrap", "--user", "Alice", "--workspace", "Tasks",
            "--token-file", "/operator/alice.token", "--json",
        ).stdout)
        office_a = (operator / "alice.token").read_text().strip()
        workspace = owner["memberships"][0]["workspace_id"]
        invitation, office_b = secrets.token_hex(32), secrets.token_hex(32)
        api(f"/v1/workspaces/{workspace}/invitations", office_a, {
            "idempotency_key": str(uuid4()), "display_name": "Bob", "invitation_digest": digest(invitation),
        })
        bob = api("/v1/invitations/accept", office_b, {"idempotency_key": str(uuid4()), "invitation_token": invitation})
        lead_a = create_agent(office_a, "Lead A", "lead")
        lead_b = create_agent(office_b, "Lead B", "lead")
        worker_a = create_agent(office_a, "Worker A", "worker")
        reviewer_b = create_agent(office_b, "Reviewer B", "reviewer")

        explicit_a = {"kind": "explicit_reviewer", "reviewer": {
            "user_id": owner["user_id"], "agent_id": lead_a[1]["agent_id"],
        }}
        create_input, main = new_task(lead_a, "Agree on a shared API format", review_policy=explicit_a)
        assert main["task"]["status"] == "draft" and main["run"] is None and UUID(main["event_cursor"])
        same_receipt(main, mcp(lead_a[0], "office_create_task", create_input))
        amended = dict(create_input, objective="Would create a second task")
        assert mcp(lead_a[0], "office_create_task", amended)["code"] == "IDEMPOTENCY_CONFLICT"
        assert get(lead_b, main)["task"]["acceptance_criteria"] == ["Record an explicit, reviewable result"]
        assert new_task(worker_a, "Cannot create as worker")[1]["code"] == "FORBIDDEN"
        assert new_task(reviewer_b, "Cannot create as reviewer")[1]["code"] == "FORBIDDEN"
        checks.extend(["typed_creation_and_criteria", "actor_roles", "idempotent_creation_and_conflict"])

        _, child = new_task(lead_b, "Use the approved API format", parent_task_id=main["task"]["task_id"])
        assert child["task"]["parent_task_id"] == main["task"]["task_id"]
        _, child = change(lead_b, child, "add_dependency", prerequisite_id=main["task"]["task_id"])
        assert change(lead_b, child, "prepare")[1]["code"] == "DEPENDENCY_NOT_READY"
        assert change(lead_a, child, "cancel", reason="cross-owner cancellation attempt")[1]["code"] == "FORBIDDEN"
        _, main = change(lead_a, main, "prepare")
        assert change(reviewer_b, main, "claim", instance_id=reviewer_b[2])[1]["code"] == "FORBIDDEN"
        assert change(lead_a, main, "claim", instance_id=lead_b[2])[1]["code"] == "FORBIDDEN"
        claim_input, active = change(lead_b, main, "claim", instance_id=lead_b[2])
        assert active["task"]["status"] == "in_progress" and active["run"]["status"] == "running"
        same_receipt(active, mcp(lead_b[0], "office_change_task", claim_input))
        assert active["run"]["executor"]["user_id"] == bob["user_id"]
        assert change(lead_a, active, "progress", message="wrong actor")[1]["code"] == "FORBIDDEN"
        _, active = change(lead_b, active, "progress", message="Wrote the contract draft")
        _, waiting = change(lead_b, active, "request_help", message="Need review of error envelope", wait_seconds=30)
        assert waiting["task"]["status"] == "blocked" and waiting["run"]["status"] == "waiting_input"
        assert waiting["run"]["waiting_until"] is not None
        assert change(lead_b, waiting, "submit", summary="cannot submit while blocked")[1]["code"] == "INVALID_TRANSITION"
        _, active = change(lead_a, waiting, "resume")
        assert active["run"]["run_id"] == waiting["run"]["run_id"] and active["run"]["waiting_until"] is None
        _, submitted = change(lead_b, active, "submit", summary="Draft ready to review")
        assert submitted["task"]["status"] == "in_review" and submitted["run"]["status"] == "succeeded"
        assert change(lead_b, child, "prepare")[1]["code"] == "DEPENDENCY_NOT_READY"
        first = submitted["run"]["run_id"]
        assert change(lead_b, submitted, "review", run_id=first, decision="accept", reason="self-review")[1]["code"] == "FORBIDDEN"
        checks.extend(["scoped_accountable_actor", "instance_bound_claim_and_replay", "help_resume_and_manual_result",
                       "dependency_requires_reviewed_done"])

        _, reviewed = change(lead_a, submitted, "review", run_id=first, decision="request_changes", reason="Add one error code")
        assert reviewed["task"]["status"] == "changes_requested"
        _, ready = change(lead_a, reviewed, "prepare")
        _, active = change(lead_b, ready, "claim", instance_id=lead_b[2])
        assert active["run"]["run_id"] != first and active["run"]["attempt_number"] == 2
        _, submitted = change(lead_b, active, "submit", summary="Errors documented")
        assert change(lead_a, submitted, "review", run_id=first, decision="accept",
                      reason="stale run")[1]["code"] == "INVALID_TRANSITION"
        _, done = change(lead_a, submitted, "review", run_id=submitted["run"]["run_id"], decision="accept",
                         reason="Reviewed the error envelope")
        assert done["task"]["status"] == "done"
        _, child = change(lead_b, child, "prepare")
        assert child["task"]["dependencies"] == [{"task_id": done["task"]["task_id"], "status": "done"}]
        first_page = get(lead_a, done, run_limit=1)
        assert first_page["runs"][0]["attempt_number"] == 2 and first_page["next_run_after"]
        assert get(lead_b, done, runs_after=first_page["next_run_after"])["runs"][0]["run_id"] == first
        checks.extend(["reviewable_attempt_history", "stale_review_denied", "accepted_dependency_unblocks_child"])

        _, assigned = new_task(lead_a, "Wait for an assigned worker")
        _, assigned = change(lead_a, assigned, "prepare")
        assert change(lead_a, assigned, "assign", agent_id=lead_b[1]["agent_id"])[1]["code"] == "FORBIDDEN"
        _, assigned = change(lead_a, assigned, "assign", agent_id=worker_a[1]["agent_id"])
        assert assigned["task"]["status"] == "assigned" and get(lead_b, assigned)["runs"] == []
        assert change(lead_b, assigned, "claim", instance_id=lead_b[2])[1]["code"] == "FORBIDDEN"
        _, assigned = change(worker_a, assigned, "claim", instance_id=worker_a[2])
        _, cancelled = change(lead_a, assigned, "cancel", reason="User changed the scope")
        assert cancelled["task"]["status"] == "cancelled" and cancelled["run"]["status"] == "cancel_requested"
        assert change(worker_a, cancelled, "submit", summary="invalid")[1]["code"] == "INVALID_TRANSITION"
        _, cancelled = change(worker_a, cancelled, "acknowledge_cancel", run_id=cancelled["run"]["run_id"])
        assert cancelled["run"]["status"] == "cancelled" and cancelled["run"]["ended_at"] is not None
        checks.extend(["assignment_is_not_execution", "cancellation_waits_for_executor_ack"])

        _, broken = new_task(lead_b, "Operation that cannot complete")
        _, broken = change(lead_b, broken, "prepare")
        _, broken = change(lead_b, broken, "claim", instance_id=lead_b[2])
        _, broken = change(lead_b, broken, "fail", reason="The required service is unavailable")
        assert broken["task"]["status"] == "failed" and broken["run"]["status"] == "failed"
        _, waiting_on_predecessors = new_task(lead_a, "Wait for successful prerequisites")
        for predecessor in (broken, cancelled):
            _, waiting_on_predecessors = change(
                lead_a, waiting_on_predecessors, "add_dependency",
                prerequisite_id=predecessor["task"]["task_id"],
            )
        assert change(lead_a, waiting_on_predecessors, "prepare")[1]["code"] == "DEPENDENCY_NOT_READY"
        assert {entry["status"] for entry in get(lead_a, waiting_on_predecessors)["task"]["dependencies"]} == {
            "failed", "cancelled",
        }
        checks.append("failed_and_cancelled_dependencies_stay_blocking")

        _, opposed_a = new_task(lead_a, "A dependency race")
        _, opposed_b = new_task(lead_b, "B dependency race")
        with ThreadPoolExecutor(max_workers=2) as pool:
            a_result = pool.submit(change, lead_a, opposed_a, "add_dependency", prerequisite_id=opposed_b["task"]["task_id"])
            b_result = pool.submit(change, lead_b, opposed_b, "add_dependency", prerequisite_id=opposed_a["task"]["task_id"])
            edges = [a_result.result()[1], b_result.result()[1]]
        assert sum("task" in edge for edge in edges) == 1 and sum(edge.get("code") == "DEPENDENCY_CYCLE" for edge in edges) == 1
        _, race = new_task(lead_a, "One execution attempt at a time")
        _, race = change(lead_a, race, "prepare")
        with ThreadPoolExecutor(max_workers=2) as pool:
            claims = [pool.submit(change, lead, race, "claim", instance_id=lead[2])
                      for lead in (lead_a, lead_b)]
            outcomes = [item.result()[1] for item in claims]
        assert sum("run" in outcome for outcome in outcomes) == 1
        rejected = next(outcome for outcome in outcomes if "code" in outcome)
        assert rejected["code"] == "REVISION_CONFLICT" and rejected["current_revision"] == race["task"]["revision"] + 1
        assert len(get(lead_b, race)["runs"]) == 1
        checks.extend(["concurrent_dependency_cycle_rejected", "exactly_one_concurrent_claim"])

        _, coding = new_task(lead_a, "Needs a real checkout", execution={
            "kind": "coding", "checkout_id": str(uuid4()), "work_scope_id": str(uuid4()), "work_scope_revision": 1,
        })
        assert change(lead_a, coding, "prepare")[1]["code"] == "ACCESS_REQUEST_REQUIRED"
        _, hidden = new_task(lead_a, "Private task marker", visibility={"kind": "owner_private"})
        assert get(lead_b, hidden)["code"] == "FORBIDDEN"
        uri = f"office://workspace/{workspace}/tasks/{hidden['task']['task_id']}"
        assert "error" in call(endpoint, lead_b[0], "resources/read", {"uri": uri})[1]
        assert sql("SET ROLE cocodeop; SELECT (SELECT count(*) FROM tasks), (SELECT count(*) FROM runs)").splitlines()[-1] == "0|0"
        checks.extend(["unverified_coding_scope_denied", "task_and_run_scope_isolation"])

        cli_payload = operator / "task.json"
        cli_payload.write_text(json.dumps({
            "workspace_id": workspace, "idempotency_key": str(uuid4()), "objective": "Created through the owner CLI",
            "acceptance_criteria": ["Check the typed CLI receipt"],
        }), encoding="utf-8")
        via_cli = subprocess.run(
            [str(office), "tasks", "create", "--file", str(cli_payload), "--url", f"http://{endpoint}", "--json"],
            env=dict(os.environ, OFFICE_TOKEN=office_b), check=True, text=True, encoding="utf-8",
            capture_output=True, timeout=10,
        )
        cli_task = json.loads(via_cli.stdout)
        assert cli_task["task"]["author"] == {"kind": "human", "user_id": bob["user_id"]}
        assert get(lead_a, cli_task)["task"] == cli_task["task"]
        checks.append("cli_created_task_visible_via_mcp")

        compose("restart", "server", timeout=45)
        endpoint = compose("port", "server", "8400").stdout.strip()
        wait_for_status(endpoint, "/readyz", 200)
        arguments = ["tasks", "get", "--workspace", workspace, "--id", done["task"]["task_id"],
                     "--url", f"http://{endpoint}", "--json"]
        result = subprocess.run([str(office), *arguments], env=dict(os.environ, OFFICE_TOKEN=office_b),
                                check=True, text=True, encoding="utf-8", capture_output=True, timeout=10)
        output = json.loads(result.stdout)
        assert output["task"]["status"] == "done" and len(output["runs"]) == 2
        recorded = mcp(lead_a[0], "office_read_inbox", {"workspace_id": workspace, "cursor": main["event_cursor"]})
        assert any(e["event_type"] == "task.accepted" and e["payload"]["task_id"] == done["task"]["task_id"]
                   for e in recorded["events"])
        checks.append("task_events_and_history_survive_restart")
        live = None
        if os.getenv("COCODEOP_LIVE_TASKS") == "1":
            binary = os.environ["COCODEOP_LIVE_CODEX_BIN"]
            sys.path.insert(0, str(ROOT / "tests" / "topology"))
            from task_agents import exercise
            live = exercise(endpoint, tmp, workspace, lead_a, lead_b, binary, mcp)
        return {"checks": checks, "live": live}
    finally:
        compose("down", "--volumes", "--rmi", "local", "--remove-orphans", check=False, timeout=90)


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="cocodeop-tasks-") as root:
        print(json.dumps(run(Path(root))))
