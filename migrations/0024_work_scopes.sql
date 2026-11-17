CREATE TABLE work_scopes (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    checkout_id uuid NOT NULL,
    task_id uuid NOT NULL,
    worker_user_id uuid NOT NULL,
    worker_agent_id uuid NOT NULL,
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, task_id),
    UNIQUE (workspace_id, checkout_id, id),
    FOREIGN KEY (workspace_id, id) REFERENCES data_scopes (workspace_id, id),
    FOREIGN KEY (workspace_id, task_id) REFERENCES tasks (workspace_id, id),
    FOREIGN KEY (workspace_id, worker_agent_id, worker_user_id) REFERENCES agents (workspace_id, id, owner_user_id)
);

CREATE TABLE work_scope_revisions (
    workspace_id uuid NOT NULL,
    checkout_id uuid NOT NULL,
    scope_id uuid NOT NULL,
    revision bigint NOT NULL CHECK (revision > 0),
    grant_id uuid NOT NULL,
    grant_revision bigint NOT NULL CHECK (grant_revision > 0),
    policy_sequence bigint GENERATED ALWAYS AS IDENTITY UNIQUE,
    state text NOT NULL DEFAULT 'pending_activation'
        CHECK (state IN ('pending_activation', 'active', 'superseded', 'rejected')),
    expires_at timestamptz NOT NULL,
    permissions jsonb NOT NULL CHECK (jsonb_typeof(permissions) = 'object'),
    acknowledged_at timestamptz,
    deployment_sha256 text CHECK (deployment_sha256 ~ '^[a-f0-9]{64}$'),
    CHECK ((state = 'pending_activation') = (acknowledged_at IS NULL)),
    CHECK ((acknowledged_at IS NULL) = (deployment_sha256 IS NULL)),
    PRIMARY KEY (workspace_id, scope_id, revision),
    FOREIGN KEY (workspace_id, checkout_id, scope_id) REFERENCES work_scopes (workspace_id, checkout_id, id),
    FOREIGN KEY (workspace_id, grant_id, checkout_id) REFERENCES checkout_grants (workspace_id, id, checkout_id)
);

CREATE UNIQUE INDEX one_active_work_scope ON work_scope_revisions (workspace_id, scope_id) WHERE state = 'active';
CREATE UNIQUE INDEX one_pending_work_scope ON work_scope_revisions (workspace_id, scope_id) WHERE state = 'pending_activation';
CREATE INDEX pending_work_scope_priority ON work_scope_revisions (workspace_id, checkout_id, policy_sequence)
    WHERE state = 'pending_activation';

CREATE FUNCTION office_verifying_scope_grant_id() RETURNS uuid LANGUAGE sql STABLE AS $$
    SELECT nullif(current_setting('office.verifying_scope_grant_id', true), '')::uuid
$$;

CREATE POLICY checkout_grant_scope_validation ON checkout_grants FOR SELECT USING (
    workspace_id = office_workspace_id() AND id = office_verifying_scope_grant_id()
);

CREATE POLICY command_template_work_scope_read ON command_templates FOR SELECT USING (
    workspace_id = office_workspace_id() AND EXISTS (SELECT 1 FROM checkout_grants grant_row
        WHERE grant_row.workspace_id = command_templates.workspace_id AND grant_row.checkout_id = command_templates.checkout_id
          AND grant_row.id = office_verifying_scope_grant_id()
          AND (grant_row.permissions -> 'command_template_ids') ? command_templates.id::text)
);

CREATE POLICY root_runner_binding ON checkout_roots FOR SELECT USING (
    workspace_id = office_workspace_id() AND host_runner_id = office_checkout_runner_id()
);

-- An authenticated runner reads deployments only for checkouts it actually hosts.
CREATE FUNCTION office_hosts_checkout(workspace uuid, checkout uuid) RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT office_checkout_runner_id() IS NOT NULL AND EXISTS (
        SELECT 1 FROM project_checkouts WHERE workspace_id = workspace AND id = checkout
            AND host_runner_id = office_checkout_runner_id() AND state = 'registered'
    )
$$;

ALTER TABLE work_scopes ENABLE ROW LEVEL SECURITY;
ALTER TABLE work_scopes FORCE ROW LEVEL SECURITY;
CREATE POLICY work_scope_read ON work_scopes FOR SELECT USING (
    workspace_id = office_workspace_id() AND (
        office_hosts_checkout(workspace_id, checkout_id) OR EXISTS (
            SELECT 1 FROM data_scopes WHERE workspace_id = work_scopes.workspace_id AND id = work_scopes.id
        )
    )
);
CREATE POLICY work_scope_create ON work_scopes FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND office_checkout_runner_id() IS NULL
    AND EXISTS (SELECT 1 FROM data_scopes WHERE workspace_id = work_scopes.workspace_id AND id = work_scopes.id
        AND office_owns_data(owner_user_id, author_id))
);

ALTER TABLE work_scope_revisions ENABLE ROW LEVEL SECURITY;
ALTER TABLE work_scope_revisions FORCE ROW LEVEL SECURITY;
CREATE POLICY work_scope_revision_read ON work_scope_revisions FOR SELECT USING (
    workspace_id = office_workspace_id() AND EXISTS (
        SELECT 1 FROM work_scopes WHERE workspace_id = work_scope_revisions.workspace_id AND id = scope_id
    )
);
CREATE POLICY work_scope_revision_create ON work_scope_revisions FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND office_checkout_runner_id() IS NULL
    AND state = 'pending_activation'
    AND EXISTS (SELECT 1 FROM checkout_grants WHERE workspace_id = work_scope_revisions.workspace_id
        AND id = grant_id AND office_owns_data(recipient_user_id, recipient_agent_id))
);
CREATE POLICY work_scope_revision_apply ON work_scope_revisions FOR UPDATE USING (
    workspace_id = office_workspace_id() AND office_hosts_checkout(workspace_id, checkout_id)
);
