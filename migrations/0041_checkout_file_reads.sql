CREATE FUNCTION office_gateway_run() RETURNS uuid LANGUAGE sql STABLE AS $$
    SELECT run_id FROM run_credentials WHERE digest=office_run_credential_digest()
$$;

CREATE POLICY scoped_worker_reads_coordinator ON checkout_coordinator_sessions FOR SELECT USING (
    workspace_id=office_workspace_id() AND office_gateway_run() IS NOT NULL
    AND checkout_id=nullif(current_setting('office.gateway_checkout_id',true),'')::uuid
);

CREATE POLICY scheduler_reads_coordinator_presence ON checkout_coordinator_sessions FOR SELECT USING (
    workspace_id=office_workspace_id() AND office_runner_planning()
    AND stopped_at IS NULL AND ready_at IS NOT NULL AND lease_until>statement_timestamp()
);

ALTER TABLE execution_jobs ADD COLUMN gateway_required boolean NOT NULL DEFAULT false;
ALTER TABLE execution_jobs ADD CHECK (NOT gateway_required OR checkout_id IS NOT NULL);

CREATE TABLE checkout_file_reads (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    checkout_id uuid NOT NULL,
    epoch bigint NOT NULL,
    host_runner_id uuid NOT NULL,
    task_id uuid NOT NULL,
    run_id uuid NOT NULL,
    executor_user_id uuid NOT NULL,
    worker_agent_id uuid NOT NULL,
    scope_id uuid NOT NULL,
    scope_revision bigint NOT NULL,
    scope_definition jsonb NOT NULL,
    path text NOT NULL CHECK (octet_length(path) BETWEEN 1 AND 1024),
    idempotency_key uuid NOT NULL,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    expires_at timestamptz NOT NULL,
    outcome jsonb,
    UNIQUE (workspace_id,run_id,idempotency_key),
    FOREIGN KEY (workspace_id,checkout_id,epoch) REFERENCES checkout_coordinator_sessions (workspace_id,checkout_id,epoch),
    FOREIGN KEY (workspace_id,task_id,run_id) REFERENCES runs (workspace_id,task_id,id),
    FOREIGN KEY (workspace_id,scope_id,scope_revision) REFERENCES work_scope_revisions (workspace_id,scope_id,revision)
);
CREATE INDEX pending_file_reads_by_host ON checkout_file_reads (workspace_id,checkout_id,epoch,created_at) WHERE outcome IS NULL;
CREATE INDEX live_file_reads_by_run ON checkout_file_reads (workspace_id,run_id,created_at);

ALTER TABLE checkout_file_reads ENABLE ROW LEVEL SECURITY;
ALTER TABLE checkout_file_reads FORCE ROW LEVEL SECURITY;
CREATE POLICY file_read_worker ON checkout_file_reads FOR SELECT USING (
    workspace_id=office_workspace_id() AND run_id=office_gateway_run()
    AND executor_user_id=office_user_id() AND worker_agent_id=office_agent_id()
);
CREATE POLICY file_read_request ON checkout_file_reads FOR INSERT WITH CHECK (
    workspace_id=office_workspace_id() AND run_id=office_gateway_run() AND outcome IS NULL
    AND executor_user_id=office_user_id() AND worker_agent_id=office_agent_id()
);
CREATE POLICY file_read_host ON checkout_file_reads FOR ALL USING (
    workspace_id=office_workspace_id() AND host_runner_id=office_checkout_runner_id()
    AND office_hosts_checkout(workspace_id,checkout_id)
) WITH CHECK (
    workspace_id=office_workspace_id() AND host_runner_id=office_checkout_runner_id()
    AND office_hosts_checkout(workspace_id,checkout_id)
);
