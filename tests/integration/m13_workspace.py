"""Check scoped export and offline deletion against PostgreSQL, preserving a second workspace."""

import hashlib
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path
from uuid import uuid4

from m12_execution_controls import set_control
from workspace_fixture import ROOT, workspace_fixture


def verify(office, root):
    other = office.api("/v1/workspaces", office.owner_a, {
        "name": "Preserved project", "idempotency_key": str(uuid4()),
    })["workspace_id"]
    marker_a = f"retire-{uuid4()}"
    marker_b = f"preserve-{uuid4()}"

    def note(workspace, text):
        return office.tool(office.owner_a, "office_upsert_note", workspace_id=workspace, title=text, body=text,
                           change={"operation": "create", "visibility": {"kind": "owner_private"}},
                           idempotency_key=str(uuid4()))

    old = note(office.workspace, marker_a)
    remain = note(other, marker_b)
    _, identity = office.agent(office.owner_b, "Retired agent", "lead")
    env = dict(os.environ, COCODEOP_SECRETS_DIR=str(root / "secrets"), COCODEOP_HTTP_PORT="0")
    base = [sys.executable, ROOT / "deploy/workspace.py", "--project", office.project]

    def invoke(command, *args, work=office.workspace):
        return subprocess.run([*base, command, "--workspace", work, "--json", *args],
                              env=env, capture_output=True, encoding="utf-8", timeout=100)

    archive = root / "workspace.jsonl"
    exported = invoke("export", "--output", archive)
    assert exported.returncode == 0, exported.stderr
    receipt = json.loads(exported.stdout)
    body = archive.read_bytes()
    rows = list(map(json.loads, body.splitlines()))
    assert receipt["sha256"] == hashlib.sha256(body).hexdigest() and receipt["bytes"] == len(body)
    assert rows[0]["workspace_id"] == office.workspace and rows[0]["format"] == "cocodeop.workspace.v1", rows[0]
    assert all(row["row"].get("workspace_id", office.workspace) == office.workspace for row in rows[1:])
    assert marker_a.encode() in body and marker_b.encode() not in body
    assert any(row["table"] == "credentials" for row in rows[1:])
    assert all("digest" not in row["row"] for row in rows[1:])
    assert invoke("export", "--output", archive).returncode == 1 and archive.read_bytes() == body
    assert invoke("delete").returncode == 1
    office.compose("stop", "server")
    forbidden = invoke("delete")
    assert forbidden.returncode == 1 and "Stop the workspace" in forbidden.stderr, forbidden
    office.restart()
    code, reply = set_control(office, office.owner_a, "workspace", 0, "stopped", 16, 4)
    assert code == 200, reply
    office.compose("stop", "server")
    before_acl = office.sql(
        "SELECT count(*) FROM pg_constraint WHERE contype='f' AND connamespace='public'::regnamespace AND condeferrable"
    )
    removed = invoke("delete")
    assert removed.returncode == 0, (removed.stdout, removed.stderr)
    deleted = json.loads(removed.stdout)
    assert deleted["workspace_id"] == office.workspace and deleted["rows_deleted"] > 4, deleted
    assert office.sql("SELECT count(*) FROM pg_constraint WHERE contype='f' AND "
                      "connamespace='public'::regnamespace AND condeferrable") == before_acl
    assert office.sql(f"SELECT count(*) FROM credentials WHERE workspace_id='{office.workspace}'") == "0"
    assert office.sql(f"SELECT count(*) FROM agents WHERE id='{identity['agent_id']}'") == "0"
    assert office.sql(f"SELECT count(*) FROM notes WHERE id='{old['note_id']}'") == "0"
    office.restart()
    visible = office.tool(office.owner_a, "office_get_note", workspace_id=other, note_id=remain["note_id"])
    assert visible["body"] == marker_b, visible
    return {"workspace_id": office.workspace, "preserved_workspace": other, "export_sha256": receipt["sha256"],
            "rows_deleted": deleted["rows_deleted"], "checks": [
                "readonly_export_scopes_rows_and_omits_bearer_digests",
                "delete_requires_offline_core_and_stopped_workspace",
                "offline_delete_rechecks_foreign_keys_and_keeps_other_workspace_and_user_credential",
            ]}


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="cocodeop-workspace-") as directory:
        root = Path(directory)
        with workspace_fixture(root, "workspace-lifecycle") as office:
            print(json.dumps(verify(office, root)))
