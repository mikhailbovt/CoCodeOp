CREATE TABLE agents (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    owner_user_id uuid NOT NULL,
    display_name text NOT NULL CHECK (octet_length(display_name) BETWEEN 1 AND 128),
    role text NOT NULL CHECK (role IN ('lead', 'worker', 'reviewer')),
    creation_key uuid NOT NULL,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    disabled_at timestamptz,
    UNIQUE (owner_user_id, creation_key),
    UNIQUE (workspace_id, id, owner_user_id),
    FOREIGN KEY (workspace_id, owner_user_id) REFERENCES workspace_memberships (workspace_id, user_id)
);

ALTER TABLE credentials
    ADD COLUMN workspace_id uuid,
    ADD COLUMN agent_id uuid,
    ADD CONSTRAINT agent_credential_scope CHECK ((workspace_id IS NULL) = (agent_id IS NULL)),
    ADD CONSTRAINT agent_credential_binding FOREIGN KEY (workspace_id, agent_id, owner_user_id)
        REFERENCES agents (workspace_id, id, owner_user_id);

CREATE INDEX credentials_by_agent ON credentials (workspace_id, agent_id) WHERE agent_id IS NOT NULL;

ALTER TABLE agents ENABLE ROW LEVEL SECURITY;
ALTER TABLE agents FORCE ROW LEVEL SECURITY;

CREATE POLICY owned_agents_read ON agents FOR SELECT USING (
    owner_user_id = nullif(current_setting('office.user_id', true), '')::uuid
);

CREATE POLICY owned_agents_insert ON agents FOR INSERT WITH CHECK (
    owner_user_id = nullif(current_setting('office.user_id', true), '')::uuid
);

CREATE POLICY owned_agents_lock ON agents FOR UPDATE
    USING (owner_user_id = nullif(current_setting('office.user_id', true), '')::uuid AND disabled_at IS NULL)
    WITH CHECK (false);
