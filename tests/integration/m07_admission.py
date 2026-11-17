"""Check real runner advertisements and owner-bound scheduling limits."""

import argparse
import json
import os
import secrets
import tempfile
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timedelta, timezone
from pathlib import Path
from uuid import uuid4

from m02_bootstrap import call
from m05a_registry import RunnerHost
from workspace_fixture import ROOT, digest, workspace_fixture


def verify(office, tmp, guest, binary):
    lead, lead_id = office.agent(office.owner_a, "Owner A lead", "lead")
    other, _ = office.agent(office.owner_b, "Owner B lead", "lead")
    worker, worker_id = office.agent(office.owner_a, "Managed worker", "worker")

    def configure(owner, limit, expected=0, key=None):
        return office.tool(owner, "office_set_execution_budget", expected_revision=expected,
                           limit_units=limit, idempotency_key=key or str(uuid4()))

    assert office.cli(office.owner_a, "scheduler", "budget", "--workspace", office.workspace) is None
    assert configure(lead, 100)["code"] == "FORBIDDEN"
    assert configure(office.owner_a, -1)["code"] == "INVALID_INPUT"
    key = str(uuid4())
    started = configure(office.owner_a, 100, key=key)
    assert started.get("budget"), started
    assert started == configure(office.owner_a, 100, key=key)
    assert configure(office.owner_a, 101, key=key)["code"] == "IDEMPOTENCY_CONFLICT"
    budget_b = configure(office.owner_b, 60)["budget"]
    assert budget_b["budget_id"] != started["budget"]["budget_id"]
    with ThreadPoolExecutor(max_workers=2) as pool:
        results = list(pool.map(lambda cap: configure(office.owner_a, cap, expected=1), [120, 90]))
    assert sum("budget" in result for result in results) == 1
    assert sum(result.get("code") == "REVISION_CONFLICT" for result in results) == 1
    current = next(result["budget"] for result in results if "budget" in result)
    assert current["revision"] == 2 and current["charged_units"] == current["reserved_units"] == 0
    assert office.cli(office.owner_b, "scheduler", "budget", "--workspace", office.workspace) == budget_b
    checks = ["owner_budgets_are_private_distinct_revisioned_and_idempotent"]

    profile_spec = {"adapter_id": "openai-compat.v1", "required_capabilities": ["linux", "network-egress"],
                    "reservation_units": 20}

    def template(token, name, kind="managed", managed=profile_spec):
        return office.tool(token, "office_create_runtime_template", kind=kind, managed=managed,
                           idempotency_key=str(uuid4()), name=name)

    assert template(lead, "Managed request")["code"] == "FORBIDDEN"
    assert template(office.owner_a, "Incomplete template", "managed", None)["code"] == "INVALID_INPUT"
    assert template(office.owner_a, "Wrong role", "interactive")["code"] == "INVALID_INPUT"
    assert template(office.owner_a, "Bad identifier", "managed", {**profile_spec, "adapter_id": "../shell"})["code"] == "INVALID_INPUT"
    configured = template(office.owner_a, "FreeLLM-compatible managed template")
    runtime = configured["template_id"]
    assert configured["managed"] == profile_spec
    other_runtime = template(office.owner_b, "Owner B runtime")["template_id"]
    interactive = template(office.owner_a, "Interactive runtime", "interactive", None)["template_id"]
    checks.append("human_owners_register_validated_adapter_profiles_with_no_provider_secrets")

    def register(owner, label):
        bearer = secrets.token_hex(32)
        result = office.api(f"/v1/workspaces/{office.workspace}/runners", owner, {
            "name": label, "idempotency_key": str(uuid4()), "credential_digest": digest(bearer),
        })
        return result["runner_id"], bearer

    runner_a, token_a = register(office.owner_a, "Owner A runner")
    runner_b, token_b = register(office.owner_b, "Owner B runner")
    host = RunnerHost(tmp, guest, binary)
    host.setup({"a": token_a, "b": token_b, "user": office.owner_a})
    try:
        def invoke(label, body=None):
            forwarded, address = host.connection_for_runner(office.endpoint)
            common = ["--url", f"http://{address}", "--token-file", f"{host.root}/{label}.token", "--json"]
            if body is None:
                return host.invoke([host.installed, "scheduler", "profile", *common], forward=forwarded)
            data = f"profile-{label}.json"
            host.replace(data, json.dumps(body))
            return host.invoke([host.installed, "scheduler", "advertise", "--file", f"{host.root}/{data}", *common],
                               forward=forwarded)

        def advertise(label, specification):
            result = invoke(label, specification)
            assert result.returncode == 0, result.stderr
            return json.loads(result.stdout)

        def own_read(label):
            result = invoke(label)
            assert result.returncode == 0, result.stderr
            return json.loads(result.stdout)

        specs = {"expected_revision": 0, "capacity": 2, "runtime_template_ids": [runtime],
                 "capabilities": ["network-egress", "local", "linux"], "ttl_seconds": 120}
        assert own_read("a") is None
        invalid = invoke("user", specs)
        assert invalid.returncode != 0 and "Unauthenticated" in invalid.stderr
        for bad_template in (other_runtime, interactive):
            invalid = invoke("a", {**specs, "runtime_template_ids": [bad_template]})
            assert invalid.returncode != 0 and "Forbidden" in invalid.stderr, invalid.stderr
        invalid = invoke("a", {**specs, "capabilities": ["linux"]})
        assert invalid.returncode != 0 and "InvalidInput" in invalid.stderr
        assert own_read("a") is None
        start = datetime.now(timezone.utc)
        profile = advertise("a", specs)
        assert profile["revision"] == 1 and profile["runner_id"] == runner_a and profile["capacity"] == 2
        expiry = datetime.fromisoformat(profile["expires_at"].replace("Z", "+00:00"))
        assert start + timedelta(seconds=105) <= expiry <= datetime.now(timezone.utc) + timedelta(seconds=125)
        assert profile["capabilities"] == sorted(specs["capabilities"])
        assert office.tool(other, "office_get_runner_profile", runner_id=runner_a)["code"] == "FORBIDDEN"
        assert office.tool(lead, "office_get_runner_profile", runner_id=runner_a) == profile
        other_profile = advertise("b", {**specs, "capacity": 1, "runtime_template_ids": [other_runtime]})
        assert other_profile["runner_id"] == runner_b and other_profile["runtime_template_ids"] == [other_runtime]
        assert own_read("a") == profile
        checks.append("real_runner_advertises_only_its_owners_managed_templates_and_matching_capabilities")

        obsolete = invoke("a", specs)
        assert obsolete.returncode != 0 and "RevisionConflict" in obsolete.stderr
        paused = advertise("a", {**specs, "expected_revision": 1, "capacity": 0})
        office.restart()
        assert own_read("a") == paused and own_read("b") == other_profile
        assert office.cli(office.owner_a, "scheduler", "profile", "--workspace", office.workspace,
                          "--runner", runner_a) == paused
        assert office.cli(office.owner_a, "scheduler", "budget", "--workspace", office.workspace) == current
        assert office.sql("SET ROLE cocodeop; SELECT count(*) FROM runner_execution_profiles; "
                          "SELECT count(*) FROM execution_budgets").splitlines()[-2:] == ["0", "0"]
        checks.append("availability_versions_and_owner_isolation_survive_restart")
    finally:
        host.close()

    team = office.tool(lead, "office_create_team", name="Scheduler team", idempotency_key=str(uuid4()))["team"]
    office.tool(lead, "office_set_team_member", team_id=team["team_id"], expected_revision=team["revision"],
                idempotency_key=str(uuid4()), agent_id=worker_id["agent_id"], role="worker")
    parent = office.tool(lead, "office_create_task", team_id=team["team_id"], idempotency_key=str(uuid4()),
                         objective="Integrate a managed adapter", acceptance_criteria=["Run the typed task"])["task"]
    limits = {"remaining_depth": 1, "expires_at": (datetime.now(timezone.utc) + timedelta(hours=2)).isoformat(),
              "max_attempts": 2, "max_subtasks": 1, "runtime_template_ids": [runtime]}
    grant = office.tool(office.owner_a, "office_issue_delegation", task_id=parent["task_id"], expected_revision=parent["revision"],
                       lead_agent_id=lead_id["agent_id"], limits=limits, idempotency_key=str(uuid4()))["grant"]
    delegated = office.tool(lead, "office_delegate_task", grant_id=grant["grant_id"], expected_revision=grant["revision"],
                            idempotency_key=str(uuid4()), worker_agent_id=worker_id["agent_id"], objective="Use a local runner",
                            acceptance_criteria=["Preserve the selected adapter"],
                            limits={**limits, "remaining_depth": 0, "max_attempts": 1, "max_subtasks": 0})
    status, response = call(office.endpoint, worker, "tools/call", {"name": "office_attach_instance", "arguments": {
        "idempotency_key": str(uuid4()), "label": "Interactive request for managed template",
    }})
    assert status == 200, response
    instance = response["result"]["structuredContent"]["instance_id"]
    refused = office.tool(worker, "office_claim_task", task_id=delegated["grant"]["task_id"],
                          expected_revision=delegated["task_revision"], instance_id=instance,
                          runtime_template_id=runtime, idempotency_key=str(uuid4()))
    assert refused["code"] == "INVALID_TRANSITION", refused
    assert office.tool(worker, "office_get_delegation", grant_id=delegated["grant"]["grant_id"])["attempts_used"] == 0
    checks.append("interactive_claim_cannot_consume_a_managed_runtime_or_delegation_attempt")
    return checks


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--linux-vm", action="store_true")
    parser.add_argument("--runner", type=Path)
    args = parser.parse_args()
    assert args.linux_vm or os.name != "nt", "Use --linux-vm on Windows"
    binary = (args.runner or ROOT / (".cocodeop-test/linux-bin/office-runner" if args.linux_vm
                                     else "target/release/office-runner")).resolve()
    assert binary.is_file(), f"Build the Linux runner: {binary}"
    with tempfile.TemporaryDirectory(prefix="cocodeop-admission-") as directory:
        tmp = Path(directory)
        with workspace_fixture(tmp, "scheduler-admission") as office:
            print(json.dumps({"checks": verify(office, tmp, args.linux_vm, binary),
                              "linux_execution": "debian13_qemu_guest" if args.linux_vm else "native_linux",
                              "independent_physical_hosts": False}))
