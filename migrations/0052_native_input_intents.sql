CREATE TABLE managed_input_intents (
    workspace_id uuid NOT NULL,
    task_id uuid NOT NULL,
    run_id uuid NOT NULL,
    job_id uuid NOT NULL,
    requester_user_id uuid NOT NULL,
    requester_agent_id uuid NOT NULL,
    idempotency_key uuid NOT NULL,
    question text NOT NULL CHECK (octet_length(question) BETWEEN 1 AND 3000),
    PRIMARY KEY (workspace_id,run_id),
    FOREIGN KEY (workspace_id,task_id,run_id) REFERENCES runs (workspace_id,task_id,id),
    FOREIGN KEY (workspace_id,requester_user_id,job_id) REFERENCES execution_jobs (workspace_id,owner_user_id,id),
    FOREIGN KEY (workspace_id,requester_agent_id,requester_user_id) REFERENCES agents (workspace_id,id,owner_user_id)
);

ALTER TABLE managed_input_intents ENABLE ROW LEVEL SECURITY;
ALTER TABLE managed_input_intents FORCE ROW LEVEL SECURITY;
CREATE POLICY managed_input_intent_read ON managed_input_intents FOR SELECT USING (
    workspace_id=office_workspace_id() AND requester_user_id=office_user_id()
    AND (requester_agent_id=office_agent_id() OR (office_runner_planning() AND job_id=office_execution_job()))
);
CREATE POLICY managed_input_intent_create ON managed_input_intents FOR INSERT WITH CHECK (
    workspace_id=office_workspace_id() AND requester_user_id=office_user_id() AND requester_agent_id=office_agent_id()
    AND EXISTS (SELECT 1 FROM runs r WHERE (r.workspace_id,r.task_id,r.id)=(managed_input_intents.workspace_id,
            managed_input_intents.task_id,managed_input_intents.run_id)
        AND r.managed_job_id=managed_input_intents.job_id AND r.status='running' AND r.lease_until>clock_timestamp()
        AND (r.executor_user_id,r.executor_agent_id)=(requester_user_id,requester_agent_id))
);
