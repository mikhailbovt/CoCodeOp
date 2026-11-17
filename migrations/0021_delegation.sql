ALTER TABLE teams ADD UNIQUE (workspace_id, id, owner_user_id);
ALTER TABLE teams ADD COLUMN lead_generation bigint NOT NULL DEFAULT 1 CHECK (lead_generation > 0);

CREATE TABLE runtime_templates (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    owner_user_id uuid NOT NULL,
    name text NOT NULL CHECK (octet_length(name) BETWEEN 1 AND 128),
    kind text NOT NULL CHECK (kind = 'interactive'),
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (workspace_id, owner_user_id, id),
    FOREIGN KEY (workspace_id, id) REFERENCES data_scopes (workspace_id, id),
    FOREIGN KEY (workspace_id, owner_user_id) REFERENCES workspace_memberships (workspace_id, user_id)
);

CREATE TABLE delegation_grants (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    team_id uuid NOT NULL,
    owner_user_id uuid NOT NULL,
    task_id uuid NOT NULL,
    parent_grant_id uuid CHECK (parent_grant_id <> id),
    issuer_agent_id uuid,
    grantee_agent_id uuid NOT NULL,
    lead_generation bigint NOT NULL CHECK (lead_generation > 0),
    expires_at timestamptz NOT NULL,
    remaining_depth smallint NOT NULL CHECK (remaining_depth BETWEEN 0 AND 3),
    max_attempts smallint NOT NULL CHECK (max_attempts BETWEEN 1 AND 128),
    max_subtasks smallint NOT NULL CHECK (max_subtasks BETWEEN 0 AND 128),
    attempts_used smallint NOT NULL DEFAULT 0 CHECK (attempts_used BETWEEN 0 AND max_attempts),
    subtasks_used smallint NOT NULL DEFAULT 0 CHECK (subtasks_used BETWEEN 0 AND max_subtasks),
    revision bigint NOT NULL DEFAULT 1 CHECK (revision > 0),
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    revoked_at timestamptz,
    CHECK ((parent_grant_id IS NULL) = (issuer_agent_id IS NULL)),
    UNIQUE (workspace_id, task_id),
    UNIQUE (workspace_id, id, task_id),
    UNIQUE (workspace_id, owner_user_id, id),
    UNIQUE (workspace_id, team_id, owner_user_id, id),
    FOREIGN KEY (workspace_id, id) REFERENCES data_scopes (workspace_id, id),
    FOREIGN KEY (workspace_id, team_id, owner_user_id) REFERENCES teams (workspace_id, id, owner_user_id),
    FOREIGN KEY (workspace_id, team_id, owner_user_id, parent_grant_id)
        REFERENCES delegation_grants (workspace_id, team_id, owner_user_id, id),
    FOREIGN KEY (workspace_id, grantee_agent_id, owner_user_id) REFERENCES agents (workspace_id, id, owner_user_id),
    FOREIGN KEY (workspace_id, issuer_agent_id, owner_user_id) REFERENCES agents (workspace_id, id, owner_user_id),
    FOREIGN KEY (workspace_id, task_id) REFERENCES tasks (workspace_id, id) DEFERRABLE INITIALLY DEFERRED
);

CREATE INDEX delegation_children ON delegation_grants (workspace_id, parent_grant_id);

CREATE TABLE delegation_templates (
    workspace_id uuid NOT NULL,
    owner_user_id uuid NOT NULL,
    grant_id uuid NOT NULL,
    template_id uuid NOT NULL,
    PRIMARY KEY (workspace_id, grant_id, template_id),
    FOREIGN KEY (workspace_id, owner_user_id, grant_id) REFERENCES delegation_grants (workspace_id, owner_user_id, id),
    FOREIGN KEY (workspace_id, owner_user_id, template_id) REFERENCES runtime_templates (workspace_id, owner_user_id, id)
);

ALTER TABLE tasks ADD COLUMN delegation_grant_id uuid;
ALTER TABLE tasks ADD CONSTRAINT task_uses_own_grant
    FOREIGN KEY (workspace_id, delegation_grant_id, id) REFERENCES delegation_grants (workspace_id, id, task_id);

ALTER TABLE runs ADD COLUMN runtime_template_id uuid;
ALTER TABLE runs ADD CONSTRAINT run_template_same_owner
    FOREIGN KEY (workspace_id, executor_user_id, runtime_template_id) REFERENCES runtime_templates (workspace_id, owner_user_id, id);

ALTER TABLE runtime_templates ENABLE ROW LEVEL SECURITY;
ALTER TABLE runtime_templates FORCE ROW LEVEL SECURITY;
CREATE POLICY runtime_template_read ON runtime_templates FOR SELECT USING (
    workspace_id = office_workspace_id() AND owner_user_id = office_user_id()
);
CREATE POLICY runtime_template_create ON runtime_templates FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND owner_user_id = office_user_id() AND office_agent_id() IS NULL
);

ALTER TABLE delegation_grants ENABLE ROW LEVEL SECURITY;
ALTER TABLE delegation_grants FORCE ROW LEVEL SECURITY;
CREATE POLICY delegation_read ON delegation_grants FOR SELECT USING (
    workspace_id = office_workspace_id() AND office_active_team_member(workspace_id, team_id)
);
CREATE POLICY delegation_create ON delegation_grants FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND owner_user_id = office_user_id()
    AND issuer_agent_id IS NOT DISTINCT FROM office_agent_id() AND office_active_team_member(workspace_id, team_id)
);
CREATE POLICY delegation_mutate ON delegation_grants FOR UPDATE USING (
    workspace_id = office_workspace_id() AND owner_user_id = office_user_id()
    AND office_active_team_member(workspace_id, team_id)
);

ALTER TABLE delegation_templates ENABLE ROW LEVEL SECURITY;
ALTER TABLE delegation_templates FORCE ROW LEVEL SECURITY;
CREATE POLICY delegation_template_read ON delegation_templates FOR SELECT USING (
    workspace_id = office_workspace_id() AND EXISTS (
        SELECT 1 FROM delegation_grants WHERE workspace_id = delegation_templates.workspace_id AND id = grant_id
    )
);
CREATE POLICY delegation_template_create ON delegation_templates FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND owner_user_id = office_user_id() AND EXISTS (
        SELECT 1 FROM delegation_grants WHERE workspace_id = delegation_templates.workspace_id AND id = grant_id
            AND issuer_agent_id IS NOT DISTINCT FROM office_agent_id()
    )
);

CREATE POLICY delegated_task_create ON tasks FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND accountable_user_id = office_user_id()
    AND EXISTS (SELECT 1 FROM delegation_grants WHERE workspace_id = tasks.workspace_id AND id = delegation_grant_id
        AND task_id = tasks.id AND team_id = tasks.team_id AND parent_grant_id IS NOT NULL
        AND issuer_agent_id = office_agent_id() AND grantee_agent_id = tasks.assignee_agent_id)
    AND EXISTS (SELECT 1 FROM data_scopes WHERE workspace_id = tasks.workspace_id AND id = tasks.id
        AND office_owns_data(owner_user_id, author_id))
);
