"""Exercise real inference and tool calls through the pinned FreeLLMAPI gateway."""

import contextlib
import hashlib
import json
import os
import re
import secrets
import subprocess
import time
import urllib.error
import urllib.request
from uuid import uuid4


IMAGE = (
    "ghcr.io/tashfeenahmed/freellmapi@"
    "sha256:798db99b52b67f7751c382332a9ef677fc0b3f120f61bfa4db3a14d19a85ec18"
)
RELEASE = "v0.16.30"
MODEL = "qwen3.5:4b"
GATEWAY_MODEL = "cocodeop-qwen-3.5-4b"
MODEL_DIGEST = "d8b0f5e9760cd1682034f292d7ef72ec46f432149be0df7574bf2d6e92e38c04"
HTTP = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def request(url, path, *, headers=None, body=None, timeout=7):
    headers = dict(headers or {})
    if body is not None:
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(url + path, headers=headers,
                                 data=json.dumps(body).encode() if body is not None else None)
    try:
        with HTTP.open(req, timeout=timeout) as response:
            return json.load(response)
    except urllib.error.HTTPError as error:
        raise RuntimeError(f"{path} returned HTTP {error.code}") from None


def docker(*args, env=None, check=True):
    result = subprocess.run(["docker", *args], env=env, capture_output=True,
                            text=True, encoding="utf-8", errors="replace", timeout=90, check=False)
    if check and result.returncode:
        raise RuntimeError(f"Docker {args[0]} failed with exit {result.returncode}: {result.stderr.strip()}")
    return result.stdout.strip()


@contextlib.contextmanager
def gateway(model=MODEL, digest=MODEL_DIGEST, alias=GATEWAY_MODEL):
    inventory = request("http://127.0.0.1:11434", "/api/tags")["models"]
    assert any(entry["name"] == model and entry["digest"] == digest for entry in inventory), model
    name_for_catalog = "CoCodeOp Qwen 3.5 4B" if alias == GATEWAY_MODEL else alias
    admin = {"email": "cocodeop-fixture@example.invalid", "password": secrets.token_hex(24)}
    bootstrap = {
        "admin": admin,
        "customProviders": [{
            "baseUrl": "http://host.docker.internal:11434/v1", "label": "CoCodeOp local Ollama",
            "apiKey": "ollama",
            "models": [{"model": model, "displayName": name_for_catalog, "supportsTools": True}],
        }],
    }
    environment = dict(os.environ, ENCRYPTION_KEY=secrets.token_hex(32), FREEAPI_CONFIG_JSON=json.dumps(bootstrap))
    name = f"cocodeop-freellmapi-{uuid4().hex[:12]}"
    docker(
        "run", "--detach", "--name", name, "--user", "1000:1000", "--read-only", "--cap-drop", "ALL",
        "--security-opt", "no-new-privileges:true", "--publish", "127.0.0.1::3001",
        "--tmpfs", "/app/server/data:rw,nosuid,noexec,size=128m,uid=1000,gid=1000,mode=0700",
        "--tmpfs", "/tmp:rw,nosuid,noexec,size=64m,mode=1777",
        "--env", "PORT=3001", "--env", "NODE_ENV=production", "--env", "ENCRYPTION_KEY",
        "--env", "FREEAPI_CONFIG_JSON", IMAGE, env=environment,
    )
    try:
        url = "http://" + docker("port", name, "3001/tcp")
        until = time.monotonic() + 40
        while True:
            try:
                request(url, "/api/ping", timeout=3)
                break
            except (OSError, RuntimeError):
                state = docker("inspect", "--format", "{{.State.Status}}", name)
                if time.monotonic() >= until or state != "running":
                    logs = subprocess.run(["docker", "logs", "--tail", "15", name],
                                          text=True, encoding="utf-8", errors="replace",
                                          stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=10).stdout
                    for value in (admin["password"], environment["ENCRYPTION_KEY"]):
                        logs = logs.replace(value, "[redacted]")
                    logs = re.sub(r"freellmapi-[A-Za-z0-9_-]{16,}", "[redacted]", logs)
                    raise RuntimeError(f"FreeLLMAPI did not become ready. Container state {state}\n{logs}") from None
                time.sleep(0.5)
        dashboard_token = request(url, "/api/auth/login", body=admin)["token"]
        metadata = request(url, "/api/settings/api-key", headers={"X-Dashboard-Token": dashboard_token})
        key = metadata["apiKey"]
        assert key.startswith("freellmapi-")
        models = request(url, "/v1/models", headers={"Authorization": f"Bearer {key}"})["data"]
        assert any(entry["id"] == alias and entry["available"] for entry in models), alias
        yield url, key
    finally:
        docker("rm", "--force", name, check=False)


def probe(url, key):
    headers = {"Authorization": f"Bearer {key}"}
    models = request(url, "/v1/models", headers=headers)["data"]
    model = next(item for item in models if item["id"] == GATEWAY_MODEL)
    assert model["available"] is True and "tools" in model["supported_parameters"]
    tool = {"type": "function", "function": {
        "name": "inspect_fixture", "description": "Read a receipt for a local test fixture.",
        "parameters": {"type": "object", "properties": {"name": {"type": "string"}},
                       "required": ["name"], "additionalProperties": False},
    }}
    messages = [{"role": "user", "content": (
        "Call inspect_fixture with name exactly probe. When the tool returns, "
        "write the receipt value in your final answer exactly."
    )}]
    common = {"model": GATEWAY_MODEL, "temperature": 0, "max_tokens": 1400, "tools": [tool]}
    generated = request(url, "/v1/chat/completions", headers=headers,
                        body=dict(common, messages=messages), timeout=120)
    choice = generated["choices"][0]["message"]
    assert len(choice["tool_calls"]) == 1
    call = choice["tool_calls"][0]
    assert call["function"]["name"] == "inspect_fixture"
    assert json.loads(call["function"]["arguments"]) == {"name": "probe"}
    data = secrets.token_bytes(16)
    actual = "receipt-" + hashlib.sha256(data).hexdigest()[:12]
    messages.extend([
        {"role": "assistant", "content": choice.get("content") or "", "tool_calls": [call]},
        {"role": "tool", "tool_call_id": call["id"], "content": json.dumps({"receipt": actual})},
    ])
    answered = request(url, "/v1/chat/completions", headers=headers,
                       body=dict(common, messages=messages), timeout=120)
    assert actual in answered["choices"][0]["message"]["content"]
    return {
        "release": RELEASE, "image": IMAGE, "endpoint": "chat_completions",
        "inference_provider": "local Ollama custom endpoint", "model": MODEL, "model_digest": MODEL_DIGEST,
        "gateway_model": GATEWAY_MODEL,
        "tool_call_verified": True, "tool_result_roundtrip_verified": True,
        "managed_cocodeop_worker": False,
    }


if __name__ == "__main__":
    with gateway() as (endpoint, token):
        print(json.dumps(probe(endpoint, token)))
