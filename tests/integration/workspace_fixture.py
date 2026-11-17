"""A disposable real CoCodeOp installation with two independent owner credentials."""

import contextlib
import hashlib
import json
import os
import secrets
import subprocess
import sys
from uuid import uuid4

from m01 import ROOT, compose_runner, wait_for_status
from m02_bootstrap import call, request


def digest(token):
    return hashlib.sha256(token.encode()).hexdigest()


class WorkspaceFixture:
    def __init__(self, compose, operator, project=None):
        self.compose = compose
        self.project = project
        self.endpoint = compose("port", "server", "8400").stdout.strip()
        user_flag = ["--user", f"{os.getuid()}:{os.getgid()}"] if hasattr(os, "getuid") else []
        initial = compose(
            "run", "--rm", "--no-deps", "-T", *user_flag, "--entrypoint", "/usr/local/bin/office",
            "-v", f"{operator}:/operator", "server", "bootstrap", "--user", "Alice", "--workspace", "Engineering",
            "--token-file", "/operator/a.token", "--json",
        )
        self.alice = json.loads(initial.stdout)
        self.workspace = self.alice["memberships"][0]["workspace_id"]
        self.owner_a = (operator / "a.token").read_text().strip()
        invitation, self.owner_b = secrets.token_hex(32), secrets.token_hex(32)
        self.api(f"/v1/workspaces/{self.workspace}/invitations", self.owner_a, {
            "idempotency_key": str(uuid4()), "display_name": "Bob", "invitation_digest": digest(invitation),
        })
        self.bob = self.api("/v1/invitations/accept", self.owner_b, {
            "idempotency_key": str(uuid4()), "invitation_token": invitation,
        })

    def api(self, path, token, body):
        status, reply = request(self.endpoint, path, token=token, body=body)
        assert status == 200, (status, reply)
        return reply["result"]

    def tool(self, token, tool_name, **arguments):
        arguments.setdefault("workspace_id", self.workspace)
        status, reply = call(self.endpoint, token, "tools/call", {"name": tool_name, "arguments": arguments})
        assert status == 200, (status, reply)
        return reply["result"]["structuredContent"]

    def agent(self, owner, name, role):
        key = secrets.token_hex(32)
        identity = self.api(f"/v1/workspaces/{self.workspace}/agents", owner, {
            "idempotency_key": str(uuid4()), "display_name": name, "role": role, "credential_digest": digest(key),
        })
        return key, identity

    def cli(self, token, *args):
        office = ROOT / "target" / "release" / ("office.exe" if os.name == "nt" else "office")
        result = subprocess.run(
            [str(office), *args, "--url", f"http://{self.endpoint}", "--json"],
            env=dict(os.environ, OFFICE_TOKEN=token), check=True, text=True, encoding="utf-8",
            capture_output=True, timeout=10,
        )
        return json.loads(result.stdout)

    def sql(self, query):
        return self.compose("exec", "-T", "db", "psql", "-U", "postgres", "-d", "cocodeop", "-XAt",
                            "-v", "ON_ERROR_STOP=1", "-c", query).stdout.strip()

    def restart(self):
        self.compose("restart", "server", timeout=45)
        self.endpoint = self.compose("port", "server", "8400").stdout.strip()
        wait_for_status(self.endpoint, "/readyz", 200)


@contextlib.contextmanager
def workspace_fixture(tmp, label, *, server_image=None):
    storage = tmp / "secrets"
    subprocess.run([sys.executable, ROOT / "deploy/init-secrets.py", storage], check=True)
    project = f"cocodeop-{label}-{uuid4().hex[:12]}"
    env = dict(os.environ, COCODEOP_SECRETS_DIR=str(storage), COCODEOP_HTTP_PORT="0")
    overrides = []
    if server_image:
        env["COCODEOP_SERVER_IMAGE"] = server_image
        overrides.append(ROOT / "deploy/compose.release.yaml")
    compose = compose_runner(project, env, *overrides)
    operator = tmp / "operator"
    operator.mkdir(mode=0o700)
    try:
        try:
            compose("up", "--no-build" if server_image else "--build", "--wait", "--wait-timeout", "90", timeout=720)
        except RuntimeError as error:
            logs = compose("logs", "--no-color", "--tail", "160", "server", "db", check=False)
            diagnostic = "\n".join(line for line in logs.stdout.splitlines()
                                   if "ERROR:" in line or "DETAIL:" in line or '"level":"ERROR"' in line)
            raise RuntimeError(f"{error}\n{diagnostic or logs.stdout[-3500:]}") from error
        yield WorkspaceFixture(compose, operator, project)
    finally:
        images = [] if server_image else ["--rmi", "local"]
        compose("down", "--volumes", *images, "--remove-orphans", check=False, timeout=90)
