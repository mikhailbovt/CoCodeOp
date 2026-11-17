"""Create the two local Compose database secrets in a new private directory."""

import argparse
from pathlib import Path
import secrets


def create_secrets(directory: Path):
    directory.mkdir(mode=0o700)
    for name in ("postgres-admin", "cocodeop-db"):
        target = directory / name
        with target.open("x", encoding="ascii", newline="\n") as stream:
            stream.write(secrets.token_hex(32) + "\n")
        # Local Compose bind-mounts files and preserves their host permissions.
        # The private parent limits host access while container non-root users can read the mount.
        target.chmod(0o444)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    create_secrets(args.directory)
