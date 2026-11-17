CREATE SEQUENCE execution_stop_generation AS bigint;

CREATE TABLE execution_controls (
    workspace_id uuid NOT NULL REFERENCES workspaces(id),
    kind text NOT NULL CHECK (kind IN ('workspace','owner','team','agent')),
    subject_id uuid NOT NULL,
    owner_user_id uuid,
    revision bigint NOT NULL CHECK (revision>0),
    mode text NOT NULL CHECK (mode IN ('running','paused','stopped')),
    last_stop_generation bigint NOT NULL CHECK (last_stop_generation>=0),
    max_outstanding integer NOT NULL CHECK (max_outstanding BETWEEN 0 AND 4096),
    max_concurrent integer NOT NULL CHECK (max_concurrent BETWEEN 0 AND 256),
    reason text NOT NULL CHECK (octet_length(reason) BETWEEN 1 AND 2048),
    updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (workspace_id,kind,subject_id),
    FOREIGN KEY (workspace_id,owner_user_id) REFERENCES workspace_memberships(workspace_id,user_id),
    CHECK ((kind='workspace')=(owner_user_id IS NULL)),
    CHECK (kind<>'workspace' OR subject_id=workspace_id),
    CHECK (kind<>'owner' OR subject_id=owner_user_id),
    CHECK (kind IN ('workspace','owner') OR (mode='running' AND last_stop_generation=0)),
    CHECK (max_concurrent<=max_outstanding)
);

ALTER TABLE execution_controls ENABLE ROW LEVEL SECURITY;
ALTER TABLE execution_controls FORCE ROW LEVEL SECURITY;
CREATE POLICY execution_control_read ON execution_controls FOR SELECT USING (
    workspace_id=office_workspace_id() AND (kind='workspace' OR owner_user_id=office_user_id())
);
CREATE POLICY execution_control_write ON execution_controls FOR ALL USING (
    workspace_id=office_workspace_id() AND office_agent_id() IS NULL AND office_checkout_runner_id() IS NULL
    AND (owner_user_id=office_user_id() OR (kind='workspace' AND EXISTS (
        SELECT 1 FROM workspace_memberships m WHERE (m.workspace_id,m.user_id)=(execution_controls.workspace_id,office_user_id())
          AND m.role IN ('owner','admin') AND m.revoked_at IS NULL
    )))
) WITH CHECK (
    workspace_id=office_workspace_id() AND office_agent_id() IS NULL AND office_checkout_runner_id() IS NULL
    AND (owner_user_id=office_user_id() OR (kind='workspace' AND EXISTS (
        SELECT 1 FROM workspace_memberships m WHERE (m.workspace_id,m.user_id)=(execution_controls.workspace_id,office_user_id())
          AND m.role IN ('owner','admin') AND m.revoked_at IS NULL
    )))
);

ALTER TABLE runs ADD COLUMN stop_generation bigint NOT NULL DEFAULT 0 CHECK (stop_generation>=0);
ALTER TABLE execution_start_effects ADD COLUMN runner_credential_id uuid REFERENCES credentials(id);
ALTER TABLE execution_start_effects ADD COLUMN permit_until timestamptz;
ALTER TABLE execution_start_effects ADD CHECK ((runner_credential_id IS NULL)=(permit_until IS NULL));
CREATE FUNCTION office_execution_stopped(workspace uuid, owner_id uuid, generation bigint)
RETURNS boolean LANGUAGE sql STABLE SET search_path=pg_catalog,public AS $$
    SELECT EXISTS (SELECT 1 FROM execution_controls c
       WHERE c.workspace_id=workspace AND c.last_stop_generation>generation
         AND ((c.kind='workspace' AND c.subject_id=workspace)
           OR (c.kind='owner' AND c.subject_id=owner_id)))
$$;

ALTER POLICY expired_run_read ON runs USING (
    workspace_id=office_workspace_id() AND executor_user_id=office_user_id()
    AND managed_job_id IS NOT NULL AND office_runner_planning()
    AND (office_recovers_run(id) OR (status IN ('starting','running','cancel_requested')
         AND (lease_until<=statement_timestamp()
             OR office_execution_stopped(workspace_id,executor_user_id,stop_generation))))
);
ALTER POLICY expired_run_recover ON runs USING (
    workspace_id=office_workspace_id() AND executor_user_id=office_user_id()
    AND managed_job_id=office_execution_job() AND office_recovers_run(id)
    AND status IN ('starting','running','cancel_requested')
    AND (lease_until<=statement_timestamp() OR office_execution_stopped(workspace_id,executor_user_id,stop_generation))
) WITH CHECK (
    workspace_id=office_workspace_id() AND executor_user_id=office_user_id()
    AND managed_job_id=office_execution_job() AND office_recovers_run(id)
    AND status IN ('lost','cancelled','start_unknown','unknown')
);
ALTER TABLE execution_jobs DROP CONSTRAINT execution_jobs_terminal_reason_check;
ALTER TABLE execution_jobs ADD CHECK (terminal_reason IN
    ('attempt_limit','eligibility_changed','start_interrupted','execution_failed','input_expired','emergency_stopped'));

CREATE INDEX execution_jobs_outstanding_by_workspace ON execution_jobs(workspace_id, owner_user_id, team_id, worker_agent_id)
    INCLUDE (state) WHERE state IN ('queued','leased','reconcile_required','waiting_input');

-- The scheduling transaction uses this context only for its aggregate capacity query.
CREATE POLICY execution_job_capacity_read ON execution_jobs FOR SELECT USING (
    workspace_id=office_workspace_id()
    AND workspace_id=nullif(current_setting('office.execution_capacity_workspace',true),'')::uuid
    AND state IN ('queued','leased','reconcile_required','waiting_input')
);
