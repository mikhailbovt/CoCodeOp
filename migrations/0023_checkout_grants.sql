ALTER TABLE project_checkouts ADD UNIQUE (workspace_id, id, owner_user_id);

CREATE TABLE checkout_grants (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    checkout_id uuid NOT NULL,
    owner_user_id uuid NOT NULL,
    recipient_user_id uuid NOT NULL,
    recipient_agent_id uuid NOT NULL,
    permissions jsonb NOT NULL CHECK (jsonb_typeof(permissions) = 'object'),
    expires_at timestamptz NOT NULL,
    revoked_at timestamptz,
    revision bigint NOT NULL DEFAULT 1 CHECK (revision > 0),
    UNIQUE (workspace_id, id, checkout_id),
    FOREIGN KEY (workspace_id, id) REFERENCES data_scopes (workspace_id, id),
    FOREIGN KEY (workspace_id, checkout_id, owner_user_id) REFERENCES project_checkouts (workspace_id, id, owner_user_id),
    FOREIGN KEY (workspace_id, recipient_agent_id, recipient_user_id) REFERENCES agents (workspace_id, id, owner_user_id)
);

CREATE INDEX checkout_grants_for_recipient ON checkout_grants (workspace_id, checkout_id, recipient_agent_id)
    WHERE revoked_at IS NULL;

CREATE TABLE command_templates (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    checkout_id uuid NOT NULL,
    owner_user_id uuid NOT NULL,
    spec jsonb NOT NULL CHECK (jsonb_typeof(spec) = 'object'),
    UNIQUE (workspace_id, id, checkout_id),
    FOREIGN KEY (workspace_id, id) REFERENCES data_scopes (workspace_id, id),
    FOREIGN KEY (workspace_id, checkout_id, owner_user_id) REFERENCES project_checkouts (workspace_id, id, owner_user_id)
);

ALTER TABLE checkout_grants ENABLE ROW LEVEL SECURITY;
ALTER TABLE checkout_grants FORCE ROW LEVEL SECURITY;
CREATE POLICY checkout_grant_read ON checkout_grants FOR SELECT USING (
    workspace_id = office_workspace_id() AND EXISTS (
        SELECT 1 FROM data_scopes WHERE workspace_id = checkout_grants.workspace_id AND id = checkout_grants.id
    )
);
CREATE POLICY checkout_grant_issue ON checkout_grants FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND owner_user_id = office_user_id() AND office_agent_id() IS NULL
);
CREATE POLICY checkout_grant_revoke ON checkout_grants FOR UPDATE USING (
    workspace_id = office_workspace_id() AND owner_user_id = office_user_id() AND office_agent_id() IS NULL
);

ALTER TABLE command_templates ENABLE ROW LEVEL SECURITY;
ALTER TABLE command_templates FORCE ROW LEVEL SECURITY;
CREATE POLICY command_template_owner ON command_templates FOR ALL
USING (workspace_id = office_workspace_id() AND owner_user_id = office_user_id() AND office_agent_id() IS NULL)
WITH CHECK (workspace_id = office_workspace_id() AND owner_user_id = office_user_id() AND office_agent_id() IS NULL);
