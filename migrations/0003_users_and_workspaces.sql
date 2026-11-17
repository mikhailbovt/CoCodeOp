CREATE TABLE users (
    id uuid PRIMARY KEY,
    display_name text NOT NULL CHECK (octet_length(display_name) BETWEEN 1 AND 128),
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    disabled_at timestamptz
);

CREATE TABLE workspaces (
    id uuid PRIMARY KEY,
    name text NOT NULL CHECK (octet_length(name) BETWEEN 1 AND 128),
    created_by_user_id uuid NOT NULL REFERENCES users(id),
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp()
);

CREATE TABLE workspace_memberships (
    workspace_id uuid NOT NULL REFERENCES workspaces(id),
    user_id uuid NOT NULL REFERENCES users(id),
    role text NOT NULL CHECK (role IN ('owner', 'admin', 'member')),
    revoked_at timestamptz,
    PRIMARY KEY (workspace_id, user_id)
);

CREATE INDEX workspace_memberships_by_user ON workspace_memberships (user_id, workspace_id)
    WHERE revoked_at IS NULL;

CREATE TABLE credentials (
    id uuid PRIMARY KEY,
    owner_user_id uuid NOT NULL REFERENCES users(id),
    digest bytea NOT NULL UNIQUE CHECK (octet_length(digest) = 32),
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    expires_at timestamptz,
    revoked_at timestamptz
);

CREATE INDEX credentials_by_owner ON credentials (owner_user_id, id);

ALTER TABLE office_installation ADD COLUMN bootstrap_user_id uuid REFERENCES users(id);

ALTER TABLE workspaces ENABLE ROW LEVEL SECURITY;
ALTER TABLE workspaces FORCE ROW LEVEL SECURITY;
ALTER TABLE workspace_memberships ENABLE ROW LEVEL SECURITY;
ALTER TABLE workspace_memberships FORCE ROW LEVEL SECURITY;

CREATE POLICY workspace_memberships_read_self ON workspace_memberships FOR SELECT USING (
    user_id = nullif(current_setting('office.user_id', true), '')::uuid
    AND revoked_at IS NULL
);

CREATE POLICY workspace_memberships_insert_self ON workspace_memberships FOR INSERT WITH CHECK (
    user_id = nullif(current_setting('office.user_id', true), '')::uuid
);

CREATE POLICY workspaces_read_member ON workspaces FOR SELECT USING (
    EXISTS (
        SELECT 1 FROM workspace_memberships membership
        WHERE membership.workspace_id = workspaces.id
          AND membership.user_id = nullif(current_setting('office.user_id', true), '')::uuid
          AND membership.revoked_at IS NULL
    )
);

CREATE POLICY workspaces_insert_creator ON workspaces FOR INSERT WITH CHECK (
    created_by_user_id = nullif(current_setting('office.user_id', true), '')::uuid
);
