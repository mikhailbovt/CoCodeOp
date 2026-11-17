# Release packages

A release includes Linux amd64 binaries, the Core and worker image archives, pinned Compose files, migrations, JSON schemas and the Codex and Claude Code plugins. The Windows CLI is included when supplied at build time.

## Install from an archive

Use the [Core installation steps](getting-started.md#1-start-the-core). Start at the package root, verify `SHA256SUMS` and load `server-linux-amd64.tar`. From `support`, initialize a private secrets directory and start Compose using `server.env` and both Compose files.

```sh
docker compose --env-file server.env -p cocodeop \
  -f deploy/compose.yaml -f deploy/compose.release.yaml up --no-build --wait
```

The release overlay uses the loaded image ID. Keep the same Compose project throughout installation and maintenance. Use the worker OCI **config** digest in runner manifests. [Verify a runner image](runners.md#load-the-release-image)

## Build and verify

From a clean source checkout, use Docker with BuildKit and Python 3.11 or newer.

```sh
python3 deploy/release.py build --output /private/cocodeop-release
python3 deploy/release.py verify --directory /private/cocodeop-release
```

For a Windows CLI, build `office` on Windows with `cargo build --locked --release -p office-cli` and pass `--windows-cli /path/to/office.exe` to the packaging command. The package records the source commit, image IDs and OCI config digests. Publish the manifest and checksums alongside the release assets.
