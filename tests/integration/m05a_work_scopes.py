"""Exercise real host activation and a failed acknowledgement after durable installation."""

import hashlib
import json
import threading
from contextlib import contextmanager
from datetime import datetime, timedelta, timezone
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from uuid import uuid4

from m02_bootstrap import call, request


@contextmanager
def unavailable_ack(destination):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_GET(self):
            connection = HTTPConnection(destination, timeout=5)
            try:
                connection.request("GET", self.path, headers={"Authorization": self.headers["Authorization"]})
                response = connection.getresponse()
                self.reply(response.status, response.read())
            finally:
                connection.close()

        def do_POST(self):
            self.rfile.read(int(self.headers["Content-Length"]))
            assert self.path == "/v1/runner-checkouts/acknowledge-scope"
            self.reply(503, json.dumps({"request_id": str(uuid4()), "timestamp": datetime.now(timezone.utc).isoformat(),
                "error": {"code": "DATABASE_UNAVAILABLE", "message": "Injected acknowledgement outage", "retryable": True}}).encode())

        def reply(self, status, payload):
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

    with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            yield f"127.0.0.1:{server.server_port}"
        finally:
            server.shutdown()
            thread.join(timeout=4)


def verify(office, host, checkout, lead_a, lead_b):
    checks = []
    lead_key, lead_id = lead_b
    worker_key, worker_id = office.agent(office.owner_b, "Scoped client worker", "worker")
    second_worker, second_id = office.agent(office.owner_b, "Other client worker", "worker")
    team = office.tool(lead_key, "office_create_team", name="Client coding", idempotency_key=str(uuid4()))["team"]
    for identity in (worker_id, second_id):
        team = office.tool(lead_key, "office_set_team_member", team_id=team["team_id"], role="worker",
                           expected_revision=team["revision"], agent_id=identity["agent_id"],
                           idempotency_key=str(uuid4()))["team"]

    def directory(path):
        return {"kind": "directory", "path": path}

    def file(path):
        return {"kind": "file", "path": path}

    def recipient(identity):
        return {"user_id": identity["owner_user_id"], "agent_id": identity["agent_id"]}

    def create_task(label):
        return office.tool(lead_key, "office_create_task", idempotency_key=str(uuid4()), objective=label,
                           acceptance_criteria=["Record the client source revision"], team_id=team["team_id"],
                           execution={"kind": "coding"})["task"]

    def task_view(token, task_id):
        return office.tool(token, "office_get_task", task_id=task_id)

    def get_scope(token, task_id, revision=None):
        return office.tool(token, "office_get_work_scope", task_id=task_id, revision=revision)

    template = office.tool(office.owner_a, "office_create_command_template", checkout_id=checkout["checkout_id"],
                           expected_revision=checkout["revision"], idempotency_key=str(uuid4()),
                           template={"name": "Verify source", "executable": "cargo",
                                     "arguments": [{"kind": "literal", "value": "test"}],
                                     "source_inputs": [file("nested/lib.rs")], "scratch_outputs": [], "timeout_seconds": 60})
    assert template.get("template_id"), template
    command_template = template["template_id"]
    caps = {"read": {"allow": [directory("nested")]}, "write": {"allow": [directory("nested")]},
            "write_operations": ["create", "replace"], "command_template_ids": [command_template]}
    granted = office.tool(office.owner_a, "office_issue_checkout_grant", checkout_id=checkout["checkout_id"],
                         expected_revision=checkout["revision"], idempotency_key=str(uuid4()), recipient=recipient(lead_id),
                         expires_at=(datetime.now(timezone.utc) + timedelta(hours=8)).isoformat(),
                         permissions={**caps, "write": {"allow": [directory("nested")], "deny": [file("nested/private.rs")]}})
    assert granted.get("grant"), granted
    grant = granted["grant"]
    expires = (datetime.now(timezone.utc) + timedelta(hours=4)).isoformat()

    def propose(token, task, permissions, revision=None, key=None, target=worker_id, expiry=expires):
        return office.tool(token, "office_set_task_scope", task_id=task["task_id"], checkout_id=checkout["checkout_id"],
                           grant_id=grant["grant_id"], worker=recipient(target), idempotency_key=key or str(uuid4()),
                           expected_task_revision=task["revision"], expected_scope_revision=revision,
                           expires_at=expiry, permissions=permissions)

    def change(token, task, operation, **fields):
        return office.tool(token, "office_change_task", task_id=task["task_id"], expected_revision=task["revision"],
                           idempotency_key=str(uuid4()), change={"operation": operation, **fields})

    def advance(token, task):
        return office.tool(token, "office_prepare_task", task_id=task["task_id"], expected_revision=task["revision"],
                           idempotency_key=str(uuid4()))

    def attach(token):
        status, response = call(office.endpoint, token, "tools/call", {"name": "office_attach_instance", "arguments": {
            "idempotency_key": str(uuid4()), "label": "Scopes integration",
        }})
        assert status == 200, response
        return response["result"]["structuredContent"]["instance_id"]

    host.invoke(["mkdir", "-m", "700", "--", f"{host.root}/state", f"{host.root}/tree/code/state"], check=True)
    task = create_task("Implement client in the shared checkout")
    assert advance(lead_key, task)["code"] == "ACCESS_REQUEST_REQUIRED"
    assert propose(lead_a, task, caps)["code"] == "FORBIDDEN"
    assert propose(worker_key, task, caps)["code"] == "FORBIDDEN"
    assert propose(lead_key, task, {**caps, "write": {"allow": [directory("src")]}})["code"] == "ACCESS_REQUEST_REQUIRED"
    assert propose(lead_key, task, {**caps, "read": {"allow": []}})["code"] == "ACCESS_REQUEST_REQUIRED"
    assert propose(lead_key, task, caps, expiry=(datetime.now(timezone.utc) + timedelta(days=1)).isoformat())["code"] == "ACCESS_REQUEST_REQUIRED"
    token = str(uuid4())
    receipt = propose(lead_key, task, caps, key=token)
    assert receipt == propose(lead_key, task, caps, key=token)
    assert receipt.get("scope"), receipt
    task["revision"] = receipt["task_revision"]
    pending = receipt["scope"]
    definition = pending["definition"]
    assert pending["state"] == "pending_activation" and definition["permissions"]["write"]["deny"] == [file("nested/private.rs")]
    assert get_scope(worker_key, task["task_id"]) == pending
    assert get_scope(second_worker, task["task_id"])["code"] == "FORBIDDEN"
    assert task_view(worker_key, task["task_id"])["task"]["execution"]["work_scope_revision"] is None
    assert advance(lead_key, task)["code"] == "ACCESS_REQUEST_REQUIRED"
    assert propose(lead_key, task, caps, revision=1)["code"] == "INVALID_TRANSITION"
    checks.extend(["coding_remains_draft_until_a_verified_scope_is_applied", "narrowing_requires_current_owner_grant_task_and_scope_revisions"])

    unexpected_host = host.sync_scopes(office.endpoint, checkout, label="b")
    assert unexpected_host.returncode != 0 and "Forbidden" in unexpected_host.stderr, unexpected_host.stderr
    wrong_root = host.sync_scopes(office.endpoint, checkout, path="independent")
    assert wrong_root.returncode != 0 and "does not match" in wrong_root.stderr, wrong_root.stderr
    nested_state = host.sync_scopes(office.endpoint, checkout, state="tree/code/state")
    assert nested_state.returncode != 0 and "owner-only state directory" in nested_state.stderr, nested_state.stderr
    assert get_scope(worker_key, task["task_id"])["state"] == "pending_activation"
    checks.append("only_the_registered_runner_on_its_bound_root_can_install_permissions")

    with unavailable_ack(office.endpoint) as endpoint:
        interrupted = host.sync_scopes(endpoint, checkout)
        assert interrupted.returncode != 0 and "Injected acknowledgement outage" in interrupted.stderr, interrupted.stderr
    local_file = f"{host.root}/state/scope-{definition['scope_id']}-1.json"
    local = host.invoke(["cat", "--", local_file], check=True).stdout.encode()
    assert json.loads(local) == {"scope": definition, "policy_sequence": pending["policy_sequence"]}
    assert get_scope(worker_key, task["task_id"])["state"] == "pending_activation"
    office.restart()
    recovered = host.sync_scopes(office.endpoint, checkout)
    assert recovered.returncode == 0, recovered.stderr
    active = json.loads(recovered.stdout)[0]
    assert active["state"] == "active" and active["acknowledged_at"] is not None
    assert get_scope(worker_key, task["task_id"]) == active
    assert office.sql(f"SELECT deployment_sha256 FROM work_scope_revisions WHERE scope_id='{definition['scope_id']}' AND revision=1") == hashlib.sha256(local).hexdigest()
    assert json.loads(host.sync_scopes(office.endpoint, checkout).stdout) == []
    assert office.cli(office.owner_b, "scope", "get", "--workspace", office.workspace, "--task", task["task_id"]) == active
    checks.extend(["durable_installation_precedes_ack_and_survives_an_ack_outage", "actual_activation_digest_and_journal_match_core_after_restart"])

    current = task_view(lead_key, task["task_id"])["task"]
    assert current["execution"]["work_scope_id"] == definition["scope_id"] and current["execution"]["work_scope_revision"] == 1
    current = advance(lead_key, current)["task"]
    assert change(lead_key, current, "assign", agent_id=second_id["agent_id"])["code"] == "FORBIDDEN"
    unrelated = change(second_worker, current, "claim", instance_id=attach(second_worker))
    assert unrelated["code"] == "ACCESS_REQUEST_REQUIRED", unrelated
    assert task_view(lead_key, task["task_id"])["runs"] == []
    running = change(worker_key, current, "claim", instance_id=attach(worker_key))
    assert running.get("run"), running
    assert running["run"]["status"] == "running"
    checks.append("activated_scope_admits_only_the_selected_worker")

    narrower = {**caps, "write": {"allow": [file("nested/lib.rs")]}}
    second = propose(lead_key, running["task"], narrower, revision=1)
    assert second.get("scope"), second
    assert second["scope"]["definition"]["revision"] == 2
    before = task_view(worker_key, task["task_id"])
    assert before["task"]["execution"]["work_scope_revision"] == 1
    assert before["task"]["status"] == "in_progress" and before["runs"] == [running["run"]]
    applied = host.sync_scopes(office.endpoint, checkout)
    assert applied.returncode == 0, applied.stderr
    assert json.loads(applied.stdout)[0]["definition"]["revision"] == 2
    after = task_view(worker_key, task["task_id"])
    assert after["task"]["execution"]["work_scope_revision"] == 2
    assert after["runs"] == [running["run"]]
    assert get_scope(worker_key, task["task_id"], revision=1)["state"] == "superseded"
    progress = change(worker_key, after["task"], "progress", message="Continue on the applied revision")
    assert progress["run"]["run_id"] == running["run"]["run_id"]
    checks.append("scope_revision_changes_keep_the_same_live_attempt")

    third = propose(lead_key, progress["task"], narrower, revision=2)
    assert third.get("scope"), third
    revoked = office.tool(office.owner_a, "office_revoke_checkout_grant", grant_id=grant["grant_id"],
                          expected_revision=grant["revision"], idempotency_key=str(uuid4()))
    assert revoked["grant"]["revoked_at"]
    after = task_view(worker_key, task["task_id"])
    assert change(worker_key, after["task"], "submit", summary="Out of scope")["code"] == "ACCESS_REQUEST_REQUIRED"
    pending_after_revoke = host.sync_scopes(office.endpoint, checkout)
    assert pending_after_revoke.returncode == 0, pending_after_revoke.stderr
    assert json.loads(pending_after_revoke.stdout)[0]["state"] == "rejected"
    assert get_scope(worker_key, task["task_id"])["state"] == "rejected"
    cancelled = change(lead_key, after["task"], "cancel", reason="The grant is withdrawn")
    assert cancelled["run"]["status"] == "cancel_requested"
    assert change(worker_key, cancelled["task"], "acknowledge_cancel", run_id=running["run"]["run_id"])["run"]["status"] == "cancelled"
    assert office.sql("SET ROLE cocodeop; SELECT (SELECT count(*) FROM work_scopes), "
                      "(SELECT count(*) FROM work_scope_revisions)").splitlines()[-1] == "0|0"
    checks.append("revocation_blocks_progress_and_cannot_activate_an_inflight_revision")
    return checks
