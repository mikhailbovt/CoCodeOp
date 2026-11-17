"""A real model requests a shared-file grant and publishes after both leads vote."""

import argparse
import hashlib
import json
import os
import sys
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from urllib.parse import urlsplit
from uuid import UUID, uuid4

from m05a_claims import Participant, file, hours, recipient
from m05a_registry import MANIFEST, RunnerHost
from m07_leases import register_runner
from m07_queue import configure
from m09_native_agents import GATEWAY_MODEL, get_attempt, install_backend, invoke
from m09_native_checkout import calls_observer, checkout_service, enqueue_task, inference_provider
from freellmapi_live import MODEL, MODEL_DIGEST
from publication_faults import publication_proxy
from workspace_fixture import ROOT, workspace_fixture


CONTRACT = "src/contract.txt"
INITIAL, UPDATED = "version=1", "version=2"


def objective(backend, permissions):
    prefix = "office_office_" if backend == "opencode" else "office_"
    request = {
        "workspace_id": "FROM_CONTEXT", "task_id": "FROM_CONTEXT", "expected_task_revision": "FROM_CONTEXT",
        "expected_scope_revision": 1, "idempotency_key": str(uuid4()),
        "reason": "Publish the agreed shared contract version", "expires_at": hours(1),
        "scope_expires_at": hours(2), "permissions": permissions,
    }
    read = {"path": CONTRACT, "scope_revision": 2, "idempotency_key": str(uuid4())}
    publish = {
        "scope_revision": 2, "base_checkout_revision": 0, "claim_fence": None,
        "idempotency_key": str(uuid4()), "operations": [{
            "kind": "replace", "path": CONTRACT, "before_sha256": "FROM_READ",
            "file": {"encoding": "utf8", "content": UPDATED},
        }], "read_dependencies": [],
    }
    return " ".join([
        "Update the shared contract only after both leads approve while this attempt is running.",
        f"After the mandatory {prefix}run_context call, call {prefix}request_access with this object.",
        "Substitute the real workspace_id, task.task_id, and task.task_revision from context for FROM_CONTEXT:",
        json.dumps(request),
        "Use the request.access_request_id from the result and the same workspace_id in",
        f"{prefix}get_access_request. If requested or approved_pending_apply, call again until state approved",
        f"(at most five checks). After approval call {prefix}run_context and verify scope_revision 2.",
        f"Call {prefix}read_file with", json.dumps(read), "and retain file.sha256.",
        f"Call {prefix}apply_changes with", json.dumps(publish),
        "after substituting the exact SHA-256 for FROM_READ. Return the actual change_id and finish.",
    ])


def approve_while_running(office, host, path, leads, worker, future, state):
    deadline = time.monotonic() + 190
    while time.monotonic() < deadline and not future.done():
        requested = office.sql(f"SELECT id FROM access_requests WHERE task_id='{worker.task}' AND state='requested'")
        if requested:
            request_id = str(UUID(requested))
            break
        time.sleep(.4)
    else:
        if future.done():
            run, _, _ = get_attempt(host, state, future.result())
            raise AssertionError(("The model completed without requesting access", run))
        raise AssertionError("The model did not submit a shared access request")

    def vote(token, expected_revision):
        return office.tool(token, "office_respond_access", access_request_id=request_id, decision="approve",
                           expected_revision=expected_revision, idempotency_key=str(uuid4()))["request"]

    opened = office.tool(leads[0][0], "office_get_access_request", access_request_id=request_id)
    assert opened["requester"] == recipient(worker.identity), opened
    assert len(opened["approvers"]) == 2
    assert all(recipient(identity) in opened["approvers"] for _, identity in leads)
    first = vote(leads[0][0], opened["revision"])
    assert first["state"] == "requested" and first["issued_scope_revision"] is None
    task = office.tool(worker.lead, "office_get_task", task_id=worker.task)["task"]
    assert task["execution"]["work_scope_revision"] == 1
    assert host.invoke(["cat", path], check=True).stdout == INITIAL
    second = vote(leads[1][0], first["revision"])
    assert second["state"] == "approved_pending_apply" and second["issued_scope_revision"] == 2
    return opened, first, second


def verify(office, host, backend, image, url, key, trace_endpoint, model):
    leads = (office.agent(office.owner_a, "Source lead", "lead"),
             office.agent(office.owner_b, "Consumer lead", "lead"))
    checkout_runner, checkout_token = register_runner(office, office.owner_a, "Contract host")
    _, worker_token = register_runner(office, office.owner_b, "Contract worker host")
    tokens = {"checkout": checkout_token, backend: worker_token}
    if key:
        tokens["provider"] = key
    host.setup(tokens)
    root = f"{host.root}/tree/code"
    path = f"{root}/{CONTRACT}"
    host.replace(f"tree/code/{CONTRACT}", INITIAL)
    host.invoke(["chmod", "755", "--", root, f"{root}/src"], check=True)
    host.invoke(["chmod", "644", "--", path], check=True)

    project = office.tool(office.owner_a, "office_create_project", idempotency_key=str(uuid4()),
                          name="Shared deployment contract", visibility={"kind": "workspace"})["project"]
    staged = office.tool(office.owner_a, "office_request_checkout", project_id=project["project_id"],
                         host_runner_id=checkout_runner, manifest=MANIFEST, idempotency_key=str(uuid4()))["checkout"]
    enrolled = host.enroll(office.endpoint, staged, "checkout", "tree/code")
    assert enrolled.returncode == 0, enrolled.stderr
    checkout = office.tool(office.owner_a, "office_get_checkout", checkout_id=staged["checkout_id"])
    policy = {"kind": "shared", "approvers": [recipient(identity) for _, identity in leads]}
    area = office.tool(office.owner_a, "office_define_area", checkout_id=checkout["checkout_id"],
                       expected_checkout_revision=checkout["revision"], idempotency_key=str(uuid4()),
                       name="Joint contract", paths=file(CONTRACT), policy=policy)
    assert area.get("area"), area

    read_only = {"read": {"allow": [file(CONTRACT)]}, "write": {"allow": []}, "write_operations": []}
    expanded = {**read_only, "write": {"allow": [file(CONTRACT)]}, "write_operations": ["replace"]}
    worker = Participant(office, checkout, leads[1], office.owner_b, "Contract update", read_only,
                         envelope=expanded, objective=objective(backend, expanded),
                         acceptance=["Two lead approvals and a confirmed update in the requesting attempt"])
    assert configure(office, office.owner_b, 45).get("budget")
    forward, service = host.connection_for_runner(trace_endpoint)
    provider_forward, endpoint = host.connection_for_runner(urlsplit(url).netloc)
    while endpoint == service:
        provider_forward, endpoint = host.connection_for_runner(urlsplit(url).netloc)
    forwards = [route for route in (forward, provider_forward) if route]
    runtime, config, state = install_backend(office, host, office.owner_b, backend, image, endpoint,
                                             checkout=True, model=model, authenticated=bool(key))
    with checkout_service(host, office, checkout), ThreadPoolExecutor(max_workers=1) as pool:
        job = enqueue_task(office, worker, runtime)
        future = pool.submit(invoke, backend, host, service, forwards, config, state)
        opened, first, second = approve_while_running(office, host, path, leads, worker, future, state)
        run, _, events = get_attempt(host, state, future.result(timeout=420))
        assert run["lease"]["job_id"] == job["job_id"] and run["executor"] == recipient(worker.identity)
        assert run["run_id"] == opened["run_id"]
        approved = office.tool(leads[1][0], "office_get_access_request", access_request_id=opened["access_request_id"])
        assert approved["state"] == "approved" and approved["issued_scope_revision"] == 2
        tools = [event["tool"] for event in events if event.get("successful") and "tool" in event]
        sequence = ["office_run_context", "office_request_access", "office_get_access_request",
                    "office_run_context", "office_read_file", "office_apply_changes"]
        for name in tools:
            if sequence and name == sequence[0]:
                sequence.pop(0)
        assert not sequence, tools
        published = office.sql(
            "SELECT jsonb_build_object('id',id,'author',executor_user_id,'scope_revision',scope_revision,"
            "'outcome',outcome)::text "
            f"FROM checkout_publications WHERE run_id='{run['run_id']}' AND state='applied'"
        ).splitlines()
        assert len(published) == 1, published
        publication = json.loads(published[0])
        assert publication["scope_revision"] == 2 and publication["author"] == run["executor"]["user_id"]
        assert publication["id"] in run["summary"]
        assert publication["outcome"]["files"][0]["after_sha256"] == hashlib.sha256(UPDATED.encode()).hexdigest()
        assert host.invoke(["cat", path], check=True).stdout == UPDATED
        return {"checkout_id": checkout["checkout_id"], "run": run, "first_vote": first["state"],
                "second_vote": second["state"], "approval": approved, "publication": publication,
                "mcp_tools": tools, "image_id": image, "linux_execution": "debian13_qemu_guest" if host.guest else "native_linux",
                "lead_votes": "integration_driver_with_distinct_lead_credentials"}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--linux-vm", action="store_true")
    parser.add_argument("--backend", choices=("codex", "opencode"), required=True)
    parser.add_argument("--image-id", required=True)
    parser.add_argument("--ollama-model", default=MODEL)
    parser.add_argument("--ollama-digest", default=MODEL_DIGEST)
    parser.add_argument("--model-alias", default=GATEWAY_MODEL)
    parser.add_argument("--ollama-direct", action="store_true")
    args = parser.parse_args()
    assert args.linux_vm or os.name != "nt", "Use --linux-vm on Windows"
    runner = ROOT / (".cocodeop-test/linux-bin/office-runner" if args.linux_vm else "target/release/office-runner")
    with tempfile.TemporaryDirectory(prefix="cocodeop-native-access-") as directory:
        root = Path(directory)
        with workspace_fixture(root, "native-access") as office, inference_provider(args) as (url, key, model, provider):
            host = RunnerHost(root, args.linux_vm, runner)
            calls = []
            try:
                with publication_proxy(office.endpoint, observe=calls_observer(calls)) as proxy:
                    report = verify(office, host, args.backend, args.image_id, url, key, proxy.endpoint, model)
                    report.update(provider=provider, ollama_model=args.ollama_model, ollama_model_digest=args.ollama_digest)
                    print(json.dumps(report))
            except Exception:
                print(json.dumps(calls[:25])[:12000], file=sys.stderr)
                raise
            finally:
                host.close()
