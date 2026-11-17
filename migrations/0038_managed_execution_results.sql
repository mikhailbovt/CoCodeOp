ALTER TABLE execution_jobs DROP CONSTRAINT execution_jobs_state_check;
ALTER TABLE execution_jobs DROP CONSTRAINT execution_job_terminal_time;
ALTER TABLE execution_jobs ADD CHECK (state IN ('queued','leased','reconcile_required','cancelled','dead_letter','completed'));
ALTER TABLE execution_jobs ADD CONSTRAINT execution_job_terminal_time
    CHECK ((state IN ('cancelled','dead_letter','completed')) = (ended_at IS NOT NULL));
ALTER TABLE execution_jobs ADD CHECK (state <> 'completed' OR current_run_id IS NOT NULL);
ALTER TABLE execution_jobs DROP CONSTRAINT execution_jobs_terminal_reason_check;
ALTER TABLE execution_jobs ADD CHECK (terminal_reason IN ('attempt_limit','eligibility_changed','start_interrupted','execution_failed'));

ALTER TABLE execution_start_effects ADD COLUMN completion_key uuid;
ALTER TABLE execution_start_effects ADD COLUMN completion_sha256 text CHECK (completion_sha256 ~ '^[a-f0-9]{64}$');
ALTER TABLE execution_start_effects ADD COLUMN process_exit_code smallint CHECK (process_exit_code BETWEEN 0 AND 255);
ALTER TABLE execution_start_effects ADD COLUMN result_task_revision bigint CHECK (result_task_revision > 0);
ALTER TABLE execution_start_effects ADD CHECK (
    num_nonnulls(completion_key,completion_sha256,process_exit_code,result_task_revision) IN (0,4));
ALTER TABLE execution_start_effects ADD CHECK (completion_key IS NULL OR (state='stopped' AND confirmed_at IS NOT NULL));

CREATE POLICY managed_task_result_submit ON task_result_revisions FOR INSERT WITH CHECK (
    workspace_id=office_workspace_id() AND author_user_id=office_user_id() AND office_runner_planning()
    AND run_id=nullif(current_setting('office.managed_result_run',true),'')::uuid
    AND EXISTS (
        SELECT 1 FROM runs r WHERE r.workspace_id=task_result_revisions.workspace_id
          AND r.task_id=task_result_revisions.task_id AND r.id=task_result_revisions.run_id
          AND r.attempt_number=task_result_revisions.revision AND r.status='succeeded'
          AND r.executor_user_id=task_result_revisions.author_user_id
          AND r.executor_agent_id=task_result_revisions.author_agent_id
          AND r.managed_job_id=office_execution_job() AND r.managed_runner_id=office_checkout_runner_id()
    )
);
