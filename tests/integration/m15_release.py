"""Start the local release image by its verified ID and exercise the installed CLI."""

import argparse
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path
from uuid import uuid4

from m01 import wait_for_status
from workspace_fixture import ROOT


def verify(directory, root):
    metadata = json.loads(subprocess.check_output(
        [sys.executable, ROOT / "deploy/release.py", "verify", "--directory", directory],
        text=True, encoding="utf-8", timeout=180,
    ))
    support = directory / "support"
    subprocess.run(["docker", "image", "load", "--input", str(directory / "server-linux-amd64.tar")],
                   check=True, capture_output=True, timeout=120)
    env = dict(os.environ, COCODEOP_SECRETS_DIR=str(root / "secrets"), COCODEOP_HTTP_PORT="0")
    env.pop("COCODEOP_SERVER_IMAGE", None)
    project = f"cocodeop-release-{uuid4().hex[:10]}"
    prefix = ["docker", "compose", "--project-name", project, "--env-file", support / "server.env",
              "-f", support / "deploy/compose.yaml", "-f", support / "deploy/compose.release.yaml"]
    subprocess.run([sys.executable, support / "deploy/init-secrets.py", root / "secrets"], check=True, timeout=30)

    def compose(*args, check=True):
        result = subprocess.run([*map(str, prefix), *map(str, args)], env=env, capture_output=True,
                                encoding="utf-8", timeout=220)
        if check:
            assert result.returncode == 0, (args, result.stdout[-2000:], result.stderr[-2000:])
        return result.stdout.strip()

    try:
        compose("up", "--no-build", "--wait", "--wait-timeout", "90")
        endpoint = compose("port", "server", "8400")
        readiness = wait_for_status(endpoint, "/readyz", 200)["result"]
        instance = compose("ps", "-q", "server")
        observed_image = subprocess.check_output(
            ["docker", "inspect", "--format", "{{.Image}}", instance], text=True, timeout=15,
        ).strip()
        assert observed_image == metadata["server_image"], observed_image
        package = directory / "office-windows-amd64.exe"
        if os.name == "nt" and package.is_file():
            observed = json.loads(subprocess.check_output(
                [str(package), "status", "--url", f"http://{endpoint}", "--json"],
                text=True, encoding="utf-8", timeout=12,
            ))
        else:
            observed = json.loads(compose("exec", "-T", "server", "office", "status", "--json"))
        assert observed["result"] == readiness, observed
        return {"version": metadata["version"], "server_image": observed_image,
                "checksummed_files": metadata["files_verified"], "schema_version": readiness["schema_version"],
                "cli_transport": "windows" if os.name == "nt" and package.is_file() else "linux",
                "started_without_build": True}
    finally:
        compose("down", "--volumes", "--remove-orphans", check=False)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--directory", type=Path, required=True)
    config = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="cocodeop-release-smoke-") as directory:
        print(json.dumps(verify(config.directory.resolve(), Path(directory))))
