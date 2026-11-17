"""Run real MCP clients in Windows and in the separately booted Linux guest."""

import argparse
import json
import os
import platform
import shlex
import subprocess
import tempfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from uuid import uuid4

from start_linux_vm import LAB, NAME, docker, ssh_args


OPENCODE_IMAGE = "cocodeop-m00-clients:0.1.0"
OPENCODE_PATH = "/opt/probe/node_modules/opencode-linux-x64-baseline/bin/opencode"
GUEST = "lab@127.0.0.1"


def connection(lab=None):
    lab = Path(lab or os.environ.get("COCODEOP_VM_STATE_DIR", LAB)).resolve()
    document = json.loads((lab / "topology.json").read_text())
    address = docker("port", document.get("container", NAME), "2222/tcp").stdout.strip()
    host, port = address.rsplit(":", 1)
    if host != "127.0.0.1":
        raise RuntimeError("The VM's SSH port must be bound to the local host")
    ssh = ssh_args(port, lab / "lab-ed25519", lab / "known-hosts")
    scp = ["scp", "-q", "-P", port, *ssh[3:]]
    return document, ssh, scp


def provision(lab=None):
    _, ssh, scp = connection(lab)
    script = Path(__file__).with_name("guest_opencode.py")
    stage = f"cocodeop-stage-{uuid4().hex}"
    with tempfile.TemporaryDirectory() as directory:
        binary = Path(directory) / "opencode"
        container = docker("create", OPENCODE_IMAGE).stdout.strip()
        try:
            docker("cp", f"{container}:{OPENCODE_PATH}", str(binary))
        finally:
            docker("rm", container)
        subprocess.run([*ssh, GUEST, "mkdir -m 700 " + stage], check=True, timeout=15)
        subprocess.run([*scp, str(binary), str(script), f"{GUEST}:{stage}/"], check=True, timeout=160)
        subprocess.run(
            [*ssh, GUEST, "set -e; sudo install -d -m 755 /opt/cocodeop-test; "
             f"sudo install -m 755 {stage}/opencode /opt/cocodeop-test/opencode; "
             f"sudo install -m 644 {stage}/guest_opencode.py /opt/cocodeop-test/guest_opencode.py; "
             f"rm -- {stage}/opencode {stage}/guest_opencode.py; rmdir -- {stage}"],
            check=True, timeout=45,
        )


def run_pair(binary, endpoint, working_dir, leads, live_codex):
    document, ssh, _ = connection()
    host, port = endpoint.rsplit(":", 1)
    if host != "127.0.0.1":
        raise RuntimeError("The forwarded MCP server must be on the host loopback")

    def linux():
        agent, token = leads[1]
        command = shlex.join(["sudo", "-n", "-u", "owner-b", "-H", "python3",
                              "/opt/cocodeop-test/guest_opencode.py"])
        process = subprocess.run(
            [*ssh, "-o", "ExitOnForwardFailure=yes", "-R", f"127.0.0.1:18400:127.0.0.1:{port}",
             "-R", "127.0.0.1:11434:127.0.0.1:11434", GUEST, command],
            input=json.dumps({"agent": agent, "bearer": token}), capture_output=True,
            text=True, timeout=240,
        )
        if process.stdout:
            evidence = json.loads(process.stdout)
            if process.returncode == 0 and evidence["mcp_identity_verified"]:
                return evidence
            raise RuntimeError(f"Guest tool verification failed: {evidence}")
        raise RuntimeError(f"Guest command failed with exit code {process.returncode}")

    with ThreadPoolExecutor(max_workers=2) as pool:
        a = pool.submit(live_codex, binary, endpoint, working_dir, leads[:1])
        b = pool.submit(linux)
        host_evidence = a.result()[0]
        guest_evidence = b.result()
    version = subprocess.check_output([binary, "--version"], text=True, timeout=10).strip()
    return {
        "host": dict(host_evidence, platform=platform.system(), client=version),
        "guest": guest_evidence,
        "guest_verification": document["guest_verification"],
        "guest_image_sha512": document["guest_image_sha512"],
        "qemu_version": document["qemu_version"],
        "different_cocodeop_owners": leads[0][0]["owner_user_id"] != leads[1][0]["owner_user_id"],
        "independent_physical_hosts": False,
    }

def guest_tool(endpoint, agent, token, tool, arguments, provider_url, provider_key):
    _, ssh, _ = connection()
    host, core_port = endpoint.rsplit(":", 1)
    provider_base, provider_port = provider_url.rsplit(":", 1)
    if host != "127.0.0.1" or provider_base != "http://127.0.0.1":
        raise RuntimeError("The VM's provider and MCP forwards must use host loopback")
    command = shlex.join(["sudo", "-n", "-u", "owner-b", "-H", "python3", "/opt/cocodeop-test/guest_opencode.py"])
    process = subprocess.run(
        [*ssh, "-o", "ExitOnForwardFailure=yes", "-R", f"127.0.0.1:18400:127.0.0.1:{core_port}",
         "-R", f"127.0.0.1:18401:127.0.0.1:{provider_port}", GUEST, command],
        input=json.dumps({"agent": agent, "bearer": token, "gateway_key": provider_key,
                          "call": {"tool": tool, "arguments": arguments}}),
        capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=240,
    )
    evidence = json.loads(process.stdout) if process.stdout else None
    if process.returncode or evidence is None:
        raise RuntimeError(f"Linux guest did not complete {tool}: {evidence}")
    return evidence


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Install the pinned OpenCode executable on a test VM")
    parser.add_argument("--state-dir", type=Path)
    args = parser.parse_args()
    provision(args.state_dir)
