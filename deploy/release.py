#!/usr/bin/env python3
"""Build checksummed Linux amd64 binaries and offline CoCodeOp image archives."""

import argparse
import hashlib
import json
import re
import shutil
import subprocess
import tarfile
import tempfile
import tomllib
from pathlib import Path

from backup import BackupError, DEPLOY, digest, run, write_json


ROOT = DEPLOY.parent
SHA = re.compile(r"[0-9a-f]{64}\Z")


def docker(*args):
    return run(["docker", *map(str, args)], timeout=3600).decode("utf-8").strip()


def image_id(tag):
    fingerprint = docker("image", "inspect", "--format", "{{.Id}}", tag)
    match = fingerprint.removeprefix("sha256:")
    if not fingerprint.startswith("sha256:") or not SHA.fullmatch(match):
        raise BackupError("Docker did not report an exact image ID")
    return fingerprint


def archive_image(tag, image_path):
    archive = image_path.with_suffix(".tar")
    docker("image", "save", "--output", archive, tag)
    print(f"Built {archive.name}", flush=True)
    return {"tag": tag, "image_id": image_id(tag), "oci_config_digest": config_digest(archive, tag),
            "archive": archive.name}


def config_digest(archive, tag):
    with tarfile.open(archive, "r:") as contents:
        manifest = contents.getmember("manifest.json")
        if not manifest.isfile() or manifest.size > 128 * 1024:
            raise BackupError("Invalid Docker archive manifest")
        entries = json.load(contents.extractfile(manifest))
        matching = [entry for entry in entries if tag in (entry.get("RepoTags") or [])]
        if len(matching) != 1:
            raise BackupError(f"Docker archive has no unique manifest for {tag}")
        config = contents.getmember(matching[0]["Config"])
        if not config.isfile() or config.size > 1024 * 1024:
            raise BackupError("Invalid OCI image config")
        return f"sha256:{hashlib.sha256(contents.extractfile(config).read()).hexdigest()}"


def build(output, windows_cli):
    with (ROOT / "Cargo.toml").open("rb") as cargo:
        version = tomllib.load(cargo)["workspace"]["package"]["version"]
    if not re.fullmatch(r"\d+\.\d+\.\d+(?:-[a-z0-9.]+)?", version):
        raise BackupError("Release version must be an image-tag-compatible SemVer")
    if windows_cli:
        observed = run([str(windows_cli.resolve()), "--version"]).decode().strip()
        if observed not in {f"office {version}", f"office-cli {version}"}:
            raise BackupError(f"CLI version does not match product version {version}")
    output.mkdir(parents=True, exist_ok=False)
    src = DEPLOY / "office-server.Dockerfile"
    prefix = ["build", "--platform", "linux/amd64", "--progress", "plain"]
    images = {}
    with tempfile.TemporaryDirectory(prefix="cocodeop-release-", dir=output.parent) as temporary:
        binaries = Path(temporary)
        docker(*prefix, "--target", "binaries", "--output", f"type=local,dest={binaries}", "-f", src, ROOT)
        with tarfile.open(output / "linux-amd64.tar.gz", "x:gz") as archive:
            for name in ("office", "office-server", "office-runner"):
                info = archive.gettarinfo(str(binaries / name), arcname=name)
                info.mode, info.uid, info.gid, info.uname, info.gname = 0o755, 0, 0, "", ""
                with (binaries / name).open("rb") as payload:
                    archive.addfile(info, payload)
        tag = f"cocodeop-server:{version}"
        docker(*prefix, "--tag", tag, "-f", src, ROOT)
        images["server"] = archive_image(tag, output / "server-linux-amd64")
        # Give Docker only the pinned runner, not the release artifacts or source checkout.
        worker_context = binaries / "worker"
        worker_context.mkdir()
        shutil.copyfile(binaries / "office-runner", worker_context / "office-runner")
        for backend in ("codex", "opencode"):
            tag = f"cocodeop-worker-{backend}:{version}"
            docker(*prefix, "--target", backend, "--tag", tag, "-f",
                   DEPLOY / "office-worker.Dockerfile", worker_context)
            images[backend] = archive_image(tag, output / f"worker-{backend}-linux-amd64")
    if windows_cli:
        shutil.copyfile(windows_cli, output / "office-windows-amd64.exe")
    return package_support(output, version, images)


def package_support(output, version, images):
    support = output / "support"
    support.mkdir()
    for directory in ("schemas", "migrations", "docs", "plugins", ".agents", ".claude-plugin", "deploy"):
        shutil.copytree(ROOT / directory, support / directory, ignore=shutil.ignore_patterns("__pycache__"))
    for filename in ("README.md", "LICENSE"):
        shutil.copyfile(ROOT / filename, support / filename)
    # Release Compose refers only to the image loaded from server-linux-amd64.tar.
    (support / "server.env").write_text(f"COCODEOP_SERVER_IMAGE={images['server']['image_id']}\n", encoding="utf-8")
    tracked = run(["git", "-C", str(ROOT), "rev-parse", "HEAD"]).decode().strip()
    unchanged = not run(["git", "-C", str(ROOT), "status", "--porcelain=v1", "-z"])
    write_json(output / "release.json", {"format": "cocodeop.release.v1", "version": version,
                                       "base_commit": tracked, "working_tree_clean": unchanged,
                                       "platform": "linux/amd64", "images": images})
    entries = [path for path in output.rglob("*") if path.is_file()]
    with (output / "SHA256SUMS").open("x", encoding="utf-8", newline="\n") as checksums:
        for path in sorted(entries):
            checksums.write(f"{digest(path)}  {path.relative_to(output).as_posix()}\n")
    return verify(output)


def verify(directory):
    root = directory.resolve(strict=True)
    verified = set()
    for line in (root / "SHA256SUMS").read_text(encoding="utf-8").splitlines():
        fingerprint, separator, name = line.partition("  ")
        file = root / name
        if (not separator or not SHA.fullmatch(fingerprint) or not file.is_file()
                or file.resolve() != file or not file.is_relative_to(root) or name in verified):
            raise BackupError("Unexpected path or SHA-256 in the release checksum file")
        if digest(file) != fingerprint:
            raise BackupError(f"Checksum mismatch for {name}")
        verified.add(name)
    if verified != {path.relative_to(root).as_posix() for path in root.rglob("*") if path.is_file()} - {"SHA256SUMS"}:
        raise BackupError("Release checksum inventory does not match the bundle")
    metadata = json.loads((root / "release.json").read_bytes())
    for image in metadata["images"].values():
        if image["archive"] not in verified or not re.fullmatch(r"sha256:[0-9a-f]{64}", image["image_id"]):
            raise BackupError("The image archive does not match the release manifest")
        if image["oci_config_digest"] != config_digest(root / image["archive"], image["tag"]):
            raise BackupError("The OCI config digest does not match the release manifest")
    return {"version": metadata["version"], "files_verified": len(verified), "path": str(root),
            "server_image": metadata["images"]["server"]["image_id"]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    build_options = commands.add_parser("build")
    build_options.add_argument("--output", type=Path, required=True)
    build_options.add_argument("--windows-cli", type=Path)
    verify_options = commands.add_parser("verify")
    verify_options.add_argument("--directory", type=Path, required=True)
    args = parser.parse_args()
    try:
        report = build(args.output, args.windows_cli) if args.command == "build" else verify(args.directory)
        print(json.dumps(report, sort_keys=True))
    except (BackupError, OSError, ValueError, KeyError, subprocess.TimeoutExpired, tarfile.TarError) as error:
        parser.exit(1, f"Release operation failed. {error}\n")


if __name__ == "__main__":
    main()
