CREATE SEQUENCE checkout_coordinator_epoch AS bigint;

CREATE TABLE checkout_coordinator_sessions (
    workspace_id uuid NOT NULL,
    checkout_id uuid NOT NULL,
    owner_user_id uuid NOT NULL,
    host_runner_id uuid NOT NULL,
    instance_key uuid NOT NULL,
    epoch bigint NOT NULL DEFAULT nextval('checkout_coordinator_epoch') CHECK (epoch > 0),
    started_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    lease_until timestamptz NOT NULL,
    ready_at timestamptz,
    stopped_at timestamptz,
    PRIMARY KEY (workspace_id,checkout_id,epoch),
    UNIQUE (workspace_id,checkout_id,instance_key),
    FOREIGN KEY (workspace_id,checkout_id,owner_user_id) REFERENCES project_checkouts (workspace_id,id,owner_user_id),
    FOREIGN KEY (workspace_id,host_runner_id,owner_user_id) REFERENCES runners (workspace_id,id,owner_user_id)
);

CREATE UNIQUE INDEX one_current_checkout_coordinator ON checkout_coordinator_sessions (workspace_id,checkout_id)
    WHERE stopped_at IS NULL;
ALTER TABLE checkout_coordinator_sessions ENABLE ROW LEVEL SECURITY;
ALTER TABLE checkout_coordinator_sessions FORCE ROW LEVEL SECURITY;
CREATE POLICY checkout_coordinator_host ON checkout_coordinator_sessions FOR ALL USING (
    workspace_id=office_workspace_id() AND owner_user_id=office_user_id()
    AND host_runner_id=office_checkout_runner_id() AND office_hosts_checkout(workspace_id,checkout_id)
) WITH CHECK (
    workspace_id=office_workspace_id() AND owner_user_id=office_user_id()
    AND host_runner_id=office_checkout_runner_id() AND office_hosts_checkout(workspace_id,checkout_id)
);
