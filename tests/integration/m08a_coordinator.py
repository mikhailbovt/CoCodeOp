"""Verify exclusive Linux checkout control, durable epochs and scope activation."""

import hashlib
import json
import time
from concurrent.futures import ThreadPoolExecutor
from uuid import uuid4

from m02_bootstrap import request
from m05a_claims import Participant


LAUNCH = """
import subprocess, sys
from pathlib import Path
with open(sys.argv[1], 'w') as out, open(sys.argv[2], 'w') as err:
    child = subprocess.Popen(sys.argv[4:], stdout=out, stderr=err)
    Path(sys.argv[3]).write_text(str(child.pid))
    raise SystemExit(child.wait())
"""

SIGNAL = """
import os, signal, sys
from pathlib import Path
pid = int(Path(sys.argv[1]).read_text())
args = Path(f'/proc/{pid}/cmdline').read_bytes().split(b'\\0')
assert args[0] == os.fsencode(sys.argv[2]) and os.fsencode(sys.argv[3]) in args
os.kill(pid, getattr(signal, sys.argv[4]))
"""


def verify(office, host, checkout, peer, token_a, token_b):
    checkout_id = checkout["checkout_id"]
    workspace = checkout["workspace_id"]
    root = f"{host.root}/tree/code"

    def state(label):
        return f"{host.root}/gateway-{label}"

    def operation(name, token, body):
        return request(office.endpoint, f"/v1/runner-checkouts/coordinator/{name}", token=token, body=body)

    def daemon(label, *, root_path=None, owner="a", managed=True):
        forward, endpoint = host.connection_for_runner(office.endpoint)
        args = [host.installed, "checkout", "serve", "--json", "--checkout", checkout_id,
                "--root", root_path or root, "--state-dir", state(label), "--url", f"http://{endpoint}",
                "--token-file", f"{host.root}/{owner}.token"]
        if managed:
            args = ["python3", "-c", LAUNCH, f"{state(label)}/stdout", f"{state(label)}/stderr", f"{state(label)}/pid", *args]
        return host.invoke(args, forward=forward)

    def wait_ready(label, future):
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            published = host.invoke(["head", "-n", "1", f"{state(label)}/stdout"]).stdout.strip()
            if published:
                lease = json.loads(published)
                assert lease["ready_at"] and lease["stopped_at"] is None
                return lease
            if future.done():
                raise AssertionError(host.invoke(["head", "-c", "1500", f"{state(label)}/stderr"]).stdout)
            time.sleep(0.2)
        raise AssertionError("The checkout service did not announce readiness")

    def stop(label, signal="SIGTERM", check=True):
        return host.invoke(["python3", "-c", SIGNAL, f"{state(label)}/pid", host.installed,
                            state(label), signal], check=check)

    for label in ("one", "two", "wrong"):
        host.invoke(["mkdir", "-m", "700", state(label)], check=True)

    with ThreadPoolExecutor(max_workers=2) as pool:
        active = pool.submit(daemon, "one")
        try:
            first = wait_ready("one", active)
            assert first["checkout_id"] == checkout_id and first["runner_id"] == checkout["host_runner_id"]
            wrong = daemon("wrong", root_path=f"{host.root}/independent", managed=False)
            assert wrong.returncode != 0 and "registered checkout" in wrong.stderr, wrong.stderr
            peer_denied = daemon("wrong", owner="b", managed=False)
            assert peer_denied.returncode != 0
            locked = daemon("two", managed=False)
            assert locked.returncode != 0 and "exclusive lock" in locked.stderr, locked.stderr
            durable = json.loads(host.invoke(["cat", f"{state('one')}/coordinator-{first['epoch']}.json"], check=True).stdout)
            assert durable["epoch"] == first["epoch"] and durable["ready_at"] is None
            stored = office.sql(f"SELECT count(*) FROM checkout_coordinator_sessions WHERE checkout_id='{checkout_id}'")
            assert stored == "1"
            checks = ["coordinator_verifies_registered_root_and_excludes_a_second_state_dir_with_an_inode_lock"]

            allowed = [{"kind": "directory", "path": "parts/gateway-test"}]
            permissions = {"read": {"allow": allowed}, "write": {"allow": []}, "write_operations": []}
            participant = Participant(office, checkout, peer, office.owner_b, "Coordinator activation", permissions)
            status = None
            deadline = time.monotonic() + 12
            while time.monotonic() < deadline:
                status = office.tool(participant.lead, "office_get_work_scope", task_id=participant.task)
                if status.get("state") == "active":
                    break
                time.sleep(0.25)
            assert status and status.get("state") == "active", status
            scope = status["definition"]
            local = host.invoke(["cat", f"{state('one')}/scope-{scope['scope_id']}-{scope['revision']}.json"], check=True)
            local_bytes = local.stdout.encode()
            assert json.loads(local_bytes)["scope"] == scope
            payload = {"checkout_id": checkout_id, "policy_sequence": status["policy_sequence"], "scope_id": scope["scope_id"],
                       "scope_revision": scope["revision"], "deployment_sha256": hashlib.sha256(local_bytes).hexdigest()}
            ack_path = "/v1/runner-checkouts/acknowledge-scope"
            assert request(office.endpoint, ack_path, token=token_a, body=payload)[1]["error"]["code"] == "COORDINATOR_STALE"
            ack = request(office.endpoint, ack_path, token=token_a, body={**payload, "coordinator_epoch": first["epoch"]})
            assert ack[0] == 200 and ack[1]["result"] == status
            checks.append("coordinator_persists_and_acknowledges_scope_under_its_live_epoch")

            old_instance = office.sql(f"SELECT instance_key FROM checkout_coordinator_sessions "
                                      f"WHERE workspace_id='{workspace}' AND epoch={first['epoch']}")
            stop("one", "SIGKILL")
            assert active.result(timeout=10).returncode != 0
            successor = pool.submit(daemon, "two")
            try:
                second = wait_ready("two", successor)
                assert second["epoch"] > first["epoch"]
                previous = {"checkout_id": checkout_id, "epoch": first["epoch"]}
                assert operation("renew", token_a, previous)[1]["error"]["code"] == "COORDINATOR_STALE"
                assert operation("renew", token_b, {**previous, "epoch": second["epoch"]})[0] != 200
                persisted = request(office.endpoint, f"/v1/runner-checkouts/{checkout_id}/next-scope", token=token_a)[1]["result"]
                replay = operation("start", token_a, {"checkout_id": checkout_id, "instance_key": old_instance, "root": persisted["root"]})
                assert replay[1]["error"]["code"] == "COORDINATOR_STALE"
                assert request(office.endpoint, ack_path, token=token_a,
                               body={**payload, "coordinator_epoch": first["epoch"]})[1]["error"]["code"] == "COORDINATOR_STALE"
                checks.append("a_crashed_coordinator_is_replaced_with_a_new_epoch_and_old_operations_fail_closed")
            finally:
                if not successor.done():
                    stop("two", check=False)
            assert successor.result(timeout=10).returncode == 0
            office.restart()
            snapshots = office.sql(f"SELECT epoch,ready_at IS NOT NULL,stopped_at IS NOT NULL "
                                   f"FROM checkout_coordinator_sessions WHERE checkout_id='{checkout_id}' ORDER BY epoch")
            assert snapshots.splitlines() == [f"{first['epoch']}|t|t", f"{second['epoch']}|t|t"]
            assert office.sql("SET ROLE cocodeop; SELECT count(*) FROM checkout_coordinator_sessions").splitlines()[-1] == "0"
            checks.append("epochs_and_confirmed_stops_survive_coordinator_restart_with_rls")
            return checks
        finally:
            if not active.done():
                stop("one", check=False)
