CREATE TABLE run_credentials (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    owner_user_id uuid NOT NULL,
    job_id uuid NOT NULL,
    run_id uuid NOT NULL UNIQUE,
    worker_agent_id uuid NOT NULL,
    runner_id uuid NOT NULL,
    runner_credential_id uuid NOT NULL REFERENCES credentials(id),
    run_fence bigint NOT NULL,
    digest bytea NOT NULL UNIQUE CHECK (octet_length(digest) = 32),
    issued_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    FOREIGN KEY (workspace_id,owner_user_id,job_id,run_id,runner_id,run_fence)
        REFERENCES runs (workspace_id,executor_user_id,managed_job_id,id,managed_runner_id,run_fence),
    FOREIGN KEY (workspace_id,worker_agent_id,owner_user_id) REFERENCES agents (workspace_id,id,owner_user_id)
);

CREATE FUNCTION office_run_credential_digest() RETURNS bytea LANGUAGE sql STABLE AS $$
    SELECT decode(nullif(current_setting('office.run_credential_digest',true),''),'hex')
$$;

ALTER TABLE run_credentials ENABLE ROW LEVEL SECURITY;
ALTER TABLE run_credentials FORCE ROW LEVEL SECURITY;
CREATE POLICY run_credential_read ON run_credentials FOR SELECT USING (
    digest = office_run_credential_digest() OR (
        workspace_id = office_workspace_id() AND owner_user_id = office_user_id()
        AND job_id = office_execution_job() AND runner_id = office_checkout_runner_id()
        AND office_runner_planning()
    )
);
CREATE POLICY run_credential_issue ON run_credentials FOR INSERT WITH CHECK (
    workspace_id=office_workspace_id() AND owner_user_id=office_user_id()
    AND runner_id=office_checkout_runner_id() AND job_id=office_execution_job() AND office_runner_planning()
    AND EXISTS (
        SELECT 1 FROM runs r WHERE r.workspace_id=run_credentials.workspace_id AND r.id=run_credentials.run_id
        AND r.executor_agent_id=run_credentials.worker_agent_id AND r.run_fence=run_credentials.run_fence
        AND r.status IN ('starting','running') AND r.lease_until>statement_timestamp()
    )
);

CREATE POLICY exact_runner_credential_for_active_run ON credentials FOR SELECT USING (
    runner_id IS NOT NULL AND EXISTS (
        SELECT 1 FROM run_credentials s WHERE s.digest=office_run_credential_digest()
        AND s.runner_credential_id=credentials.id AND s.workspace_id=credentials.workspace_id
        AND s.runner_id=credentials.runner_id AND s.owner_user_id=credentials.owner_user_id
    )
);
