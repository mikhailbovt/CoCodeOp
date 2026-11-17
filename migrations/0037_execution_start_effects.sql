ALTER TABLE execution_jobs DROP CONSTRAINT execution_jobs_state_check;
ALTER TABLE execution_jobs ADD CHECK (state IN ('queued','leased','reconcile_required','cancelled','dead_letter'));
ALTER TABLE execution_jobs ADD CHECK (state <> 'reconcile_required' OR current_run_id IS NOT NULL);
ALTER TABLE execution_jobs DROP CONSTRAINT execution_jobs_terminal_reason_check;
ALTER TABLE execution_jobs ADD CHECK (terminal_reason IN ('attempt_limit','eligibility_changed','start_interrupted'));
DROP INDEX one_active_execution_per_task;
CREATE UNIQUE INDEX one_active_execution_per_task ON execution_jobs (workspace_id,task_id)
    WHERE state IN ('queued','leased','reconcile_required');

ALTER TABLE runs DROP CONSTRAINT runs_status_check;
ALTER TABLE runs ADD CHECK (status IN ('starting','start_unknown','running','waiting_input','succeeded',
    'failed','cancel_requested','cancelled','lost','unknown'));
ALTER TABLE runs ADD CHECK (status NOT IN ('start_unknown','lost','unknown') OR managed_job_id IS NOT NULL);
ALTER TABLE runs ADD UNIQUE (workspace_id,executor_user_id,managed_job_id,id,managed_runner_id,run_fence);
DROP INDEX one_active_run_per_task;
CREATE UNIQUE INDEX one_active_run_per_task ON runs (workspace_id,task_id)
    WHERE status IN ('starting','running','waiting_input','cancel_requested','start_unknown','unknown');
DROP INDEX runner_starting_count;
CREATE INDEX runner_active_runs ON runs (workspace_id,managed_runner_id)
    WHERE status IN ('starting','running','waiting_input','cancel_requested','start_unknown','unknown');
DROP INDEX expired_managed_runs;
CREATE INDEX expired_managed_runs ON runs (workspace_id,executor_user_id,lease_until,id)
    WHERE managed_job_id IS NOT NULL AND status IN ('starting','running','cancel_requested');

ALTER POLICY expired_run_read ON runs USING (
    workspace_id=office_workspace_id() AND executor_user_id=office_user_id()
    AND managed_job_id IS NOT NULL AND office_runner_planning()
    AND (office_recovers_run(id) OR (status IN ('starting','running','cancel_requested')
         AND lease_until<=statement_timestamp()))
);
ALTER POLICY expired_run_recover ON runs USING (
    workspace_id=office_workspace_id() AND executor_user_id=office_user_id()
    AND managed_job_id=office_execution_job() AND office_recovers_run(id)
    AND status IN ('starting','running','cancel_requested') AND lease_until<=statement_timestamp()
) WITH CHECK (
    workspace_id=office_workspace_id() AND executor_user_id=office_user_id()
    AND managed_job_id=office_execution_job() AND office_recovers_run(id)
    AND status IN ('lost','start_unknown','unknown')
);

ALTER TABLE execution_budget_reservations ADD COLUMN charged_units bigint NOT NULL DEFAULT 0;
ALTER TABLE execution_budget_reservations ADD CHECK (charged_units BETWEEN 0 AND units);
ALTER TABLE execution_budget_reservations ADD CHECK (released_at IS NOT NULL OR charged_units=0);

CREATE TABLE execution_start_effects (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    owner_user_id uuid NOT NULL,
    job_id uuid NOT NULL,
    run_id uuid NOT NULL UNIQUE,
    runner_id uuid NOT NULL,
    run_fence bigint NOT NULL,
    idempotency_key uuid NOT NULL,
    approval_sha256 text NOT NULL CHECK (approval_sha256 ~ '^[a-f0-9]{64}$'),
    state text NOT NULL CHECK (state IN ('authorized','started','unknown','never_started','stopped')),
    external_id text CHECK (external_id ~ '^[A-Za-z0-9_.:-]{1,128}$'),
    authorized_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    confirmed_at timestamptz,
    reconciled_at timestamptz,
    UNIQUE (workspace_id,runner_id,idempotency_key),
    CHECK ((state IN ('never_started','stopped')) = (reconciled_at IS NOT NULL)),
    CHECK (state NOT IN ('authorized','never_started') OR external_id IS NULL),
    CHECK (state NOT IN ('started','stopped') OR external_id IS NOT NULL),
    CHECK (confirmed_at IS NULL OR external_id IS NOT NULL),
    FOREIGN KEY (workspace_id,owner_user_id,job_id,run_id,runner_id,run_fence)
        REFERENCES runs (workspace_id,executor_user_id,managed_job_id,id,managed_runner_id,run_fence)
);

CREATE FUNCTION office_controls_start_effect(job uuid, run uuid, runner uuid) RETURNS boolean
LANGUAGE sql STABLE AS $$
    SELECT job=office_execution_job() AND (
        runner=office_checkout_runner_id() OR office_checkout_runner_id() IS NULL OR office_recovers_run(run))
$$;
ALTER TABLE execution_start_effects ENABLE ROW LEVEL SECURITY;
ALTER TABLE execution_start_effects FORCE ROW LEVEL SECURITY;
CREATE POLICY execution_start_effect_read ON execution_start_effects FOR SELECT USING (
    workspace_id=office_workspace_id() AND owner_user_id=office_user_id()
    AND office_controls_start_effect(job_id,run_id,runner_id)
);
CREATE POLICY execution_start_effect_insert ON execution_start_effects FOR INSERT WITH CHECK (
    workspace_id=office_workspace_id() AND owner_user_id=office_user_id()
    AND runner_id=office_checkout_runner_id() AND job_id=office_execution_job() AND state='authorized'
);
CREATE POLICY execution_start_effect_change ON execution_start_effects FOR UPDATE USING (
    workspace_id=office_workspace_id() AND owner_user_id=office_user_id()
    AND job_id=office_execution_job() AND (runner_id=office_checkout_runner_id() OR office_recovers_run(run_id))
) WITH CHECK (
    workspace_id=office_workspace_id() AND owner_user_id=office_user_id()
    AND job_id=office_execution_job() AND (runner_id=office_checkout_runner_id() OR (office_recovers_run(run_id) AND state='unknown'))
);
