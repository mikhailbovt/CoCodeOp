# Backup and restore

Use a private destination for the Core's PostgreSQL archive and each checkout host's source, snapshots and coordinator state. Store database secrets, provider keys and bearer files separately.

## Capture a backup

Stop admission with `office scheduler set-control` in each workspace and wait for active runs and publications to reconcile. Capture the current snapshot of each checkout and stop its coordinator after the snapshot is ready.

From the Core installation directory, export one plan per checkout. Keep `COCODEOP_SECRETS_DIR` set to the installation's private database directory.

```sh
python3 deploy/backup.py --project cocodeop plan --checkout CHECKOUT_ID \
  --output /private/checkout-plan.json
```

On each checkout machine, run as the coordinator user. Use the actual runner binary path, source root and state directory. The tool checks the expected source and snapshots.

```sh
python3 deploy/backup.py host-pack --plan /private/checkout-plan.json \
  --checkout CHECKOUT_ID --root /srv/project --state-dir /private/checkout-state \
  --runner /usr/local/bin/office-runner --output /private/checkout.tar.gz
```

Transfer the archives to the operator. Stop the Core's `server` service while leaving PostgreSQL running. Pass one `--checkout-archive` for each registered checkout.

```sh
core stop server
python3 deploy/backup.py --project cocodeop create --output /private/cocodeop-backup \
  --checkout-archive /private/checkout.tar.gz
```

Use the `core` shell command from the [installation guide](getting-started.md#1-start-the-core). The backup directory contains the dump, host archives and checksums.

## Restore a backup

Use a fresh Compose project such as `restored`, a new database volume and a new secrets directory. Use the same release image ID. Start only its `db` service before restoring PostgreSQL.

```sh
python3 deploy/backup.py --project restored restore --bundle /private/cocodeop-backup
python3 deploy/backup.py host-plan --bundle /private/cocodeop-backup \
  --checkout CHECKOUT_ID --output /private/host-plan.json
```

Start the restored Core. On each host, restore using its original registered source path and a new coordinator state directory. The source path must have the registered identity. Use an empty checkout directory or source that matches the backup exactly.

```sh
python3 deploy/backup.py host-restore --plan /private/host-plan.json \
  --archive /private/checkout.tar.gz --root /srv/project \
  --state-dir /private/restored-checkout --runner /usr/local/bin/office-runner \
  --receipt /private/host-receipt.json
```

Bring each coordinator back with its restored state. Verify credentials before making the endpoint reachable by users. Send all host receipts to the Core operator.

```sh
python3 deploy/backup.py --project restored resume --bundle /private/cocodeop-backup \
  --host-receipt /private/host-receipt.json
```

The restored installation quarantines old jobs and invalidates previous leases. Inspect quarantined jobs, verify the runner and artifact hashes, then explicitly return workspaces to `running`.
