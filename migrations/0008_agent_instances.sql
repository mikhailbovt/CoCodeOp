ALTER TABLE credentials ADD CONSTRAINT credentials_full_agent_scope UNIQUE (id, workspace_id, agent_id, owner_user_id);

CREATE TABLE agent_instances (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    agent_id uuid NOT NULL,
    owner_user_id uuid NOT NULL,
    credential_id uuid NOT NULL,
    attachment_key uuid NOT NULL,
    label text NOT NULL CHECK (octet_length(label) BETWEEN 1 AND 128),
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    expires_at timestamptz NOT NULL DEFAULT transaction_timestamp() + interval '8 hours',
    UNIQUE (credential_id, attachment_key),
    FOREIGN KEY (credential_id, workspace_id, agent_id, owner_user_id)
        REFERENCES credentials (id, workspace_id, agent_id, owner_user_id)
);

ALTER TABLE agent_instances ENABLE ROW LEVEL SECURITY;
ALTER TABLE agent_instances FORCE ROW LEVEL SECURITY;

CREATE POLICY instance_credential ON agent_instances USING (
    credential_id = nullif(current_setting('office.credential_id', true), '')::uuid
) WITH CHECK (
    credential_id = nullif(current_setting('office.credential_id', true), '')::uuid
);
