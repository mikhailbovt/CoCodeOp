"""Load one verified release worker image into a test VM owner's rootless Podman store."""

import argparse
import hashlib
import json
import re
import shlex
import subprocess
from pathlib import Path
from uuid import uuid4

from live_clients import GUEST, connection


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while data := source.read(1024 * 1024):
            digest.update(data)
    return digest.hexdigest()


def load(bundle, backend, vm):
    metadata = json.loads((bundle / "release.json").read_text(encoding="utf-8"))["images"][backend]
    archive_name = metadata["archive"]
    assert archive_name == f"worker-{backend}-linux-amd64.tar"
    archive = (bundle / archive_name).resolve(strict=True)
    assert archive.parent == bundle.resolve()
    expected = next(line.split("  ", 1)[0] for line in (bundle / "SHA256SUMS").read_text().splitlines()
                    if line.endswith("  " + archive_name))
    assert sha256(archive) == expected, "Worker archive checksum mismatch"
    image = metadata["oci_config_digest"].removeprefix("sha256:")
    assert re.fullmatch("[0-9a-f]{64}", image)
    topology, ssh, scp = connection(vm)
    owner, uid = topology["owner"], topology["owner_uid"]
    assert owner in ("owner-a", "owner-b") and isinstance(uid, int) and uid > 0
    podman = ["sudo", "-n", "-u", owner, "-H", "--", "env", "-C", "/tmp",
              f"XDG_RUNTIME_DIR=/run/user/{uid}", f"DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{uid}/bus",
              "/usr/bin/podman", "--remote=false"]

    def remote(arguments, *, timeout=30, check=True):
        return subprocess.run([*ssh, GUEST, shlex.join(arguments)], capture_output=True,
                              text=True, encoding="utf-8", timeout=timeout, check=check)

    if remote([*podman, "image", "exists", "sha256:" + image], check=False).returncode != 0:
        stage = f"cocodeop-image-{uuid4().hex}.tar"
        target = f"/home/{owner}/{stage}"
        try:
            subprocess.run([*scp, str(archive), f"{GUEST}:{stage}"], check=True, timeout=250)
            remote(["sudo", "-n", "install", "-m", "600", "-o", owner, "-g", owner, stage, target])
            imported = remote([*podman, "load", "-i", target], timeout=340, check=False)
            assert imported.returncode == 0, imported.stderr
        finally:
            remote(["rm", "-f", "--", stage], check=False)
            remote(["sudo", "-n", "-u", owner, "rm", "-f", "--", target], check=False)
    observed = remote([*podman, "image", "inspect", "--format={{.Id}}", "sha256:" + image]).stdout.strip()
    assert observed.removeprefix("sha256:") == image, (image, observed)
    return {"backend": backend, "image_id": image, "owner": owner, "guest": topology["machine_id_sha256"]}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bundle", type=Path, required=True)
    parser.add_argument("--backend", choices=["codex", "opencode"], required=True)
    parser.add_argument("--vm", type=Path, required=True)
    options = parser.parse_args()
    print(json.dumps(load(options.bundle, options.backend, options.vm)))
