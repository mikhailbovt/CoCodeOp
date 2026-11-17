ALTER TABLE work_scope_revisions ADD COLUMN required_read_paths jsonb NOT NULL DEFAULT '[]';
ALTER TABLE work_scope_revisions ADD CHECK (
    jsonb_typeof(required_read_paths)='array' AND jsonb_array_length(required_read_paths)<=16
);

ALTER TABLE checkout_publications ADD COLUMN read_dependencies jsonb NOT NULL DEFAULT '[]';
ALTER TABLE checkout_publications ADD CHECK (
    jsonb_typeof(read_dependencies)='array' AND jsonb_array_length(read_dependencies)<=16
);
