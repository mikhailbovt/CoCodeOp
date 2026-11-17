#!/usr/bin/env python3
"""Executable protocol fixture for GenericProcessBackend integration tests."""

import hashlib
import json
import os
import subprocess
import sys
from uuid import uuid4


def event(kind, **fields):
    print(json.dumps({"version": 1, "kind": kind, **fields}), flush=True)


packet = json.loads(sys.stdin.readline())
assert packet["version"] == 1 and packet["inference"] is None


def call(name, **arguments):
    message = {"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": arguments}}
    result = subprocess.run(packet["mcp_command"], input=json.dumps(message) + "\n", text=True,
                            capture_output=True, timeout=15, check=True)
    reply = json.loads(result.stdout)["result"]
    assert not reply["isError"], reply
    return reply["structuredContent"]


context = call("office_run_context")
assert context["run_id"] == packet["run"]["run_id"]
assert context["executor"] == packet["run"]["executor"]
assert any(event["event_type"] == "execution.start_observed" for event in packet["inbox"]["events"])
assert all(event["payload"]["task_id"] == context["task"]["task_id"] for event in packet["inbox"]["events"])
event("started", session_id=f"test-{uuid4().hex}", extra_field="accepted")
event("progress", phase="authenticated_context", tool="office_run_context", successful=True)
objective = context["task"]["objective"]
if objective.startswith("Reject "):
    event("unrecognized_terminal", summary="This event must never complete an attempt")
    sys.exit(0)
checkpoint_data = json.dumps(context, sort_keys=True).encode()
fd = os.open("/work/checkpoint.json", os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
with os.fdopen(fd, "wb") as checkpoint_file:
    checkpoint_file.write(checkpoint_data)
    checkpoint_file.flush()
    os.fsync(fd)
checkpoint = hashlib.sha256(checkpoint_data).hexdigest()
event("checkpoint", checkpoint_id=f"sha256:{checkpoint}")
event("usage", usage={"input_tokens": 50, "output_tokens": 20})
if objective.startswith("Ask "):
    if continuation := context.get("continuation"):
        assert continuation["run_id"] != context["run_id"]
        event("completed", summary=f"Continued from {continuation['checkpoint_id']} with {continuation['answer']}")
    else:
        if context["task"]["checkout_id"]:
            claim = call("office_claim_paths", workspace_id=context["workspace_id"], task_id=context["task"]["task_id"],
                         run_id=context["run_id"], scope_revision=context["scope_revision"],
                         paths=[{"kind": "file", "path": "src/app.rs"}], idempotency_key=str(uuid4()), ttl_seconds=120)
            assert claim["claim"]["run_id"] == context["run_id"], claim
            event("progress", phase="claimed_fixture_path", tool="office_claim_paths", successful=True)
        event("input_required", question="Which reference should be used to complete this task?")
    sys.exit(0)
event("artifact", attachment={"name": "run-identity", "format": "json",
                              "content": json.dumps({"run_id": context["run_id"]})})
event("completed", summary="Verified the assigned run through the bridge's MCP command")
