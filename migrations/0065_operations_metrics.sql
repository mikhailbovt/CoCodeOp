ALTER TABLE checkout_publications ADD COLUMN finalized_at timestamptz;

CREATE INDEX metrics_owned_runs_recent ON runs (workspace_id, executor_user_id, ended_at)
    WHERE ended_at IS NOT NULL;
CREATE INDEX metrics_owned_attempts_started ON runs (workspace_id, executor_user_id, started_at)
    WHERE managed_job_id IS NOT NULL;
CREATE INDEX metrics_job_attempts ON runs (managed_job_id, started_at)
    WHERE managed_job_id IS NOT NULL;
CREATE INDEX metrics_owner_publications ON checkout_publications (workspace_id, executor_user_id, finalized_at)
    WHERE finalized_at IS NOT NULL;
CREATE INDEX events_scope_type_sequence ON events (workspace_id, scope_id, event_type, sequence);
CREATE INDEX metrics_retained_retries ON events (workspace_id, created_at)
    WHERE event_type = 'execution.requeued';
CREATE INDEX events_attempt_sequence ON events (workspace_id, scope_id, (payload->>'run_id'), sequence)
    WHERE payload->>'run_id' IS NOT NULL;

CREATE POLICY publication_metrics_owner ON checkout_publications FOR SELECT USING (
    current_setting('office.metrics_read', true) = 'on'
    AND workspace_id = office_workspace_id() AND executor_user_id = office_user_id()
    AND office_agent_id() IS NULL AND office_checkout_runner_id() IS NULL
    AND EXISTS (
        SELECT 1 FROM runs r WHERE (r.workspace_id, r.task_id, r.id) =
            (checkout_publications.workspace_id, checkout_publications.task_id, checkout_publications.run_id)
    )
);
