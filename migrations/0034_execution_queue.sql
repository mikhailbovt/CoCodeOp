ALTER TABLE execution_budgets ADD UNIQUE (workspace_id, owner_user_id, id);

CREATE TABLE execution_jobs (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    task_id uuid NOT NULL,
    task_revision bigint NOT NULL CHECK (task_revision > 0),
    owner_user_id uuid NOT NULL,
    issuer_agent_id uuid,
    worker_agent_id uuid NOT NULL,
    team_id uuid,
    runtime_template_id uuid NOT NULL,
    checkout_id uuid,
    scope_id uuid,
    scope_revision bigint,
    revision bigint NOT NULL DEFAULT 1 CHECK (revision > 0),
    state text NOT NULL DEFAULT 'queued' CHECK (state IN ('queued', 'cancelled')),
    reserved_units bigint NOT NULL CHECK (reserved_units > 0),
    enqueued_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    ended_at timestamptz,
    CHECK ((state = 'cancelled') = (ended_at IS NOT NULL)),
    CHECK (num_nonnulls(checkout_id, scope_id, scope_revision) IN (0, 3)),
    UNIQUE (workspace_id, owner_user_id, id),
    FOREIGN KEY (workspace_id, task_id) REFERENCES tasks (workspace_id, id),
    FOREIGN KEY (workspace_id, owner_user_id, runtime_template_id) REFERENCES runtime_templates (workspace_id, owner_user_id, id),
    FOREIGN KEY (workspace_id, worker_agent_id, owner_user_id) REFERENCES agents (workspace_id, id, owner_user_id),
    FOREIGN KEY (workspace_id, issuer_agent_id, owner_user_id) REFERENCES agents (workspace_id, id, owner_user_id),
    FOREIGN KEY (workspace_id, team_id, owner_user_id) REFERENCES teams (workspace_id, id, owner_user_id),
    FOREIGN KEY (workspace_id, checkout_id, scope_id) REFERENCES work_scopes (workspace_id, checkout_id, id),
    FOREIGN KEY (workspace_id, scope_id, scope_revision) REFERENCES work_scope_revisions (workspace_id, scope_id, revision)
);

CREATE UNIQUE INDEX one_active_execution_per_task ON execution_jobs (workspace_id, task_id)
    WHERE state = 'queued';
CREATE INDEX execution_queue_by_owner ON execution_jobs (workspace_id, owner_user_id, enqueued_at, id)
    WHERE state = 'queued';

CREATE TABLE execution_budget_reservations (
    job_id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    owner_user_id uuid NOT NULL,
    budget_id uuid NOT NULL,
    units bigint NOT NULL CHECK (units > 0),
    reserved_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    released_at timestamptz,
    FOREIGN KEY (workspace_id, owner_user_id, job_id) REFERENCES execution_jobs (workspace_id, owner_user_id, id),
    FOREIGN KEY (workspace_id, owner_user_id, budget_id) REFERENCES execution_budgets (workspace_id, owner_user_id, id)
);

CREATE FUNCTION office_execution_job() RETURNS uuid LANGUAGE sql STABLE AS $$
    SELECT nullif(current_setting('office.execution_job_context', true), '')::uuid
$$;

ALTER TABLE execution_jobs ENABLE ROW LEVEL SECURITY;
ALTER TABLE execution_jobs FORCE ROW LEVEL SECURITY;
CREATE POLICY execution_job_read ON execution_jobs FOR SELECT USING (
    workspace_id = office_workspace_id() AND (
        task_id = nullif(current_setting('office.execution_task_context', true), '')::uuid OR (
            owner_user_id = office_user_id() AND office_checkout_runner_id() IS NULL
            AND (office_agent_id() IS NULL OR office_agent_id() IN (issuer_agent_id, worker_agent_id))
            AND EXISTS (SELECT 1 FROM tasks t WHERE t.workspace_id = execution_jobs.workspace_id AND t.id = execution_jobs.task_id)
        )
    )
);
CREATE POLICY execution_job_create ON execution_jobs FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND owner_user_id = office_user_id() AND office_checkout_runner_id() IS NULL
    AND issuer_agent_id IS NOT DISTINCT FROM office_agent_id() AND state = 'queued'
    AND EXISTS (SELECT 1 FROM tasks t WHERE t.workspace_id = execution_jobs.workspace_id AND t.id = execution_jobs.task_id
        AND t.accountable_user_id = execution_jobs.owner_user_id AND t.revision = execution_jobs.task_revision
        AND t.team_id IS NOT DISTINCT FROM execution_jobs.team_id AND t.status IN ('ready','assigned'))
);
CREATE POLICY execution_job_change ON execution_jobs FOR UPDATE USING (
    workspace_id = office_workspace_id() AND id = office_execution_job()
) WITH CHECK (
    workspace_id = office_workspace_id() AND id = office_execution_job()
);

CREATE POLICY budget_for_execution_read ON execution_budgets FOR SELECT USING (
    workspace_id = office_workspace_id() AND owner_user_id = office_user_id() AND EXISTS (
        SELECT 1 FROM execution_jobs j WHERE j.workspace_id = execution_budgets.workspace_id AND j.id = office_execution_job()
        AND j.owner_user_id = execution_budgets.owner_user_id
    )
);
CREATE POLICY budget_for_execution_update ON execution_budgets FOR UPDATE USING (
    workspace_id = office_workspace_id() AND owner_user_id = office_user_id() AND EXISTS (
        SELECT 1 FROM execution_jobs j WHERE j.workspace_id = execution_budgets.workspace_id AND j.id = office_execution_job()
        AND j.owner_user_id = execution_budgets.owner_user_id
    )
);

ALTER TABLE execution_budget_reservations ENABLE ROW LEVEL SECURITY;
ALTER TABLE execution_budget_reservations FORCE ROW LEVEL SECURITY;
CREATE POLICY execution_budget_reservation_scope ON execution_budget_reservations FOR ALL USING (
    workspace_id = office_workspace_id() AND owner_user_id = office_user_id() AND job_id = office_execution_job()
) WITH CHECK (
    workspace_id = office_workspace_id() AND owner_user_id = office_user_id() AND job_id = office_execution_job()
);
