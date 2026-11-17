ALTER TABLE runs ADD COLUMN backend_evidence jsonb;
ALTER TABLE runs ADD CONSTRAINT backend_evidence_for_managed_run
    CHECK (backend_evidence IS NULL OR (managed_job_id IS NOT NULL AND jsonb_typeof(backend_evidence) = 'object'));

CREATE UNIQUE INDEX run_backend_session_per_owner
    ON runs (workspace_id, executor_user_id, (backend_evidence ->> 'backend'), (backend_evidence ->> 'session_id'))
    WHERE backend_evidence ->> 'session_id' IS NOT NULL;
