"""Use two real agent clients to publish dependent code on one shared checkout."""

import argparse
import contextlib
import hashlib
import json
import os
import sys
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from urllib.parse import urlsplit
from uuid import uuid4

from m05a_claims import Participant, directory, file, hours, recipient
from m05a_registry import MANIFEST, RunnerHost
from m07_leases import register_runner
from m07_queue import configure, enqueue
from m08a_coordinator import LAUNCH, SIGNAL
from m09_native_agents import GATEWAY_MODEL, IMAGE, gateway, get_attempt, install_backend, invoke
from freellmapi_live import MODEL, MODEL_DIGEST, request
from publication_faults import publication_proxy
from workspace_fixture import ROOT, workspace_fixture


ALPHA = "src/alpha.py"
BETA = "src/beta.py"
INITIAL = "def label(): return 'initial'"
CONSUMER = "def label(): return __import__('alpha').label() + ':consumer'"
SANDBOX_PROBE = """
test -r /task.json
test -S /run/cocodeop/mcp.sock
test ! -e /snapshot
test ! -e "$1"
test ! -e "$2"
if printf 'denied fixture' > "$1/unauthorized.txt"; then exit 13; fi
if printf 'denied fixture' > /task.json; then exit 14; fi
printf 'denied\\n'
"""


def enqueue_task(office, participant, runtime):
    ready = office.tool(participant.lead, "office_prepare_task", task_id=participant.task,
                        expected_revision=participant.task_revision, idempotency_key=str(uuid4()))["task"]
    return enqueue(office, participant.lead, ready, participant.identity, runtime)["job"]


def work(backend, write_path, new_content, dependency=None):
    prefix = "office_office_" if backend == "opencode" else "office_"
    read_tool, write_tool = prefix + "read_file", prefix + "apply_changes"
    reads = [write_path] if dependency is None else [dependency, write_path]
    steps = [
        f"Call {read_tool} with " + json.dumps({
            "path": path, "scope_revision": 1, "idempotency_key": str(uuid4()),
        }) + " and keep the real result's file.sha256."
        for path in reads
    ]
    publish = {
        "scope_revision": 1,
        "base_checkout_revision": 0,
        "claim_fence": None,
        "idempotency_key": str(uuid4()),
        "operations": [{
            "kind": "replace", "path": write_path, "before_sha256": "FROM_TOOL",
            "file": {"encoding": "utf8", "content": new_content},
        }],
        "read_dependencies": [] if dependency is None else [{"path": dependency, "sha256": "FROM_DEPENDENCY"}],
    }
    return " ".join([
        "After office_run_context, follow these steps in order. Use every idempotency_key exactly as written.",
        *steps,
        f"Call {write_tool} with this object, filling placeholders from the actual read results:",
        json.dumps(publish),
        "After a successful publication reply with the change_id and finish the turn.",
    ])


@contextlib.contextmanager
def checkout_service(host, office, checkout, *, state_name="controller", endpoint=None, commands_file=None,
                     lifetime_seconds=1200):
    state = f"{host.root}/{state_name}"
    host.invoke(["mkdir", "-p", "-m", "700", state], check=True)
    forward, address = host.connection_for_runner(endpoint or office.endpoint)
    checkout_id = checkout["checkout_id"]
    args = [host.installed, "checkout", "serve", "--checkout", checkout_id, "--root", f"{host.root}/tree/code",
            "--state-dir", state, "--token-file", f"{host.root}/checkout.token", "--url", f"http://{address}",
            "--json"]
    if commands_file:
        args.extend(["--commands-file", commands_file])
    launch = f"{state}/serve-{uuid4().hex}"
    managed = ["python3", "-c", LAUNCH, f"{launch}.stdout", f"{launch}.stderr", f"{launch}.pid", *args]
    with ThreadPoolExecutor(max_workers=1) as pool:
        future = pool.submit(host.invoke, managed, forward=forward, timeout=lifetime_seconds)
        try:
            deadline = time.monotonic() + 18
            while time.monotonic() < deadline:
                published = host.invoke(["head", "-n", "1", f"{launch}.stdout"]).stdout.strip()
                if published:
                    yield json.loads(published)
                    return
                if future.done():
                    break
                time.sleep(.25)
            raise AssertionError(host.invoke(["tail", "-c", "1800", f"{launch}.stderr"]).stdout)
        finally:
            if not future.done():
                host.invoke(["python3", "-c", SIGNAL, f"{launch}.pid", host.installed, checkout_id, "SIGTERM"], check=True)
            future.result(timeout=20)


def execute_scoped(backend, office, host, job, root, service, forwards, config, state):
    with ThreadPoolExecutor(max_workers=1) as pool:
        attempt = pool.submit(invoke, backend, host, service, forwards, config, state)
        deadline = time.monotonic() + 45
        container = None
        while time.monotonic() < deadline and not attempt.done():
            rows = office.sql(
                "SELECT e.external_id FROM execution_start_effects e JOIN execution_jobs j ON j.current_run_id=e.run_id "
                f"WHERE j.id='{job['job_id']}' AND e.state='started'"
            ).splitlines()
            if rows:
                (container,) = rows
                break
            time.sleep(.16)
        if not container and attempt.done():
            raise AssertionError(("No started container for the requested job", attempt.result()))
        assert container and len(container) == 64 and all(value in "0123456789abcdef" for value in container), container
        uid = host.invoke(["id", "-u"], check=True).stdout.strip()
        isolated = host.invoke([
            "env", "-C", "/tmp", f"XDG_RUNTIME_DIR=/run/user/{uid}",
            f"DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{uid}/bus",
            "/usr/bin/podman", "--remote=false", "exec", container, "/bin/sh", "-ec", SANDBOX_PROBE,
            "scope-check", root, f"{host.root}/provider.token",
        ], check=True)
        assert isolated.stdout.strip() == "denied"
        report = attempt.result(timeout=420)
    host.invoke(["test", "!", "-e", f"{root}/unauthorized.txt"], check=True)
    return report


def confirm_consumer(office, host, author, checkout, registration, service, forwards):
    runtime, config, state = registration
    read = {"path": BETA, "scope_revision": 1, "idempotency_key": str(uuid4())}
    task = office.tool(
        author.lead, "office_create_task",
        objective=f"Call office_read_file with {json.dumps(read)}. Report its exact file.sha256 in the final answer.",
        acceptance_criteria=["Return the hash of the consumer's published file"],
        team_id=author.team_id, visibility={"kind": "team", "team_id": author.team_id},
        execution={"kind": "coding"}, idempotency_key=str(uuid4()),
    )["task"]
    posted = office.tool(
        author.lead, "office_set_task_scope", task_id=task["task_id"], checkout_id=checkout["checkout_id"],
        worker=recipient(author.identity), grant_id=author.grant["grant_id"], expected_task_revision=task["revision"],
        expected_scope_revision=None, idempotency_key=str(uuid4()), expires_at=hours(3),
        permissions={"read": {"allow": [file(BETA)]}, "write": {"allow": []}, "write_operations": []},
    )
    assert "scope" in posted, posted
    deadline = time.monotonic() + 12
    while time.monotonic() < deadline:
        scope = office.tool(author.lead, "office_get_work_scope", task_id=task["task_id"])
        if scope.get("state") == "active":
            break
        time.sleep(.18)
    else:
        raise AssertionError("The host did not install the producer's read-only follow-up scope")
    ready = office.tool(author.lead, "office_prepare_task", task_id=task["task_id"],
                        expected_revision=posted["task_revision"], idempotency_key=str(uuid4()))["task"]
    job = enqueue(office, author.lead, ready, author.identity, runtime)["job"]
    report = invoke("codex", host, service, forwards, config, state)
    run, _, events = get_attempt(host, state, report)
    expected = hashlib.sha256(CONSUMER.encode()).hexdigest()
    assert expected in run["summary"] and run["lease"]["job_id"] == job["job_id"], run
    assert run["executor"]["agent_id"] == author.identity["agent_id"]
    assert any(event.get("tool") == "office_read_file" and event["successful"] for event in events)
    return {"execution": run, "file": BETA, "sha256": expected}


def verify(office, host, images, url, key, trace_endpoint, model):
    lead_a = office.agent(office.owner_a, "Code lead A", "lead")
    lead_b = office.agent(office.owner_b, "Code lead B", "lead")
    checkout_runner_id, checkout_token = register_runner(office, office.owner_a, "Shared source authority")
    _, codex_token = register_runner(office, office.owner_a, "Local Codex")
    _, opencode_token = register_runner(office, office.owner_b, "Local OpenCode")
    tokens = {"checkout": checkout_token, "codex": codex_token, "opencode": opencode_token}
    if key:
        tokens["provider"] = key
    host.setup(tokens)
    root = f"{host.root}/tree/code"
    host.invoke(["chmod", "755", "--", root, f"{root}/src"], check=True)
    for path in (ALPHA, BETA):
        host.replace(f"tree/code/{path}", INITIAL)
        host.invoke(["chmod", "644", f"{root}/{path}"], check=True)

    project = office.tool(office.owner_a, "office_create_project", idempotency_key=str(uuid4()),
                          name="Shared Python modules", visibility={"kind": "workspace"})["project"]
    request = office.tool(office.owner_a, "office_request_checkout", project_id=project["project_id"],
                          host_runner_id=checkout_runner_id, manifest=MANIFEST,
                          idempotency_key=str(uuid4()))["checkout"]
    enrolled = host.enroll(office.endpoint, request, "checkout", "tree/code")
    assert enrolled.returncode == 0, enrolled.stderr
    checkout = office.tool(office.owner_a, "office_get_checkout", checkout_id=request["checkout_id"])
    assert checkout["state"] == "registered"

    marker = f"source-{uuid4().hex[:12]}"
    alpha = f"def label(): return {marker!r}"
    def rules(path):
        return {"read": {"allow": [directory("src")]}, "write": {"allow": [file(path)]},
                "write_operations": ["replace"]}

    author = Participant(office, checkout, lead_a, office.owner_a, "Codex producer", rules(ALPHA),
                         objective=work("codex", ALPHA, alpha), acceptance=["Publish the producer through MCP"])
    consumer = Participant(office, checkout, lead_b, office.owner_b, "OpenCode consumer", rules(BETA),
                           objective=work("opencode", BETA, CONSUMER, dependency=ALPHA),
                           acceptance=["Read the producer and publish the dependent module through MCP"],
                           required_read_paths=[ALPHA])
    coordinator_forward, service = host.connection_for_runner(trace_endpoint)
    inference_forward, inference = host.connection_for_runner(urlsplit(url).netloc)
    while inference == service:
        inference_forward, inference = host.connection_for_runner(urlsplit(url).netloc)
    forwards = [route for route in (coordinator_forward, inference_forward) if route]
    instances = []
    registrations = {}
    with checkout_service(host, office, checkout) as authority:
        for backend, owner, actor in (("codex", office.owner_a, author), ("opencode", office.owner_b, consumer)):
            assert configure(office, owner, 45).get("budget")
            runtime, config, state = install_backend(office, host, owner, backend, images[backend], inference,
                                                     checkout=True, model=model, authenticated=bool(key))
            registrations[backend] = (runtime, config, state)
            job = enqueue_task(office, actor, runtime)
            report = execute_scoped(backend, office, host, job, root, service, forwards, config, state)
            run, evidence, events = get_attempt(host, state, report)
            assert run["lease"]["job_id"] == job["job_id"] and run["executor"]["agent_id"] == actor.identity["agent_id"]
            confirmed = {event["tool"] for event in events if event.get("successful") and "tool" in event}
            assert {"office_read_file", "office_apply_changes"} <= confirmed, events
            publications = office.sql(
                "SELECT jsonb_build_object('id',id,'author',executor_user_id,'scope_revision',scope_revision,"
                "'event_sequence',event_sequence,'read_dependencies',read_dependencies,'outcome',outcome)::text "
                f"FROM checkout_publications WHERE run_id='{run['run_id']}' AND state='applied'"
            ).splitlines()
            assert len(publications) == 1, publications
            publication = json.loads(publications[0])
            assert publication["author"] == run["executor"]["user_id"]
            assert publication["id"] in run["summary"], run["summary"]
            if backend == "opencode":
                assert publication["read_dependencies"] == [{"path": ALPHA, "sha256": hashlib.sha256(alpha.encode()).hexdigest()}]
            instances.append({"execution": run, "session_id": evidence["session_id"], "mcp_tools": sorted(confirmed),
                              "publication": publication, "native_source_write_denied": True, "image_id": images[backend]})
            path, content = (ALPHA, alpha) if backend == "codex" else (BETA, CONSUMER)
            assert host.invoke(["cat", f"{root}/{path}"], check=True).stdout == content
        assert len({item["execution"]["executor"]["user_id"] for item in instances}) == 2
        check = host.invoke(["python3", "-I", "-B", "-c", "import sys; sys.path.insert(0,sys.argv[1]); "
                             "from beta import label; assert label()==sys.argv[2]; print(label())",
                             f"{root}/src", marker + ":consumer"], check=True)
        cross_read = confirm_consumer(office, host, author, checkout, registrations["codex"], service, forwards)
        checkout_id = checkout["checkout_id"]
        return {"checkout_id": checkout_id, "coordinator_epoch": authority["epoch"],
                "worker_runs": instances, "producer_confirmed_consumer": cross_read,
                "integration_result": check.stdout.strip(), "exit_code": check.returncode,
                "linux_execution": "debian13_qemu_guest" if host.guest else "native_linux"}


def calls_observer(calls):
    def collect(path, payload, status, body):
        if path != "/run/mcp" or not payload or not body:
            return
        frame = json.loads(payload)
        if frame.get("method") != "tools/call":
            return
        try:
            response = json.loads(body)
        except ValueError:
            response = {"error": body[:400].decode(errors="replace")}
        result = response.get("result", {})
        error = response.get("error")
        if error is None and result.get("isError"):
            error = result.get("structuredContent") or result.get("content")
        calls.append({
            "tool": frame["params"]["name"], "http_status": status, "is_error": result.get("isError"),
            "error": json.dumps(error)[:600] if error else None,
            "arguments": frame["params"].get("arguments"),
        })
    return collect


@contextlib.contextmanager
def inference_provider(args):
    if args.ollama_direct:
        url = getattr(args, "ollama_url", "http://127.0.0.1:11434")
        models = request(url, "/api/tags")["models"]
        assert any(row["name"] == args.ollama_model and row["digest"] == args.ollama_digest for row in models)
        yield url, None, args.ollama_model, "Ollama " + request(url, "/api/version")["version"]
    else:
        with gateway(model=args.ollama_model, digest=args.ollama_digest, alias=args.model_alias) as (url, key):
            yield url, key, args.model_alias, IMAGE


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--linux-vm", action="store_true")
    parser.add_argument("--codex-image", required=True)
    parser.add_argument("--opencode-image", required=True)
    parser.add_argument("--ollama-model", default=MODEL)
    parser.add_argument("--ollama-digest", default=MODEL_DIGEST)
    parser.add_argument("--model-alias", default=GATEWAY_MODEL)
    parser.add_argument("--ollama-direct", action="store_true")
    args = parser.parse_args()
    assert args.linux_vm or os.name != "nt", "Use --linux-vm on Windows"
    binary = ROOT / (".cocodeop-test/linux-bin/office-runner" if args.linux_vm else "target/release/office-runner")
    with tempfile.TemporaryDirectory(prefix="cocodeop-native-checkout-") as directory_path:
        root = Path(directory_path)
        with workspace_fixture(root, "native-checkout") as office, inference_provider(args) as (url, key, model, provider):
            host = RunnerHost(root, args.linux_vm, binary)
            calls = []
            try:
                with publication_proxy(office.endpoint, observe=calls_observer(calls)) as proxy:
                    report = verify(office, host, {"codex": args.codex_image, "opencode": args.opencode_image},
                                    url, key, proxy.endpoint, model)
                    report.update(provider=provider, ollama_model=args.ollama_model, ollama_model_digest=args.ollama_digest)
                    print(json.dumps(report))
            except Exception:
                print(json.dumps(calls[:20])[:9000], file=sys.stderr)
                raise
            finally:
                host.close()
