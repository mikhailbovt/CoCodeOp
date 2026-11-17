"""Drive one real task through the Windows and Linux lead clients."""

import json
import os
import subprocess
import sys
from pathlib import Path
from uuid import uuid4

from live_clients import guest_tool

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "protocol"))
from freellmapi_live import gateway


def call_codex(binary, endpoint, cwd, token, tool, arguments):
    prompt = (
        f"Call the CoCodeOp tool {tool} exactly once with the JSON arguments "
        f"{json.dumps(arguments)}. Report whether the tool succeeded."
    )
    process = subprocess.run(
        [binary, "--ask-for-approval", "never", "exec", "--json", "--ephemeral", "--sandbox", "read-only",
         "--skip-git-repo-check", "--ignore-user-config", "--ignore-rules", "--color", "never",
         "-c", f'mcp_servers.office.url="http://{endpoint}/mcp"',
         "-c", 'mcp_servers.office.bearer_token_env_var="OFFICE_TOKEN"',
         "-c", "mcp_servers.office.required=true", "-c", f'mcp_servers.office.enabled_tools=["{tool}"]',
         "-c", 'web_search="disabled"', prompt],
        cwd=cwd, env=dict(os.environ, OFFICE_TOKEN=token), capture_output=True,
        text=True, encoding="utf-8", errors="replace", timeout=150,
    )
    for line in reversed(process.stdout.splitlines()):
        try:
            event = json.loads(line)
        except ValueError:
            continue
        item = event.get("item", {})
        if (event.get("type") == "item.completed" and item.get("type") == "mcp_tool_call"
                and item.get("server") == "office" and item.get("tool") == tool):
            outcome = item.get("result") or {}
            result = outcome.get("structured_content") or outcome.get("structuredContent")
            if process.returncode == 0 and result and "code" not in result:
                return result
            raise RuntimeError(f"Windows Codex tool {tool} failed: {item.get('error') or outcome}")
    raise RuntimeError(f"Windows Codex did not complete {tool}: exit {process.returncode}")


def exercise(endpoint, work, workspace, lead_a, lead_b, binary, mcp):
    cwd = work / "live-run-codex"
    cwd.mkdir()
    receipts = []
    reader_b = {"user_id": lead_b[1]["owner_user_id"], "agent_id": lead_b[1]["agent_id"]}

    def windows(tool, arguments):
        value = call_codex(binary, endpoint, cwd, lead_a[0], tool, arguments)
        receipts.append({"lead": "Windows Codex", "tool": tool, "request_id": value["request_id"],
                         "status": value["task"]["status"], "event_cursor": value["event_cursor"]})
        return value

    def target(current):
        return {
            "workspace_id": workspace, "task_id": current["task"]["task_id"],
            "idempotency_key": str(uuid4()), "expected_revision": current["task"]["revision"],
        }

    draft = windows("office_create_task", {
        "workspace_id": workspace, "idempotency_key": str(uuid4()),
        "objective": "Record the error contract we will use for the demo",
        "acceptance_criteria": ["Specify a 409 JSON response for a revision conflict"],
        "review_policy": {"kind": "explicit_reviewer", "reviewer": {
            "user_id": lead_a[1]["owner_user_id"], "agent_id": lead_a[1]["agent_id"],
        }},
    })
    assert draft["task"]["status"] == "draft"
    ready = windows("office_prepare_task", target(draft))
    assert ready["task"]["status"] == "ready"
    with gateway() as (provider_url, provider_key):
        def linux(tool, arguments):
            return guest_tool(endpoint, lead_b[1], lead_b[0], tool, arguments, provider_url, provider_key)

        attached_report = linux("office_attach_instance", {"idempotency_key": str(uuid4()), "label": "Linux lead task"})
        attached = attached_report["tool_result"]
        assert attached["agent_id"] == lead_b[1]["agent_id"]
        claimed_report = linux("office_claim_task", {
            **target(ready), "instance_id": attached["instance_id"],
        })
        claimed = claimed_report["tool_result"]
        assert claimed["run"]["executor"] == reader_b and claimed["run"]["status"] == "running"
        submitted_report = linux("office_submit_result", {
            **target(claimed), "summary": 'HTTP 409 JSON body {"code":"REVISION_CONFLICT"}',
        })
        submitted = submitted_report["tool_result"]
        assert submitted["task"]["status"] == "in_review"
        for tool, report in (("office_claim_task", claimed_report), ("office_submit_result", submitted_report)):
            value = report["tool_result"]
            receipts.append({"lead": "Linux OpenCode", "tool": tool,
                             "request_id": value["request_id"], "status": value["task"]["status"],
                             "event_cursor": value["event_cursor"]})
    done = windows("office_review_task", {**target(submitted), "run_id": submitted["run"]["run_id"],
                                         "decision": "accept", "reason": "Verified the 409 error contract"})
    observed = mcp(lead_b[0], "office_get_task", {"workspace_id": workspace, "task_id": done["task"]["task_id"]})
    assert observed["task"]["status"] == "done"
    assert len(observed["runs"]) == 1 and observed["runs"][0]["run_id"] == submitted["run"]["run_id"]
    assert observed["runs"][0]["summary"] == 'HTTP 409 JSON body {"code":"REVISION_CONFLICT"}'
    return {
        "task_id": done["task"]["task_id"], "run_id": observed["runs"][0]["run_id"],
        "final_task_status": observed["task"]["status"], "stored_summary": observed["runs"][0]["summary"],
        "receipts": receipts, "windows_client": subprocess.check_output([binary, "--version"], text=True, timeout=10).strip(),
        "linux_client": attached_report["version"], "provider": attached_report["provider"],
        "provider_model": attached_report["provider_model"], "guest_kernel": attached_report["linux"],
        "guest_uid": attached_report["uid"], "independent_physical_hosts": False, "managed_worker": False,
    }
