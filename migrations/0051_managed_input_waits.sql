ALTER TABLE runs DROP CONSTRAINT runs_status_check;
ALTER TABLE runs DROP CONSTRAINT run_terminal_time;
ALTER TABLE runs ADD CHECK (status IN ('starting','start_unknown','running','waiting_input','paused','succeeded',
    'failed','cancel_requested','cancelled','lost','unknown'));
ALTER TABLE runs ADD CONSTRAINT run_terminal_time
    CHECK ((ended_at IS NOT NULL) = (status IN ('paused','succeeded','failed','cancelled','lost')));
ALTER TABLE runs ADD CHECK (status<>'paused' OR managed_job_id IS NOT NULL);

ALTER TABLE execution_jobs DROP CONSTRAINT execution_jobs_state_check;
ALTER TABLE execution_jobs DROP CONSTRAINT execution_job_terminal_time;
ALTER TABLE execution_jobs DROP CONSTRAINT execution_jobs_terminal_reason_check;
ALTER TABLE execution_jobs ADD CHECK (state IN ('queued','leased','waiting_input','resumed','reconcile_required','cancelled','dead_letter','completed'));
ALTER TABLE execution_jobs ADD CONSTRAINT execution_job_terminal_time
    CHECK ((state IN ('resumed','cancelled','dead_letter','completed')) = (ended_at IS NOT NULL));
ALTER TABLE execution_jobs ADD CHECK (terminal_reason IN ('attempt_limit','eligibility_changed','start_interrupted','execution_failed','input_expired'));
ALTER TABLE execution_jobs ADD CHECK (state NOT IN ('waiting_input','resumed') OR current_run_id IS NOT NULL);
DROP INDEX one_active_execution_per_task;
CREATE UNIQUE INDEX one_active_execution_per_task ON execution_jobs (workspace_id,task_id)
    WHERE state IN ('queued','leased','reconcile_required','waiting_input');

CREATE TABLE managed_input_waits (
    workspace_id uuid NOT NULL,
    task_id uuid NOT NULL,
    run_id uuid NOT NULL,
    job_id uuid NOT NULL UNIQUE,
    activation_job_id uuid,
    requester_user_id uuid NOT NULL,
    requester_agent_id uuid NOT NULL,
    checkpoint_id text NOT NULL CHECK (checkpoint_id ~ '^[a-zA-Z0-9_:.\-]{1,160}$'),
    question text NOT NULL CHECK (octet_length(question) BETWEEN 1 AND 3000),
    expires_at timestamptz NOT NULL DEFAULT clock_timestamp()+interval '1 day',
    answer text CHECK (octet_length(answer) BETWEEN 1 AND 4096),
    respondent_user_id uuid,
    respondent_agent_id uuid,
    resumed_job_id uuid UNIQUE,
    PRIMARY KEY (workspace_id, run_id),
    CHECK (num_nonnulls(answer,respondent_user_id,resumed_job_id) IN (0,3)),
    FOREIGN KEY (workspace_id,task_id,run_id) REFERENCES runs (workspace_id,task_id,id),
    FOREIGN KEY (workspace_id,requester_user_id,job_id) REFERENCES execution_jobs (workspace_id,owner_user_id,id),
    FOREIGN KEY (workspace_id,activation_job_id) REFERENCES activated_executions (workspace_id,job_id),
    FOREIGN KEY (workspace_id,requester_user_id,resumed_job_id) REFERENCES execution_jobs (workspace_id,owner_user_id,id),
    FOREIGN KEY (workspace_id,requester_agent_id,requester_user_id) REFERENCES agents (workspace_id,id,owner_user_id),
    FOREIGN KEY (workspace_id,respondent_user_id) REFERENCES workspace_memberships (workspace_id,user_id),
    FOREIGN KEY (workspace_id,respondent_agent_id,respondent_user_id) REFERENCES agents (workspace_id,id,owner_user_id)
);
CREATE INDEX managed_input_waits_by_task ON managed_input_waits (workspace_id,task_id);
CREATE INDEX managed_input_waits_by_expiry ON managed_input_waits (workspace_id,requester_user_id,expires_at) WHERE answer IS NULL;
CREATE INDEX managed_input_waits_by_activation ON managed_input_waits (workspace_id,activation_job_id) WHERE resumed_job_id IS NOT NULL;

ALTER TABLE managed_input_waits ENABLE ROW LEVEL SECURITY;
ALTER TABLE managed_input_waits FORCE ROW LEVEL SECURITY;
CREATE POLICY managed_input_read ON managed_input_waits FOR SELECT USING (
    workspace_id=office_workspace_id() AND requester_user_id=office_user_id()
    AND EXISTS (SELECT 1 FROM tasks t WHERE (t.workspace_id,t.id)=(managed_input_waits.workspace_id,managed_input_waits.task_id)
        AND (office_agent_id() IS NULL OR office_agent_id() IN (requester_agent_id,t.accountable_agent_id)))
);
CREATE POLICY managed_input_create ON managed_input_waits FOR INSERT WITH CHECK (
    workspace_id=office_workspace_id() AND requester_user_id=office_user_id() AND office_runner_planning()
    AND job_id=office_execution_job() AND answer IS NULL
    AND EXISTS (SELECT 1 FROM runs r WHERE (r.workspace_id,r.task_id,r.id)=(managed_input_waits.workspace_id,managed_input_waits.task_id,managed_input_waits.run_id)
        AND r.managed_job_id=managed_input_waits.job_id AND r.status='paused'
        AND r.executor_agent_id=managed_input_waits.requester_agent_id AND r.managed_runner_id=office_checkout_runner_id())
);
CREATE POLICY managed_input_answer ON managed_input_waits FOR UPDATE USING (
    workspace_id=office_workspace_id() AND requester_user_id=office_user_id()
    AND office_checkout_runner_id() IS NULL AND job_id=office_execution_job() AND answer IS NULL
    AND EXISTS (SELECT 1 FROM tasks t WHERE (t.workspace_id,t.id)=(managed_input_waits.workspace_id,managed_input_waits.task_id)
        AND t.accountable_user_id=office_user_id() AND (office_agent_id() IS NULL OR t.accountable_agent_id=office_agent_id()))
) WITH CHECK (
    workspace_id=office_workspace_id() AND requester_user_id=office_user_id() AND job_id=office_execution_job()
    AND answer IS NOT NULL AND (respondent_user_id,respondent_agent_id) IS NOT DISTINCT FROM (office_user_id(),office_agent_id())
    AND EXISTS (SELECT 1 FROM execution_jobs j WHERE (j.workspace_id,j.id)=(managed_input_waits.workspace_id,managed_input_waits.resumed_job_id)
        AND j.task_id=managed_input_waits.task_id AND j.worker_agent_id=managed_input_waits.requester_agent_id AND j.state='queued')
);

CREATE FUNCTION office_activation_job(workspace uuid, job uuid) RETURNS uuid LANGUAGE sql STABLE
SET search_path = pg_catalog, public AS $$
    SELECT coalesce((SELECT w.activation_job_id FROM managed_input_waits w
                     WHERE (w.workspace_id,w.resumed_job_id)=(workspace,job)),job)
$$;
