"""Confirm packaged workers fail before inference or writes when run MCP is unavailable."""

import argparse
import contextlib
import http.server
import json
import tempfile
import threading
from pathlib import Path
from uuid import uuid4

from m05a_claims import Participant, file
from m05a_registry import MANIFEST, RunnerHost
from m07_leases import register_runner
from m07_queue import configure
from m09_native_agents import install_backend, invoke
from m09_native_checkout import checkout_service, enqueue_task
from m10_activation import wait_for
from publication_faults import publication_proxy
from workspace_fixture import ROOT, workspace_fixture


@contextlib.contextmanager
def inference_probe():
    calls = []

    class UnexpectedInference(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            calls.append(self.path)
            self.send_error(503)

        def log_message(self, *_):
            return

    with http.server.ThreadingHTTPServer(("127.0.0.1", 0), UnexpectedInference) as server:
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            yield f"127.0.0.1:{server.server_port}", calls
        finally:
            server.shutdown()
            thread.join(5)


def verify(office, host, images):
    runner, authority = register_runner(office, office.owner_a, "Fail-closed checkout")
    lead = office.agent(office.owner_a, "Fail-closed lead", "lead")
    tokens = {"checkout": authority}
    for backend in images:
        _, tokens[backend] = register_runner(office, office.owner_a, f"Fail-closed {backend}")
    host.setup(tokens)
    project = office.tool(office.owner_a, "office_create_project", name="Fail-closed runtime",
                          visibility={"kind": "workspace"}, idempotency_key=str(uuid4()))["project"]
    checkout = office.tool(office.owner_a, "office_request_checkout", project_id=project["project_id"],
                           host_runner_id=runner, manifest=MANIFEST,
                           idempotency_key=str(uuid4()))["checkout"]
    enrolled = host.enroll(office.endpoint, checkout, "checkout", "tree/code")
    assert enrolled.returncode == 0, enrolled.stderr
    checkout = json.loads(enrolled.stdout)
    path = "src/app.rs"
    source = f"{host.root}/tree/code/{path}"
    original = host.invoke(["sha256sum", source], check=True).stdout.split()[0]
    assert configure(office, office.owner_a, 90).get("budget")
    reports = []
    with (
        checkout_service(host, office, checkout),
        publication_proxy(office.endpoint, mode="run_mcp_unavailable") as proxy,
        inference_probe() as (target, observed),
    ):
        service_forward, service = host.connection_for_runner(proxy.endpoint)
        provider_forward, provider = host.connection_for_runner(target)
        while service == provider:
            provider_forward, provider = host.connection_for_runner(target)
        for name, image in images.items():
            participant = Participant(office, checkout, lead, office.owner_a, name,
                                      permissions={"read": {"allow": [file(path)]},
                                                   "write": {"allow": [file(path)]}, "write_operations": ["replace"]})
            template, config, state = install_backend(office, host, office.owner_a, name, image, provider,
                                                       checkout=True, model="unreachable", authenticated=False)
            def active():
                check = office.tool(lead[0], "office_get_work_scope", task_id=participant.task)
                return check if check.get("state") == "active" else None

            wait_for(active, 15)
            job = enqueue_task(office, participant, template)
            report = invoke(name, host, service, [service_forward, provider_forward], config, state)
            run = report["receipt"]["execution"]["run"]
            assert run["lease"]["job_id"] == job["job_id"] and run["status"] == "failed", run
            assert "mandatory run MCP service is unavailable" in run["summary"], run
            assert run["backend"]["session_id"] is None and run["backend"]["terminal_state"] == "failed", run
            assert host.invoke(["sha256sum", source], check=True).stdout.split()[0] == original
            assert not observed and proxy.dropped.is_set(), observed
            reports.append({"backend": name, "image": image, "run": run})
    assert office.sql("SELECT count(*) FROM checkout_publications") == "0"
    return {"checks": ["mandatory_mcp_refusal_stops_both_native_adapters_before_inference_or_source_change"],
            "checkout_id": checkout["checkout_id"], "source_sha256": original, "attempts": reports}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codex-image", required=True)
    parser.add_argument("--opencode-image", required=True)
    parser.add_argument("--vm", type=Path, default=ROOT / ".cocodeop-test/linux-vm")
    arguments = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="cocodeop-mcp-fail-closed-") as directory:
        root = Path(directory)
        with workspace_fixture(root, "mcp-fail-closed") as office:
            host = RunnerHost(root, True, ROOT / ".cocodeop-test/linux-bin/office-runner", vm_state=arguments.vm)
            try:
                print(json.dumps(verify(office, host, {"codex": arguments.codex_image, "opencode": arguments.opencode_image})))
            finally:
                host.close()
