"""Verify a managed worker MCP session against actual isolated runner identities and PostgreSQL."""

import argparse
import json
import os
import secrets
import tempfile
from pathlib import Path
from uuid import uuid4

from m02_bootstrap import request
from m05a_registry import RunnerHost
from m07_leases import RunnerControl, register_runner
from m07_queue import configure, create_template, enqueue
from m07_start_effects import PROCESS, proof
from workspace_fixture import ROOT, digest, workspace_fixture


def verify(office, tmp, guest, binary):
    workers = {}
    runner_tokens = {}
    for label, owner in {"a": office.owner_a, "b": office.owner_b}.items():
        lead, _ = office.agent(owner, f"Lead {label}", "lead")
        token, identity = office.agent(owner, f"Worker {label}", "worker")
        assert configure(office, owner, 50).get("budget")
        runtime = create_template(office, owner, units=10)
        _, runner_token = register_runner(office, owner, f"Run credential fixture {label}")
        runner_tokens[label] = runner_token
        task = office.tool(lead, "office_create_task", objective=f"Handle private task {label}",
                           acceptance_criteria=["Return only the task assigned to this run"],
                           visibility={"kind": "explicit_acl"}, grants=[{
                               "user_id": identity["owner_user_id"], "agent_id": identity["agent_id"], "right": "read",
                           }], idempotency_key=str(uuid4()))["task"]
        unrelated = office.tool(lead, "office_create_task", objective=f"Visible unrelated task {label}",
                                acceptance_criteria=["Keep separate work separate"], visibility={"kind": "workspace"},
                                idempotency_key=str(uuid4()))["task"]
        ready = office.tool(lead, "office_prepare_task", task_id=task["task_id"], expected_revision=task["revision"],
                            idempotency_key=str(uuid4()))["task"]
        workers[label] = {"lead": lead, "agent_token": token, "identity": identity,
                          "runtime": runtime, "task": task, "unrelated": unrelated,
                          "job": enqueue(office, lead, ready, identity, runtime)["job"]}

    host = RunnerHost(tmp, guest, binary)
    host.setup(runner_tokens)
    ctl = RunnerControl(office, host)

    def mcp(token, method, params, path="/run/mcp"):
        return request(office.endpoint, path, token=token, rpc={"jsonrpc": "2.0", "id": 1, "method": method, "params": params})

    def context(token):
        status, result = mcp(token, "tools/call", {"name": "office_run_context", "arguments": {}})
        assert status == 200, (status, result)
        return result["result"]["structuredContent"]

    processes = []
    try:
        for label, fixture in workers.items():
            assert ctl.advertise(label, 1, [fixture["runtime"]])
            run = ctl.claim(label)["run"]
            assert run["lease"]["job_id"] == fixture["job"]["job_id"]
            start = ctl.send(label, "authorize-start", {"lease": proof(run), "idempotency_key": str(uuid4())})
            effect = {"lease": proof(run), "effect_id": start["effect"]["effect_id"]}
            raw = "crun_" + secrets.token_hex(32)
            issue = {"effect": effect, "credential_digest": digest(raw)}
            peer = "b" if label == "a" else "a"
            assert ctl.send(peer, "issue-run-credential", issue, check=False).returncode != 0
            issued = ctl.send(label, "issue-run-credential", issue)
            assert issued["run_id"] == run["run_id"] and "credential_digest" not in issued
            assert ctl.send(label, "issue-run-credential", issue) == issued
            replacement = {**issue, "credential_digest": digest("crun_" + secrets.token_hex(32))}
            assert "IdempotencyConflict" in ctl.send(label, "issue-run-credential", replacement, check=False).stderr
            assert mcp(raw, "tools/list", {})[0] == 401
            process = json.loads(host.invoke(["python3", "-c", PROCESS, "launch"], check=True).stdout)
            processes.append(process)
            assert ctl.send(label, "observe-start", {"effect": effect, "external_id": process["id"]})["run"]["status"] == "running"
            fixture.update(run=run, run_token=raw, process=process, effect=effect)

        a, b = workers["a"], workers["b"]
        for fixture in workers.values():
            token = fixture["run_token"]
            status, result = mcp(token, "tools/list", {})
            assert status == 200 and {tool["name"] for tool in result["result"]["tools"]} == {
                "office_run_context", "office_run_work_context", "office_request_run_input",
            }
            found = context(token)
            assert found["run_id"] == fixture["run"]["run_id"] and found["task"]["task_id"] == fixture["task"]["task_id"]
            assert found["executor"]["agent_id"] == fixture["identity"]["agent_id"]
            assert found["task"]["objective"] == fixture["task"]["objective"]
            agent_inbox = office.tool(fixture["agent_token"], "office_read_inbox")["events"]
            assert any(e["payload"].get("task_id") == fixture["unrelated"]["task_id"] for e in agent_inbox)
            status, reply = mcp(token, "tools/call", {"name": "office_run_work_context", "arguments": {"limit": 2}})
            work = reply["result"]["structuredContent"]
            assert status == 200 and work["run"] == found
            assert len(work["inbox"]["events"]) == 2 and work["inbox"]["has_more"]
            events = work["inbox"]["events"]
            status, reply = mcp(token, "tools/call", {"name": "office_run_work_context", "arguments": {
                "inbox_cursor": work["inbox"]["next_cursor"], "limit": 25,
            }})
            assert status == 200
            next_inbox = reply["result"]["structuredContent"]["inbox"]
            assert next_inbox["events"] and not next_inbox["has_more"]
            events += next_inbox["events"]
            assert len({event["event_id"] for event in events}) == len(events)
            assert all(e["payload"]["task_id"] == found["task"]["task_id"] for e in events)
            assert mcp(token, "tools/list", {}, path="/mcp")[0] == 401
            assert request(office.endpoint, "/v1/me", token=token)[0] == 401
            assert request(office.endpoint, "/v1/runner-scheduler/claim", token=token,
                           body={"claim_key": str(uuid4())})[0] == 401
            assert mcp(fixture["agent_token"], "tools/list", {})[0] == 401
            status, denied = mcp(token, "tools/call", {"name": "office_create_agent", "arguments": {}})
            assert status == 200 and ("error" in denied or denied["result"]["isError"])
        checks = ["two_simultaneous_run_bearers_have_only_their_own_context_and_no_human_agent_or_runner_authority",
                  "paginated_run_inbox_excludes_unrelated_events_visible_to_the_interactive_agent"]

        for label, fixture in workers.items():
            ctl.heartbeat(label, fixture["run"])
        office.restart()
        assert context(a["run_token"])["run_id"] == a["run"]["run_id"]
        assert context(b["run_token"])["run_id"] == b["run"]["run_id"]
        assert office.sql("SET ROLE cocodeop; SELECT count(*) FROM run_credentials").splitlines()[-1] == "0"
        checks.append("credential_binding_survives_restart_and_database_rls_hides_digests")

        current = office.tool(a["lead"], "office_get_task", task_id=a["task"]["task_id"])["task"]
        cancelled = office.tool(a["lead"], "office_change_task", task_id=current["task_id"], expected_revision=current["revision"],
                                idempotency_key=str(uuid4()), change={"operation": "cancel", "reason": "Close this session"})
        assert cancelled["run"]["status"] == "cancel_requested"
        assert mcp(a["run_token"], "tools/list", {})[0] == 401 and context(b["run_token"])["run_id"] == b["run"]["run_id"]
        checks.append("cancellation_ends_worker_mcp_access_immediately_without_affecting_the_peer")

        status, reply = request(office.endpoint, "/v1/runners/self", token=runner_tokens["b"])
        assert status == 200
        owning_credential = reply["result"]["credential_id"]
        revoked = office.api(f"/v1/credentials/{owning_credential}/revoke", office.owner_b,
                             {"idempotency_key": str(uuid4())})
        assert revoked
        assert mcp(b["run_token"], "tools/list", {})[0] == 401
        checks.append("revoking_the_issuing_runner_credential_ends_the_worker_session_before_expiry")
        return checks
    finally:
        try:
            for process in processes:
                host.invoke(["python3", "-c", PROCESS, "stop", str(process["pid"]), process["token"]], check=True)
        finally:
            host.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--linux-vm", action="store_true")
    parser.add_argument("--runner", type=Path)
    args = parser.parse_args()
    assert args.linux_vm or os.name != "nt", "Use --linux-vm on Windows"
    binary = (args.runner or ROOT / (".cocodeop-test/linux-bin/office-runner" if args.linux_vm
                                     else "target/release/office-runner")).resolve()
    assert binary.is_file(), f"Build the Linux runner: {binary}"
    with tempfile.TemporaryDirectory(prefix="cocodeop-run-credentials-") as directory:
        tmp = Path(directory)
        with workspace_fixture(tmp, "worker-sessions") as office:
            print(json.dumps({"checks": verify(office, tmp, args.linux_vm, binary),
                              "linux_execution": "debian13_qemu_guest" if args.linux_vm else "native_linux",
                              "independent_physical_hosts": False}))
