"""Measure 64 KiB auto-claimed writes through two real workers and the durable journal."""

import argparse
import json
import os
import tempfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from uuid import uuid4

from m05a_claims import Participant, file
from m05a_registry import RunnerHost
from m07_leases import register_runner
from m07_queue import configure
from m08a_gateway_write import ALPHA, BETA, sha
from m09_native_agents import install_json, invoke
from m09_native_checkout import checkout_service, enqueue_task
from m10_activation import wait_for
from m15_benchmark import describe
from workspace_fixture import ROOT, workspace_fixture


WORKLOAD = r"""
import hashlib, http.client, json, os, socket, sys, time, traceback, uuid
from pathlib import Path

class Gateway(http.client.HTTPConnection):
    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(12)
        self.sock.connect('/run/cocodeop/mcp.sock')

path, before, rounds = sys.argv[1:]
samples = []
mcp = Gateway('localhost')
try:
    for sequence in range(int(rounds)):
        version = f'{sequence:08d}'
        data = (version * (65536 // len(version))).encode('ascii')
        after = hashlib.sha256(data).hexdigest()
        arguments = {
            'base_checkout_revision': 0, 'scope_revision': 1,
            'idempotency_key': str(uuid.uuid4()),
            'operations': [{'kind': 'replace', 'path': path, 'before_sha256': before,
                            'file': {'encoding': 'utf8', 'content': data.decode('ascii')}}],
        }
        frame = json.dumps({'jsonrpc': '2.0', 'id': sequence + 1, 'method': 'tools/call',
                            'params': {'name': 'office_apply_changes', 'arguments': arguments}})
        started = time.perf_counter()
        mcp.request('POST', '/mcp', frame, {'Content-Type': 'application/json', 'Mcp-Protocol-Version': '2025-11-25'})
        response = mcp.getresponse()
        body = response.read()
        assert response.status == 200, (response.status, body[:512])
        result = json.loads(body)
        latency = (time.perf_counter() - started) * 1000
        receipt = result.get('result', {}).get('structuredContent', {})
        assert receipt.get('claim_fence', 0) > 0, result
        assert receipt['files'][0]['after_sha256'] == after, receipt
        samples.append({'ms': latency, 'change_id': receipt['change_id'],
                        'revision': receipt['checkout_revision'], 'claim_fence': receipt['claim_fence']})
        before = after
except Exception:
    Path('/work/failure.txt').write_text(traceback.format_exc())
    raise
finally:
    mcp.close()
Path('/work/latencies.json').write_text(json.dumps({'samples': samples, 'final_sha256': before}))
Path(os.environ['COCODEOP_RESULT']).write_text(json.dumps({'kind': 'succeeded', 'summary': 'Durable writes completed'}))
"""


def register_writer(office, host, owner, checkout, side, path, before, image, rounds):
    lead = office.agent(owner, f"Benchmark lead {side}", "lead")
    scope = {"read": {"allow": [file(path)]}, "write": {"allow": [file(path)]}, "write_operations": ["replace"]}
    participant = Participant(office, checkout, lead, owner, side, scope)
    runner, token = register_runner(office, owner, f"Benchmark writer {side}")
    host.replace(f"{side}.token", token)
    host.invoke(["chmod", "600", f"{host.root}/{side}.token"], check=True)
    manifest = {"adapter_id": "publication-benchmark.v1", "image_id": image,
                "entrypoint": "/usr/local/bin/python3", "args": ["-c", WORKLOAD, path, before, str(rounds)],
                "timeout_seconds": 240, "memory_mib": 128, "cpus": 1, "pids_limit": 64,
                "run_mcp": True, "checkout_gateway": True}
    spec = install_json(host, f"{side}-manifest.json", manifest)
    digest = json.loads(host.invoke([host.installed, "runtime-manifest", "--file", spec, "--json"], check=True).stdout)
    runtime = office.tool(owner, "office_create_runtime_template", name=f"Benchmark writer {side}", kind="managed",
                          managed={"adapter_id": manifest["adapter_id"], "reservation_units": 10,
                                   "manifest_sha256": digest["manifest_sha256"]}, idempotency_key=str(uuid4()))
    config = install_json(host, f"{side}-runner.json", {"capabilities": ["linux"], "runtimes": [
        {"template_id": runtime["template_id"], "manifest": manifest}]})
    state = f"{host.root}/{side}-state"
    host.invoke(["mkdir", "-m", "700", state], check=True)
    assert configure(office, owner, 30).get("budget")

    def execute():
        def active():
            status = office.tool(lead[0], "office_get_work_scope", task_id=participant.task)
            return status.get("state") == "active"

        wait_for(active, 20)
        job = enqueue_task(office, participant, runtime["template_id"])
        forward, address = host.connection_for_runner(office.endpoint)
        result = invoke(side, host, address, forward, config, state)
        receipt = result["receipt"]
        run = receipt.get("execution", receipt)["run"]
        assert run["lease"]["job_id"] == job["job_id"] and run["lease"]["runner_id"] == runner, result
        scratch = f"{state}/run-{run['run_id']}/scratch"
        if run["status"] != "succeeded":
            raise AssertionError((run, host.invoke(["cat", f"{scratch}/failure.txt"]).stdout))
        raw = host.invoke(["cat", f"{scratch}/latencies.json"], check=True).stdout
        report = json.loads(raw)
        assert len(report["samples"]) == rounds
        assert host.invoke(["sha256sum", f"{host.root}/tree/code/{path}"], check=True).stdout.split()[0] == report["final_sha256"]
        return report

    return execute


def verify(office, host, image, rounds):
    checkout_host, token = register_runner(office, office.owner_a, "Benchmark checkout host")
    host.setup({"checkout": token})
    root = f"{host.root}/tree/code"
    records = (("a", office.owner_a, ALPHA, "alpha before"), ("b", office.owner_b, BETA, "beta before"))
    for _, _, path, content in records:
        host.replace(f"tree/code/{path}", content)
    host.invoke(["chmod", "go-w", "--", root, f"{root}/src", f"{root}/{ALPHA}", f"{root}/{BETA}"], check=True)
    project = office.tool(office.owner_a, "office_create_project", name="Checkout latency benchmark",
                          visibility={"kind": "workspace"}, idempotency_key=str(uuid4()))["project"]
    checkout = office.tool(office.owner_a, "office_request_checkout", project_id=project["project_id"],
                           host_runner_id=checkout_host, manifest={"include": [file(ALPHA), file(BETA)]},
                           idempotency_key=str(uuid4()))["checkout"]
    enrolled = host.enroll(office.endpoint, checkout, "checkout", "tree/code")
    assert enrolled.returncode == 0, enrolled.stderr
    checkout = json.loads(enrolled.stdout)
    writers = [register_writer(office, host, owner, checkout, side, path, sha(before), image, rounds)
               for side, owner, path, before in records]
    with checkout_service(host, office, checkout) as first, ThreadPoolExecutor(max_workers=2) as pool:
        futures = [pool.submit(writer) for writer in writers]
        results = [future.result() for future in futures]
    samples = [sample for result in results for sample in result["samples"]]
    count = 2 * rounds
    assert len({item["change_id"] for item in samples}) == len({item["claim_fence"] for item in samples}) == count
    assert sorted(item["revision"] for item in samples) == list(range(1, count + 1))
    with checkout_service(host, office, checkout) as reopened:
        assert reopened["epoch"] > first["epoch"]
        head = office.tool(office.owner_a, "office_get_checkout_head", checkout_id=checkout["checkout_id"])
        assert head["checkout_revision"] == count
    assert office.sql("SELECT count(*) FROM checkout_publications WHERE state='applied'") == str(count)
    receipt_times = office.sql("SELECT extract(epoch FROM (finalized_at-created_at))*1000 "
                               "FROM checkout_publications WHERE state='applied'").splitlines()
    claim_times = office.sql(
        "SELECT extract(epoch FROM (p.finalized_at-c.started_at))*1000 "
        "FROM checkout_publications p JOIN path_claims c ON "
        "(c.workspace_id,c.checkout_id,c.run_id,c.fence)=(p.workspace_id,p.checkout_id,p.run_id,p.claim_fence) "
        "WHERE p.state='applied' AND c.state='released'"
    ).splitlines()
    assert len(receipt_times) == len(claim_times) == count
    assert host.invoke(["test", "!", "-e", f"{host.root}/controller/publication-pending.json"]).returncode == 0
    storage = host.invoke(["findmnt", "--json", "--target", root, "--output", "SOURCE,FSTYPE"], check=True)
    return {"bytes_per_publication": 65536, "writers": 2, "rounds_per_writer": rounds,
            "mcp_round_trip": describe([item["ms"] for item in samples]), "recovered_revision": count,
            "queue_to_durable_receipt": describe([float(value) for value in receipt_times]),
            "auto_claim_to_durable_receipt": describe([float(value) for value in claim_times]),
            "unique_auto_claim_fences": count, "coordinator_epochs": [first["epoch"], reopened["epoch"]],
            "host": host.topology if host.guest else {"kernel": os.uname().release, "cpus": os.cpu_count()},
            "storage": json.loads(storage.stdout)["filesystems"][0],
            "schema_version": int(office.sql("SELECT max(version) FROM _sqlx_migrations"))}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", required=True)
    parser.add_argument("--rounds", type=int, default=30)
    topology = parser.add_mutually_exclusive_group()
    topology.add_argument("--vm", type=Path, default=ROOT / ".cocodeop-test/linux-vm")
    topology.add_argument("--native", action="store_true", help="Run on the local Linux host with rootless Podman")
    parser.add_argument("--runner", type=Path)
    parser.add_argument("--server-image", help="Use a previously built local Core image by exact ID")
    args = parser.parse_args()
    assert 1 <= args.rounds <= 60
    if args.native and (os.name == "nt" or os.geteuid() == 0):
        parser.error("--native needs a regular Linux user with rootless Podman")
    runner = (args.runner or ROOT / ("target/release/office-runner" if args.native
                                    else ".cocodeop-test/linux-bin/office-runner")).resolve()
    with tempfile.TemporaryDirectory(prefix="cocodeop-gateway-benchmark-") as directory:
        root = Path(directory)
        with workspace_fixture(root, "gateway-benchmark", server_image=args.server_image) as office:
            host = RunnerHost(root, not args.native, runner, vm_state=args.vm)
            try:
                print(json.dumps(verify(office, host, args.image, args.rounds)))
            finally:
                host.close()
