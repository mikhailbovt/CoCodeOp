"""Execute one real OpenCode lead turn using the owner's own MCP connection."""

import hashlib
import json
import os
import pathlib
import re
import subprocess
import sys
import tempfile


BINARY = "/opt/cocodeop-test/opencode"


def run():
    request = json.load(sys.stdin)
    assert os.geteuid() != 0
    model = request["model"]
    provider = "cocodeop-local"
    qualified = f"{provider}/{model}"
    names = {f"office_{name}" for name in request["tools"]}
    assert all(re.fullmatch("office_office_[a-z_]+", name) for name in names)
    with tempfile.TemporaryDirectory(prefix="cocodeop-lead-") as directory:
        root = pathlib.Path(directory)
        config = root / "opencode.json"
        settings = {
            "share": "disabled", "snapshot": False,
            "enabled_providers": [provider], "model": qualified, "small_model": qualified,
            "permission": {"*": "deny", **dict.fromkeys(names, "allow")},
            "provider": {provider: {
                "npm": "@ai-sdk/openai-compatible", "name": provider,
                "options": {"baseURL": request["inference_url"]},
                "models": {model: {"name": model, "tool_call": True,
                                   "limit": {"context": 24576, "output": 4096}}},
            }},
            "mcp": {"office": {
                "type": "remote", "url": request["mcp_url"],
                "headers": {"Authorization": "Bearer " + request["bearer"]},
                "oauth": False, "timeout": 10000,
            }},
        }
        with os.fdopen(os.open(config, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as handle:
            json.dump(settings, handle)
        env = dict(os.environ, OPENCODE_CONFIG=str(config), OPENCODE_DISABLE_AUTOUPDATE="true",
                   OPENCODE_DISABLE_MODELS_FETCH="true", OPENCODE_DISABLE_LSP_DOWNLOAD="true",
                   XDG_CONFIG_HOME=str(root / "config"), XDG_DATA_HOME=str(root / "data"),
                   XDG_STATE_HOME=str(root / "state"))
        version = subprocess.check_output([BINARY, "--version"], env=env, cwd=root, text=True, timeout=20).strip()
        process = subprocess.run(
            [BINARY, "run", "--format", "json", "--model", qualified, request["prompt"]],
            cwd=root, env=env, capture_output=True, encoding="utf-8", timeout=420,
        )
        calls, output, errors, usage = [], [], [], []
        session = None
        for line in process.stdout.splitlines():
            try:
                event = json.loads(line)
            except ValueError:
                continue
            session = event.get("sessionID", session)
            part = event.get("part", {})
            if event.get("type") == "text":
                output.append(part.get("text", ""))
            if event.get("type") == "step_finish":
                usage.append(part.get("tokens", {}))
            if event.get("type") == "error":
                errors.append(str(event.get("error"))[:900])
            if event.get("type") == "tool_use" and part.get("tool") in names:
                state = part.get("state", {})
                result = state.get("output") or state.get("error")
                try:
                    result = json.loads(result)
                except (ValueError, TypeError):
                    pass
                calls.append({"tool": part["tool"].removeprefix("office_"),
                              "arguments": state.get("input"), "result": result,
                              "status": state.get("status")})
        output = {"client": "OpenCode", "version": version, "uid": os.geteuid(), "session_id": session,
                  "machine_id_sha256": hashlib.sha256(pathlib.Path("/etc/machine-id").read_bytes()).hexdigest(),
                  "calls": calls, "exit_code": process.returncode, "messages": output, "usage": usage,
                  "errors": errors, "stderr": process.stderr[-1800:]}
        print(json.dumps(output, ensure_ascii=True).replace(request["bearer"], "[redacted]"))
        return process.returncode


if __name__ == "__main__":
    sys.exit(run())
