"""Check answer and review activations in a continuously running real agent controller."""

import argparse
import json
import os
import sys
import tempfile
from pathlib import Path
from urllib.parse import urlsplit
from uuid import uuid4

from m05a_claims import recipient
from m05a_registry import RunnerHost
from m07_leases import register_runner
from m07_queue import configure
from m09_native_agents import GATEWAY_MODEL, get_attempt, install_backend
from m09_native_checkout import inference_provider
from m10_activation import running_controller, wait_for
from freellmapi_live import MODEL, MODEL_DIGEST
from workspace_fixture import ROOT, workspace_fixture


def verify(office, host, image, url, key, model):
    lead_a, a = office.agent(office.owner_a, "Source lead", "lead")
    lead_b, b = office.agent(office.owner_b, "Consumer lead", "lead")
    reviewer, review_actor = office.agent(office.owner_b, "Independent reviewer", "reviewer")
    runner, bearer = register_runner(office, office.owner_b, "Lead and reviewer controller")
    files = {"codex": bearer}
    if key:
        files["provider"] = key
    host.setup(files)
    assert configure(office, office.owner_b, 60).get("budget")
    coordinator_forward, address = host.connection_for_runner(office.endpoint)
    provider_forward, endpoint = host.connection_for_runner(urlsplit(url).netloc)
    while endpoint == address:
        provider_forward, endpoint = host.connection_for_runner(urlsplit(url).netloc)
    forwards = [forward for forward in (coordinator_forward, provider_forward) if forward]
    template, config, state = install_backend(office, host, office.owner_b, "codex", image, endpoint,
                                              model=model, authenticated=bool(key))
    for identity, trigger in ((b, "answer"), (review_actor, "review_request")):
        policy = office.tool(office.owner_b, "office_set_activation_policy", agent_id=identity["agent_id"],
                             expected_revision=0, idempotency_key=str(uuid4()), managed={
                                 "runner_id": runner, "runtime_template_id": template, "triggers": [trigger],
                                 "debounce_seconds": 1, "cooldown_seconds": 1, "max_turns_per_thread": 2,
                                 "max_turns_per_hour": 4,
                             })
        assert policy["revision"] == 1 and policy["managed"]["triggers"] == [trigger], policy

    room = office.tool(lead_b, "office_create_channel", name="Contract responses", audience={"kind": "workspace"},
                       idempotency_key=str(uuid4()))["channel_id"]
    with running_controller(host, config, state, address, forwards) as output:
        wait_for(lambda: office.sql(f"SELECT runner_id FROM runner_execution_profiles WHERE runner_id='{runner}'"), 20)
        answer_marker = "contract-" + uuid4().hex[:12]
        asked = office.tool(lead_b, "office_ask_question", channel_id=room, recipient_user_id=a["owner_user_id"],
                            recipient_agent_id=a["agent_id"], body="Which contract reference should the next turn summarize?",
                            idempotency_key=str(uuid4()))
        answered = office.tool(lead_a, "office_answer_question", question_id=asked["question_id"],
                               body=f"Use {answer_marker}. Preserve that exact reference in the next summary.",
                               idempotency_key=str(uuid4()))

        def complete(index):
            lines = host.invoke(["cat", output], check=True).stdout.splitlines()
            return json.loads(lines[index]) if len(lines) > index else None

        wait_for(lambda: office.sql("SELECT count(*) FROM activated_executions WHERE trigger='message.answer'") == "1", 22)
        follow_up, _, _ = get_attempt(host, state, wait_for(lambda: complete(0), 410))
        assert follow_up["executor"] == recipient(b) and answer_marker in follow_up["summary"], follow_up
        assert office.sql("SELECT event_id FROM activation_events "
                          f"WHERE job_id='{follow_up['lease']['job_id']}'") == answered["answer_event_id"]

        marker = "checked-" + uuid4().hex[:10]
        target = office.tool(office.owner_a, "office_create_task", objective="Review the exact agreed contract reference",
                             acceptance_criteria=[f"The current result summary contains the exact marker {marker}."],
                             review_policy={"kind": "explicit_reviewer", "reviewer": recipient(review_actor)},
                             visibility={"kind": "explicit_acl"}, grants=[{**recipient(review_actor), "right": "read"}],
                             idempotency_key=str(uuid4()))["task"]
        ready = office.tool(office.owner_a, "office_prepare_task", task_id=target["task_id"], expected_revision=target["revision"],
                            idempotency_key=str(uuid4()))["task"]
        started = office.tool(office.owner_a, "office_claim_task", task_id=target["task_id"], expected_revision=ready["revision"],
                              idempotency_key=str(uuid4()))
        submitted = office.tool(office.owner_a, "office_submit_result", task_id=target["task_id"],
                                expected_revision=started["task"]["revision"], summary=f"Verified reference {marker}.",
                                idempotency_key=str(uuid4()))
        assert submitted["task"]["status"] == "in_review", submitted
        wait_for(lambda: office.sql("SELECT count(*) FROM activated_executions WHERE trigger='task.review_requested'") == "1", 22)
        review_run, _, events = get_attempt(host, state, wait_for(lambda: complete(1), 410))
        assert review_run["executor"] == recipient(review_actor)
        successful = {event.get("tool") for event in events if event.get("successful")}
        assert {"office_get_activation_review", "office_review_activation_result"} <= successful, events
        reviewed = office.tool(reviewer, "office_get_task_result", task_id=target["task_id"])
        assert reviewed["run_id"] == started["run"]["run_id"]
        assert reviewed["review"]["reviewer"] == recipient(review_actor) and reviewed["review"]["decision"] == "accept"
        assert office.tool(office.owner_a, "office_get_task", task_id=target["task_id"])["task"]["status"] == "done"
        assert office.sql("SELECT count(*) FROM activated_executions") == "2"
        return {"answer_run": follow_up, "review_run": review_run, "reviewed_result": reviewed,
                "checks": ["answer_event_wakes_only_the_opted_in_asker", "reviewer_gets_exact_cross_owner_result_revision",
                           "native_reviewer_records_decision_under_its_own_identity"], "image": image}


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
    with tempfile.TemporaryDirectory(prefix="cocodeop-role-activation-") as directory:
        root = Path(directory)
        with workspace_fixture(root, "role-activation") as office, inference_provider(args) as (url, key, model, provider):
            host = RunnerHost(root, args.linux_vm, binary)
            try:
                report = verify(office, host, args.codex_image, url, key, model)
                report.update(provider=provider, model=model, independent_physical_hosts=False,
                              linux_execution="debian13_qemu_guest" if host.guest else "native_linux")
                print(json.dumps(report))
            except Exception:
                print(office.compose("logs", "--no-color", "--tail", "18", "db").stdout[-5000:], file=sys.stderr)
                raise
            finally:
                host.close()
