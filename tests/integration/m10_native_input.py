"""Verify a Codex model's real MCP clarification and subsequent fresh attempt."""

import argparse
import hashlib
import json
import os
import tempfile
import time
from pathlib import Path
from urllib.parse import urlsplit
from uuid import uuid4

from m05a_registry import RunnerHost
from m07_leases import register_runner
from m07_queue import configure
from m09_native_agents import GATEWAY_MODEL, get_attempt, install_backend, invoke
from m09_native_checkout import inference_provider
from freellmapi_live import MODEL, MODEL_DIGEST
from workspace_fixture import ROOT, workspace_fixture


def verify(office, host, image, url, key, model):
    owner = office.owner_b
    lead, _ = office.agent(owner, "Clarification lead", "lead")
    _, agent = office.agent(owner, "Clarification worker", "worker")
    runner, bearer = register_runner(office, owner, "Clarification process")
    tokens = {"codex": bearer}
    if key:
        tokens["provider"] = key
    host.setup(tokens)
    assert configure(office, owner, 50).get("budget")
    coordinator_forward, address = host.connection_for_runner(office.endpoint)
    provider_forward, endpoint = host.connection_for_runner(urlsplit(url).netloc)
    while endpoint == address:
        provider_forward, endpoint = host.connection_for_runner(urlsplit(url).netloc)
    forwards = [forward for forward in (coordinator_forward, provider_forward) if forward]
    template, config, state = install_backend(office, host, owner, "codex", image, endpoint,
                                              model=model, authenticated=bool(key))
    managed = office.tool(owner, "office_set_activation_policy", agent_id=agent["agent_id"],
                          expected_revision=0, idempotency_key=str(uuid4()), managed={
                              "runner_id": runner, "runtime_template_id": template, "triggers": ["assignment"],
                              "debounce_seconds": 1, "cooldown_seconds": 1, "max_turns_per_thread": 2,
                              "max_turns_per_hour": 4,
                          })
    assert managed.get("revision") == 1, managed
    question = "Which approved reference should this report use?"
    objective = ("When office_run_context returns continuation, read its answer and report that exact reference. "
                 "On the initial attempt only, call office_request_run_input with " + json.dumps({
                     "question": question, "idempotency_key": str(uuid4()),
                 }) + ". After the request is recorded, finish this turn promptly.")
    task = office.tool(lead, "office_create_task", objective=objective, acceptance_criteria=["Report the approved reference"],
                      idempotency_key=str(uuid4()))["task"]
    ready = office.tool(lead, "office_prepare_task", task_id=task["task_id"], expected_revision=task["revision"],
                       idempotency_key=str(uuid4()))["task"]
    assigned = office.tool(lead, "office_change_task", task_id=task["task_id"], expected_revision=ready["revision"],
                           change={"operation": "assign", "agent_id": agent["agent_id"]}, idempotency_key=str(uuid4()))
    assert assigned.get("task", {}).get("status") == "assigned", assigned
    # The one-shot controller can only admit work after the debounce window.
    time.sleep(1.1)
    paused_receipt = invoke("codex", host, address, forwards, config, state)
    paused = paused_receipt.get("receipt", {}).get("execution", {}).get("run", {})
    assert paused.get("status") == "paused", paused_receipt
    origin = paused["lease"]["job_id"]
    assert office.sql(f"SELECT trigger FROM activated_executions WHERE job_id='{origin}'") == "task.assignment_requested"
    assert paused["backend"]["exit_code"] == 0
    assert paused["backend"]["terminal_state"] == "input_required" and paused["summary"] == question
    scratch = f"{state}/run-{paused['run_id']}/scratch/agent"
    checkpoint = host.invoke(["cat", f"{scratch}/checkpoint.json"], check=True).stdout.encode()
    assert paused["backend"]["checkpoint_id"] == "sha256:" + hashlib.sha256(checkpoint).hexdigest()
    events = [json.loads(line) for line in host.invoke(["cat", f"{scratch}/events.jsonl"], check=True).stdout.splitlines()]
    assert any(event.get("tool") == "office_request_run_input" and event["successful"] for event in events)
    assert events[-1]["kind"] == "input_required"
    assert office.tool(owner, "office_get_execution_budget")["reserved_units"] == 0

    request = {"task_id": task["task_id"], "run_id": paused["run_id"]}
    waiting = office.tool(lead, "office_get_managed_input", **request)
    current = office.tool(lead, "office_get_task", task_id=task["task_id"])["task"]
    assert waiting["question"] == question and current["status"] == "blocked"
    answer = "approved-ref-" + uuid4().hex[:10]
    receipt = office.tool(lead, "office_answer_managed_input", **request, expected_task_revision=current["revision"],
                         idempotency_key=str(uuid4()), answer=answer)
    assert receipt.get("input", {}).get("state") == "answered" and receipt["job"]["state"] == "queued", receipt
    continued, _, _ = get_attempt(host, state, invoke("codex", host, address, forwards, config, state))
    assert continued["lease"]["job_id"] == receipt["job"]["job_id"] and continued["attempt_number"] == 2
    assert continued["backend"]["session_id"] != paused["backend"]["session_id"]
    assert continued["executor"] == paused["executor"] and answer in continued["summary"]
    assert office.sql("SELECT activation_job_id FROM managed_input_waits "
                      f"WHERE resumed_job_id='{continued['lease']['job_id']}'") == origin
    return {"paused": paused, "continued": continued, "image_id": image,
            "checks": ["model_requested_input_in_real_mcp_call", "runner_verified_a_hashed_checkpoint_before_pause",
                       "pause_released_reservation", "fresh_real_session_read_the_recorded_answer",
                       "continuation_keeps_the_original_activation_and_runner"],
            "linux_execution": "debian13_qemu_guest" if host.guest else "native_linux"}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--linux-vm", action="store_true")
    parser.add_argument("--codex-image", required=True)
    parser.add_argument("--ollama-model", default=MODEL)
    parser.add_argument("--ollama-digest", default=MODEL_DIGEST)
    parser.add_argument("--model-alias", default=GATEWAY_MODEL)
    parser.add_argument("--ollama-direct", action="store_true")
    args = parser.parse_args()
    assert args.linux_vm or os.name != "nt", "Use --linux-vm on Windows"
    binary = ROOT / (".cocodeop-test/linux-bin/office-runner" if args.linux_vm else "target/release/office-runner")
    with tempfile.TemporaryDirectory(prefix="cocodeop-native-input-") as directory:
        root = Path(directory)
        with workspace_fixture(root, "native-input") as office, inference_provider(args) as (url, key, model, provider):
            host = RunnerHost(root, args.linux_vm, binary)
            try:
                report = verify(office, host, args.codex_image, url, key, model)
                report.update(provider=provider, model=model, independent_physical_hosts=False)
                print(json.dumps(report))
            finally:
                host.close()
