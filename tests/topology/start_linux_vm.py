"""Boot an isolated Debian guest and report a verified SSH connection."""

import argparse
import hashlib
import json
import re
import shutil
import subprocess
import time
from pathlib import Path
from uuid import uuid4


ROOT = Path(__file__).resolve().parents[2]
LAB = ROOT / ".cocodeop-test" / "linux-vm"
IMAGE = "cocodeop-topology-qemu:debian13"
NAME = "cocodeop-topology-linux"
IMAGE_SHA512 = (
    "3247873136ba09b060fab45e9c115239f81c1d401fce00b8fa9b872013f7135d6a4"
    "0b12462ab5ac03b9fa4ed5955dd0e8d1c900109a939ad7a0a4dafe7dea47b"
)


def docker(*args, check=True):
    return subprocess.run(["docker", *args], check=check, capture_output=True, text=True, timeout=90)


def ssh_args(port, key, known_hosts):
    return [
        "ssh", "-p", port, "-i", str(key), "-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes",
        "-o", "ConnectTimeout=4", "-o", "StrictHostKeyChecking=accept-new",
        "-o", f"UserKnownHostsFile={known_hosts.as_posix()}",
    ]


def start(lab=LAB, name=NAME, owner="owner-b", base_image=None):
    if not re.fullmatch(r"[a-z][a-z0-9-]{0,47}", name) or owner not in ("owner-a", "owner-b"):
        raise ValueError("Use a dedicated CoCodeOp VM name and one of the test owner accounts")
    lab = lab.resolve()
    lab.mkdir(parents=True, exist_ok=True)
    disks = lab / "disks"
    disks.mkdir(exist_ok=True)
    outside = lab / "base.qcow2"
    base = disks / "base.qcow2"
    if not base.exists() and outside.exists():
        outside.rename(base)
    if not base.exists() and base_image:
        with base_image.open("rb") as stream:
            if hashlib.file_digest(stream, "sha512").hexdigest() != IMAGE_SHA512:
                raise RuntimeError("Unexpected checksum of source Debian image")
        shutil.copyfile(base_image, base)
    with base.open("rb") as stream:
        if hashlib.file_digest(stream, "sha512").hexdigest() != IMAGE_SHA512:
            raise RuntimeError("Unexpected Debian image checksum")

    key = lab / "lab-ed25519"
    known_hosts = lab / "known-hosts"
    if not key.exists():
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(key)], check=True)
    public_key = key.with_suffix(".pub").read_text().strip()

    mount = ["--mount", f"type=bind,source={disks},target=/lab"]
    if not (disks / "guest.qcow2").exists():
        (disks / "meta-data").write_text(
            f"instance-id: {uuid4()}\nlocal-hostname: {name.removeprefix('cocodeop-topology-')}\n", newline="\n"
        )
        (disks / "user-data").write_text(
            f"""#cloud-config
disable_root: true
ssh_pwauth: false
users:
  - name: lab
    shell: /bin/bash
    sudo: ["ALL=(ALL) NOPASSWD:ALL"]
    lock_passwd: true
    ssh_authorized_keys:
      - {public_key}
  - name: {owner}
    shell: /bin/bash
    lock_passwd: true
""",
            newline="\n",
        )
        docker("run", "--rm", "--network", "none", *mount, "--entrypoint", "cloud-localds", IMAGE,
               "/lab/seed.img", "/lab/user-data", "/lab/meta-data")
        docker("run", "--rm", "--network", "none", *mount, "--entrypoint", "qemu-img", IMAGE,
               "create", "-f", "qcow2", "-F", "qcow2", "-b", "/lab/base.qcow2", "/lab/guest.qcow2", "16G")

    state_id = (disks / "meta-data").read_text().splitlines()[0].removeprefix("instance-id: ")
    status = docker("inspect", "--type", "container", name, check=False)
    if status.returncode == 0:
        existing = json.loads(status.stdout)[0]
        labels = existing["Config"]["Labels"]
        legacy = name == NAME and lab == LAB.resolve() and owner == "owner-b"
        if labels.get("io.cocodeop.topology") != "isolated-linux" or (
            labels.get("io.cocodeop.topology.instance", state_id if legacy else "") != state_id
        ):
            raise RuntimeError(f"Existing container {name} belongs to another topology state")
        if not existing["State"]["Running"]:
            docker("start", name)
    else:
        docker(
            "run", "--detach", "--name", name, "--label", "io.cocodeop.topology=isolated-linux",
            "--label", f"io.cocodeop.topology.instance={state_id}",
            "--publish", "127.0.0.1::2222", "--memory", "5g", "--cpus", "4", "--cap-drop", "ALL",
            "--security-opt", "no-new-privileges:true", *mount, "--entrypoint", "sh", IMAGE,
            "-c", 'socat TCP-LISTEN:2222,reuseaddr,fork TCP:127.0.0.1:2200 & exec qemu-system-x86_64 "$@"',
            "qemu-system-x86_64",
            "-machine", "q35,accel=tcg", "-cpu", "max", "-smp", "4", "-m", "4096",
            "-display", "none", "-monitor", "none", "-serial", "stdio", "-no-reboot",
            "-drive", "file=/lab/guest.qcow2,if=virtio,format=qcow2",
            "-drive", "file=/lab/seed.img,if=virtio,format=raw,readonly=on",
            "-nic", "user,model=virtio-net-pci,restrict=on,ipv6=off,hostfwd=tcp:127.0.0.1:2200-:22",
        )
    address = docker("port", name, "2222/tcp").stdout.strip()
    host, port = address.rsplit(":", 1)
    if host != "127.0.0.1":
        raise RuntimeError(f"Unexpected VM SSH address {address}")

    deadline = time.monotonic() + 180
    while time.monotonic() < deadline:
        result = subprocess.run(
            [*ssh_args(port, key, known_hosts), "lab@127.0.0.1",
             f"cloud-init status && uname -sr && id {owner} && systemd-detect-virt "
             f"&& id -u {owner} && sha256sum /etc/machine-id"],
            capture_output=True, text=True, timeout=30,
        )
        if result.returncode == 0:
            verification = result.stdout.splitlines()
            return {
                "container": name,
                "owner": owner,
                "owner_uid": int(verification[-2]),
                "machine_id_sha256": verification[-1].split()[0],
                "ssh": address,
                "guest_verification": verification[:-2],
                "qemu_version": docker("exec", name, "qemu-system-x86_64", "--version").stdout.splitlines()[0],
                "guest_image_sha512": IMAGE_SHA512,
            }
        time.sleep(2)
    raise RuntimeError(f"Guest SSH was not ready: {result.stderr.strip()}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state-dir", type=Path, default=LAB)
    parser.add_argument("--name", default=NAME)
    parser.add_argument("--owner", choices=("owner-a", "owner-b"), default="owner-b")
    parser.add_argument("--base-image", type=Path)
    options = parser.parse_args()
    print(json.dumps(start(options.state_dir, options.name, options.owner, options.base_image)))
