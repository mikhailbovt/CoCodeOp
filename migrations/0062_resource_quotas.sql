CREATE TABLE message_rate_windows (
    workspace_id uuid NOT NULL REFERENCES workspaces(id),
    kind text NOT NULL CHECK (kind IN ('workspace','owner','agent')),
    subject_id uuid NOT NULL,
    minute bigint NOT NULL,
    messages smallint NOT NULL CHECK (messages BETWEEN 1 AND 1200),
    PRIMARY KEY (workspace_id,kind,subject_id)
);
ALTER TABLE message_rate_windows ENABLE ROW LEVEL SECURITY;
ALTER TABLE message_rate_windows FORCE ROW LEVEL SECURITY;
CREATE POLICY message_rate_window_scope ON message_rate_windows FOR ALL USING (
    workspace_id=office_workspace_id() AND (
      (kind='workspace' AND subject_id=office_workspace_id()) OR
      (kind='owner' AND subject_id=office_user_id()) OR (kind='agent' AND subject_id=office_agent_id()))
) WITH CHECK (
    workspace_id=office_workspace_id() AND (
      (kind='workspace' AND subject_id=office_workspace_id()) OR
      (kind='owner' AND subject_id=office_user_id()) OR (kind='agent' AND subject_id=office_agent_id()))
);

CREATE POLICY managed_worker_quota_read ON managed_workers FOR SELECT USING (
    workspace_id=office_workspace_id()
    AND workspace_id=nullif(current_setting('office.managed_worker_quota_workspace',true),'')::uuid
);
CREATE POLICY managed_worker_quota_agent_read ON agents FOR SELECT USING (
    workspace_id=office_workspace_id()
    AND workspace_id=nullif(current_setting('office.managed_worker_quota_workspace',true),'')::uuid
);
CREATE POLICY artifact_storage_quota_read ON artifact_storage_usage FOR SELECT USING (
    workspace_id=office_workspace_id()
    AND workspace_id=nullif(current_setting('office.artifact_quota_workspace',true),'')::uuid
);

CREATE POLICY checkout_command_host_run_state ON runs FOR SELECT USING (
    workspace_id=office_workspace_id() AND office_checkout_runner_id() IS NOT NULL AND EXISTS (
        SELECT 1 FROM checkout_commands c WHERE (c.workspace_id,c.run_id)=(runs.workspace_id,runs.id)
          AND c.host_runner_id=office_checkout_runner_id()
          AND c.id=nullif(current_setting('office.active_checkout_command_id',true),'')::uuid
          AND c.state IN ('authorized','running')
    )
);
CREATE POLICY checkout_command_host_stop_state ON execution_controls FOR SELECT USING (
    workspace_id=office_workspace_id() AND kind='owner' AND office_checkout_runner_id() IS NOT NULL AND EXISTS (
        SELECT 1 FROM checkout_commands c
          WHERE (c.workspace_id,c.executor_user_id)=(execution_controls.workspace_id,execution_controls.subject_id)
          AND c.host_runner_id=office_checkout_runner_id()
          AND c.id=nullif(current_setting('office.active_checkout_command_id',true),'')::uuid
          AND c.state IN ('authorized','running')
    )
);
