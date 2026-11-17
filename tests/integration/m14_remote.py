"""Run a real worker on B against owner A's writable checkout and command host."""

import argparse
import contextlib
import hashlib
import http.client
import http.server
import json
import secrets
import tempfile
import threading
from pathlib import Path
from urllib.parse import urlsplit
from uuid import uuid4

from m05a_claims import Participant, file
from m05a_registry import RunnerHost
from m07_leases import register_runner
from m07_queue import configure
from m09_native_agents import get_attempt, install_backend, install_json
from m09_native_checkout import checkout_service, enqueue_task, execute_scoped
from m10_activation import wait_for
from workspace_fixture import ROOT, workspace_fixture


@contextlib.contextmanager
def authorizing_relay(upstream, key):
    accepted = []

    class Relay(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            if self.path not in ("/v1/chat/completions", "/v1/responses"):
                self.send_error(404)
                return
            if not secrets.compare_digest(self.headers.get("Authorization", ""), f"Bearer {key}"):
                self.send_error(401)
                return
            size = int(self.headers.get("Content-Length", "0"))
            if not 0 < size <= 1024 * 1024:
                self.send_error(413)
                return
            request = self.rfile.read(size)
            accepted.append(size)
            connection = http.client.HTTPConnection(upstream, timeout=170)
            try:
                connection.request("POST", self.path, request, {"Content-Type": "application/json"})
                with connection.getresponse() as response:
                    self.send_response(response.status)
                    self.send_header("Content-Type", response.getheader("Content-Type", "application/json"))
                    self.end_headers()
                    for line in iter(response.readline, b""):
                        self.wfile.write(line)
                        self.wfile.flush()
            finally:
                connection.close()

        def log_message(self, *_arguments):
            pass

    with http.server.ThreadingHTTPServer(("127.0.0.1", 0), Relay) as server:
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            yield f"127.0.0.1:{server.server_port}", accepted
        finally:
            server.shutdown()
            thread.join(5)


def verify(office, hosts, options):
    a, b = hosts
    backend = "codex" if options.codex_image else "opencode"
    image = options.codex_image or options.opencode_image
    prefix = "mcp__office__office_" if backend == "codex" else "office_office_"
    host_id, checkout_key = register_runner(office, office.owner_a, "A checkout")
    worker_runner, runner_key = register_runner(office, office.owner_b, f"B {backend} runtime")
    key = secrets.token_hex(32)
    a.setup({"checkout": checkout_key})
    b.setup({backend: runner_key, "provider": key})
    assert a.topology["machine_id_sha256"] != b.topology["machine_id_sha256"]

    path = "src/answer.py"
    dependency = "src/contract.txt"
    a.replace(f"tree/code/{path}", "def value():\n    return 0\n")
    contract_text = "The answer must equal 42.\n"
    a.replace(f"tree/code/{dependency}", contract_text)
    root = f"{a.root}/tree/code"
    a.invoke(["chmod", "go-w", "--", root, f"{root}/src", f"{root}/{path}"], check=True)
    project = office.tool(office.owner_a, "office_create_project", name="Remote worker proof",
                          visibility={"kind": "workspace"}, idempotency_key=str(uuid4()))["project"]
    requested = office.tool(office.owner_a, "office_request_checkout", project_id=project["project_id"],
                            host_runner_id=host_id, manifest={"include": [file(path), file(dependency)]},
                            idempotency_key=str(uuid4()))["checkout"]
    enrolled = a.enroll(office.endpoint, requested, "checkout", "tree/code")
    assert enrolled.returncode == 0, enrolled.stderr
    checkout = json.loads(enrolled.stdout)
    checkout_id = checkout["checkout_id"]
    template = office.tool(office.owner_a, "office_create_command_template", checkout_id=checkout_id,
                           expected_revision=checkout["revision"], idempotency_key=str(uuid4()), template={
                               "name": "Verify answer on A", "executable": "python3", "source_inputs": [file(path)],
                               "arguments": [{"kind": "literal", "value": "-Bc"}, {"kind": "literal", "value":
                                   "exec(open('src/answer.py').read()); assert value() == 42; print('answer verified')"}],
                               "timeout_seconds": 20, "scratch_outputs": [],
                           })
    template_id = template["template_id"]
    spec = install_json(a, "remote-command.json", template)
    described = a.invoke([a.installed, "checkout", "command-manifest", "--file", spec, "--json"], check=True)
    commands = install_json(a, "remote-host.json", {"templates": [{
        "template_id": template_id, "spec_sha256": json.loads(described.stdout)["spec_sha256"],
        "image_id": options.command_image, "entrypoint": "/usr/local/bin/python3",
        "memory_mib": 128, "cpus": 1, "pids_limit": 64,
    }]})
    lead = office.agent(office.owner_b, "Remote checkout lead", "lead")
    scope = {"read": {"allow": [file(path), file(dependency)]}, "write": {"allow": [file(path)]},
             "write_operations": ["replace"], "command_template_ids": [template_id]}
    read = {"path": path, "scope_revision": 1, "idempotency_key": str(uuid4())}
    contract = {"path": dependency, "scope_revision": 1, "idempotency_key": str(uuid4())}
    change = {"scope_revision": 1, "base_checkout_revision": 0, "idempotency_key": str(uuid4()), "path": path,
              "before_sha256": "SOURCE_HASH", "content": "def value():\n    return 42\n",
              "dependency_path": dependency, "dependency_sha256": "CONTRACT_HASH"}
    verify_command = {"template_id": template_id, "scope_revision": 1, "arguments": {}, "idempotency_key": str(uuid4())}
    objective = (f"Call {prefix}run_context. Call {prefix}read_file with {json.dumps(read)} and then {json.dumps(contract)}. "
                 f"Replace the file through {prefix}replace_text_file so Python function value() returns 42. "
                 f"Pass {json.dumps(change)} with both hashes from the two reads. "
                 "Leave claim_fence out. The publication takes an automatic claim. "
                 f"After receiving the change_id call {prefix}run_command with {json.dumps(verify_command)}. "
                 f"Use {prefix}get_command with its command_id and scope_revision 1 until finished. "
                 "Report the change_id and command_id.")
    worker = Participant(office, checkout, lead, office.owner_b, "Remote editor", scope, objective=objective,
                         required_read_paths=[dependency])
    endpoint = urlsplit(options.ollama_url)
    assert endpoint.scheme == "http" and endpoint.hostname == "127.0.0.1" and endpoint.port
    with authorizing_relay(endpoint.netloc, key) as (relay, requests):
        probe = http.client.HTTPConnection(relay, timeout=4)
        probe.request("POST", "/v1/chat/completions", "{}")
        assert probe.getresponse().status == 401
        probe.close()
        assert not requests
        service_forward, service = b.connection_for_runner(office.endpoint)
        provider_forward, provider = b.connection_for_runner(relay)
        while provider == service:
            provider_forward, provider = b.connection_for_runner(relay)
        runtime, configuration, state = install_backend(office, b, office.owner_b, backend, image,
                                                         provider, checkout=True, model=options.model)
        assert configure(office, office.owner_b, 50).get("budget")
        with checkout_service(a, office, checkout, commands_file=commands) as authority:
            def active():
                value = office.tool(lead[0], "office_get_work_scope", task_id=worker.task)
                return value if value.get("state") == "active" else None

            wait_for(active, 18)
            job = enqueue_task(office, worker, runtime)
            result = execute_scoped(backend, office, b, job, root, service,
                                    [service_forward, provider_forward], configuration, state)
            run, evidence, events = get_attempt(b, state, result)
            assert run["lease"]["runner_id"] == worker_runner and run["lease"]["job_id"] == job["job_id"]
            assert run["executor"]["agent_id"] == worker.identity["agent_id"] and requests
            tools = {event["tool"] for event in events if event.get("tool") and event.get("successful")}
            assert {"office_read_file", "office_replace_text_file", "office_run_command", "office_get_command"} <= tools, events
            attempt = run["run_id"]
            publication = json.loads(office.sql("SELECT jsonb_build_object('id',id,'host',host_runner_id,'outcome',outcome,"
                        "'read_dependencies',read_dependencies)::text "
                        f"FROM checkout_publications WHERE run_id='{attempt}' AND state='applied'"))
            command = json.loads(office.sql("SELECT jsonb_build_object('id',id,'host',host_runner_id,'outcome',outcome)::text "
                        f"FROM checkout_commands WHERE run_id='{attempt}' AND state='finished'"))
            assert publication["host"] == command["host"] == host_id
            assert publication["read_dependencies"] == [
                {"path": dependency, "sha256": hashlib.sha256(contract_text.encode()).hexdigest()},
            ]
            assert command["outcome"]["exit_code"] == 0 and command["outcome"]["stdout"].strip() == "answer verified", command
            assert command["outcome"]["snapshot"]["checkout_revision"] == publication["outcome"]["head"]["checkout_revision"]
            assert a.invoke(["test", "!", "-e", f"{b.root}/provider.token"]).returncode == 0
            assert b.invoke(["test", "!", "-e", root]).returncode == 0
            assert key not in json.dumps({"attempt": result, "publication": publication, "command": command})
            return {"checkout_id": checkout_id, "epoch": authority["epoch"], "owner_a": a.topology, "owner_b": b.topology,
                    "backend": backend, "image_id": image,
                    "run": run, "usage": evidence["usage"], "provider_requests": len(requests),
                    "credential_isolation": True, "mcp_tools": sorted(tools), "publication": publication, "command": command}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runner", type=Path, required=True)
    parser.add_argument("--server-image", required=True)
    parser.add_argument("--command-image", required=True)
    worker_image = parser.add_mutually_exclusive_group(required=True)
    worker_image.add_argument("--opencode-image")
    worker_image.add_argument("--codex-image")
    parser.add_argument("--ollama-url", default="http://127.0.0.1:11434")
    parser.add_argument("--model", required=True)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="cocodeop-remote-acceptance-") as directory:
        root = Path(directory)
        with workspace_fixture(root, "remote-worker", server_image=args.server_image) as office:
            hosts = []
            try:
                for label, vm in (("a", "linux-vm-a"), ("b", "linux-vm")):
                    guest = root / label
                    guest.mkdir(mode=0o700)
                    hosts.append(RunnerHost(guest, True, args.runner, vm_state=ROOT / ".cocodeop-test" / vm))
                print(json.dumps(verify(office, hosts, args)))
            finally:
                for host in hosts:
                    host.close()
