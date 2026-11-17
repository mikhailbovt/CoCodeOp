CREATE TABLE activated_executions (
    job_id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    owner_user_id uuid NOT NULL,
    agent_id uuid NOT NULL,
    runner_id uuid NOT NULL,
    policy_revision bigint NOT NULL CHECK (policy_revision > 0),
    thread_id uuid NOT NULL,
    trigger text NOT NULL CHECK (trigger IN ('task.assignment_requested','message.question','task.review_requested','message.answer')),
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (workspace_id, job_id),
    FOREIGN KEY (workspace_id, owner_user_id, job_id) REFERENCES execution_jobs (workspace_id, owner_user_id, id),
    FOREIGN KEY (workspace_id, agent_id, owner_user_id) REFERENCES agents (workspace_id, id, owner_user_id),
    FOREIGN KEY (workspace_id, runner_id, owner_user_id) REFERENCES runners (workspace_id, id, owner_user_id),
    FOREIGN KEY (workspace_id, agent_id) REFERENCES activation_policies (workspace_id, agent_id)
);
CREATE INDEX activated_executions_by_thread ON activated_executions (workspace_id, agent_id, thread_id);
CREATE INDEX activated_executions_recent ON activated_executions (workspace_id, agent_id, created_at DESC);

CREATE TABLE activation_events (
    workspace_id uuid NOT NULL,
    event_id uuid NOT NULL,
    job_id uuid NOT NULL,
    PRIMARY KEY (workspace_id, event_id),
    FOREIGN KEY (workspace_id, event_id) REFERENCES events (workspace_id, id),
    FOREIGN KEY (workspace_id, job_id) REFERENCES activated_executions (workspace_id, job_id)
);
CREATE INDEX activation_events_by_job ON activation_events (workspace_id,job_id);
CREATE INDEX addressed_events_by_recipient ON events (workspace_id,recipient_user_id,recipient_agent_id,sequence)
    WHERE recipient_agent_id IS NOT NULL;

ALTER TABLE activated_executions ENABLE ROW LEVEL SECURITY;
ALTER TABLE activated_executions FORCE ROW LEVEL SECURITY;
CREATE POLICY activated_execution_read ON activated_executions FOR SELECT USING (
    workspace_id=office_workspace_id() AND owner_user_id=office_user_id()
    AND (office_agent_id() IS NULL OR agent_id=office_agent_id())
);
CREATE POLICY activated_execution_enqueue ON activated_executions FOR INSERT WITH CHECK (
    workspace_id=office_workspace_id() AND owner_user_id=office_user_id() AND office_agent_id() IS NULL
    AND office_runner_planning() AND runner_id=office_checkout_runner_id() AND job_id=office_execution_job()
    AND EXISTS (SELECT 1 FROM activation_policies p WHERE p.workspace_id=activated_executions.workspace_id
                AND p.agent_id=activated_executions.agent_id AND p.revision=activated_executions.policy_revision
                AND p.managed->>'runner_id'=runner_id::text)
);

ALTER TABLE activation_events ENABLE ROW LEVEL SECURITY;
ALTER TABLE activation_events FORCE ROW LEVEL SECURITY;
CREATE POLICY activation_events_read ON activation_events FOR SELECT USING (
    workspace_id=office_workspace_id() AND EXISTS (
        SELECT 1 FROM activated_executions a WHERE (a.workspace_id,a.job_id)=(activation_events.workspace_id,activation_events.job_id)
    )
);
CREATE POLICY activation_events_enqueue ON activation_events FOR INSERT WITH CHECK (
    workspace_id=office_workspace_id() AND office_runner_planning() AND office_agent_id() IS NULL
    AND job_id=office_execution_job() AND EXISTS (
        SELECT 1 FROM activated_executions a WHERE (a.workspace_id,a.job_id)=(activation_events.workspace_id,activation_events.job_id)
        AND a.runner_id=office_checkout_runner_id()
    )
);

CREATE POLICY execution_job_enqueue_activation ON execution_jobs FOR INSERT WITH CHECK (
    workspace_id=office_workspace_id() AND owner_user_id=office_user_id() AND office_agent_id() IS NULL
    AND office_runner_planning() AND issuer_agent_id IS NULL AND state='queued'
    AND id=nullif(current_setting('office.activation_job_context',true),'')::uuid
    AND EXISTS (SELECT 1 FROM activation_policies p WHERE p.workspace_id=execution_jobs.workspace_id
        AND p.agent_id=execution_jobs.worker_agent_id AND p.owner_user_id=execution_jobs.owner_user_id
        AND p.managed->>'runner_id'=office_checkout_runner_id()::text
        AND p.managed->>'runtime_template_id'=execution_jobs.runtime_template_id::text)
    AND EXISTS (SELECT 1 FROM tasks t WHERE (t.workspace_id,t.id)=(execution_jobs.workspace_id,execution_jobs.task_id)
        AND t.accountable_user_id=execution_jobs.owner_user_id AND t.revision=execution_jobs.task_revision
        AND t.assignee_agent_id=execution_jobs.worker_agent_id AND t.assignee_user_id=execution_jobs.owner_user_id
        AND t.status='assigned' AND t.team_id IS NOT DISTINCT FROM execution_jobs.team_id)
);
