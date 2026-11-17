#!/usr/bin/env python3
"""Coordinated PostgreSQL and checkout backup for the Compose installation."""

import argparse
import hashlib
import json
import os
import re
import shutil
import stat
import subprocess
import tarfile
import tempfile
from pathlib import Path, PurePosixPath
from uuid import UUID, uuid4


DEPLOY = Path(__file__).resolve().parent
ARCHIVE_VERSION = 1
RECORD_LIMIT = 8 * 1024 * 1024
CHECKOUT_FIELDS = (
    "checkout_id", "workspace_id", "host_runner_id", "manifest", "root",
    "head", "state_id", "epoch", "accepted_commit", "snapshots",
)
JOURNAL = re.compile(
    r"(?:coordinator-\d+|scope-[0-9a-f-]{36}-\d+|publication-state|"
    r"publication-[0-9a-f-]{36}\.(?:history|receipt))\.json\Z"
)


class BackupError(RuntimeError):
    pass


def run(args, *, input_data=None, input_file=None, output_file=None, timeout=300, env=None):
    result = subprocess.run(args, input=input_data, stdin=input_file, stdout=output_file or subprocess.PIPE,
                            stderr=subprocess.PIPE, timeout=timeout, env=env)
    if result.returncode:
        diagnostic = result.stderr.decode("utf-8", errors="replace").strip()
        raise BackupError(f"{Path(args[0]).name} exited {result.returncode}\n{diagnostic[-1800:]}")
    return result.stdout or b""


def digest(path):
    sha = hashlib.sha256()
    with path.open("rb") as source:
        while block := source.read(1024 * 1024):
            sha.update(block)
    return sha.hexdigest()


def read_json(path):
    with path.open("rb") as source:
        data = source.read(RECORD_LIMIT + 1)
    if len(data) > RECORD_LIMIT:
        raise BackupError("Backup metadata exceeds the supported size")
    return json.loads(data)


def write_json(path, value):
    with path.open("x", encoding="utf-8", newline="\n") as output:
        json.dump(value, output, sort_keys=True, indent=2)
        output.write("\n")
        output.flush()
        os.fsync(output.fileno())


class Compose:
    def __init__(self, project):
        self.prefix = ["docker", "compose", "-f", str(DEPLOY / "compose.yaml"), "--project-name", project]

    def database(self, *args, **options):
        return run([*self.prefix, "exec", "-T", "db", *args], **options)

    def sql(self, text, *variables):
        return self.database("psql", "-U", "postgres", "-d", "cocodeop", "-XAtq",
                             "-v", "ON_ERROR_STOP=1", *variables, input_data=text.encode("utf-8"))

    def inventory(self):
        return json.loads(self.sql((DEPLOY / "backup-inventory.sql").read_text(encoding="utf-8")))

    def require_offline(self):
        output = run([*self.prefix, "ps", "-q", "--status=running", "server"])
        if output.strip():
            raise BackupError("Stop the Compose server before copying or replacing its database")


def checkout_record(row):
    return {key: row[key] for key in CHECKOUT_FIELDS}


def require_frozen(inventory, *, offline):
    if not inventory["workspaces_stopped"]:
        raise BackupError("Set every backed-up workspace to stopped before freezing the backup")
    if inventory["unresolved_executions"] or inventory["unresolved_checkouts"]:
        raise BackupError("Reconcile running or unconfirmed execution and checkout effects before backup")
    if offline and inventory["live_coordinators"]:
        raise BackupError("Stop checkout coordinators before finalizing the backup")
    if inventory["restore_pending"]:
        raise BackupError("Complete the current restore review before creating another backup")


def current_snapshot(record):
    head = record["head"]
    matches = [row for row in record["snapshots"] if all(row[key] == value for key, value in head.items())]
    if len(matches) != 1:
        raise BackupError(f"Checkout {record['checkout_id']} requires a ready snapshot of its full current head")
    return matches[0]


def inspect_source(runner, root, manifest, directory):
    manifest_path = directory / f"manifest-{uuid4()}.json"
    write_json(manifest_path, manifest)
    try:
        return json.loads(run([str(runner), "checkout", "inspect", "--root", str(root),
                               "--manifest", str(manifest_path), "--json"]))
    finally:
        manifest_path.unlink()


def verify_source(observed, plan, *, enrolled):
    if enrolled and observed["root"] != plan["root"]:
        raise BackupError("The source root identity differs from its enrollment")
    if observed["baseline"]["manifest_sha256"] != plan["head"]["source_sha256"]:
        raise BackupError("The source content differs from the paused checkout head")


def validate_checkpoints(state, plan, runner):
    if (state / "publication-pending.json").exists():
        raise BackupError("The coordinator still has a pending publication journal")
    repo = state / "checkpoints.git"
    with tempfile.TemporaryDirectory(prefix="cocodeop-checkpoint-verify-") as directory:
        temporary = Path(directory)
        for item in plan["snapshots"]:
            snapshot_id = str(UUID(item["snapshot_id"]))
            snapshot = state / "snapshots" / snapshot_id
            recorded = read_json(snapshot / "capture.json")
            if (recorded["order"]["checkout_id"] != plan["checkout_id"]
                    or recorded["order"]["snapshot_id"] != snapshot_id
                    or recorded["proof"] != item["proof"]):
                raise BackupError("A snapshot record differs from its database proof")
            observed = inspect_source(runner, snapshot / "source", plan["manifest"], temporary)
            if observed["baseline"] != item["proof"]["baseline"]:
                raise BackupError("A sealed source snapshot differs from its database proof")
            actual = git_object(repo, f"refs/cocodeop/wip/{snapshot_id}")
            if actual != item["proof"]["git_commit_sha256"]:
                raise BackupError("A WIP checkpoint ref differs from its database proof")
            if git_object(repo, f"{actual}^{{tree}}") != item["proof"]["git_tree_sha256"]:
                raise BackupError("A checkpoint tree differs from its recorded hash")
    if plan["accepted_commit"] is not None:
        if git_object(repo, "refs/cocodeop/accepted") != plan["accepted_commit"]:
            raise BackupError("The accepted checkpoint differs from the database")
    if plan["state_id"] is not None and read_json(state / "publication-state.json")["state_id"] != plan["state_id"]:
        raise BackupError("The publication journal belongs to a different checkout state")


def git_object(repo, reference):
    environment = {"PATH": "/usr/bin:/bin", "LC_ALL": "C", "GIT_CONFIG_NOSYSTEM": "1",
                   "GIT_CONFIG_GLOBAL": "/dev/null"}
    return run(["/usr/bin/git", "--git-dir", str(repo), "rev-parse", "--verify", reference],
               env=environment).decode().strip()


def add_path(archive, path, archive_name):
    metadata = path.lstat()
    if stat.S_ISDIR(metadata.st_mode):
        archive.add(path, arcname=archive_name, recursive=False)
        for child in sorted(path.iterdir()):
            add_path(archive, child, f"{archive_name}/{child.name}")
    elif stat.S_ISREG(metadata.st_mode) and metadata.st_nlink == 1:
        archive.add(path, arcname=archive_name, recursive=False)
    else:
        raise BackupError("Checkout archives require owner-controlled files and directories")


def host_pack(options):
    import fcntl

    inventory = read_json(options.plan)
    checkout_id = str(UUID(options.checkout))
    try:
        plan = next(checkout_record(row) for row in inventory["checkouts"] if row["checkout_id"] == checkout_id)
    except StopIteration as error:
        raise BackupError("The requested checkout is absent from the backup plan") from error
    current_snapshot(plan)
    root = options.root.resolve(strict=True)
    state = options.state_dir.resolve(strict=True)
    fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        with tempfile.TemporaryDirectory(prefix="cocodeop-host-backup-") as directory:
            work = Path(directory)
            verify_source(inspect_source(options.runner, root, plan["manifest"], work), plan, enrolled=True)
            validate_checkpoints(state, plan, options.runner)
            manifest = work / "checkout.json"
            write_json(manifest, plan)
            with options.output.open("xb") as output:
                with tarfile.open(fileobj=output, mode="w:gz") as archive:
                    archive.add(manifest, arcname="checkout.json", recursive=False)
                    add_path(archive, state / "checkpoints.git", "state/checkpoints.git")
                    for snapshot in plan["snapshots"]:
                        name = str(UUID(snapshot["snapshot_id"]))
                        add_path(archive, state / "snapshots" / name, f"state/snapshots/{name}")
                    for path in sorted(state.iterdir()):
                        if JOURNAL.fullmatch(path.name):
                            add_path(archive, path, f"state/{path.name}")
                output.flush()
                os.fsync(output.fileno())
            verify_source(inspect_source(options.runner, root, plan["manifest"], work), plan, enrolled=True)
    finally:
        os.close(fd)
    return {"checkout_id": checkout_id, "archive_sha256": digest(options.output)}


def archive_record(path):
    with tarfile.open(path, mode="r:gz") as archive:
        record = archive.next()
        if record is None or not record.isfile() or record.name != "checkout.json" or record.size > RECORD_LIMIT:
            raise BackupError("Missing checkout backup metadata")
        with archive.extractfile(record) as source:
            return json.load(source)


def bundle_checkouts(inventory, archives):
    expected = {row["checkout_id"]: checkout_record(row) for row in inventory["checkouts"]}
    checked = {}
    for path in archives:
        plan = archive_record(path)
        checkout_id = str(UUID(plan["checkout_id"]))
        if checkout_id in checked or expected.get(checkout_id) != plan:
            raise BackupError("A host archive is duplicated or differs from the frozen database inventory")
        current_snapshot(plan)
        checked[checkout_id] = path
    if checked.keys() != expected.keys():
        raise BackupError("Provide one verified host archive for every registered checkout")
    return checked


def create(office, options):
    office.require_offline()
    inventory = office.inventory()
    require_frozen(inventory, offline=True)
    checkouts = bundle_checkouts(inventory, options.checkout_archive)
    output = options.output
    output.mkdir(mode=0o700, parents=False)
    files = {}
    database = output / "database.dump"
    with database.open("xb") as destination:
        office.database("pg_dump", "-U", "postgres", "-d", "cocodeop", "--format=custom",
                        output_file=destination, timeout=1800)
        destination.flush()
        os.fsync(destination.fileno())
    files[database.name] = digest(database)
    for checkout_id, archive in checkouts.items():
        copy = output / f"{checkout_id}.tar.gz"
        shutil.copyfile(archive, copy)
        files[copy.name] = digest(copy)
    office.require_offline()
    if office.inventory() != inventory:
        raise BackupError("The database inventory changed during capture")
    metadata = {"version": ARCHIVE_VERSION, "backup_id": str(uuid4()), "inventory": inventory, "files": files}
    write_json(output / "backup.json", metadata)
    return {"backup_id": metadata["backup_id"], "file_count": len(files), "schema_version": inventory["schema_version"]}


def checked_bundle(path):
    metadata = read_json(path / "backup.json")
    if metadata["version"] != ARCHIVE_VERSION:
        raise BackupError("Unsupported backup archive version")
    UUID(metadata["backup_id"])
    inventory = metadata["inventory"]
    expected = {"database.dump", *(f"{UUID(row['checkout_id'])}.tar.gz" for row in inventory["checkouts"])}
    if metadata["files"].keys() != expected:
        raise BackupError("The bundle does not contain the complete recorded inventory")
    for name, expected_hash in metadata["files"].items():
        if digest(path / name) != expected_hash:
            raise BackupError(f"Backup file {name} failed SHA-256 verification")
    bundle_checkouts(inventory, [path / name for name in expected if name.endswith(".tar.gz")])
    return metadata


def unpack_checkout(archive_path, target, record):
    snapshot_ids = {item["snapshot_id"] for item in record["snapshots"]}
    names = set()
    with tarfile.open(archive_path, mode="r:gz") as archive:
        for entry in archive:
            if entry.name == "checkout.json":
                continue
            path = PurePosixPath(entry.name)
            if (path.as_posix() != entry.name or path.is_absolute() or ".." in path.parts or "\\" in entry.name
                    or len(path.parts) < 2 or path.parts[0] != "state" or entry.name in names):
                raise BackupError("Invalid checkout archive path")
            section = path.parts[1]
            if not (section == "checkpoints.git"
                    or (section == "snapshots" and len(path.parts) >= 3 and path.parts[2] in snapshot_ids)
                    or (len(path.parts) == 2 and JOURNAL.fullmatch(section))):
                raise BackupError("Unknown checkout archive member")
            names.add(entry.name)
            destination = target.joinpath(*path.parts)
            if entry.isdir():
                destination.mkdir(parents=True, mode=0o700, exist_ok=True)
            elif entry.isfile():
                destination.parent.mkdir(parents=True, mode=0o700, exist_ok=True)
                with archive.extractfile(entry) as source, destination.open("xb") as output:
                    shutil.copyfileobj(source, output)
                destination.chmod(0o600 | (entry.mode & 0o100))
            else:
                raise BackupError("Links and special files are outside the checkout archive format")


def restore_receipt(bundle, record):
    checkout_id = record["checkout_id"]
    return {"backup_id": bundle["backup_id"], "checkout_id": checkout_id,
            "head": record["head"], "archive_sha256": bundle["files"][f"{checkout_id}.tar.gz"]}


def host_restore(options):
    import fcntl

    receipt = read_json(options.plan)
    checkout_id = str(UUID(receipt["checkout_id"]))
    UUID(receipt["backup_id"])
    if digest(options.archive) != receipt["archive_sha256"]:
        raise BackupError("The host archive failed SHA-256 verification")
    record = archive_record(options.archive)
    if record["checkout_id"] != checkout_id or record["head"] != receipt["head"]:
        raise BackupError("The host archive differs from its restore plan")
    snapshot = current_snapshot(record)
    root = options.root.resolve(strict=True)
    state_dir = options.state_dir.resolve()
    if state_dir.exists() or state_dir.is_relative_to(root) or root.is_relative_to(state_dir):
        raise BackupError("Use a new state directory outside the enrolled checkout")
    fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        with tempfile.TemporaryDirectory(prefix="cocodeop-host-restore-", dir=state_dir.parent) as directory:
            work = Path(directory)
            empty = not any(root.iterdir())
            manifest = {"include": [{"kind": "tree"}]} if empty else record["manifest"]
            observed = inspect_source(options.runner, root, manifest, work)
            if observed["root"] != record["root"]:
                raise BackupError("The original enrolled root directory is required for this restore")
            if not empty:
                verify_source(observed, record, enrolled=True)
            unpack_checkout(options.archive, work, record)
            state = work / "state"
            validate_checkpoints(state, record, options.runner)
            if empty:
                source = state / "snapshots" / snapshot["snapshot_id"] / "source"
                shutil.copytree(source, root, dirs_exist_ok=True)
            verify_source(inspect_source(options.runner, root, record["manifest"], work), record, enrolled=True)
            state_dir.mkdir(mode=0o700)
            shutil.copytree(state, state_dir, dirs_exist_ok=True)
    finally:
        os.close(fd)
    write_json(options.receipt, receipt)
    return receipt


def host_plan(options):
    bundle = checked_bundle(options.bundle)
    checkout_id = str(UUID(options.checkout))
    for record in bundle["inventory"]["checkouts"]:
        if record["checkout_id"] == checkout_id:
            plan = restore_receipt(bundle, record)
            write_json(options.output, plan)
            return plan
    raise BackupError("The requested checkout is absent from the backup")


def restore(office, options):
    bundle = checked_bundle(options.bundle)
    office.require_offline()
    empty = office.sql("SELECT (to_regclass('public.office_installation') IS NULL)::text;").strip()
    if empty != b"true":
        raise BackupError("Restore requires a fresh Compose database")
    with (options.bundle / "database.dump").open("rb") as source:
        office.database("pg_restore", "-U", "postgres", "-d", "cocodeop", "--single-transaction",
                        input_file=source, timeout=1800)
    restored = office.inventory()
    if restored != bundle["inventory"]:
        raise BackupError("Restored inventory differs from the backup")
    operations = (DEPLOY / "backup-quarantine.sql").read_text(encoding="utf-8")
    jobs = json.loads(office.sql(operations, "-v", f"backup_id={bundle['backup_id']}"))
    return {"backup_id": bundle["backup_id"], "restore_pending": True, "quarantined_jobs": jobs}


def resume(office, options):
    bundle = checked_bundle(options.bundle)
    archived = bundle["inventory"]
    current = office.inventory()
    if (not current["restore_pending"] or current["restored_backup_id"] != bundle["backup_id"]
            or current["installation_id"] != archived["installation_id"]):
        raise BackupError("This installation is not reviewing the specified backup")
    if current["unresolved_executions"] or current["unresolved_checkouts"]:
        raise BackupError("Reconcile outstanding effects before resuming the restoration")
    previous = {row["checkout_id"]: checkout_record(row) for row in archived["checkouts"]}
    receipts = {}
    for path in options.host_receipt:
        receipt = read_json(path)
        checkout_id = receipt["checkout_id"]
        if checkout_id in receipts or checkout_id not in previous:
            raise BackupError("A checkout restoration receipt is duplicated or foreign to this backup")
        if receipt != restore_receipt(bundle, previous[checkout_id]):
            raise BackupError("A host restored a different checkout archive")
        receipts[checkout_id] = receipt
    if receipts.keys() != previous.keys():
        raise BackupError("Provide one host restoration receipt per registered checkout")
    for entry in current["checkouts"]:
        old = previous.pop(entry["checkout_id"], None)
        if (old is None or old["head"] != entry["head"] or old["state_id"] != entry["state_id"]
                or old["accepted_commit"] != entry["accepted_commit"]
                or entry["ready_epoch"] is None or entry["ready_epoch"] <= old["epoch"]
                or (entry["state_id"] is not None and entry["ready_state_id"] != entry["state_id"])):
            raise BackupError("A restored checkout still requires verification in a new coordinator epoch")
    if previous:
        raise BackupError("One or more archived checkouts are missing")
    saved = office.sql(
        "UPDATE office_installation SET restore_pending=false WHERE singleton AND restore_pending "
        "AND restored_backup_id=:'backup_id'::uuid RETURNING installation_id;",
        "-v", f"backup_id={bundle['backup_id']}",
    )
    if not saved.strip():
        raise BackupError("The restoration state changed during verification")
    return {"backup_id": bundle["backup_id"], "restore_pending": False}


def main():
    if os.name == "posix":
        os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--project", default="deploy", help="Docker Compose project name")
    commands = parser.add_subparsers(dest="command", required=True)
    plan = commands.add_parser("plan", help="Record the paused database and checkout inventory")
    plan.add_argument("--output", type=Path, required=True)
    plan.add_argument("--checkout", help="Emit the inventory of one checkout to its host")
    host = commands.add_parser("host-pack", help="Archive a paused Linux checkout's sealed snapshots and journal")
    host.add_argument("--plan", type=Path, required=True)
    host.add_argument("--checkout", required=True)
    host.add_argument("--root", type=Path, required=True)
    host.add_argument("--state-dir", type=Path, required=True)
    host.add_argument("--runner", type=Path, required=True)
    host.add_argument("--output", type=Path, required=True)
    capture = commands.add_parser("create", help="Create a complete offline backup")
    capture.add_argument("--output", type=Path, required=True)
    capture.add_argument("--checkout-archive", action="append", default=[], type=Path)
    checkout_plan = commands.add_parser("host-plan", help="Emit the scoped plan for a host restore")
    checkout_plan.add_argument("--bundle", type=Path, required=True)
    checkout_plan.add_argument("--checkout", required=True)
    checkout_plan.add_argument("--output", type=Path, required=True)
    host_recovery = commands.add_parser("host-restore", help="Verify and restore a checkout's snapshots and journal")
    for name in ("plan", "archive", "root", "state-dir", "runner", "receipt"):
        host_recovery.add_argument(f"--{name}", type=Path, required=True)
    for command, help_text in [("restore", "Restore to a fresh Compose database"),
                               ("resume", "Release the restore hold after checkout reconciliation")]:
        subcommand = commands.add_parser(command, help=help_text)
        subcommand.add_argument("--bundle", type=Path, required=True)
        if command == "resume":
            subcommand.add_argument("--host-receipt", action="append", default=[], type=Path)
    options = parser.parse_args()
    try:
        office = Compose(options.project)
        if options.command == "plan":
            inventory = office.inventory()
            require_frozen(inventory, offline=False)
            if options.checkout:
                checkout_id = str(UUID(options.checkout))
                inventory["checkouts"] = [row for row in inventory["checkouts"] if row["checkout_id"] == checkout_id]
                if not inventory["checkouts"]:
                    raise BackupError("The requested checkout is absent from the inventory")
            for record in inventory["checkouts"]:
                current_snapshot(record)
            write_json(options.output, inventory)
            report = {"installation_id": inventory["installation_id"], "checkouts": len(inventory["checkouts"])}
        elif options.command == "host-pack":
            report = host_pack(options)
        elif options.command == "host-plan":
            report = host_plan(options)
        elif options.command == "host-restore":
            report = host_restore(options)
        else:
            report = {"create": create, "restore": restore, "resume": resume}[options.command](office, options)
    except (BackupError, OSError, ValueError, KeyError, subprocess.TimeoutExpired, tarfile.TarError) as error:
        parser.exit(1, f"Backup operation failed. {error}\n")
    print(json.dumps(report, sort_keys=True))


if __name__ == "__main__":
    main()
