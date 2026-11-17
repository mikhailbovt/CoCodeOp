ALTER TABLE execution_jobs DROP CONSTRAINT execution_jobs_state_check;
ALTER TABLE execution_jobs DROP CONSTRAINT execution_jobs_check;
ALTER TABLE execution_jobs ADD CHECK (state IN ('queued','leased','cancelled','dead_letter'));
ALTER TABLE execution_jobs ADD CONSTRAINT execution_job_terminal_time
    CHECK ((state IN ('cancelled','dead_letter')) = (ended_at IS NOT NULL));
ALTER TABLE execution_jobs ADD COLUMN managed_attempts integer NOT NULL DEFAULT 0 CHECK (managed_attempts BETWEEN 0 AND 3);
ALTER TABLE execution_jobs NO FORCE ROW LEVEL SECURITY;
UPDATE execution_jobs SET managed_attempts=1 WHERE current_run_id IS NOT NULL;
ALTER TABLE execution_jobs FORCE ROW LEVEL SECURITY;
ALTER TABLE execution_jobs ADD COLUMN retry_not_before timestamptz;
ALTER TABLE execution_jobs ADD COLUMN terminal_reason text;
ALTER TABLE execution_jobs ADD CHECK (retry_not_before IS NULL OR state='queued');
ALTER TABLE execution_jobs ADD CHECK ((terminal_reason IS NOT NULL) = (state='dead_letter'));
ALTER TABLE execution_jobs ADD CHECK (terminal_reason IN ('attempt_limit','eligibility_changed'));

ALTER TABLE runs DROP CONSTRAINT runs_status_check;
ALTER TABLE runs DROP CONSTRAINT runs_check;
ALTER TABLE runs ADD CHECK (status IN ('starting','running','waiting_input','succeeded','failed','cancel_requested','cancelled','lost'));
ALTER TABLE runs ADD CONSTRAINT run_terminal_time
    CHECK ((ended_at IS NOT NULL) = (status IN ('succeeded','failed','cancelled','lost')));
CREATE INDEX expired_managed_runs ON runs (workspace_id,executor_user_id,lease_until,id)
    WHERE status='starting';

CREATE FUNCTION office_recovers_run(candidate uuid) RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT office_runner_planning()
       AND candidate = nullif(current_setting('office.execution_recovery_run', true), '')::uuid
$$;

CREATE POLICY expired_run_read ON runs FOR SELECT USING (
    workspace_id=office_workspace_id() AND executor_user_id=office_user_id()
    AND managed_job_id IS NOT NULL AND office_runner_planning()
    AND (office_recovers_run(id) OR (status='starting' AND lease_until<=statement_timestamp()))
);
CREATE POLICY expired_run_recover ON runs FOR UPDATE USING (
    workspace_id=office_workspace_id() AND executor_user_id=office_user_id()
    AND managed_job_id=office_execution_job() AND office_recovers_run(id)
    AND status='starting' AND lease_until<=statement_timestamp()
) WITH CHECK (
    workspace_id=office_workspace_id() AND executor_user_id=office_user_id()
    AND managed_job_id=office_execution_job() AND office_recovers_run(id) AND status='lost'
);
