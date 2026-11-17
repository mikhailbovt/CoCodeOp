ALTER TABLE checkout_commands ADD COLUMN source_snapshot_id uuid;
ALTER TABLE checkout_commands ADD CONSTRAINT checkout_command_sealed_source
    FOREIGN KEY (workspace_id,checkout_id,source_snapshot_id)
    REFERENCES checkout_snapshots (workspace_id,checkout_id,id);
CREATE INDEX checkout_commands_by_source ON checkout_commands(workspace_id,checkout_id,source_snapshot_id)
    WHERE source_snapshot_id IS NOT NULL;
