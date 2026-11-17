"""Restore a frozen installation into a distinct PostgreSQL volume and reconcile its host."""

import argparse
import copy
import json
import os
import shlex
import subprocess
import sys
import tempfile
from pathlib import Path
from uuid import uuid4

from m01 import compose_runner
from m05a_registry import MANIFEST, RunnerHost
from m07_leases import register_runner
from m07_queue import configure, create_template, enqueue
from m09_native_checkout import checkout_service
from m10_activation import wait_for
from m11_artifacts import bytes_request, sha
from m12_execution_controls import set_control
from workspace_fixture import ROOT, workspace_fixture


def compose_environment(secrets):
    return dict(os.environ, COCODEOP_SECRETS_DIR=str(secrets), COCODEOP_HTTP_PORT="0")


def backup(project, env, *arguments, rejected=None):
    result = subprocess.run([sys.executable, ROOT / "deploy/backup.py", "--project", project, *map(str, arguments)],
                            env=env, capture_output=True, encoding="utf-8", timeout=180)
    if rejected:
        assert result.returncode == 1 and rejected in result.stderr, (result.returncode, result.stderr)
        return None
    assert result.returncode == 0, result.stderr
    return json.loads(result.stdout)


def transfer(host, local, remote, *, receive=False):
    if not host.guest:
        source, target = (Path(remote), local) if receive else (local, Path(remote))
        target.write_bytes(source.read_bytes())
        return
    as_owner = ["sudo", "-n", "-u", host.owner, "-H", "--"]
    if receive:
        with local.open("xb") as output:
            subprocess.run([*host.ssh, host.target, shlex.join([*as_owner, "cat", remote])],
                           stdout=output, check=True, timeout=90)
    else:
        with local.open("rb") as source:
            subprocess.run([*host.ssh, host.target, shlex.join([*as_owner, "tee", remote])],
                           stdin=source, stdout=subprocess.DEVNULL, check=True, timeout=90)
        host.invoke(["chmod", "600", remote], check=True)


def prepare_checkout(office, host, token, runner):
    host.setup({"checkout": token})
    project = office.tool(office.owner_a, "office_create_project", name="Recover the source tree",
                          visibility={"kind": "workspace"}, idempotency_key=str(uuid4()))["project"]
    checkout = office.tool(office.owner_a, "office_request_checkout", project_id=project["project_id"],
                           host_runner_id=runner, manifest=MANIFEST, idempotency_key=str(uuid4()))["checkout"]
    enrolled = host.enroll(office.endpoint, checkout, "checkout", "tree/code")
    assert enrolled.returncode == 0, enrolled.stderr
    with checkout_service(host, office, checkout) as coordinator:
        output = office.tool(office.owner_a, "office_create_checkout_snapshot", checkout_id=checkout["checkout_id"],
                             expected_checkout_revision=0, idempotency_key=str(uuid4()))
        assert "snapshot_id" in output, output
        snapshot = output["snapshot_id"]

        def ready():
            item = office.tool(office.owner_a, "office_get_checkout_snapshot",
                               checkout_id=checkout["checkout_id"], snapshot_id=snapshot)
            return item if item["state"] != "queued" else None

        sealed = wait_for(ready, 30)
        assert sealed["state"] == "ready", sealed
    return checkout, sealed, coordinator["epoch"]


def verify(office, root, host=None):
    original = json.loads(office.compose("ps", "--format", "json", "db").stdout)["Project"]
    original_env = compose_environment(root / "secrets")
    lead, _ = office.agent(office.owner_a, "Backup lead", "lead")
    _, worker = office.agent(office.owner_a, "Backup worker", "worker")
    runner_id, runner_token = register_runner(office, office.owner_a, "Checkout and execution host")
    template = create_template(office, office.owner_a, units=10)
    assert configure(office, office.owner_a, 100)["budget"]["limit_units"] == 100
    task = office.tool(lead, "office_create_task", objective="A pending job held for restore review",
                       acceptance_criteria=["Run only after explicit review"], idempotency_key=str(uuid4()))["task"]
    task = office.tool(lead, "office_prepare_task", task_id=task["task_id"], expected_revision=task["revision"],
                       idempotency_key=str(uuid4()))["task"]
    job = enqueue(office, lead, task, worker, template)["job"]
    content = b"CoCodeOp restored validation\x00\xff"
    artifact = office.tool(office.owner_a, "office_begin_artifact", name="proof.bin",
                           mime="application/octet-stream", expected_revision=0, size_bytes=len(content),
                           sha256=sha(content), retention_days=7, idempotency_key=str(uuid4()))
    upload = f"/v1/artifacts/upload/{office.workspace}/{artifact['upload_id']}/0"
    assert bytes_request(office, office.owner_a, upload, content)[0] == 200
    finalized = office.tool(office.owner_a, "office_finalize_artifact", upload_id=artifact["upload_id"])
    assert finalized["sha256"] == sha(content)

    captured = prepare_checkout(office, host, runner_token, runner_id) if host else None
    plan = root / "plan.json"
    backup(original, original_env, "plan", "--output", plan, rejected="Set every backed-up workspace to stopped")
    status, reply = set_control(office, office.owner_a, "workspace", 0, "stopped", 2048, 128)
    assert status == 200, reply
    archives = []
    if captured:
        checkout, sealed, old_epoch = captured
        checkout_id = checkout["checkout_id"]
        backup(original, original_env, "plan", "--checkout", checkout_id, "--output", plan)
        remote = host.root
        script = f"{remote}/backup.py"
        transfer(host, ROOT / "deploy/backup.py", script)
        transfer(host, plan, f"{remote}/plan.json")
        packed = host.invoke(["python3", script, "host-pack", "--plan", f"{remote}/plan.json", "--checkout",
                              checkout_id, "--root", f"{remote}/tree/code", "--state-dir", f"{remote}/controller",
                              "--runner", host.installed, "--output", f"{remote}/checkout.tar.gz"], timeout=80)
        assert packed.returncode == 0, packed.stderr
        archive = root / "checkout.tar.gz"
        transfer(host, archive, f"{remote}/checkout.tar.gz", receive=True)
        assert json.loads(packed.stdout)["archive_sha256"] == sha(archive.read_bytes())
        archives = ["--checkout-archive", archive]

    bundle = root / "bundle"
    backup(original, original_env, "create", "--output", bundle, *archives, rejected="Stop the Compose server")
    office.compose("stop", "server")
    inventory = backup(original, original_env, "create", "--output", bundle, *archives)
    restored_project = f"cocodeop-restore-{uuid4().hex[:12]}"
    restored_secrets = root / "restored-secrets"
    subprocess.run([sys.executable, ROOT / "deploy/init-secrets.py", restored_secrets], check=True)
    target_env = compose_environment(restored_secrets)
    target = compose_runner(restored_project, target_env)
    try:
        target("up", "--wait", "db", timeout=90)
        restored = backup(restored_project, target_env, "restore", "--bundle", bundle)
        assert restored["restore_pending"] and restored["backup_id"] == inventory["backup_id"]
        assert restored["quarantined_jobs"] == [job["job_id"]], restored
        backup(restored_project, target_env, "restore", "--bundle", bundle, rejected="fresh Compose database")
        target("up", "--build", "--wait", "--wait-timeout", "90", "server", timeout=720)
        peer = copy.copy(office)
        peer.compose = target
        peer.endpoint = target("port", "server", "8400").stdout.strip()
        data_path = f"/v1/artifacts/content/{office.workspace}/{artifact['artifact_id']}/1/0"
        assert bytes_request(peer, office.owner_b, data_path) == (403, "FORBIDDEN")
        assert bytes_request(peer, office.owner_a, data_path) == (200, content)
        assert peer.tool(lead, "office_bootstrap")["workspace_id"] == office.workspace
        recovered_job = peer.tool(lead, "office_get_execution", job_id=job["job_id"])
        assert recovered_job["state"] == "dead_letter" and recovered_job["terminal_reason"] == "restore_review_required"
        assert peer.tool(office.owner_a, "office_get_execution_budget")["reserved_units"] == 0
        status, reply = set_control(peer, office.owner_a, "workspace", 1, "running", 2048, 128)
        assert status == 200, reply
        assert enqueue(peer, lead, task, worker, template)["code"] == "EXECUTION_PAUSED"

        if captured:
            backup(restored_project, target_env, "resume", "--bundle", bundle,
                   rejected="one host restoration receipt")
            host_plan = root / "host-plan.json"
            backup(restored_project, target_env, "host-plan", "--bundle", bundle, "--checkout", checkout_id, "--output", host_plan)
            transfer(host, host_plan, f"{remote}/host-plan.json")
            retired = f"{remote}/original-source"
            host.invoke(["mkdir", "-m", "700", retired], check=True)
            # Lose the fixture files while retaining the registered directory identity.
            move = "from pathlib import Path; import sys; a,b=map(Path,sys.argv[1:]); [p.rename(b/p.name) for p in a.iterdir()]"
            host.invoke(["python3", "-c", move, f"{remote}/tree/code", retired], check=True)
            host_receipt = f"{remote}/restored.json"
            result = host.invoke(["python3", script, "host-restore", "--plan", f"{remote}/host-plan.json",
                                  "--archive", f"{remote}/checkout.tar.gz", "--root", f"{remote}/tree/code",
                                  "--state-dir", f"{remote}/restored-state", "--runner", host.installed,
                                  "--receipt", host_receipt], timeout=80)
            assert result.returncode == 0, result.stderr
            receipt = root / "host-receipt.json"
            transfer(host, receipt, host_receipt, receive=True)
            assert host.invoke(["test", "!", "-e", f"{remote}/tree/code/.env"]).returncode == 0
            args = ("resume", "--bundle", bundle, "--host-receipt", receipt)
            backup(restored_project, target_env, *args, rejected="verification in a new coordinator epoch")
            with checkout_service(host, peer, checkout, state_name="restored-state") as coordinator:
                assert coordinator["epoch"] > old_epoch
                assert peer.tool(office.owner_a, "office_get_checkout_snapshot", checkout_id=checkout_id,
                                 snapshot_id=sealed["snapshot_id"]) == sealed
                result = backup(restored_project, target_env, *args)
        else:
            result = backup(restored_project, target_env, "resume", "--bundle", bundle)
        assert not result["restore_pending"]
        profile = {"capacity": 1, "expected_revision": 0, "ttl_seconds": 60,
                   "capabilities": ["linux"], "runtime_template_ids": [template]}
        peer.api("/v1/runner-scheduler/profile", runner_token, profile)
        claim = lambda: peer.api("/v1/runner-scheduler/claim", runner_token,
                                 {"claim_key": str(uuid4()), "analysis_only": True})
        assert claim() is None
        fresh_job = enqueue(peer, lead, task, worker, template)["job"]
        assert fresh_job["job_id"] != job["job_id"]
        assert claim()["run"]["lease"]["job_id"] == fresh_job["job_id"]
        return {"backup_id": result["backup_id"], "schema": inventory["schema_version"],
                "restored_artifact_sha256": sha(content), "quarantined_jobs": restored["quarantined_jobs"],
                "checks": ["offline_backup_captures_actual_postgresql_and_binary_artifact_bytes",
                           "restored_credentials_and_artifact_acl_survive_in_a_new_database",
                           "queued_jobs_are_quarantined_with_budget_released",
                           "restore_hold_blocks_admission_until_explicit_review",
                           "old_jobs_stay_quarantined_and_explicit_new_jobs_can_claim"]
                          + (["restore_reconstructs_only_manifest_source_and_immutable_git_state",
                              "resume_requires_a_verified_host_receipt_and_a_new_ready_epoch"] if captured else [])}
    finally:
        target("down", "--volumes", "--rmi", "local", "--remove-orphans", check=False, timeout=90)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--linux-vm", action="store_true")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="cocodeop-backup-") as directory:
        root = Path(directory)
        with workspace_fixture(root, "backup-source") as office:
            host = RunnerHost(root, True, ROOT / ".cocodeop-test/linux-bin/office-runner") if args.linux_vm else None
            try:
                print(json.dumps(verify(office, root, host)))
            finally:
                if host:
                    host.close()
