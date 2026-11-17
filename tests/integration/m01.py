"""Exercise the M01 server and schema lifecycle through the real Compose deployment."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
from urllib.error import HTTPError, URLError
from urllib.request import ProxyHandler, build_opener
from uuid import UUID, uuid4


ROOT = Path(__file__).resolve().parents[2]
HTTP = build_opener(ProxyHandler({}))


def compose_runner(project, env, *overrides):
    config = ["-f", str(ROOT / "deploy/compose.yaml")]
    for override in overrides:
        config.extend(["-f", str(override)])

    def run(*args, check=True, timeout=45):
        process = subprocess.run(
            ["docker", "compose", *config, "-p", project, *args],
            cwd=ROOT, env=env, capture_output=True, text=True, timeout=timeout,
        )
        if check and process.returncode != 0:
            raise RuntimeError(f"Compose {args[0]} failed: {process.stderr[-2500:]}")
        return process
    return run


def request(endpoint, path):
    try:
        with HTTP.open(f"http://{endpoint}{path}", timeout=4) as response:
            return response.status, response.headers, json.load(response)
    except HTTPError as error:
        with error:
            return error.code, error.headers, json.load(error)


def wait_for_status(endpoint, path, expected):
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        try:
            status, headers, body = request(endpoint, path)
            if status == expected:
                assert UUID(headers["x-request-id"]) == UUID(body["request_id"])
                return body
        except (URLError, ConnectionError, TimeoutError):
            pass
        time.sleep(0.4)
    raise AssertionError(f"{path} never returned HTTP {expected}")


def run(tmp):
    secrets_dir = tmp / "secrets"
    subprocess.run([sys.executable, ROOT / "deploy/init-secrets.py", secrets_dir], check=True)
    env = dict(os.environ, COCODEOP_SECRETS_DIR=str(secrets_dir), COCODEOP_HTTP_PORT="0")
    project = f"cocodeop-m01-{uuid4().hex[:12]}"
    compose = compose_runner(project, env)

    def sql(*statements):
        args = ["exec", "-T", "db", "psql", "-U", "postgres", "-d", "cocodeop", "-XAt", "-v", "ON_ERROR_STOP=1", "-1"]
        for statement in statements:
            args.extend(["-c", statement])
        return compose(*args).stdout.strip()

    try:
        compose("up", "--build", "--wait", "--wait-timeout", "90", timeout=720)
        endpoint = compose("port", "server", "8400").stdout.strip()
        boot = wait_for_status(endpoint, "/readyz", 200)["result"]
        assert boot["api_version"] == 1 and boot["schema_version"] >= 2
        assert sql("SELECT rolsuper, rolbypassrls FROM pg_roles WHERE rolname = 'cocodeop'") == "f|f"
        cli = compose("exec", "-T", "server", "office", "status", "--json")
        assert json.loads(cli.stdout)["result"] == boot
        history = sql("SELECT version, checksum, installed_on FROM _sqlx_migrations ORDER BY version")
        assert len(history.splitlines()) == boot["schema_version"]

        compose("restart", "server")
        endpoint = compose("port", "server", "8400").stdout.strip()
        assert wait_for_status(endpoint, "/readyz", 200)["result"] == boot
        assert sql("SELECT version, checksum, installed_on FROM _sqlx_migrations ORDER BY version") == history

        compose("stop", "db")
        wait_for_status(endpoint, "/healthz", 200)
        unavailable = wait_for_status(endpoint, "/readyz", 503)
        assert unavailable["error"]["code"] == "DATABASE_UNAVAILABLE"
        cli = compose("exec", "-T", "server", "office", "status", "--json", check=False)
        assert cli.returncode != 0 and json.loads(cli.stdout)["error"]["retryable"] is True
        compose("start", "db")
        assert wait_for_status(endpoint, "/readyz", 200)["result"] == boot

        # Reconstruct the prior schema inside this disposable database and exercise upgrade.
        compose("stop", "server")
        stopped = compose("ps", "-aq", "server").stdout.strip()
        state = json.loads(subprocess.check_output(["docker", "inspect", "--format", "{{json .State}}", stopped], text=True))
        assert state["ExitCode"] == 0 and not state["OOMKilled"], (
            f"Graceful shutdown reported exit={state['ExitCode']} oom={state['OOMKilled']} "
            f"finished_at={state['FinishedAt']} error={state['Error']}"
        )
        original_migration = (ROOT / "migrations/0001_installation.sql").read_text(encoding="utf-8")
        sql(
            "CREATE TEMP TABLE previous_migrations (LIKE _sqlx_migrations INCLUDING ALL)",
            "INSERT INTO previous_migrations SELECT * FROM _sqlx_migrations WHERE version = 1",
            "CREATE TEMP TABLE previous_installation AS SELECT installation_id, created_at FROM office_installation",
            "DROP SCHEMA public CASCADE",
            "CREATE SCHEMA public AUTHORIZATION cocodeop",
            "SET LOCAL ROLE cocodeop",
            original_migration,
            "RESET ROLE",
            "CREATE TABLE public._sqlx_migrations (LIKE previous_migrations INCLUDING ALL)",
            "INSERT INTO public._sqlx_migrations SELECT * FROM previous_migrations",
            "ALTER TABLE public._sqlx_migrations OWNER TO cocodeop",
            "UPDATE office_installation SET installation_id = previous_installation.installation_id, "
            "created_at = previous_installation.created_at FROM previous_installation WHERE singleton",
        )
        assert sql("SELECT version FROM _sqlx_migrations ORDER BY version") == "1"
        compose("start", "server")
        endpoint = compose("port", "server", "8400").stdout.strip()
        assert wait_for_status(endpoint, "/readyz", 200)["result"] == boot
        assert sql("SELECT installed_on FROM _sqlx_migrations WHERE version = 1") == history.splitlines()[0].split("|")[-1]

        sql("INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) "
            "VALUES (99999, 'future fixture', true, decode('00', 'hex'), 0)")
        incompatible = wait_for_status(endpoint, "/readyz", 503)
        assert incompatible["error"]["code"] == "INCOMPATIBLE_SCHEMA"
        compose("stop", "server")
        compose("start", "server", check=False)
        assert compose("wait", "server", check=False).returncode != 0
        assert "database migration failed" in compose("logs", "--no-color", "server").stdout
        assert sql("SELECT max(version) FROM _sqlx_migrations") == "99999"
        print(json.dumps({
            "postgres": sql("SHOW server_version"),
            "migration_version": boot["schema_version"],
            "checks": ["fresh_install", "no_reapply", "database_outage", "database_reconnect", "graceful_shutdown",
                       "upgrade_preserves_installation", "incompatible_schema_denied", "unprivileged_database_role", "cli_json"],
        }))
    except Exception:
        print(compose("logs", "--no-color", "server", check=False).stdout[-4000:], file=sys.stderr)
        raise
    finally:
        compose("down", "--volumes", "--rmi", "local", "--remove-orphans", check=False, timeout=90)


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="cocodeop-m01-") as directory:
        run(Path(directory))
