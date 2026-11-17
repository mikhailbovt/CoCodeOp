ALTER TABLE checkout_coordinator_sessions ADD COLUMN publication_state_id uuid;

CREATE TABLE checkout_content_heads (
    workspace_id uuid NOT NULL,
    checkout_id uuid NOT NULL,
    host_runner_id uuid NOT NULL,
    state_id uuid NOT NULL,
    checkout_revision bigint NOT NULL DEFAULT 0 CHECK (checkout_revision>=0),
    source_sha256 text NOT NULL CHECK (source_sha256 ~ '^[0-9a-f]{64}$'),
    blocked boolean NOT NULL DEFAULT false,
    PRIMARY KEY (workspace_id,checkout_id),
    FOREIGN KEY (workspace_id,checkout_id) REFERENCES project_checkouts(workspace_id,id),
    FOREIGN KEY (host_runner_id) REFERENCES runners(id)
);

CREATE TABLE checkout_publications (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    checkout_id uuid NOT NULL,
    host_runner_id uuid NOT NULL,
    epoch bigint NOT NULL,
    task_id uuid NOT NULL,
    run_id uuid NOT NULL,
    executor_user_id uuid NOT NULL,
    worker_agent_id uuid NOT NULL,
    run_fence bigint NOT NULL CHECK (run_fence>0),
    scope_id uuid NOT NULL,
    scope_revision bigint NOT NULL,
    idempotency_key uuid NOT NULL,
    request_sha256 bytea NOT NULL CHECK (octet_length(request_sha256)=32),
    definition jsonb,
    scope_definition jsonb,
    state text NOT NULL DEFAULT 'queued' CHECK (state IN ('queued','applying','applied','rejected','reconcile_required')),
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    admit_until timestamptz NOT NULL,
    offered_until timestamptz,
    previous_revision bigint,
    previous_sha256 text,
    claim_fence bigint,
    permit_until timestamptz,
    outcome jsonb,
    event_sequence bigint,
    CHECK ((state IN ('queued','applying','reconcile_required'))=(definition IS NOT NULL)),
    CHECK ((state='applied')=(event_sequence IS NOT NULL)),
    CHECK ((state IN ('applied','rejected','reconcile_required'))=(outcome IS NOT NULL)),
    UNIQUE (workspace_id,run_id,idempotency_key),
    FOREIGN KEY (workspace_id,checkout_id,epoch) REFERENCES checkout_coordinator_sessions(workspace_id,checkout_id,epoch),
    FOREIGN KEY (workspace_id,task_id,run_id) REFERENCES runs(workspace_id,task_id,id),
    FOREIGN KEY (workspace_id,scope_id,scope_revision) REFERENCES work_scope_revisions(workspace_id,scope_id,revision)
);
CREATE UNIQUE INDEX one_applying_publication ON checkout_publications(workspace_id,checkout_id) WHERE state='applying';
CREATE INDEX pending_publications_by_checkout ON checkout_publications(workspace_id,checkout_id,epoch,created_at) WHERE state='queued';

CREATE FUNCTION office_gateway_checkout() RETURNS uuid LANGUAGE sql STABLE AS $$
    SELECT nullif(current_setting('office.gateway_checkout_id',true),'')::uuid
$$;
ALTER TABLE checkout_content_heads ENABLE ROW LEVEL SECURITY;
ALTER TABLE checkout_content_heads FORCE ROW LEVEL SECURITY;
CREATE POLICY publication_head_host ON checkout_content_heads FOR ALL
USING (workspace_id=office_workspace_id() AND host_runner_id=office_checkout_runner_id()
       AND office_hosts_checkout(workspace_id,checkout_id))
WITH CHECK (workspace_id=office_workspace_id() AND host_runner_id=office_checkout_runner_id()
       AND office_hosts_checkout(workspace_id,checkout_id));
CREATE POLICY publication_head_worker ON checkout_content_heads FOR SELECT USING (
    workspace_id=office_workspace_id() AND checkout_id=office_gateway_checkout() AND office_gateway_run() IS NOT NULL
);
ALTER TABLE checkout_publications ENABLE ROW LEVEL SECURITY;
ALTER TABLE checkout_publications FORCE ROW LEVEL SECURITY;
CREATE POLICY publication_host ON checkout_publications FOR ALL
USING (workspace_id=office_workspace_id() AND host_runner_id=office_checkout_runner_id()
       AND office_hosts_checkout(workspace_id,checkout_id))
WITH CHECK (workspace_id=office_workspace_id() AND host_runner_id=office_checkout_runner_id()
       AND office_hosts_checkout(workspace_id,checkout_id));
CREATE POLICY publication_worker ON checkout_publications FOR SELECT USING (
    workspace_id=office_workspace_id() AND checkout_id=office_gateway_checkout() AND run_id=office_gateway_run()
    AND executor_user_id=office_user_id() AND worker_agent_id=office_agent_id()
);
CREATE POLICY publication_request ON checkout_publications FOR INSERT WITH CHECK (
    workspace_id=office_workspace_id() AND checkout_id=office_gateway_checkout() AND run_id=office_gateway_run()
    AND executor_user_id=office_user_id() AND worker_agent_id=office_agent_id() AND state='queued'
);
CREATE POLICY publication_admit ON checkout_publications FOR UPDATE USING (
    workspace_id=office_workspace_id() AND checkout_id=office_gateway_checkout() AND run_id=office_gateway_run()
    AND executor_user_id=office_user_id() AND worker_agent_id=office_agent_id() AND state='queued'
) WITH CHECK (
    workspace_id=office_workspace_id() AND checkout_id=office_gateway_checkout() AND run_id=office_gateway_run()
    AND executor_user_id=office_user_id() AND worker_agent_id=office_agent_id() AND state='applying'
);

-- Other workers' operation bodies stay private. A checkout-scoped advisory lock and
-- this marker serialize admissions across runs without exposing another worker's row.
ALTER TABLE checkout_content_heads ADD COLUMN applying_change_id uuid REFERENCES checkout_publications(id);
CREATE POLICY publication_admission_marker ON checkout_content_heads FOR UPDATE
USING (workspace_id=office_workspace_id() AND checkout_id=office_gateway_checkout() AND office_gateway_run() IS NOT NULL
       AND applying_change_id IS NULL AND NOT blocked)
WITH CHECK (
    workspace_id=office_workspace_id() AND checkout_id=office_gateway_checkout() AND office_gateway_run() IS NOT NULL
    AND NOT blocked AND EXISTS (SELECT 1 FROM checkout_publications p WHERE p.id=applying_change_id AND p.state='applying')
);
