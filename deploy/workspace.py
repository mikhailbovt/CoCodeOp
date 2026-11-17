#!/usr/bin/env python3
"""Export or remove an exact workspace using the local Compose database."""

import argparse
import json
import os
import subprocess
import tempfile
from pathlib import Path
from uuid import UUID

from backup import BackupError, Compose, DEPLOY, digest


# The role owns the migrated CoCodeOp relations. Preserve global users and credentials.
TABLES = """
SELECT c.oid, c.relname::text AS name,
       CASE WHEN c.relname='workspaces' THEN 'id' ELSE 'workspace_id' END AS key
FROM pg_class c
JOIN pg_namespace n ON n.oid=c.relnamespace
JOIN pg_roles r ON r.oid=c.relowner
WHERE n.nspname='public' AND r.rolname='cocodeop' AND c.relkind='r'
  AND (c.relname='workspaces' OR EXISTS (
      SELECT 1 FROM pg_attribute a WHERE a.attrelid=c.oid AND a.attname='workspace_id'
        AND a.atttypid='uuid'::regtype AND NOT a.attisdropped
  ))
"""


def export(office, workspace, target):
    sql = rf"""
BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY;
SELECT jsonb_build_object(
    'format','cocodeop.workspace.v1','workspace_id',w.id,'name',w.name,
    'schema_version',(SELECT max(version) FROM _sqlx_migrations),
    'captured_at',transaction_timestamp(),
    'users',coalesce((SELECT jsonb_agg(to_jsonb(u) ORDER BY u.id)
        FROM users u JOIN workspace_memberships m ON m.user_id=u.id WHERE m.workspace_id=w.id),'[]'::jsonb)
) FROM workspaces w WHERE w.id=:'workspace'::uuid;
SELECT format(
    'SELECT jsonb_build_object(''table'',%L,''row'',to_jsonb(t)-''digest'') FROM public.%I t WHERE %I=%L::uuid',
    name,name,key,:'workspace')
FROM ({TABLES}) selected ORDER BY name
\gexec
COMMIT;
"""
    with tempfile.NamedTemporaryFile(dir=target.parent, prefix=".workspace-export-", delete=False) as staging:
        path = Path(staging.name)
        try:
            office.database("psql", "-U", "postgres", "-d", "cocodeop", "-XAtq",
                            "-v", "ON_ERROR_STOP=1", "-v", f"workspace={workspace}",
                            input_data=sql.encode("utf-8"), output_file=staging, timeout=900)
            staging.flush()
            os.fsync(staging.fileno())
            staging.seek(0)
            header = staging.readline(1024 * 1024)
            if not header:
                raise BackupError("The selected workspace does not exist")
            metadata = json.loads(header)
            if metadata.get("workspace_id") != workspace or metadata.get("format") != "cocodeop.workspace.v1":
                raise BackupError("Export did not return the exact workspace header")
            staging.close()
            sha = digest(path)
            length = path.stat().st_size
            os.link(path, target)
            return {"workspace_id": workspace, "path": str(target), "sha256": sha, "bytes": length,
                    "schema_version": metadata["schema_version"]}
        finally:
            staging.close()
            path.unlink(missing_ok=True)


def delete(office, workspace):
    office.require_offline()
    script = (DEPLOY / "workspace-delete.sql").read_text(encoding="utf-8")
    return json.loads(office.sql(script.replace("/* WORKSPACE TABLES */", TABLES), "-v", f"workspace={workspace}"))


def main():
    if os.name == "posix":
        os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--project", default="deploy")
    commands = parser.add_subparsers(dest="command", required=True)
    export_options = commands.add_parser("export", help="Write an immutable JSON Lines export")
    export_options.add_argument("--output", type=Path, required=True)
    delete_options = commands.add_parser("delete", help="Permanently remove an exact stopped workspace from Core")
    for command in (export_options, delete_options):
        command.add_argument("--workspace", type=UUID, required=True)
        command.add_argument("--json", action="store_true")
    options = parser.parse_args()
    workspace = str(options.workspace)
    office = Compose(options.project)
    try:
        if options.command == "export":
            report = export(office, workspace, options.output)
            output = f"Exported {workspace} to {report['path']} · SHA-256 {report['sha256']}"
        else:
            report = delete(office, workspace)
            output = f"Deleted {workspace} · {report['rows_deleted']} database rows"
        print(json.dumps(report, sort_keys=True) if options.json else output)
    except (BackupError, OSError, ValueError, KeyError, subprocess.TimeoutExpired) as error:
        parser.exit(1, f"{error}\n")


if __name__ == "__main__":
    main()
