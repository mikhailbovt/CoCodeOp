CREATE TABLE checkout_notification_areas (
    workspace_id uuid NOT NULL,
    sequence bigint NOT NULL,
    checkout_id uuid NOT NULL,
    task_id uuid NOT NULL,
    area_key text NOT NULL CHECK (octet_length(area_key) BETWEEN 1 AND 80),
    PRIMARY KEY (workspace_id,sequence,area_key),
    FOREIGN KEY (workspace_id,sequence) REFERENCES outbox (workspace_id,sequence) ON DELETE CASCADE,
    FOREIGN KEY (workspace_id,checkout_id) REFERENCES project_checkouts (workspace_id,id),
    FOREIGN KEY (workspace_id,task_id) REFERENCES tasks (workspace_id,id)
);
CREATE INDEX checkout_notification_groups
    ON checkout_notification_areas (workspace_id,checkout_id,task_id,area_key,sequence);

ALTER TABLE checkout_notification_areas ENABLE ROW LEVEL SECURITY;
ALTER TABLE checkout_notification_areas FORCE ROW LEVEL SECURITY;
CREATE POLICY checkout_notification_read ON checkout_notification_areas FOR SELECT USING (
    workspace_id=office_workspace_id() AND EXISTS (
        SELECT 1 FROM outbox o WHERE (o.workspace_id,o.sequence)=
            (checkout_notification_areas.workspace_id,checkout_notification_areas.sequence)
    )
);
CREATE POLICY checkout_notification_add ON checkout_notification_areas FOR INSERT WITH CHECK (
    workspace_id=office_workspace_id() AND office_hosts_checkout(workspace_id,checkout_id)
    AND EXISTS (
        SELECT 1 FROM checkout_publications p
        WHERE (p.workspace_id,p.checkout_id,p.task_id,p.event_sequence)=
            (checkout_notification_areas.workspace_id,checkout_notification_areas.checkout_id,
             checkout_notification_areas.task_id,checkout_notification_areas.sequence)
          AND p.host_runner_id=office_checkout_runner_id() AND p.state='applied'
    )
);
