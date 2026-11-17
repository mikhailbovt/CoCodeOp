"""Run opt-in activation through the continuously running owner-side Codex controller."""

import argparse
import contextlib
import json
import os
import sys
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from urllib.parse import urlsplit
from uuid import uuid4

from m05a_claims import recipient
from m05a_registry import RunnerHost
from m07_leases import register_runner
from m07_queue import configure
from m08a_coordinator import LAUNCH, SIGNAL
from m09_native_agents import GATEWAY_MODEL, get_attempt, install_backend
from m09_native_checkout import inference_provider
from freellmapi_live import MODEL, MODEL_DIGEST
from workspace_fixture import ROOT, workspace_fixture


@contextlib.contextmanager
def running_controller(host, config, state, address, forwards):
    stdout, stderr, pid = (f"{host.root}/activation.{key}" for key in ("out", "err", "pid"))
    command = [host.installed, "serve", "--file", config, "--state-dir", state,
               "--token-file", f"{host.root}/codex.token", "--url", f"http://{address}", "--json"]
    with ThreadPoolExecutor(max_workers=1) as pool:
        process = pool.submit(host.invoke, ["python3", "-c", LAUNCH, stdout, stderr, pid, *command],
                              forward=forwards, timeout=760)
        try:
            yield stdout
        except Exception:
            diagnostic = host.invoke(["tail", "-c", "3200", stderr]).stdout
            observed = host.invoke(["tail", "-c", "3600", stdout]).stdout
            if diagnostic or observed:
                raise AssertionError((diagnostic, observed))
            raise
        finally:
            if not process.done():
                host.invoke(["python3", "-c", SIGNAL, pid, host.installed, config, "SIGTERM"], check=True)
            completed = process.result(timeout=20)
            assert completed.returncode == 0, completed.stderr


def wait_for(predicate, seconds):
    stop = time.monotonic() + seconds
    while time.monotonic() < stop:
        value = predicate()
        if value:
            return value
        time.sleep(.32)
    raise AssertionError("The activation controller did not reach the expected state")


def verify(office, host, image, url, key, model):
    lead_a, a = office.agent(office.owner_a, "Requesting lead", "lead")
    lead_b, b = office.agent(office.owner_b, "Managed lead", "lead")
    interactive, c = office.agent(office.owner_b, "Interactive lead", "lead")
    runner, raw = register_runner(office, office.owner_b, "Recipient's local controller")
    tokens = {"codex": raw}
    if key:
        tokens["provider"] = key
    host.setup(tokens)
    assert configure(office, office.owner_b, 90).get("budget")
    coordinator_forward, address = host.connection_for_runner(office.endpoint)
    provider_forward, endpoint = host.connection_for_runner(urlsplit(url).netloc)
    while endpoint == address:
        provider_forward, endpoint = host.connection_for_runner(urlsplit(url).netloc)
    forwards = [forward for forward in (coordinator_forward, provider_forward) if forward]
    template, config, state = install_backend(office, host, office.owner_b, "codex", image, endpoint,
                                              model=model, authenticated=bool(key))
    policy = {"agent_id": b["agent_id"], "expected_revision": 0, "idempotency_key": str(uuid4()),
              "managed": {"runner_id": runner, "runtime_template_id": template, "triggers": ["question", "assignment"],
                          "debounce_seconds": 2, "cooldown_seconds": 1, "max_turns_per_thread": 1,
                          "max_turns_per_hour": 5}}
    default = office.tool(interactive, "office_get_activation_policy", agent_id=c["agent_id"])
    assert default["revision"] == 0 and default["managed"] is None
    assert office.tool(office.owner_a, "office_set_activation_policy", **policy)["code"] == "FORBIDDEN"
    assert office.tool(lead_b, "office_set_activation_policy", **policy)["code"] == "FORBIDDEN"
    enabled = office.tool(office.owner_b, "office_set_activation_policy", **policy)
    assert enabled["revision"] == 1 and enabled["managed"] == policy["managed"], enabled
    assert office.tool(office.owner_b, "office_set_activation_policy", **policy) == enabled
    assert office.tool(lead_b, "office_get_activation_policy", agent_id=b["agent_id"]) == enabled

    room = office.tool(lead_a, "office_create_channel", name="Activation test room", audience={"kind": "workspace"},
                       idempotency_key=str(uuid4()))["channel_id"]

    def ask(target, body, reply_to=None):
        request = {"idempotency_key": str(uuid4()), "channel_id": room, "recipient_user_id": target["owner_user_id"],
                   "recipient_agent_id": target["agent_id"], "body": body, "reply_to": reply_to}
        first = office.tool(lead_a, "office_ask_question", **request)
        assert first == office.tool(lead_a, "office_ask_question", **request)
        return first

    with running_controller(host, config, state, address, forwards) as output:
        wait_for(lambda: office.sql(f"SELECT runner_id FROM runner_execution_profiles WHERE runner_id='{runner}'"), 20)
        time.sleep(2.5)
        assert office.sql("SELECT count(*) FROM runs") == "0"
        pending = ask(c, "Is the interactive lead available?")
        office.tool(lead_a, "office_send_message", channel_id=room, body="Thanks for being available.",
                    mentions=[recipient(b)], idempotency_key=str(uuid4()))
        time.sleep(2.5)
        assert office.sql("SELECT count(*) FROM runs") == "0"

        markers = [f"ready-{uuid4().hex[:8]}", f"scope-{uuid4().hex[:8]}"]
        first = ask(b, f"Can you acknowledge the integration check? Answer with this exact reference {markers[0]}.")
        second = ask(b, f"Can you confirm this separate scope reference {markers[1]}? Include it in the answer.", first["question_id"])
        wait_for(lambda: office.sql("SELECT count(*) FROM activated_executions") == "1", 22)

        def completed(index):
            lines = host.invoke(["cat", output], check=True).stdout.splitlines()
            if len(lines) <= index:
                return None
            result = json.loads(lines[index])
            assert result["status"] == "completed", result
            return result

        report = wait_for(lambda: completed(0), 410)
        run, evidence, events = get_attempt(host, state, report)
        assert run["executor"] == recipient(b) and evidence["backend"] == "codex-exec.v1"
        assert sum(event.get("tool") == "office_answer_activation_question" and event.get("successful", False)
                   for event in events) >= 2
        linked = office.sql("SELECT event_id FROM activation_events "
                            f"WHERE job_id='{run['lease']['job_id']}' ORDER BY event_id").splitlines()
        assert set(linked) == {first["question_event_id"], second["question_event_id"]}
        assert office.sql("SELECT count(*) FROM activated_executions") == "1"
        answers = [office.tool(lead_a, "office_get_question", question_id=question["question_id"]) for question in (first, second)]
        assert all(answer["answer_id"] for answer in answers)
        thread = office.tool(lead_a, "office_read_thread", channel_id=room, thread_id=first["question_id"])["messages"]
        messages = {item["message_id"]: item for item in thread}
        for answer, marker in zip(answers, markers):
            item = messages[answer["answer_id"]]
            assert marker in item["body"] and item["author"]["agent_id"] == b["agent_id"]
        assert office.tool(lead_a, "office_inbox_delivery", event_id=first["question_event_id"])["state"] == "delivered"
        assert office.tool(lead_a, "office_inbox_delivery", event_id=pending["question_event_id"])["state"] == "waiting_for_agent"

        limited = ask(b, "Can you start another turn on this same thread?", first["question_id"])
        time.sleep(3.5)
        assert office.sql("SELECT count(*) FROM activated_executions") == "1"
        assert office.tool(lead_a, "office_inbox_delivery", event_id=limited["question_event_id"])["state"] == "waiting_for_agent"

        task_marker = "assigned-" + uuid4().hex[:10]
        task = office.tool(lead_b, "office_create_task", objective=f"Read the run context and then return {task_marker}.",
                           acceptance_criteria=["Return the marker"], idempotency_key=str(uuid4()))["task"]
        ready = office.tool(lead_b, "office_prepare_task", task_id=task["task_id"], expected_revision=task["revision"],
                            idempotency_key=str(uuid4()))["task"]
        assigned = office.tool(lead_b, "office_change_task", task_id=task["task_id"], expected_revision=ready["revision"],
                               change={"operation": "assign", "agent_id": b["agent_id"]}, idempotency_key=str(uuid4()))
        assert assigned.get("task", {}).get("status") == "assigned", assigned
        assigned_run, _, _ = get_attempt(host, state, wait_for(lambda: completed(1), 410))
        assert assigned_run["task_id"] == task["task_id"] and task_marker in assigned_run["summary"]

        disabled = office.tool(office.owner_b, "office_set_activation_policy", agent_id=b["agent_id"],
                               expected_revision=1, idempotency_key=str(uuid4()), managed=None)
        assert disabled["revision"] == 2 and disabled["managed"] is None
        waiting = ask(b, "This new question can wait for your next interactive turn.")
        time.sleep(3.5)
        assert office.sql("SELECT count(*) FROM activated_executions") == "2"
        assert office.tool(lead_a, "office_inbox_delivery", event_id=waiting["question_event_id"])["state"] == "waiting_for_agent"
        return {"question_run": run, "answers": answers, "assignment_run": assigned_run, "question_source_events": sorted(linked),
                "checks": ["only_owner_can_enable_managed_policy", "idle_and_ordinary_mentions_do_not_start_a_turn",
                           "interactive_recipient_keeps_waiting_inbox", "two_questions_share_one_real_managed_codex_turn",
                           "one_thread_turn_limit_leaves_later_work_in_inbox", "assignment_runs_without_manual_queue_command",
                           "disable_restores_interactive_pending_delivery"], "image": image,
                "source_lead": recipient(a), "provider_model": model,
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
    runner = ROOT / (".cocodeop-test/linux-bin/office-runner" if args.linux_vm else "target/release/office-runner")
    with tempfile.TemporaryDirectory(prefix="cocodeop-activation-") as directory:
        tmp = Path(directory)
        with workspace_fixture(tmp, "activation") as office, inference_provider(args) as (url, key, model, provider):
            host = RunnerHost(tmp, args.linux_vm, runner)
            try:
                result = verify(office, host, args.codex_image, url, key, model)
                result.update(provider=provider, independent_physical_hosts=False)
                print(json.dumps(result))
            except Exception:
                print(office.compose("logs", "--no-color", "--tail", "24", "server", "db").stdout[-9000:], file=sys.stderr)
                raise
            finally:
                host.close()
