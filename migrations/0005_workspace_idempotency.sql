ALTER TABLE workspaces ADD COLUMN creation_key uuid;
ALTER TABLE workspaces ADD CONSTRAINT workspace_creation_by_owner UNIQUE (created_by_user_id, creation_key);
