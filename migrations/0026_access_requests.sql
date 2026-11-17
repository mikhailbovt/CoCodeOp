CREATE TABLE access_requests (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    checkout_id uuid NOT NULL,
    task_id uuid NOT NULL,
    scope_id uuid NOT NULL,
    base_scope_revision bigint NOT NULL,
    run_id uuid,
    requester_user_id uuid NOT NULL,
    requester_agent_id uuid,
    worker_user_id uuid NOT NULL,
    worker_agent_id uuid NOT NULL,
    lead_user_id uuid NOT NULL,
    lead_agent_id uuid NOT NULL,
    host_owner_user_id uuid NOT NULL,
    effective_grant_id uuid NOT NULL,
    needs_owner_grant boolean NOT NULL,
    reason text NOT NULL CHECK (octet_length(reason) BETWEEN 1 AND 2048),
    permissions jsonb NOT NULL CHECK (jsonb_typeof(permissions) = 'object'),
    approvers jsonb NOT NULL CHECK (jsonb_typeof(approvers) = 'array' AND jsonb_array_length(approvers) BETWEEN 1 AND 14),
    approvals jsonb NOT NULL DEFAULT '[]'::jsonb CHECK (jsonb_typeof(approvals) = 'array'),
    area_revisions jsonb NOT NULL CHECK (jsonb_typeof(area_revisions) = 'array'),
    state text NOT NULL DEFAULT 'requested' CHECK (state IN ('requested', 'approved_pending_apply', 'approved', 'rejected', 'cancelled', 'expired')),
    revision bigint NOT NULL DEFAULT 1 CHECK (revision > 0),
    expires_at timestamptz NOT NULL,
    scope_expires_at timestamptz NOT NULL,
    issued_scope_revision bigint,
    decision_by_user_id uuid,
    decision_by_agent_id uuid,
    decided_at timestamptz,
    UNIQUE (workspace_id, id),
    CHECK (state NOT IN ('approved_pending_apply', 'approved') OR issued_scope_revision IS NOT NULL),
    CHECK (issued_scope_revision IS NULL OR issued_scope_revision > base_scope_revision),
    FOREIGN KEY (workspace_id, id) REFERENCES data_scopes (workspace_id, id),
    FOREIGN KEY (workspace_id, task_id) REFERENCES tasks (workspace_id, id),
    FOREIGN KEY (workspace_id, task_id, run_id) REFERENCES runs (workspace_id, task_id, id),
    FOREIGN KEY (workspace_id, checkout_id, scope_id) REFERENCES work_scopes (workspace_id, checkout_id, id),
    FOREIGN KEY (workspace_id, scope_id, base_scope_revision) REFERENCES work_scope_revisions (workspace_id, scope_id, revision),
    FOREIGN KEY (workspace_id, scope_id, issued_scope_revision) REFERENCES work_scope_revisions (workspace_id, scope_id, revision),
    FOREIGN KEY (workspace_id, effective_grant_id, checkout_id) REFERENCES checkout_grants (workspace_id, id, checkout_id),
    FOREIGN KEY (workspace_id, checkout_id, host_owner_user_id) REFERENCES project_checkouts (workspace_id, id, owner_user_id),
    FOREIGN KEY (workspace_id, requester_user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    FOREIGN KEY (workspace_id, requester_agent_id, requester_user_id) REFERENCES agents (workspace_id, id, owner_user_id),
    FOREIGN KEY (workspace_id, worker_agent_id, worker_user_id) REFERENCES agents (workspace_id, id, owner_user_id),
    FOREIGN KEY (workspace_id, lead_agent_id, lead_user_id) REFERENCES agents (workspace_id, id, owner_user_id),
    FOREIGN KEY (workspace_id, decision_by_user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    FOREIGN KEY (workspace_id, decision_by_agent_id, decision_by_user_id) REFERENCES agents (workspace_id, id, owner_user_id)
);

CREATE INDEX access_requests_by_task ON access_requests (workspace_id, task_id, id);
ALTER TABLE work_scope_revisions ADD COLUMN access_request_id uuid;
ALTER TABLE work_scope_revisions ADD CONSTRAINT scope_access_approval_in_workspace
    FOREIGN KEY (workspace_id, access_request_id) REFERENCES access_requests (workspace_id, id);
CREATE UNIQUE INDEX one_scope_revision_per_approved_request ON work_scope_revisions (workspace_id, access_request_id)
    WHERE access_request_id IS NOT NULL;

CREATE FUNCTION office_access_request_context() RETURNS uuid LANGUAGE sql STABLE AS $$
    SELECT nullif(current_setting('office.access_request_context', true), '')::uuid
$$;

ALTER TABLE access_requests ENABLE ROW LEVEL SECURITY;
ALTER TABLE access_requests FORCE ROW LEVEL SECURITY;

CREATE POLICY access_request_read ON access_requests FOR SELECT USING (
    workspace_id = office_workspace_id() AND (
        office_hosts_checkout(workspace_id, checkout_id) OR EXISTS (
            SELECT 1 FROM data_scopes WHERE workspace_id = access_requests.workspace_id AND id = access_requests.id
        )
    )
);
CREATE POLICY access_request_create ON access_requests FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND requester_user_id = office_user_id()
    AND requester_agent_id IS NOT DISTINCT FROM office_agent_id() AND office_checkout_runner_id() IS NULL
);
CREATE POLICY access_request_update ON access_requests FOR UPDATE USING (
    workspace_id = office_workspace_id() AND (office_hosts_checkout(workspace_id, checkout_id)
        OR id = office_access_request_context())
);

CREATE POLICY task_read_for_access_decision ON tasks FOR SELECT USING (
    workspace_id = office_workspace_id() AND EXISTS (
        SELECT 1 FROM access_requests WHERE id = office_access_request_context()
            AND workspace_id = tasks.workspace_id AND task_id = tasks.id
    )
);

CREATE POLICY work_scope_for_access_decision ON work_scopes FOR SELECT USING (
    workspace_id = office_workspace_id() AND EXISTS (
        SELECT 1 FROM access_requests WHERE id = office_access_request_context()
            AND workspace_id = work_scopes.workspace_id AND scope_id = work_scopes.id
    )
);

CREATE POLICY checkout_for_access_context ON project_checkouts FOR SELECT USING (
    workspace_id = office_workspace_id()
    AND id = nullif(current_setting('office.access_checkout_context', true), '')::uuid
);

CREATE POLICY approved_work_scope_revision_create ON work_scope_revisions FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND office_checkout_runner_id() IS NULL
    AND access_request_id = office_access_request_context() AND state = 'pending_activation'
    AND EXISTS (SELECT 1 FROM access_requests WHERE id = access_request_id
       AND workspace_id = work_scope_revisions.workspace_id
       AND scope_id = work_scope_revisions.scope_id AND checkout_id = work_scope_revisions.checkout_id
       AND state = 'requested')
);
