"""Verify the binary artifact workflow against a disposable real PostgreSQL installation."""

import hashlib
import json
import tempfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from urllib.error import HTTPError
from urllib.request import Request, urlopen
from uuid import uuid4

from m02_bootstrap import request
from workspace_fixture import workspace_fixture


CHUNK = 1024 * 1024


def sha(content):
    return hashlib.sha256(content).hexdigest()


def bytes_request(office, token, path, data=None, media_type="application/octet-stream"):
    headers = {"Authorization": f"Bearer {token}"}
    if data is not None:
        headers["Content-Type"] = media_type
    query = Request(f"http://{office.endpoint}{path}", data=data, headers=headers, method="PUT" if data is not None else "GET")
    try:
        with urlopen(query, timeout=12) as reply:
            if data is None:
                assert reply.headers.get("Content-Type") == "application/octet-stream"
                assert reply.headers.get("Cache-Control") == "private, no-store"
                assert reply.headers.get("X-Content-Type-Options") == "nosniff"
                return reply.status, reply.read()
            return reply.status, json.load(reply)["result"]
    except HTTPError as error:
        return error.code, json.load(error)["error"]["code"]


def verify(office, tmp):
    reviewer, identity = office.agent(office.owner_b, "Exact artifact reader", "reviewer")
    sibling, _ = office.agent(office.owner_b, "Other agent", "reviewer")
    owner = office.owner_a
    task = office.tool(owner, "office_create_task", objective="Produce a private validation archive",
                       acceptance_criteria=["The validation bytes have a recorded SHA-256"], visibility={"kind": "owner_private"},
                       idempotency_key=str(uuid4()))["task"]
    ready = office.tool(owner, "office_prepare_task", task_id=task["task_id"], expected_revision=task["revision"],
                        idempotency_key=str(uuid4()))["task"]
    run = office.tool(owner, "office_claim_task", task_id=task["task_id"], expected_revision=ready["revision"],
                     idempotency_key=str(uuid4()))["run"]
    content = bytes(range(256)) * (CHUNK // 128) + bytes(range(37))
    spec = {"name": "validation.bin", "mime": "application/octet-stream", "expected_revision": 0,
            "size_bytes": len(content), "sha256": sha(content), "retention_days": 7, "idempotency_key": str(uuid4()),
            "visibility": {"kind": "explicit_acl"}, "grants": [
                {"user_id": identity["owner_user_id"], "agent_id": identity["agent_id"], "right": "read"}
            ], "provenance": {"task_id": task["task_id"], "run_id": run["run_id"]}}
    pending = office.tool(owner, "office_begin_artifact", **spec)
    assert pending.get("chunk_count") == 3 and pending["chunk_bytes"] == CHUNK, pending
    assert office.tool(owner, "office_begin_artifact", **spec) == pending
    artifact = pending["artifact_id"]
    upload = pending["upload_id"]
    upload_path = f"/v1/artifacts/upload/{office.workspace}/{upload}"
    path = f"/v1/artifacts/content/{office.workspace}/{artifact}/1"
    assert office.tool(reviewer, "office_get_artifact", artifact_id=artifact)["code"] == "ARTIFACT_UNAVAILABLE"
    assert office.tool(owner, "office_finalize_artifact", upload_id=upload)["code"] == "ARTIFACT_INCOMPLETE"
    assert bytes_request(office, reviewer, f"{upload_path}/2", content[2 * CHUNK:]) == (403, "FORBIDDEN")
    assert bytes_request(office, owner, f"{upload_path}/0", content[:CHUNK + 1]) == (400, "INVALID_INPUT")
    assert bytes_request(office, owner, f"{upload_path}/2", content[2 * CHUNK:], "application/json") == (400, "INVALID_INPUT")
    for index in (2, 0, 1):
        body = content[index * CHUNK:(index + 1) * CHUNK]
        status, saved = bytes_request(office, owner, f"{upload_path}/{index}", body)
        assert status == 200 and saved == {"index": index, "size_bytes": len(body), "sha256": sha(body)}, saved
    assert bytes_request(office, owner, f"{upload_path}/2", content[2 * CHUNK:] + b"extra") == (400, "INVALID_INPUT")
    assert bytes_request(office, owner, f"{upload_path}/2", b"X" + content[2 * CHUNK + 1:]) == (409, "IDEMPOTENCY_CONFLICT")
    assert bytes_request(office, reviewer, f"{path}/2") == (410, "ARTIFACT_UNAVAILABLE")
    office.restart()
    published = office.tool(owner, "office_finalize_artifact", upload_id=upload)
    assert published.get("available") and published["sha256"] == sha(content) and published["revision"] == 1, published
    assert published["provenance"] == spec["provenance"]
    assert office.tool(owner, "office_finalize_artifact", upload_id=upload) == published
    assert office.tool(reviewer, "office_get_artifact", artifact_id=artifact)["provenance"] is None
    assert office.tool(sibling, "office_get_artifact", artifact_id=artifact)["code"] == "FORBIDDEN"
    assert bytes_request(office, office.owner_b, f"{path}/0") == (403, "FORBIDDEN")
    destination = tmp / "verified.bin"
    received = office.cli(reviewer, "artifacts", "download", "--workspace", office.workspace, "--id", artifact,
                          "--revision", "1", "--out", str(destination))
    assert received["sha256"] == sha(destination.read_bytes()) == sha(content)

    new = b"second revision"
    revise = {"artifact_id": artifact, "expected_revision": 1, "idempotency_key": str(uuid4()),
              "name": "validation.bin", "mime": "text/plain", "retention_days": 7}
    assert office.tool(reviewer, "office_begin_artifact", **revise, size_bytes=len(new), sha256=sha(new))["code"] == "FORBIDDEN"
    request_file = tmp / "upload.json"
    request_file.write_text(json.dumps(dict(revise, workspace_id=office.workspace)), encoding="utf-8")
    source_file = tmp / "source"
    source_file.write_bytes(new)
    second = office.cli(owner, "artifacts", "upload", "--file", str(request_file), "--content", str(source_file))
    assert second["revision"] == 2 and second["sha256"] == sha(new)
    assert office.tool(reviewer, "office_get_artifact", artifact_id=artifact)["revision"] == 2
    assert bytes_request(office, reviewer, f"{path}/2") == (200, content[2 * CHUNK:])
    assert office.tool(owner, "office_begin_artifact", **dict(revise, idempotency_key=str(uuid4())),
                       size_bytes=len(new), sha256=sha(new))["code"] == "REVISION_CONFLICT"

    invalid = dict(spec, workspace_id=office.workspace, mime="text/html")
    status, reply = request(office.endpoint, "/v1/artifacts/begin", token=owner, body=invalid)
    assert status == 400 and reply["error"]["code"] == "INVALID_INPUT"
    wrong = dict(revise, expected_revision=2, idempotency_key=str(uuid4()), size_bytes=3, sha256=sha(b"abc"))
    orphan = office.tool(owner, "office_begin_artifact", **wrong)
    assert office.tool(owner, "office_begin_artifact", **dict(wrong, idempotency_key=str(uuid4())))["code"] == "ARTIFACT_UPLOAD_OPEN"
    assert bytes_request(office, owner, f"/v1/artifacts/upload/{office.workspace}/{orphan['upload_id']}/0", b"xyz")[0] == 200
    assert office.tool(owner, "office_finalize_artifact", upload_id=orphan["upload_id"])["code"] == "ARTIFACT_DIGEST_MISMATCH"

    # Advance the test fixture's retention timestamps without waiting seven days.
    office.sql(f"""ALTER TABLE artifact_versions DISABLE TRIGGER protect_artifact_version;
    UPDATE artifact_versions SET upload_until=clock_timestamp()-interval '1 second' WHERE upload_id='{orphan["upload_id"]}';
    UPDATE artifact_versions SET expires_at=clock_timestamp()-interval '1 second',
       finalized_at=clock_timestamp()-interval '2 days' WHERE upload_id='{upload}';
    ALTER TABLE artifact_versions ENABLE TRIGGER protect_artifact_version;""")
    assert bytes_request(office, reviewer, f"{path}/2") == (410, "ARTIFACT_UNAVAILABLE")
    freed = office.cli(owner, "artifacts", "cleanup", "--workspace", office.workspace)
    assert freed == {"abandoned_uploads": 1, "expired_revisions": 1, "freed_bytes": len(content) + 3}, freed
    older = office.tool(reviewer, "office_get_artifact", artifact_id=artifact, revision=1)
    assert older["sha256"] == sha(content) and not older["available"]
    assert office.sql(f"SELECT count(*) FROM artifact_chunks WHERE upload_id IN ('{upload}','{orphan['upload_id']}')") == "0"
    assert office.sql("SET ROLE cocodeop; SELECT count(*) FROM artifact_chunks").splitlines()[-1] == "0"

    def reserve(_):
        return office.tool(owner, "office_begin_artifact", name="reservation.bin", mime="application/octet-stream",
                           expected_revision=0, idempotency_key=str(uuid4()), size_bytes=64 * CHUNK,
                           sha256="0" * 64, retention_days=1)

    with ThreadPoolExecutor(max_workers=9) as pool:
        allocations = list(pool.map(reserve, range(9)))
    assert sum("upload_id" in item for item in allocations) == 7
    assert [item.get("code") for item in allocations].count("ARTIFACT_QUOTA_EXCEEDED") == 2
    return {"artifact_id": artifact, "revision_1_sha256": sha(content), "revision_2_sha256": sha(new),
            "bytes_transferred": len(content), "cleanup": freed, "schema": 54,
            "checks": ["binary_chunks_out_of_order_and_idempotent", "allocation_requires_exact_upload_owner",
                       "mime_size_and_digest_are_enforced", "publication_survives_server_restart",
                       "exact_revision_acl_and_redacted_provenance", "verified_cli_binary_download_and_upload",
                       "new_revision_preserves_previous_bytes", "quota_is_atomic_under_parallel_reservations",
                       "expired_content_and_incomplete_uploads_are_reclaimed"]}


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="cocodeop-artifacts-") as directory:
        tmp = Path(directory)
        with workspace_fixture(tmp, "artifacts") as office:
            try:
                print(json.dumps(verify(office, tmp)))
            except Exception:
                import sys
                print(office.compose("logs", "--no-color", "--tail", "15", "db").stdout[-4500:], file=sys.stderr)
                raise
