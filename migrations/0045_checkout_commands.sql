CREATE TABLE checkout_commands (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    checkout_id uuid NOT NULL,
    host_runner_id uuid NOT NULL,
    epoch bigint NOT NULL,
    task_id uuid NOT NULL,
    run_id uuid NOT NULL,
    executor_user_id uuid NOT NULL,
    worker_agent_id uuid NOT NULL,
    scope_id uuid NOT NULL,
    scope_revision bigint NOT NULL,
    template_id uuid NOT NULL REFERENCES command_templates(id),
    idempotency_key uuid NOT NULL,
    request_sha256 bytea NOT NULL CHECK (octet_length(request_sha256)=32),
    spec jsonb NOT NULL,
    argv jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    expires_at timestamptz NOT NULL,
    offered_until timestamptz,
    permit_until timestamptz,
    scope_definition jsonb,
    head jsonb,
    snapshot jsonb,
    outcome jsonb,
    state text NOT NULL DEFAULT 'queued' CHECK (state IN ('queued','authorized','running','finished')),
    CHECK (state<>'running' OR snapshot IS NOT NULL),
    CHECK ((state='finished')=(outcome IS NOT NULL)),
    CHECK (state NOT IN ('authorized','running') OR num_nonnulls(scope_definition,head,permit_until)=3),
    UNIQUE (workspace_id,run_id,idempotency_key),
    FOREIGN KEY (workspace_id,checkout_id,epoch) REFERENCES checkout_coordinator_sessions(workspace_id,checkout_id,epoch),
    FOREIGN KEY (workspace_id,task_id,run_id) REFERENCES runs(workspace_id,task_id,id),
    FOREIGN KEY (workspace_id,scope_id,scope_revision) REFERENCES work_scope_revisions(workspace_id,scope_id,revision)
);
CREATE UNIQUE INDEX one_checkout_command ON checkout_commands(workspace_id,checkout_id) WHERE state IN ('authorized','running');
CREATE INDEX queued_checkout_commands ON checkout_commands(workspace_id,checkout_id,epoch,created_at) WHERE state='queued';

ALTER TABLE checkout_commands ENABLE ROW LEVEL SECURITY;
ALTER TABLE checkout_commands FORCE ROW LEVEL SECURITY;
CREATE POLICY checkout_command_host ON checkout_commands FOR ALL USING (
    workspace_id=office_workspace_id() AND host_runner_id=office_checkout_runner_id()
    AND office_hosts_checkout(workspace_id,checkout_id)
) WITH CHECK (
    workspace_id=office_workspace_id() AND host_runner_id=office_checkout_runner_id()
    AND office_hosts_checkout(workspace_id,checkout_id)
);
CREATE POLICY checkout_command_worker ON checkout_commands FOR SELECT USING (
    workspace_id=office_workspace_id() AND checkout_id=office_gateway_checkout() AND run_id=office_gateway_run()
    AND executor_user_id=office_user_id() AND worker_agent_id=office_agent_id()
);
CREATE POLICY checkout_command_request ON checkout_commands FOR INSERT WITH CHECK (
    workspace_id=office_workspace_id() AND checkout_id=office_gateway_checkout() AND run_id=office_gateway_run()
    AND executor_user_id=office_user_id() AND worker_agent_id=office_agent_id() AND state='queued'
);
CREATE POLICY checkout_command_authorize ON checkout_commands FOR UPDATE USING (
    workspace_id=office_workspace_id() AND checkout_id=office_gateway_checkout() AND run_id=office_gateway_run()
    AND executor_user_id=office_user_id() AND worker_agent_id=office_agent_id() AND state='queued'
) WITH CHECK (
    workspace_id=office_workspace_id() AND checkout_id=office_gateway_checkout() AND run_id=office_gateway_run()
    AND executor_user_id=office_user_id() AND worker_agent_id=office_agent_id() AND state='authorized'
);
