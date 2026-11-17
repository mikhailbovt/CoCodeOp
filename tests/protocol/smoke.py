"""Run isolated M00 clients against one disposable source directory."""

import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import subprocess
import tempfile
import time
from uuid import uuid4


SERVER_IMAGE = "cocodeop-m00-server:0.1.0"
CLIENT_IMAGE = "cocodeop-m00-clients:0.1.0"
MCP_URL = "http://127.0.0.1:8765/mcp"
SANDBOX = ["--read-only", "--cap-drop=ALL", "--security-opt=no-new-privileges", "--pids-limit=256"]


def docker(*args, timeout=30, env=None, required=True):
    process = subprocess.run(["docker", *args], text=True, capture_output=True, check=False,
                             timeout=timeout, env=env)
    if required and process.returncode != 0:
        raise RuntimeError(f"Docker operation {args[0]} failed with exit code {process.returncode}")
    return process


def read_startup(name):
    end = time.monotonic() + 10
    while time.monotonic() < end:
        output = docker("logs", name, required=False).stdout.splitlines()
        if output:
            info = json.loads(output[0])
            root = info["fixture_root"]
            if info["url"] != MCP_URL or not re.fullmatch(r"/fixture/cocodeop-m00-[a-zA-Z0-9]+", root):
                raise RuntimeError("The probe did not create a disposable checkout")
            return root
        time.sleep(0.2)
    raise RuntimeError("The probe server did not report readiness")


def client(prefix, suffix, volume, network, *args, vals=None, auth_file=None):
    env = os.environ.copy()
    env_args = []
    if vals:
        for key, value in vals.items():
            env[key] = value
            env_args.extend(["--env", key])
    name = f"{prefix}-{suffix}"
    auth_mount = []
    if auth_file:
        path = Path(auth_file).expanduser().resolve(strict=True)
        if not path.is_file() or "," in str(path):
            raise ValueError("Expected a Codex auth.json file on the current host")
        auth_mount = ["--mount", f"type=bind,source={path},target=/run/cocodeop-provider-auth.json,readonly"]
    try:
        process = docker("run", "--rm", "--name", name, *SANDBOX, "--memory=1g",
                         "--network", network, "--tmpfs", "/tmp:rw,nodev,nosuid,mode=1777,size=128m",
                         "--mount", f"type=volume,source={volume},target=/fixture,readonly",
                         *env_args, *auth_mount, CLIENT_IMAGE, *args, env=env, timeout=150, required=False)
        try:
            data = json.loads(process.stdout)
        except json.JSONDecodeError:
            data = {"report_error": "client_did_not_write_a_report", "mode": args[0]}
        data["container_exit"] = process.returncode
        return data
    finally:
        docker("rm", "--force", name, required=False)


def run(work, selections):
    prefix = f"cocodeop-m00-{uuid4().hex[:12]}"
    volume = f"{prefix}-source"
    server = f"{prefix}-server"
    net = f"container:{server}"
    tokens = {actor: secrets.token_hex(32) for actor in ("lead-a", "lead-b")}
    hashes = {actor.replace("-", "_"): hashlib.sha256(token.encode()).hexdigest()
              for actor, token in tokens.items()}
    work.chmod(0o755)
    (work / "credentials.json").write_text(json.dumps(hashes), encoding="utf-8")
    (work / "source-config.toml").write_text(
        '[mcp_servers.unapproved_fixture]\nurl = "http://127.0.0.1:1/mcp"\n', encoding="utf-8")
    docker("volume", "create", volume)
    try:
        docker("run", "--detach", "--rm", "--name", server, *SANDBOX,
               "--mount", f"type=bind,source={work},target=/config,readonly",
               "--mount", f"type=volume,source={volume},target=/fixture",
               "--env", "TMPDIR=/fixture", SERVER_IMAGE, "/config/credentials.json", "127.0.0.1:8765")
        root = read_startup(server)
        docker("exec", server, "install", "-D", "/config/source-config.toml", f"{root}/.codex/config.toml")

        native = client(prefix, "native", volume, "none", "native-write-check", root)
        negative = client(prefix, "offline", volume, net, "codex-missing-office", "lead-a", MCP_URL,
                          vals={"OFFICE_TOKEN": tokens["lead-a"]})
        opencode_negative = None
        if "opencode" in selections.values():
            opencode_negative = client(prefix, "offline-opencode", volume, net, "opencode-missing-office", "lead-a",
                                       MCP_URL, vals={"OFFICE_TOKEN": tokens["lead-a"]})
        cases = []
        with ThreadPoolExecutor(max_workers=2) as pool:
            for actor, backend in selections.items():
                if backend:
                    actor_env = f"COCODEOP_{actor.upper().replace('-', '_')}"
                    name = "OPENAI_API_KEY" if backend == "codex" else "GEMINI_API_KEY"
                    vals = {"OFFICE_TOKEN": tokens[actor]}
                    source = "unset"
                    auth_file = None
                    if backend == "codex":
                        auth_file = os.getenv(f"{actor_env}_CODEX_AUTH_FILE") or os.getenv("COCODEOP_CODEX_AUTH_FILE")
                    if auth_file:
                        source = "local_codex_auth_file"
                    elif backend == "opencode":
                        source = "local_ollama"
                    elif key := os.getenv(f"{actor_env}_{name}"):
                        vals[name] = key
                        source = "per_actor_env"
                    elif key := os.getenv(name):
                        vals[name] = key
                        source = "shared_env"
                    future = pool.submit(client, prefix, actor, volume, net, backend, actor, MCP_URL,
                                         vals=vals, auth_file=auth_file)
                    cases.append((actor, source, future))

            real_clients = []
            for actor, source, future in cases:
                result = future.result()
                result["provider_credential_source"] = source
                file = docker("exec", server, "cat", f"{root}/{actor}.txt", required=False)
                result["physical_file_verified"] = file.returncode == 0 and file.stdout == f"CoCodeOp M00 {actor}"
                real_clients.append(result)

        files = docker("exec", server, "ls", "-a1", root).stdout.splitlines()
        denied_files_absent = not {"native-node.txt", "native-child.txt"}.intersection(files)
        result = {
            "native_write": native,
            "missing_office": negative,
            "opencode_missing_office": opencode_negative,
            "native_markers_absent": denied_files_absent,
            "clients": real_clients,
        }
        passed = denied_files_absent and native["container_exit"] == 0 and negative["container_exit"] == 0
        passed = passed and (opencode_negative is None or opencode_negative["container_exit"] == 0)
        passed = passed and all(c["container_exit"] == 0 and c["physical_file_verified"] for c in real_clients)
        print(json.dumps(result, indent=2))
        return 0 if passed else 1
    finally:
        docker("rm", "--force", server, required=False)
        docker("volume", "rm", volume, required=False)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lead-a", choices=("codex", "gemini", "opencode"))
    parser.add_argument("--lead-b", choices=("codex", "gemini", "opencode"))
    options = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="cocodeop-m00-") as temp:
        raise SystemExit(run(Path(temp), {"lead-a": options.lead_a, "lead-b": options.lead_b}))
