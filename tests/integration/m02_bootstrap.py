"""Verify initial identity and live bearer enforcement using the running stack."""

import hashlib
import json
import os
import secrets
from pathlib import Path
from concurrent.futures import ThreadPoolExecutor
import subprocess
import sys
import tempfile
from urllib.error import HTTPError
from urllib.request import Request
from uuid import uuid4

from m01 import HTTP, ROOT, compose_runner, wait_for_status


def request(endpoint, path, *, token=None, rpc=None, body=None):
    headers = {}
    if token is not None:
        headers["Authorization"] = f"Bearer {token}"
    data = None
    if rpc:
        headers.update({"Accept": "application/json, text/event-stream", "Mcp-Protocol-Version": "2025-11-25"})
        body = rpc
    if body is not None:
        headers["Content-Type"] = "application/json"
        data = json.dumps(body).encode()
    req = Request(f"http://{endpoint}{path}", headers=headers, data=data)
    try:
        with HTTP.open(req, timeout=5) as response:
            return response.status, json.load(response)
    except HTTPError as error:
        with error:
            return error.code, json.load(error)


def call(endpoint, token, method, params):
    return request(endpoint, "/mcp", token=token, rpc={"jsonrpc": "2.0", "id": 1, "method": method, "params": params})


def live_codex(binary, endpoint, workspace, leads):
    def run(access):
        agent, token = access
        cwd = workspace / agent["display_name"]
        cwd.mkdir()
        env = dict(os.environ, OFFICE_TOKEN=token)
        args = [binary, "--ask-for-approval", "never", "exec", "--json", "--ephemeral", "--sandbox", "read-only",
                "--skip-git-repo-check", "--ignore-user-config", "--ignore-rules", "--color", "never",
                "-c", f'mcp_servers.office.url="http://{endpoint}/mcp"',
                "-c", 'mcp_servers.office.bearer_token_env_var="OFFICE_TOKEN"',
                "-c", "mcp_servers.office.required=true", "-c", 'mcp_servers.office.enabled_tools=["office_bootstrap"]',
                "-c", 'web_search="disabled"', "Call the CoCodeOp office_bootstrap tool and report the authenticated agent ID."]
        process = subprocess.run(args, cwd=cwd, env=env, text=True, encoding="utf-8", errors="replace",
                                 capture_output=True, timeout=120)
        confirmed = False
        for line in process.stdout.splitlines():
            try:
                event = json.loads(line)
            except ValueError:
                continue
            item = event.get("item", {})
            if (event.get("type") == "item.completed" and item.get("type") == "mcp_tool_call"
                    and item.get("server") == "office" and item.get("tool") == "office_bootstrap"):
                confirmed |= (item.get("result") or {}).get("structured_content") == agent
        assert process.returncode == 0 and confirmed, f"Live Codex did not authenticate {agent['display_name']}"
        return {"actor": agent["display_name"], "role": agent["role"], "exit_code": process.returncode,
                "mcp_identity_verified": confirmed}

    with ThreadPoolExecutor(max_workers=2) as pool:
        return list(pool.map(run, leads))


def run(tmp):
    secrets_dir = tmp / "secrets"
    subprocess.run([sys.executable, ROOT / "deploy/init-secrets.py", secrets_dir], check=True)
    operator = tmp / "operator"
    operator.mkdir(mode=0o700)
    env = dict(os.environ, COCODEOP_SECRETS_DIR=str(secrets_dir), COCODEOP_HTTP_PORT="0")
    compose = compose_runner(f"cocodeop-m02-{uuid4().hex[:12]}", env)
    office = ROOT / "target" / "release" / ("office.exe" if os.name == "nt" else "office")

    def sql(query):
        return compose("exec", "-T", "db", "psql", "-U", "postgres", "-d", "cocodeop", "-XAt", "-v", "ON_ERROR_STOP=1", "-c", query).stdout.strip()

    def bootstrap(name, filename, check=True):
        user = ["--user", f"{os.getuid()}:{os.getgid()}"] if hasattr(os, "getuid") else []
        return compose("run", "--rm", "--no-deps", "-T", *user, "--entrypoint", "/usr/local/bin/office",
                       "-v", f"{operator}:/operator", "server", "bootstrap", "--user", name,
                       "--workspace", "Engineering", "--token-file", f"/operator/{filename}", "--json", check=check)

    def cli(*args, token=None, expect_success=True):
        cli_env = os.environ.copy()
        cli_env.pop("OFFICE_TOKEN", None)
        if token is not None:
            cli_env["OFFICE_TOKEN"] = token
        result = subprocess.run([str(office), *args, "--url", f"http://{endpoint}", "--json"],
                                env=cli_env, cwd=ROOT, capture_output=True, text=True, timeout=10)
        if expect_success:
            assert result.returncode == 0, result.stderr
            return json.loads(result.stdout)
        assert result.returncode != 0
        return result.stderr

    try:
        compose("up", "--build", "--wait", "--wait-timeout", "90", timeout=720)
        endpoint = compose("port", "server", "8400").stdout.strip()
        wait_for_status(endpoint, "/readyz", 200)
        assert request(endpoint, "/v1/me")[0] == 401
        out = bootstrap("Owner A", "owner.token")
        body = json.loads(out.stdout)
        assert body["display_name"] == "Owner A"
        assert len(body["memberships"]) == 1 and body["memberships"][0]["role"] == "owner"
        token = (operator / "owner.token").read_text().strip()
        assert len(token) == 64 and token not in out.stdout + out.stderr
        assert json.loads(bootstrap("Owner A", "owner.token").stdout) == body
        assert bootstrap("Uninvited", "extra.token", check=False).returncode != 0
        assert sql("SELECT count(*) FROM users") == "1"
        assert sql("SELECT encode(digest, 'hex') FROM credentials") == hashlib.sha256(token.encode()).hexdigest()
        assert sql("SET ROLE cocodeop; SELECT count(*) FROM workspaces").splitlines()[-1] == "0"
        assert sql("SET ROLE cocodeop; SELECT count(*) FROM credentials").splitlines()[-1] == "0"

        status, whoami = request(endpoint, "/v1/me", token=token)
        assert status == 200 and whoami["result"] == body
        status, discovered = call(endpoint, token, "initialize", {
            "protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "M02 test", "version": "1"},
        })
        assert status == 200 and discovered["result"]["serverInfo"]["name"] == "cocodeop"
        status, tool = call(endpoint, token, "tools/call", {"name": "office_bootstrap", "arguments": {}})
        assert status == 200 and tool["result"]["structuredContent"] == body
        status, tools = call(endpoint, token, "tools/list", {})
        names = {tool["name"] for tool in tools["result"]["tools"]}
        assert status == 200 and "office_bootstrap" in names
        assert names.isdisjoint({"office_attach_instance", "office_get_instance"})
        uri = f"office://workspace/{body['memberships'][0]['workspace_id']}/overview"
        status, resource = call(endpoint, token, "resources/read", {"uri": uri})
        assert status == 200 and json.loads(resource["result"]["contents"][0]["text"]) == body["memberships"][0]
        status, missing = call(endpoint, token, "resources/read", {"uri": f"office://workspace/{uuid4()}/overview"})
        assert status == 200 and "error" in missing
        assert cli("whoami", token=token) == body

        workspace = body["memberships"][0]["workspace_id"]
        invitation_file = operator / "invite.token"
        invitation_args = ["user", "invite", "--workspace", workspace, "--name", "Owner B",
                           "--invitation-file", str(invitation_file)]
        invitation = cli(*invitation_args, token=token)
        assert cli(*invitation_args, token=token) == invitation
        changed_invite = ["Owner Impostor" if value == "Owner B" else value for value in invitation_args]
        assert "conflicts" in cli(*changed_invite, token=token, expect_success=False)
        code = invitation_file.read_text().strip()
        assert request(endpoint, "/v1/me", token=code)[0] == 401
        token_file = operator / "owner-b.token"
        join = ["user", "join", "--invitation-file", str(invitation_file), "--token-file", str(token_file)]
        b = cli(*join)
        assert cli(*join) == b
        b_token = token_file.read_text().strip()
        assert b_token not in (code, token)
        assert b["display_name"] == "Owner B" and b["user_id"] != body["user_id"]
        assert b["memberships"] == [{"workspace_id": workspace, "name": "Engineering", "role": "member"}]
        assert cli("whoami", token=b_token) == b
        assert "invalid or inactive" in cli(*join[:-1], str(operator / "impostor.token"), expect_success=False)
        assert "does not allow" in cli(*invitation_args, token=b_token, expect_success=False)
        assert sql("SELECT count(*) FROM workspace_invitations") == "1"
        assert sql("SELECT count(*) FROM users") == "2"
        assert sql("SET ROLE cocodeop; SELECT count(*) FROM workspace_invitations").splitlines()[-1] == "0"

        race_file = operator / "race.token"
        cli("user", "invite", "--workspace", workspace, "--name", "Concurrent Owner",
            "--invitation-file", str(race_file), token=token)
        race_code = race_file.read_text().strip()
        race_payload = {"idempotency_key": str(uuid4()), "invitation_token": race_code}

        def try_join(candidate):
            return request(endpoint, "/v1/invitations/accept", token=candidate, body=race_payload)[0]

        with ThreadPoolExecutor(max_workers=2) as pool:
            results = list(pool.map(try_join, (secrets.token_hex(32), secrets.token_hex(32))))
        assert sorted(results) == [200, 401]
        assert sql("SELECT count(*) FROM users") == "3"

        expired_file = operator / "expired.token"
        expired = cli("user", "invite", "--workspace", workspace, "--name", "Expired Owner",
                      "--invitation-file", str(expired_file), token=token)
        sql(f"UPDATE workspace_invitations SET expires_at = transaction_timestamp() WHERE id = '{expired['invitation_id']}'")
        expired_payload = {"idempotency_key": str(uuid4()), "invitation_token": expired_file.read_text().strip()}
        assert request(endpoint, "/v1/invitations/accept", token=secrets.token_hex(32), body=expired_payload)[0] == 401
        assert sql("SELECT count(*) FROM users") == "3"

        create = ["workspace", "create", "Owner B private", "--idempotency-key", str(uuid4())]
        private = cli(*create, token=b_token)
        assert cli(*create, token=b_token) == private and private["role"] == "owner"
        altered = ["Different workspace" if value == "Owner B private" else value for value in create]
        assert "conflicts" in cli(*altered, token=b_token, expect_success=False)
        spoofed = {"name": "Spoof", "idempotency_key": str(uuid4()), "created_by_user_id": body["user_id"]}
        assert request(endpoint, "/v1/workspaces", token=b_token, body=spoofed)[0] == 400
        private_uri = f"office://workspace/{private['workspace_id']}/overview"
        assert request(endpoint, "/v1/me", token=token)[1]["result"] == body
        status, visible = call(endpoint, token, "resources/list", {})
        assert status == 200 and [ref["uri"] for ref in visible["result"]["resources"]] == [uri]
        assert "error" in call(endpoint, token, "resources/read", {"uri": private_uri})[1]
        assert "result" in call(endpoint, b_token, "resources/read", {"uri": private_uri})[1]
        b_now = cli("whoami", token=b_token)
        assert private in b_now["memberships"]

        leads = []
        for label, owner, human in (("lead-a", body, token), ("lead-b", b_now, b_token)):
            agent_file = operator / f"{label}.token"
            command = ["agent", "create", "--workspace", workspace, "--name", label,
                       "--role", "lead", "--token-file", str(agent_file)]
            agent = cli(*command, token=human)
            assert cli(*command, token=human) == agent
            assert agent["owner_user_id"] == owner["user_id"]
            assert agent["agent_id"] != agent["owner_user_id"] and agent["role"] == "lead"
            key = agent_file.read_text().strip()
            assert request(endpoint, "/v1/me", token=key)[0] == 401
            assert request(endpoint, "/v1/workspaces", token=key, body={"name": "Escape", "idempotency_key": str(uuid4())})[0] == 401
            assert "error" in call(endpoint, key, "resources/read", {"uri": private_uri})[1]
            leads.append((agent, key))

        with ThreadPoolExecutor(max_workers=2) as pool:
            mcp_results = list(pool.map(lambda access: call(endpoint, access[1], "tools/call",
                {"name": "office_bootstrap", "arguments": {}}), leads))
        for (status, mcp_result), (agent, _) in zip(mcp_results, leads):
            assert status == 200 and mcp_result["result"]["structuredContent"] == agent
        assert leads[0][0]["agent_id"] != leads[1][0]["agent_id"]
        status, tools = call(endpoint, leads[0][1], "tools/list", {})
        assert status == 200 and {tool["name"] for tool in tools["result"]["tools"]} >= {
            "office_bootstrap", "office_attach_instance", "office_get_instance",
        }
        binary = os.getenv("COCODEOP_LIVE_CODEX_BIN")
        topology = None
        if os.getenv("COCODEOP_LIVE_TOPOLOGY") == "1":
            if not binary:
                raise RuntimeError("COCODEOP_LIVE_CODEX_BIN is required for the topology test")
            sys.path.insert(0, str(ROOT / "tests" / "topology"))
            from live_clients import run_pair
            topology = run_pair(binary, endpoint, tmp, leads, live_codex)
        real_clients = live_codex(binary, endpoint, tmp, leads) if binary and topology is None else []
        assert "does not allow" in cli("agent", "create", "--workspace", private["workspace_id"], "--name", "Intruder",
            "--role", "lead", "--token-file", str(operator / "intruder.token"), token=token, expect_success=False)
        assert sql("SET ROLE cocodeop; SELECT count(*) FROM agents").splitlines()[-1] == "0"

        attach = {"name": "office_attach_instance", "arguments": {"idempotency_key": str(uuid4()), "label": "interactive"}}
        status, attached = call(endpoint, leads[0][1], "tools/call", attach)
        assert status == 200
        instance = attached["result"]["structuredContent"]
        assert instance["agent_id"] == leads[0][0]["agent_id"]
        assert call(endpoint, leads[0][1], "tools/call", attach)[1]["result"]["structuredContent"] == instance
        read_instance = {"name": "office_get_instance", "arguments": {"instance_id": instance["instance_id"]}}
        assert call(endpoint, leads[0][1], "tools/call", read_instance)[1]["result"]["structuredContent"] == instance
        assert call(endpoint, leads[1][1], "tools/call", read_instance)[1]["result"]["structuredContent"]["code"] == "FORBIDDEN"
        assert "error" in call(endpoint, token, "tools/call", attach)[1]
        assert "error" in call(endpoint, token, "tools/call", read_instance)[1]
        assert sql("SET ROLE cocodeop; SELECT count(*) FROM agent_instances").splitlines()[-1] == "0"

        lead_a = leads[0][0]
        replacement_file = operator / "lead-a-rotated.token"
        issue_agent = ["credential", "issue", "--agent", lead_a["agent_id"], "--workspace", workspace,
                       "--token-file", str(replacement_file)]
        assert "does not allow" in cli(*issue_agent[:-1], str(operator / "foreign-issue.token"),
                                       token=b_token, expect_success=False)
        replacement = cli(*issue_agent, token=token)
        assert cli(*issue_agent, token=token) == replacement
        replacement_key = replacement_file.read_text().strip()
        expected_agent = dict(lead_a, credential_id=replacement["credential_id"])
        status, registered = call(endpoint, replacement_key, "tools/call", {"name": "office_bootstrap", "arguments": {}})
        assert status == 200 and registered["result"]["structuredContent"] == expected_agent
        assert call(endpoint, replacement_key, "tools/call", read_instance)[1]["result"]["structuredContent"]["code"] == "FORBIDDEN"
        revoke = ["credential", "revoke", lead_a["credential_id"]]
        assert "does not allow" in cli(*revoke, token=b_token, expect_success=False)
        receipt = cli(*revoke, token=token)
        assert cli(*revoke, token=token) == receipt
        assert call(endpoint, leads[0][1], "tools/call", {"name": "office_bootstrap", "arguments": {}})[0] == 401
        assert call(endpoint, replacement_key, "tools/call", {"name": "office_bootstrap", "arguments": {}})[0] == 200

        b_replacement_file = operator / "owner-b-rotated.token"
        b_replacement = cli("credential", "issue", "--token-file", str(b_replacement_file), token=b_token)
        renewed_b_token = b_replacement_file.read_text().strip()
        assert cli("whoami", token=renewed_b_token) == dict(b_now, credential_id=b_replacement["credential_id"])
        cli("credential", "revoke", b_now["credential_id"], token=renewed_b_token)
        assert request(endpoint, "/v1/me", token=b_token)[0] == 401
        b_now = dict(b_now, credential_id=b_replacement["credential_id"])

        runners = []
        for name, human, owner in (("runner-a", token, body), ("runner-b", renewed_b_token, b_now)):
            runner_file = operator / f"{name}.token"
            register = ["runner", "register", "--name", name, "--workspace", workspace, "--token-file", str(runner_file)]
            runner = cli(*register, token=human)
            assert cli(*register, token=human) == runner
            assert runner["owner_user_id"] == owner["user_id"]
            runner_key = runner_file.read_text().strip()
            assert cli("runner", "whoami", token=runner_key) == runner
            assert request(endpoint, "/v1/me", token=runner_key)[0] == 401
            assert call(endpoint, runner_key, "tools/list", {})[0] == 401
            assert request(endpoint, "/v1/runners/self", token=human)[0] == 401
            runners.append((runner, runner_key))
        assert sql("SET ROLE cocodeop; SELECT count(*) FROM runners").splitlines()[-1] == "0"
        runner_b = runners[1][0]
        runner_rotation = ["credential", "issue", "--runner", runner_b["runner_id"], "--workspace", workspace]
        assert "does not allow" in cli(*runner_rotation, "--token-file", str(operator / "foreign-runner.token"),
                                       token=token, expect_success=False)
        runner_replacement_file = operator / "runner-b-rotated.token"
        runner_replacement = cli(*runner_rotation, "--token-file", str(runner_replacement_file), token=renewed_b_token)
        renewed_runner_token = runner_replacement_file.read_text().strip()
        assert cli("runner", "whoami", token=renewed_runner_token) == dict(runner_b, credential_id=runner_replacement["credential_id"])
        cli("credential", "revoke", runner_b["credential_id"], token=renewed_b_token)
        assert request(endpoint, "/v1/runners/self", token=runners[1][1])[0] == 401

        sql(f"UPDATE credentials SET revoked_at = transaction_timestamp() WHERE id = '{body['credential_id']}'")
        assert request(endpoint, "/v1/me", token=token)[0] == 401
        assert call(endpoint, token, "resources/read", {"uri": uri})[0] == 401
        sql(f"UPDATE credentials SET revoked_at = NULL WHERE id = '{body['credential_id']}'; "
            f"UPDATE users SET disabled_at = transaction_timestamp() WHERE id = '{body['user_id']}'")
        assert call(endpoint, token, "tools/call", {"name": "office_bootstrap", "arguments": {}})[0] == 401
        assert cli("whoami", token=renewed_b_token) == b_now
        assert call(endpoint, leads[0][1], "tools/call", {"name": "office_bootstrap", "arguments": {}})[0] == 401
        assert call(endpoint, replacement_key, "tools/call", {"name": "office_bootstrap", "arguments": {}})[0] == 401
        assert call(endpoint, leads[1][1], "tools/call", {"name": "office_bootstrap", "arguments": {}})[0] == 200
        assert request(endpoint, "/v1/runners/self", token=runners[0][1])[0] == 401
        assert request(endpoint, "/v1/runners/self", token=renewed_runner_token)[0] == 200
        logs = compose("logs", "--no-color", "server").stdout
        assert all(secret not in logs for secret in (token, b_token, code, renewed_b_token,
                                                     replacement_key, *(key for _, key in leads)))
        assert all(secret not in logs for secret in (renewed_runner_token, *(key for _, key in runners)))
        print(json.dumps({"checks": ["operator_bootstrap", "bootstrap_replay", "second_bootstrap_denied", "digest_only",
                                     "postgres_rls_unscoped_deny", "http_identity", "mcp_tool", "mcp_resource_acl",
                                     "cli_identity", "revoked_bearer", "disabled_user", "token_absent_from_log",
                                     "second_owner_invited", "unique_owner_bearers", "invitation_idempotency", "invitation_payload_conflict",
                                     "accepted_invitation_replay", "invitation_cannot_authenticate", "foreign_credential_reuse_denied",
                                     "member_cannot_invite", "second_owner_survives_first_owner_deactivation",
                                     "exactly_one_concurrent_invitation_claim", "expired_invitation_denied",
                                     "workspace_idempotency", "workspace_author_spoof_denied", "workspace_resource_isolation",
                                     "two_concurrent_agent_principals", "agent_cannot_use_human_api", "agent_workspace_scope",
                                     "foreign_workspace_agent_creation_denied", "owner_deactivation_revokes_agent_scope",
                                     "agent_credential_rotation", "owner_credential_rotation", "credential_revoke_idempotency",
                                     "cross_owner_issue_and_revoke_denied", "instance_attachment_replay",
                                     "instance_cross_credentials_denied", "human_cannot_impersonate_agent_instance",
                                     "runner_registration_and_replay", "runner_scope_cannot_access_mcp_or_human_api",
                                     "foreign_runner_credential_denied", "runner_rotation", "owner_deactivation_revokes_runner_scope",
                                     "postgres_rls_credential_privacy", "agent_only_tools_hidden_and_direct_invocation_denied"],
                          "live_codex": real_clients, "live_topology": topology}))
    finally:
        compose("down", "--volumes", "--rmi", "local", "--remove-orphans", check=False, timeout=90)


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="cocodeop-m02-") as directory:
        run(Path(directory))
