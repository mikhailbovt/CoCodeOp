"""Verify the diagnostic commands against the installed stack and a real runner image."""

import argparse
import json
import os
import secrets
import subprocess
import tempfile
import time
from pathlib import Path
from uuid import uuid4

from m02_bootstrap import request
from m05a_claims import Participant, file
from m05a_registry import MANIFEST, RunnerHost
from m07_leases import register_runner
from m07_queue import configure, create_template, enqueue
from m09_native_checkout import checkout_service
from m12_execution_controls import set_control
from workspace_fixture import ROOT, workspace_fixture


def verify_client(office):
    token, lead = office.agent(office.owner_a, "Diagnostics lead", "lead")
    runner_id, runner_token = register_runner(office, office.owner_a, "Diagnostics runner")
    template_id = create_template(office, office.owner_a)
    office.api("/v1/runner-scheduler/profile", runner_token, {
        "capacity": 1, "expected_revision": 0, "ttl_seconds": 120,
        "capabilities": ["linux"], "runtime_template_ids": [template_id],
    })
    args = ["doctor", "--agent", lead["agent_id"], "--runner", runner_id]
    report = office.cli(token, *args)
    checks = {item["component"]: item for item in report["checks"]}
    assert set(checks) == {"network", "database", "mcp_auth", "protocol", "runner"}, report
    assert all(item["status"] == "passed" for item in checks.values()), report
    assert lead["agent_id"] in checks["mcp_auth"]["detail"]
    assert token not in json.dumps(report)

    executable = ROOT / "target/release" / ("office.exe" if os.name == "nt" else "office")
    denied = subprocess.run(
        [str(executable), "doctor", "--url", f"http://{office.endpoint}", "--json"],
        env=dict(os.environ, OFFICE_TOKEN=secrets.token_hex(32)),
        capture_output=True, encoding="utf-8", timeout=20,
    )
    assert denied.returncode == 1, denied
    rejected = {item["component"]: item for item in json.loads(denied.stdout)["checks"]}
    assert rejected["database"]["status"] == "passed", rejected
    assert rejected["protocol"]["status"] == "failed" and "401" in rejected["protocol"]["detail"], rejected
    return ["doctor_negotiates_mcp_and_reads_the_exact_agent_and_runner", "doctor_reports_a_rejected_bearer_as_a_distinct_failure"]


def verify_runner(office, directory, vm, binary, image_id):
    _, token = register_runner(office, office.owner_a, "Owner A host")
    _, other_token = register_runner(office, office.owner_b, "Owner B host")
    host = RunnerHost(directory, vm, binary)
    try:
        host.setup({"a": token})
        host.invoke(["mkdir", "-m", "700", f"{host.root}/state"], check=True)
        manifest = {"adapter_id": "diagnostic-shell.v1", "image_id": image_id,
                    "entrypoint": "/bin/sh", "timeout_seconds": 20, "memory_mib": 128,
                    "cpus": 1, "pids_limit": 64}
        host.replace("runtime.json", json.dumps(manifest))
        host.invoke(["chmod", "600", f"{host.root}/runtime.json"], check=True)
        sha = json.loads(host.invoke([host.installed, "runtime-manifest", "--file",
                                      f"{host.root}/runtime.json", "--json"], check=True).stdout)
        template = office.tool(office.owner_a, "office_create_runtime_template", name="Owner-approved shell",
                               kind="managed", idempotency_key=str(uuid4()), managed={
                                   "adapter_id": manifest["adapter_id"], "reservation_units": 10,
                                   "manifest_sha256": sha["manifest_sha256"],
                               })
        template_id = template["template_id"]
        code, reply = request(office.endpoint, f"/v1/runner-scheduler/templates/{template_id}", token=other_token)
        assert code == 403 and reply["error"]["code"] == "FORBIDDEN", (code, reply)

        def doctor(installed):
            host.replace("runner.json", json.dumps({"runtimes": [{"template_id": template_id, "manifest": installed}]}))
            host.invoke(["chmod", "600", f"{host.root}/runner.json"], check=True)
            forward, endpoint = host.connection_for_runner(office.endpoint)
            return host.invoke([host.installed, "doctor", "--file", f"{host.root}/runner.json",
                                "--state-dir", f"{host.root}/state", "--token-file", f"{host.root}/a.token",
                                "--url", f"http://{endpoint}", "--json"], forward=forward, timeout=45)

        result = doctor(manifest)
        checks = json.loads(result.stdout)["checks"]
        assert result.returncode == 0 and len(checks) == 6, (result.stderr, checks)
        assert all(check["status"] == "passed" for check in checks), checks
        assert any("podman version" in check["detail"] for check in checks), checks
        tampered = doctor({**manifest, "timeout_seconds": 21})
        failures = [check for check in json.loads(tampered.stdout)["checks"] if check["status"] == "failed"]
        assert tampered.returncode == 1 and [check["component"] for check in failures] == [f"template/{template_id}"], failures
        return ["runner_diagnoses_owner_readable_state_and_runs_an_isolated_executable_probe",
                "runner_diagnoses_manifest_mismatch_and_peer_cannot_read_the_template"]
    finally:
        host.close()


def verify_checkout(office, directory, vm, binary, image_id):
    directory.mkdir(mode=0o700)
    host = RunnerHost(directory, vm, binary)
    runner, token = register_runner(office, office.owner_a, "Checkout doctor host")
    _, peer = register_runner(office, office.owner_b, "Checkout doctor peer")
    lead = office.agent(office.owner_a, "Checkout diagnostics lead", "lead")
    try:
        host.setup({"checkout": token})
        project = office.tool(office.owner_a, "office_create_project", name="Verified host diagnostics",
                              visibility={"kind": "workspace"}, idempotency_key=str(uuid4()))["project"]
        checkout = office.tool(office.owner_a, "office_request_checkout", project_id=project["project_id"],
                               host_runner_id=runner, manifest=MANIFEST, idempotency_key=str(uuid4()))["checkout"]
        enrolled = host.enroll(office.endpoint, checkout, "checkout", "tree/code")
        assert enrolled.returncode == 0, enrolled.stderr
        checkout = json.loads(enrolled.stdout)
        participant = Participant(office, checkout, lead, office.owner_a, "Checkout diagnostics",
                                  permissions={"read": {"allow": [file("src/app.rs")]},
                                               "write": {"allow": []}, "write_operations": []})
        status, reply = request(office.endpoint, "/v1/runner-checkouts/diagnostics", token=peer,
                                body={"checkout_id": checkout["checkout_id"], "scope_task_id": participant.task})
        assert status == 403 and reply["error"]["code"] == "FORBIDDEN", reply

        with checkout_service(host, office, checkout):
            for _ in range(45):
                scope = office.tool(lead[0], "office_get_work_scope", task_id=participant.task)
                if scope.get("state") == "active":
                    break
                time.sleep(.2)
            else:
                raise AssertionError(scope)
            def doctor(extra=None):
                forward, endpoint = host.connection_for_runner(office.endpoint)
                return host.invoke([host.installed, "checkout", "doctor", "--checkout", checkout["checkout_id"],
                                    "--root", f"{host.root}/tree/code", "--state-dir", f"{host.root}/controller",
                                    "--token-file", f"{host.root}/checkout.token", "--url", f"http://{endpoint}",
                                    "--scope-task", participant.task, "--json", *(extra or [])],
                                   forward=forward, timeout=65)
            result = doctor(["--probe-image", image_id])
            checks = {item["component"]: item for item in json.loads(result.stdout)["checks"]}
            assert result.returncode == 0 and all(item["status"] == "passed" for item in checks.values()), (checks, result.stderr)
            assert "1 visible files" in checks["scoped_read_view"]["detail"], checks
            assert "exclusive checkout lock" in checks["coordinator"]["detail"], checks
            host.replace("tree/code/src/app.rs", "pub fn owner_edit() {}\n")
            mismatch = doctor()
            problems = [entry["component"] for entry in json.loads(mismatch.stdout)["checks"]
                        if entry["status"] == "failed"]
            assert mismatch.returncode == 1 and problems == ["source_head"], (problems, mismatch.stderr)
        return ["checkout_doctor_verifies_active_epoch_journal_exact_scope_and_native_write_denial",
                "checkout_doctor_rejects_out_of_band_source_and_denies_peer_runner"]
    finally:
        host.close()


def verify_task_and_logs(office):
    lead, _ = office.agent(office.owner_a, "Task observer", "lead")
    worker, identity = office.agent(office.owner_a, "Task participant", "worker")
    runtime = create_template(office, office.owner_a)
    assert configure(office, office.owner_a, 100).get("budget")
    task = office.tool(lead, "office_create_task", objective="Observe scheduling conditions",
                       acceptance_criteria=["Explain the current wait"], idempotency_key=str(uuid4()))["task"]
    assert office.tool(lead, "office_explain_task", task_id=task["task_id"])["condition"]["kind"] == "prepare"
    ready = office.tool(lead, "office_prepare_task", task_id=task["task_id"], expected_revision=task["revision"],
                        idempotency_key=str(uuid4()))["task"]
    assert enqueue(office, lead, ready, identity, runtime).get("job")

    def explanation():
        return office.cli(lead, "task", "explain", "--workspace", office.workspace, "--id", task["task_id"])

    assert explanation()["condition"] == {"kind": "awaiting_compatible_runner", "template_id": runtime}
    _, token = register_runner(office, office.owner_a, "Task observer runner")
    office.api("/v1/runner-scheduler/profile", token, {
        "capacity": 1, "expected_revision": 0, "ttl_seconds": 120,
        "capabilities": ["linux"], "runtime_template_ids": [runtime],
    })
    assert explanation()["condition"]["kind"] == "awaiting_dispatch"
    assert set_control(office, office.owner_a, "workspace", 0, "running", 16, 0)[0] == 200
    assert explanation()["condition"] == {"kind": "capacity_limit", "scope": "workspace"}
    assert set_control(office, office.owner_a, "workspace", 1, "paused", 16, 4)[0] == 200
    assert explanation()["condition"]["kind"] == "execution_paused"
    assert set_control(office, office.owner_a, "workspace", 2, "running", 16, 4)[0] == 200
    peer = office.tool(office.owner_b, "office_explain_task", task_id=task["task_id"])
    assert peer["condition"] == {"kind": "awaiting_dispatch", "template_id": runtime}, peer

    private = office.tool(office.owner_a, "office_create_task", objective="Record a run log",
                          acceptance_criteria=["Persist a progress event"], visibility={"kind": "owner_private"},
                          idempotency_key=str(uuid4()))["task"]
    private = office.tool(office.owner_a, "office_prepare_task", task_id=private["task_id"],
                          expected_revision=private["revision"], idempotency_key=str(uuid4()))["task"]
    denied = office.tool(office.owner_b, "office_explain_task", task_id=private["task_id"])
    assert denied.get("code") == "FORBIDDEN", denied
    claimed = office.tool(office.owner_a, "office_claim_task", task_id=private["task_id"],
                          expected_revision=private["revision"], idempotency_key=str(uuid4()))
    run = claimed["run"]
    progress = office.tool(office.owner_a, "office_change_task", task_id=private["task_id"],
                           expected_revision=claimed["task"]["revision"], idempotency_key=str(uuid4()),
                           change={"operation": "progress", "message": "Persistent diagnostic progress"})
    assert progress.get("run"), progress
    options = ["runs", "logs", "--workspace", office.workspace, "--task", private["task_id"], "--run", run["run_id"], "--limit", "1"]
    first = office.cli(office.owner_a, *options)
    assert first["events"][0]["event_type"] == "task.claimed" and first["next_after"], first
    second = office.cli(office.owner_a, *options, "--after", first["next_after"])
    assert second["events"][0]["event_type"] == "task.progress", second
    assert second["events"][0]["detail"] == "Persistent diagnostic progress" and second["next_after"] is None, second
    denied, reply = request(office.endpoint, "/v1/runs/logs", token=worker, body={
        "workspace_id": office.workspace, "task_id": private["task_id"], "run_id": run["run_id"],
    })
    assert denied == 403 and reply["error"]["code"] == "FORBIDDEN", reply
    metrics_args = ["metrics", "--workspace", office.workspace, "--window-seconds", "3600"]
    own = office.cli(office.owner_a, *metrics_args)
    assert own["executions"]["queued_jobs"] == 1 and own["executions"]["running_attempts"] == 1, own
    assert own["instance_rejections"]["denied_actions"] >= 2 and own["usage"]["input_tokens"] == "0", own
    assert own["checkout"]["publication"] == {"samples": 0, "p95_ms": None}, own
    peer = office.cli(office.owner_b, *metrics_args)
    assert peer["executions"]["queued_jobs"] == 0 and peer["executions"]["running_attempts"] == 0, peer
    assert peer["instance_rejections"] is None, peer
    denied, reply = request(office.endpoint, "/v1/operations/metrics", token=lead, body={
        "workspace_id": office.workspace, "window_seconds": 3600,
    })
    assert denied == 401 and reply["error"]["code"] == "UNAUTHENTICATED", reply
    return ["task_explain_distinguishes_template_capacity_and_pause_using_current_state",
            "run_logs_paginate_durable_events_with_exact_attempt_and_task_acl",
            "metrics_show_durable_owner_counts_and_restrict_instance_counters"]


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--linux-vm", action="store_true")
    parser.add_argument("--runner", type=Path)
    parser.add_argument("--image-id")
    args = parser.parse_args()
    binary = args.runner or ROOT / (".cocodeop-test/linux-bin/office-runner" if args.linux_vm
                                  else "target/release/office-runner")
    with tempfile.TemporaryDirectory(prefix="cocodeop-doctor-") as directory:
        root = Path(directory)
        with workspace_fixture(root, "doctor") as office:
            checks = verify_client(office)
            checks.extend(verify_task_and_logs(office))
            if args.image_id:
                checks.extend(verify_runner(office, root, args.linux_vm, binary, args.image_id))
                checks.extend(verify_checkout(office, root / "checkout", args.linux_vm, binary, args.image_id))
            print(json.dumps({"checks": checks, "linux_runner": args.image_id is not None}))
