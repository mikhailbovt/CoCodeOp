CREATE TABLE runners (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    owner_user_id uuid NOT NULL,
    name text NOT NULL CHECK (octet_length(name) BETWEEN 1 AND 128),
    registration_key uuid NOT NULL,
    disabled_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    UNIQUE (owner_user_id, registration_key),
    UNIQUE (workspace_id, id, owner_user_id),
    FOREIGN KEY (workspace_id, owner_user_id) REFERENCES workspace_memberships (workspace_id, user_id)
);

ALTER TABLE credentials
    DROP CONSTRAINT agent_credential_scope,
    ADD COLUMN runner_id uuid,
    ADD CONSTRAINT credential_scope CHECK (
        (workspace_id IS NULL AND agent_id IS NULL AND runner_id IS NULL)
        OR (workspace_id IS NOT NULL AND num_nonnulls(agent_id, runner_id) = 1)
    ),
    ADD CONSTRAINT runner_credential_binding FOREIGN KEY (workspace_id, runner_id, owner_user_id)
        REFERENCES runners (workspace_id, id, owner_user_id);

CREATE INDEX credentials_by_runner ON credentials (workspace_id, runner_id) WHERE runner_id IS NOT NULL;

ALTER TABLE runners ENABLE ROW LEVEL SECURITY;
ALTER TABLE runners FORCE ROW LEVEL SECURITY;
CREATE POLICY owned_runners_select ON runners FOR SELECT USING (
    owner_user_id = nullif(current_setting('office.user_id', true), '')::uuid
);
CREATE POLICY owned_runners_insert ON runners FOR INSERT WITH CHECK (
    owner_user_id = nullif(current_setting('office.user_id', true), '')::uuid
);
CREATE POLICY owned_runners_lock ON runners FOR UPDATE USING (
    owner_user_id = nullif(current_setting('office.user_id', true), '')::uuid AND disabled_at IS NULL
) WITH CHECK (false);
