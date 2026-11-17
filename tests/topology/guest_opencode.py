"""Run one MCP check as the unprivileged owner-b account in the Debian guest."""

import json
import os
import platform
import subprocess
import sys
import tempfile
from pathlib import Path
from urllib.request import Request, urlopen


MODEL = "qwen3.5:4b"
GATEWAY_MODEL = "cocodeop-qwen-3.5-4b"
MODEL_DIGEST = "d8b0f5e9760cd1682034f292d7ef72ec46f432149be0df7574bf2d6e92e38c04"
BINARY = "/opt/cocodeop-test/opencode"


def run():
    request = json.load(sys.stdin)
    assert os.geteuid() != 0
    if "gateway_key" in request:
        provider, model = "freellmapi", GATEWAY_MODEL
        options = {"baseURL": "http://127.0.0.1:18401/v1", "apiKey": request["gateway_key"]}
        inventory = Request(f"{options['baseURL']}/models", headers={
            "Authorization": f"Bearer {request['gateway_key']}",
        })
        with urlopen(inventory, timeout=5) as response:
            assert any(entry["id"] == model and entry["available"] for entry in json.load(response)["data"])
    else:
        provider, model = "ollama", MODEL
        options = {"baseURL": "http://127.0.0.1:11434/v1"}
        with urlopen("http://127.0.0.1:11434/api/tags", timeout=5) as response:
            models = json.load(response)["models"]
            assert any(entry["name"] == MODEL and entry["digest"] == MODEL_DIGEST for entry in models)

    call = request.get("call", {"tool": "office_bootstrap", "arguments": {}})
    tool_name = "office_" + call["tool"]
    prompt = (
        f"Call CoCodeOp tool {tool_name} with these exact JSON arguments: "
        f"{json.dumps(call['arguments'])}. After receiving the tool result, report it briefly."
    )

    with tempfile.TemporaryDirectory(prefix="cocodeop-mcp-") as directory:
        root = Path(directory)
        config = root / "opencode.json"
        configuration = {
            "share": "disabled",
            "snapshot": False,
            "enabled_providers": [provider],
            "model": f"{provider}/{model}",
            "small_model": f"{provider}/{model}",
            "permission": {"*": "deny", tool_name: "allow"},
            "provider": {provider: {
                "npm": "@ai-sdk/openai-compatible",
                "name": provider,
                "options": options,
                "models": {model: {"name": "Qwen 3.5 4B", "tool_call": True,
                                   "limit": {"context": 16384, "output": 2048}}},
            }},
            "mcp": {"office": {
                "type": "remote", "url": "http://127.0.0.1:18400/mcp",
                "headers": {"Authorization": "Bearer " + request["bearer"]},
                "oauth": False, "timeout": 5000,
            }},
        }
        with os.fdopen(os.open(config, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as stream:
            json.dump(configuration, stream)

        environment = dict(
            os.environ, OPENCODE_CONFIG=str(config), OPENCODE_DISABLE_AUTOUPDATE="true",
            OPENCODE_DISABLE_MODELS_FETCH="true", OPENCODE_DISABLE_LSP_DOWNLOAD="true",
            XDG_CONFIG_HOME=str(root / "config"), XDG_DATA_HOME=str(root / "data"),
            XDG_STATE_HOME=str(root / "state"),
        )
        version = subprocess.check_output([BINARY, "--version"], text=True, cwd=root, env=environment).strip()
        process = subprocess.run(
            [BINARY, "run", "--format", "json", "--model", f"{provider}/{model}", prompt],
            cwd=root, env=environment, capture_output=True, text=True, timeout=200,
        )
        confirmed = False
        completed = None
        tool_status = []
        tool_errors = []
        for line in process.stdout.splitlines():
            try:
                event = json.loads(line)
            except ValueError:
                continue
            part = event.get("part", {})
            if event.get("type") != "tool_use" or part.get("tool") != tool_name:
                continue
            state = part.get("state", {})
            tool_status.append(state.get("status"))
            if state.get("status") == "error":
                detail = str(state.get("error", ""))
                for field in ("bearer", "gateway_key"):
                    if request.get(field):
                        detail = detail.replace(request[field], "[redacted]")
                tool_errors.append(detail[:1000])
            if state.get("status") == "completed":
                try:
                    result = json.loads(state["output"])
                    if "call" in request:
                        completed = result
                    else:
                        confirmed |= result == request["agent"]
                except (KeyError, TypeError, ValueError):
                    pass
        report = {
            "actor": request["agent"]["display_name"], "client": "OpenCode", "version": version,
            "linux": platform.release(), "uid": os.geteuid(), "model": MODEL, "model_digest": MODEL_DIGEST,
            "exit_code": process.returncode, "tool_status": tool_status,
        }
        if "call" in request:
            report.update(provider=provider, provider_model=model, tool_result=completed)
            if tool_errors:
                report["tool_errors"] = tool_errors
            confirmed = completed is not None and "code" not in completed
        else:
            report["mcp_identity_verified"] = confirmed
        print(json.dumps(report))
        return 0 if process.returncode == 0 and confirmed else 1


if __name__ == "__main__":
    sys.exit(run())
