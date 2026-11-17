"""Validate the installed local marketplace packages against a live CoCodeOp server."""

import argparse
import json
import os
import queue
import subprocess
import tempfile
import threading
from pathlib import Path
from uuid import uuid4

from workspace_fixture import ROOT, workspace_fixture


class Rpc:
    def __init__(self, args, env, cwd):
        self.process = subprocess.Popen(
            [str(arg) for arg in args], env=env, cwd=cwd, stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, encoding="utf-8",
        )
        self.responses = queue.Queue()
        self.errors = []
        self.sequence = 0
        self.reader = threading.Thread(target=self.read, daemon=True)
        self.error_reader = threading.Thread(target=self.read_errors, daemon=True)

    def __enter__(self):
        self.reader.start()
        self.error_reader.start()
        return self

    def __exit__(self, exc_type, exc, traceback):
        try:
            self.process.stdin.close()
            self.process.wait(timeout=10)
        except (BrokenPipeError, subprocess.TimeoutExpired):
            self.process.kill()
            self.process.wait(timeout=10)
        finally:
            self.reader.join(timeout=5)
            self.error_reader.join(timeout=5)
            self.process.stdout.close()
            self.process.stderr.close()

    def read(self):
        for line in self.process.stdout:
            self.responses.put(json.loads(line))
        self.responses.put(None)

    def read_errors(self):
        for line in self.process.stderr:
            self.errors = (self.errors + [line.strip()])[-5:]

    def send(self, message):
        self.process.stdin.write(json.dumps(message) + "\n")
        self.process.stdin.flush()

    def call(self, method, params):
        self.sequence += 1
        self.send({"jsonrpc": "2.0", "id": self.sequence, "method": method, "params": params})
        while True:
            try:
                reply = self.responses.get(timeout=45)
            except queue.Empty:
                raise AssertionError(f"{method} timed out: {self.errors}") from None
            if reply is None:
                raise AssertionError(f"{method} ended early: {self.errors}")
            if reply.get("id") == self.sequence:
                assert "error" not in reply, (method, reply, self.errors)
                return reply["result"]


def cli(binary, args, env, cwd):
    result = subprocess.run([str(binary), *map(str, args)], env=env, cwd=cwd,
                            capture_output=True, encoding="utf-8", timeout=60)
    assert result.returncode == 0, (args, result.stdout, result.stderr)
    return result.stdout.strip()


def check_stdio(office, cwd):
    token, identity = office.agent(office.owner_a, "Plugin lead", "lead")
    token_path = cwd / "lead.token"
    token_path.write_text(token + "\n", encoding="utf-8")
    token_path.chmod(0o600)
    profile = cwd / "client.json"
    release = ROOT / "target/release"
    binary = release / ("office.exe" if os.name == "nt" else "office")
    env = dict(os.environ, COCODEOP_CLIENT_CONFIG=str(profile),
               PATH=str(release) + os.pathsep + os.environ["PATH"])
    commands = ["connect", "--url", f"http://{office.endpoint}", "--token-file", token_path]
    saved = cli(binary, [*commands, "--agent", identity["agent_id"], "--json"], env, cwd)
    before = profile.read_bytes()
    assert json.loads(saved)["agent_id"] == identity["agent_id"] and token.encode() not in before
    mismatch = subprocess.run([str(binary), *map(str, commands), "--agent", str(uuid4())],
                              env=env, cwd=cwd, capture_output=True, timeout=25)
    assert mismatch.returncode == 1 and profile.read_bytes() == before

    with Rpc([binary, "mcp-stdio"], env, cwd) as rpc:
        init = rpc.call("initialize", {"protocolVersion": "2025-11-25",
                                      "clientInfo": {"name": "cocodeop-plugin-test", "version": "1"}, "capabilities": {}})
        assert init["protocolVersion"] == "2025-11-25", init
        rpc.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        tools = rpc.call("tools/list", {})["tools"]
        assert {"office_bootstrap", "office_worker_status", "office_create_managed_worker"} <= {tool["name"] for tool in tools}
        proposal = next(tool for tool in tools if tool["name"] == "office_propose_decision")["inputSchema"]["properties"]
        assert proposal["content"]["type"] == "object" and proposal["target"]["type"] == "object", proposal
        actor = rpc.call("tools/call", {"name": "office_bootstrap", "arguments": {}})
        assert actor["structuredContent"]["agent_id"] == identity["agent_id"], actor
    return env, identity["agent_id"], len(tools)


def check_codex(binary, env, cwd, agent_id, marketplace_root=ROOT):
    state = cwd / "codex-profile"
    state.mkdir(mode=0o700)
    env = dict(env, CODEX_HOME=str(state))
    marketplace = "cocodeop@cocodeop-local"
    cli(binary, ["plugin", "marketplace", "add", marketplace_root], env, cwd)
    installed = json.loads(cli(binary, ["plugin", "add", marketplace, "--json"], env, cwd))
    assert marketplace in json.dumps(installed), installed
    with Rpc([binary, "app-server"], env, cwd) as rpc:
        rpc.call("initialize", {"clientInfo": {"name": "cocodeop-plugin-test", "version": "1"},
                                "capabilities": {"experimentalApi": True}})
        rpc.send({"method": "initialized"})
        thread = rpc.call("thread/start", {"cwd": str(cwd), "ephemeral": True,
                                           "approvalPolicy": "never"})["thread"]["id"]
        listed = rpc.call("mcpServerStatus/list", {"threadId": thread})["data"]
        plugin = next((item for item in listed if item["pluginId"] == marketplace), None)
        assert plugin and "office_bootstrap" in plugin["tools"], (listed, rpc.errors)
        response = rpc.call("mcpServer/tool/call", {"threadId": thread, "server": plugin["name"],
                                                  "tool": "office_bootstrap", "arguments": {}})
        assert response["structuredContent"]["agent_id"] == agent_id, response
    return {"version": cli(binary, ["--version"], env, cwd), "mcp_tools": len(plugin["tools"]),
            "call": "office_bootstrap", "agent_id": agent_id}


def check_claude(binary, env, cwd, marketplace_root=ROOT):
    state = cwd / "claude-profile"
    state.mkdir(mode=0o700)
    env = dict(env, CLAUDE_CONFIG_DIR=str(state))
    marketplace = "cocodeop@cocodeop-local"
    cli(binary, ["plugin", "validate", marketplace_root / "plugins/cocodeop"], env, cwd)
    cli(binary, ["plugin", "marketplace", "add", marketplace_root], env, cwd)
    cli(binary, ["plugin", "install", marketplace], env, cwd)
    detail = cli(binary, ["plugin", "details", marketplace], env, cwd)
    assert "coordinate" in detail and "cocodeop" in detail, detail
    health = cli(binary, ["mcp", "list"], env, cwd)
    assert any("cocodeop" in line and "Connected" in line for line in health.splitlines()), health
    return {"version": cli(binary, ["--version"], env, cwd), "mcp_connected": True,
            "plugin_skill": "coordinate"}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codex", type=Path)
    parser.add_argument("--claude", type=Path)
    parser.add_argument("--marketplace-root", type=Path, default=ROOT, help="Install from a release's support directory.")
    parser.add_argument("--server-image", help="Use an already imported Core image.")
    args = parser.parse_args()
    market = args.marketplace_root.resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="cocodeop-plugins-") as directory:
        cwd = Path(directory)
        with workspace_fixture(cwd, "plugins", server_image=args.server_image) as office:
            env, agent_id, tools = check_stdio(office, cwd)
            report = {"stdio": {"mcp_tools": tools, "verified_agent_id": agent_id, "bearer_in_profile": False}}
            if args.codex:
                report["codex"] = check_codex(args.codex, env, cwd, agent_id, market)
            if args.claude:
                report["claude"] = check_claude(args.claude, env, cwd, market)
            print(json.dumps(report))
