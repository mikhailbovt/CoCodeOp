ALTER TABLE office_installation
    ADD COLUMN restore_pending boolean NOT NULL DEFAULT false,
    ADD COLUMN restored_backup_id uuid,
    ADD CHECK (NOT restore_pending OR restored_backup_id IS NOT NULL);

ALTER TABLE execution_jobs DROP CONSTRAINT execution_jobs_terminal_reason_check;
ALTER TABLE execution_jobs ADD CHECK (terminal_reason IN (
    'attempt_limit', 'eligibility_changed', 'start_interrupted', 'execution_failed',
    'input_expired', 'emergency_stopped', 'restore_review_required'
));
