"""Install trusted local Debian packages and prepare rootless Podman on a test VM."""

import argparse
import json
import shlex
import subprocess
from pathlib import Path
from uuid import uuid4

from live_clients import GUEST, connection


def provision(lab, package_dirs):
    vm, ssh, scp = connection(lab)
    owner = vm["owner"]
    uid = int(vm["owner_uid"])
    if owner not in ("owner-a", "owner-b") or uid < 1000:
        raise ValueError("Expected an unprivileged topology fixture account")

    def remote(arguments, timeout=90):
        result = subprocess.run([*ssh, GUEST, shlex.join(arguments)], capture_output=True,
                                text=True, encoding="utf-8", timeout=timeout)
        if result.returncode:
            raise RuntimeError(result.stderr[-2500:] or result.stdout[-2000:])
        return result.stdout.strip()

    for directory in package_dirs:
        packages = sorted(directory.resolve().glob("*.deb"))
        if not packages:
            raise ValueError(f"No Debian packages in {directory}")
        stage = f"/tmp/cocodeop-packages-{uuid4().hex}"
        remote(["install", "-d", "-m", "755", stage])
        paths = [f"{stage}/{package.name}" for package in packages]
        try:
            subprocess.run([*scp, *map(str, packages), f"{GUEST}:{stage}/"], check=True, timeout=180)
            remote(["sudo", "-n", "env", "DEBIAN_FRONTEND=noninteractive", "dpkg",
                    "--install", *paths], timeout=240)
        finally:
            remote(["rm", "-f", "--", *paths])
            remote(["rmdir", "--", stage])
    remote(["sudo", "-n", "loginctl", "enable-linger", owner])
    remote(["sudo", "-n", "systemctl", "start", f"user@{uid}.service"])
    rootless = remote(["sudo", "-n", "-Hu", owner, "env", "-C", "/tmp", f"XDG_RUNTIME_DIR=/run/user/{uid}",
                       f"DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{uid}/bus", "/usr/bin/podman",
                       "--remote=false", "info", "--format", "{{.Host.Security.Rootless}}"])
    if rootless != "true":
        raise RuntimeError("The guest did not report a rootless container engine")
    return {"owner": owner, "machine_id_sha256": vm["machine_id_sha256"], "rootless": True,
            "podman": remote(["/usr/bin/podman", "--version"]), "git": remote(["/usr/bin/git", "--version"])}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state-dir", type=Path, required=True)
    parser.add_argument("--packages", action="append", type=Path, default=[])
    options = parser.parse_args()
    print(json.dumps(provision(options.state_dir, options.packages)))
