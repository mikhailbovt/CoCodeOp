ALTER TABLE runner_execution_profiles ADD COLUMN hosted_checkout_ids uuid[] NOT NULL DEFAULT '{}'
    CHECK (cardinality(hosted_checkout_ids) <= 32);

CREATE TABLE checkout_execution_presence (
    workspace_id uuid NOT NULL,
    checkout_id uuid NOT NULL,
    owner_user_id uuid NOT NULL,
    runner_id uuid NOT NULL,
    expires_at timestamptz NOT NULL,
    PRIMARY KEY (workspace_id, checkout_id),
    FOREIGN KEY (workspace_id, checkout_id, owner_user_id) REFERENCES project_checkouts (workspace_id, id, owner_user_id),
    FOREIGN KEY (workspace_id, runner_id, owner_user_id) REFERENCES runners (workspace_id, id, owner_user_id)
);
CREATE INDEX checkout_presence_by_runner ON checkout_execution_presence (workspace_id, runner_id);

CREATE FUNCTION office_runner_planning() RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT office_checkout_runner_id() IS NOT NULL
       AND office_checkout_runner_id() = nullif(current_setting('office.runner_planning_id', true), '')::uuid
$$;

ALTER TABLE checkout_execution_presence ENABLE ROW LEVEL SECURITY;
ALTER TABLE checkout_execution_presence FORCE ROW LEVEL SECURITY;
CREATE POLICY checkout_presence_plan ON checkout_execution_presence FOR SELECT USING (
    workspace_id = office_workspace_id()
    AND (office_runner_planning() OR runner_id = office_checkout_runner_id())
);
CREATE POLICY checkout_presence_host ON checkout_execution_presence FOR ALL USING (
    workspace_id = office_workspace_id() AND runner_id = office_checkout_runner_id()
    AND owner_user_id = office_user_id()
) WITH CHECK (
    workspace_id = office_workspace_id() AND runner_id = office_checkout_runner_id()
    AND owner_user_id = office_user_id() AND office_hosts_checkout(workspace_id, checkout_id)
);

ALTER TABLE execution_jobs DROP CONSTRAINT execution_jobs_state_check;
ALTER TABLE execution_jobs ADD CHECK (state IN ('queued', 'leased', 'cancelled'));
ALTER TABLE execution_jobs ADD COLUMN current_run_id uuid;
ALTER TABLE execution_jobs ADD CHECK ((state <> 'leased' OR current_run_id IS NOT NULL)
    AND (state <> 'queued' OR current_run_id IS NULL));
DROP INDEX one_active_execution_per_task;
CREATE UNIQUE INDEX one_active_execution_per_task ON execution_jobs (workspace_id, task_id)
    WHERE state IN ('queued', 'leased');
CREATE POLICY execution_job_runner_read ON execution_jobs FOR SELECT USING (
    workspace_id = office_workspace_id() AND owner_user_id = office_user_id() AND office_runner_planning()
);

CREATE TABLE execution_dispatch_order (
    workspace_id uuid NOT NULL,
    owner_user_id uuid NOT NULL,
    lane_id uuid NOT NULL,
    last_fence bigint NOT NULL CHECK (last_fence > 0),
    PRIMARY KEY (workspace_id, owner_user_id, lane_id),
    FOREIGN KEY (workspace_id, owner_user_id) REFERENCES workspace_memberships (workspace_id, user_id)
);
ALTER TABLE execution_dispatch_order ENABLE ROW LEVEL SECURITY;
ALTER TABLE execution_dispatch_order FORCE ROW LEVEL SECURITY;
CREATE POLICY execution_dispatch_owner ON execution_dispatch_order FOR ALL USING (
    workspace_id = office_workspace_id() AND owner_user_id = office_user_id() AND office_runner_planning()
) WITH CHECK (
    workspace_id = office_workspace_id() AND owner_user_id = office_user_id() AND office_runner_planning()
);

CREATE POLICY task_read_for_scheduling ON tasks FOR SELECT USING (
    workspace_id = office_workspace_id() AND accountable_user_id = office_user_id() AND office_runner_planning()
    AND office_agent_id() IS NULL
    AND id = nullif(current_setting('office.execution_task_context', true), '')::uuid
);
CREATE POLICY task_start_for_scheduling ON tasks FOR UPDATE USING (
    workspace_id = office_workspace_id() AND accountable_user_id = office_user_id() AND office_runner_planning()
    AND office_agent_id() IS NULL
    AND id = nullif(current_setting('office.execution_task_context', true), '')::uuid
) WITH CHECK (
    workspace_id = office_workspace_id() AND accountable_user_id = office_user_id() AND office_runner_planning()
    AND office_agent_id() IS NULL
    AND id = nullif(current_setting('office.execution_task_context', true), '')::uuid
);

CREATE SEQUENCE execution_run_fence AS bigint;
ALTER TABLE runs DROP CONSTRAINT runs_status_check;
ALTER TABLE runs DROP CONSTRAINT runs_check2;
ALTER TABLE runs ADD COLUMN managed_job_id uuid;
ALTER TABLE runs ADD COLUMN managed_runner_id uuid;
ALTER TABLE runs ADD COLUMN run_fence bigint;
ALTER TABLE runs ADD COLUMN lease_until timestamptz;
ALTER TABLE runs ADD COLUMN runner_claim_key uuid;
ALTER TABLE runs ADD CHECK (status IN ('starting','running','waiting_input','succeeded','failed','cancel_requested','cancelled'));
ALTER TABLE runs ADD CHECK (status <> 'starting' OR managed_job_id IS NOT NULL);
ALTER TABLE runs ADD CHECK (num_nonnulls(managed_job_id,managed_runner_id,run_fence,lease_until,runner_claim_key) IN (0,5));
ALTER TABLE runs ADD CHECK ((managed_job_id IS NULL AND (instance_id IS NULL) = (executor_agent_id IS NULL))
    OR (managed_job_id IS NOT NULL AND executor_agent_id IS NOT NULL));
ALTER TABLE runs ADD UNIQUE (workspace_id, managed_runner_id, runner_claim_key);
ALTER TABLE runs ADD UNIQUE (run_fence);
ALTER TABLE runs ADD FOREIGN KEY (workspace_id, executor_user_id, managed_job_id)
    REFERENCES execution_jobs (workspace_id, owner_user_id, id);
ALTER TABLE runs ADD FOREIGN KEY (workspace_id, managed_runner_id, executor_user_id)
    REFERENCES runners (workspace_id, id, owner_user_id);
ALTER TABLE execution_jobs ADD FOREIGN KEY (workspace_id, task_id, current_run_id)
    REFERENCES runs (workspace_id, task_id, id);
CREATE INDEX runner_starting_count ON runs (workspace_id, managed_runner_id) WHERE status = 'starting';
DROP INDEX one_active_run_per_task;
CREATE UNIQUE INDEX one_active_run_per_task ON runs (workspace_id, task_id)
    WHERE status IN ('starting', 'running', 'waiting_input', 'cancel_requested');

ALTER POLICY run_start ON runs WITH CHECK (
    workspace_id = office_workspace_id() AND executor_user_id = office_user_id()
    AND executor_agent_id IS NOT DISTINCT FROM office_agent_id() AND managed_job_id IS NULL
    AND EXISTS (SELECT 1 FROM tasks task WHERE task.workspace_id = runs.workspace_id AND task.id = runs.task_id)
);
ALTER POLICY run_change ON runs USING (
    workspace_id = office_workspace_id() AND managed_job_id IS NULL
    AND EXISTS (SELECT 1 FROM tasks task WHERE task.workspace_id = runs.workspace_id AND task.id = runs.task_id)
);
CREATE POLICY managed_run_read ON runs FOR SELECT USING (
    workspace_id = office_workspace_id() AND executor_user_id = office_user_id()
    AND managed_runner_id = office_checkout_runner_id() AND office_runner_planning()
);
CREATE POLICY managed_run_start ON runs FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND executor_user_id = office_user_id()
    AND managed_runner_id = office_checkout_runner_id() AND office_runner_planning()
    AND managed_job_id = office_execution_job() AND status = 'starting'
    AND EXISTS (
        SELECT 1 FROM execution_jobs j WHERE j.workspace_id = runs.workspace_id AND j.id = managed_job_id
        AND j.owner_user_id = runs.executor_user_id AND j.worker_agent_id = runs.executor_agent_id
        AND j.task_id = runs.task_id AND j.runtime_template_id = runs.runtime_template_id AND j.state = 'queued'
    )
);
CREATE POLICY managed_run_control ON runs FOR UPDATE USING (
    workspace_id = office_workspace_id() AND executor_user_id = office_user_id()
    AND managed_job_id = office_execution_job()
    AND (managed_runner_id = office_checkout_runner_id() OR office_checkout_runner_id() IS NULL)
) WITH CHECK (
    workspace_id = office_workspace_id() AND executor_user_id = office_user_id()
    AND managed_job_id = office_execution_job()
    AND (managed_runner_id = office_checkout_runner_id() OR office_checkout_runner_id() IS NULL)
);
