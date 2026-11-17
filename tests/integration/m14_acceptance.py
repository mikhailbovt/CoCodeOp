"""Observe real lead models provisioning their own teams on separate Linux guests."""

import argparse
import hashlib
import json
import shlex
import subprocess
import sys
import tempfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from urllib.parse import urlsplit
from uuid import uuid4

from m05a_registry import RunnerHost
from m09_native_checkout import inference_provider
from m14_project import call_lead, completed_results, tool_result, verify as verify_project
from workspace_fixture import ROOT, workspace_fixture


MODEL = "cocodeop-qwen9b-validation:latest"
MODEL_DIGEST = "a410fda3519bb79264b1e486b06d6250571133276cc8b811e0590b4d8e62cd51"


class Lead:
    def __init__(self, host, lead, office, provider, model):
        self.host = host
        self.token, self.identity = lead
        self.office = office
        self.model = model
        self.turns = []
        self.forward_provider, self.provider = host.connection_for_runner(urlsplit(provider).netloc)
        script = (ROOT / "tests/topology/guest_lead.py").read_text(encoding="utf-8")
        host.replace("lead.py", script)

    def turn(self, label, prompt, *tools):
        host = self.host
        command = shlex.join(["sudo", "-n", "-u", host.owner, "-H", "--", "python3", f"{host.root}/lead.py"])
        service_forward, service = host.connection_for_runner(self.office.endpoint)
        while service == self.provider:
            service_forward, service = host.connection_for_runner(self.office.endpoint)
        routes = ["-R", service_forward, "-R", self.forward_provider]
        process = subprocess.run([*host.ssh, "-o", "ExitOnForwardFailure=yes", *routes,
                                  host.target, command], input=json.dumps({
            "bearer": self.token, "model": self.model, "mcp_url": f"http://{service}/mcp",
            "inference_url": f"http://{self.provider}/v1", "prompt": prompt, "tools": tools,
        }), capture_output=True, encoding="utf-8", timeout=465)
        if not process.stdout:
            raise AssertionError(f"{label} exited {process.returncode}: {process.stderr[-1200:]}")
        record = json.loads(process.stdout)
        assert process.returncode == record["exit_code"], (label, record)
        assert record["session_id"] or not record["calls"], (label, record)
        expected_machine = self.host.topology["machine_id_sha256"]
        assert record["machine_id_sha256"] == expected_machine, (record, expected_machine)
        assert record["uid"] == self.host.topology["owner_uid"], record
        self.turns.append({"step": label, **record})
        print(f"{self.host.owner} {label} session={record['session_id']} calls={len(record['calls'])} "
              f"exit={record['exit_code']}",
              file=sys.stderr, flush=True)
        return record

    def form_team(self, name, work):
        workspace = self.identity["workspace_id"]
        team_key, worker_key, member_key = (str(uuid4()) for _ in range(3))
        setup = f"""You are a CoCodeOp tech lead asked to assemble your own team for {work}.
Use only these CoCodeOp tools in order and wait for each response.
1. Call office_office_bootstrap with no arguments and confirm your identity.
2. Call office_office_create_team with {json.dumps({
    "workspace_id": workspace, "name": name, "idempotency_key": team_key,
})}.
3. Call office_office_create_managed_worker with workspace_id {workspace} and
idempotency_key {worker_key}. Choose a human-readable person's display_name yourself.
Write a short standing instructions string describing how this specialist should perform {work}.
Make the instructions reusable for planning, implementation and verification tasks. Tell the worker
to complete the exact assigned objective using the tools and paths in its active scope.
4. Call office_office_set_team_member with workspace_id {workspace}, the exact team_id and
expected_revision from step 2, the exact agent_id of worker from step 3, role worker, and
idempotency_key {member_key}. Report the team and worker name. All four real tool calls are required."""
        record = self.turn("create_team_and_worker", setup, "office_bootstrap", "office_create_team",
                           "office_create_managed_worker", "office_set_team_member")
        identity_call = record if completed_results(record, "office_bootstrap") else self.turn(
            "confirm_identity", "Call office_office_bootstrap with no arguments to verify your CoCodeOp identity.",
            "office_bootstrap",
        )
        observed = tool_result(identity_call, "office_bootstrap")
        assert observed["agent_id"] == self.identity["agent_id"] and observed["owner_user_id"] == self.identity["owner_user_id"]
        if completed_results(record, "office_create_team"):
            team = tool_result(record, "office_create_team")["team"]
        else:
            team = call_lead(self, "complete_team_creation", "office_create_team", {
                "name": name, "idempotency_key": team_key,
            })["team"]
        worker_call = record if completed_results(record, "office_create_managed_worker") else self.turn(
            "complete_worker_creation",
            f"Use office_office_create_managed_worker with workspace_id {workspace} and idempotency_key {worker_key}. "
            f"Choose a person's name and write standing instructions for {work} within the active task scope.",
            "office_create_managed_worker",
        )
        worker = tool_result(worker_call, "office_create_managed_worker")
        assert team["name"] == name and team["owner_user_id"] == self.identity["owner_user_id"], team
        assert worker["lead"]["agent_id"] == observed["agent_id"], worker
        assert worker["worker"]["user_id"] == observed["owner_user_id"], worker
        assert len(worker["profile"]["instructions"]) >= 16 and worker["profile"]["display_name"], worker
        if completed_results(record, "office_set_team_member"):
            member = tool_result(record, "office_set_team_member")
        else:
            member = call_lead(self, "complete_team_registration", "office_set_team_member", {
                "team_id": team["team_id"], "expected_revision": team["revision"],
                "agent_id": worker["worker"]["agent_id"], "role": "worker", "idempotency_key": member_key,
            })
        assert worker["worker"]["agent_id"] in json.dumps(member), member
        return {"team": team, "worker": worker}


def verify_teams(office, hosts, url, model):
    identity_a = office.agent(office.owner_a, "Backend lead", "lead")
    identity_b = office.agent(office.owner_b, "Client lead", "lead")
    leads = [Lead(host, identity, office, url, model)
             for host, identity in zip(hosts, (identity_a, identity_b))]
    with ThreadPoolExecutor(max_workers=2) as pool:
        a = pool.submit(leads[0].form_team, "Backend", "implementing a small HTTP task-list service")
        b = pool.submit(leads[1].form_team, "Client QA", "building and checking a CLI for the task-list service")
        teams = [a.result(), b.result()]
    assert len({lead.identity["owner_user_id"] for lead in leads}) == 2
    assert len({lead.host.topology["machine_id_sha256"] for lead in leads}) == 2
    for lead, roster in zip(leads, teams):
        worker = roster["worker"]
        persisted = office.tool(
            lead.token, "office_worker_status",
            worker={"by": "name", "display_name": worker["profile"]["display_name"].swapcase()},
        )
        assert persisted["worker"] == worker and persisted["tasks"] == [], persisted
    return leads, teams


def timeline(office):
    rows = office.sql(
        "SELECT jsonb_build_object('id',id,'sequence',sequence,'scope_id',scope_id,'type',event_type,"
        "'at',created_at,'sender_user_id',sender_user_id,'sender_agent_id',sender_agent_id,"
        "'refs',jsonb_strip_nulls(jsonb_build_object('task_id',payload->'task_id','run_id',payload->'run_id',"
        "'change_id',payload->'change_id','checkout_id',payload->'checkout_id','snapshot_id',payload->'snapshot_id',"
        "'status',payload->'status','revision',payload->'revision','release_id',payload->'release_id')))"
        " FROM events ORDER BY sequence"
    )
    return [json.loads(line) for line in rows.splitlines()]


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--vm-a", type=Path, default=ROOT / ".cocodeop-test/linux-vm-a")
    parser.add_argument("--vm-b", type=Path, default=ROOT / ".cocodeop-test/linux-vm")
    parser.add_argument("--runner", type=Path, default=ROOT / ".cocodeop-test/linux-bin/office-runner")
    parser.add_argument("--server-image")
    parser.add_argument("--ollama-model", default=MODEL)
    parser.add_argument("--ollama-digest", default=MODEL_DIGEST)
    parser.add_argument("--ollama-url", default="http://127.0.0.1:11434")
    parser.add_argument("--codex-image")
    parser.add_argument("--opencode-image")
    parser.add_argument("--command-image")
    args = parser.parse_args()
    args.ollama_direct = True
    with tempfile.TemporaryDirectory(prefix="cocodeop-m14-") as directory:
        root = Path(directory)
        with (workspace_fixture(root, "two-owners", server_image=args.server_image) as office,
              inference_provider(args) as (url, _, model, provider)):
            hosts = []
            try:
                for index, vm in enumerate((args.vm_a, args.vm_b)):
                    guest = root / f"guest-{index}"
                    guest.mkdir(mode=0o700)
                    host = RunnerHost(guest, True, args.runner, vm_state=vm)
                    hosts.append(host)
                    host.setup({})
                leads, teams = verify_teams(office, hosts, url, model)
                core = office.compose("ps", "-q", "server").stdout.strip()
                report = {"provider": provider, "model": model, "model_digest": args.ollama_digest,
                          "schema": int(office.sql("SELECT max(version) FROM _sqlx_migrations")),
                          "build": {"server_image": subprocess.check_output(
                                        ["docker", "inspect", "--format", "{{.Image}}", core],
                                        encoding="utf-8", timeout=15).strip(),
                                    "runner_sha256": hashlib.sha256(args.runner.read_bytes()).hexdigest()},
                          "leads": [{"identity": lead.identity, "topology": lead.host.topology,
                                     "roster": roster, "turns": lead.turns}
                                    for lead, roster in zip(leads, teams)]}
                images = {"codex": args.codex_image, "opencode": args.opencode_image, "command": args.command_image}
                if any(images.values()):
                    assert all(images.values()), "Provide all three images to run coding and contract tests"
                    report["project"] = {}
                    try:
                        verify_project(office, hosts, leads, teams, images, url, model, report["project"])
                    except Exception:
                        print(json.dumps({**report, "project_status": "failed", "timeline": timeline(office)}),
                              flush=True)
                        raise
                report["timeline"] = timeline(office)
                print(json.dumps(report))
            finally:
                for host in hosts:
                    host.close()
