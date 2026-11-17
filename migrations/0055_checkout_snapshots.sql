CREATE FUNCTION office_may_capture_checkout(workspace uuid, checkout uuid) RETURNS boolean LANGUAGE sql STABLE
SET search_path = pg_catalog, public AS $$
    SELECT workspace=office_workspace_id() AND EXISTS (
        SELECT 1 FROM project_checkouts c WHERE (c.workspace_id,c.id,c.owner_user_id)=(workspace,checkout,office_user_id())
          AND (office_agent_id() IS NULL OR EXISTS (
              SELECT 1 FROM agents a WHERE (a.workspace_id,a.id,a.owner_user_id)=(workspace,office_agent_id(),c.owner_user_id)
                AND a.role='lead' AND a.disabled_at IS NULL
          ))
    )
$$;

CREATE POLICY publication_head_checkout_owner ON checkout_content_heads FOR SELECT USING (
    office_checkout_runner_id() IS NULL AND office_may_capture_checkout(workspace_id,checkout_id)
);

CREATE TABLE checkout_snapshots (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    checkout_id uuid NOT NULL,
    host_runner_id uuid NOT NULL,
    checkout_revision bigint NOT NULL CHECK (checkout_revision>=0),
    source_sha256 text NOT NULL CHECK (source_sha256 ~ '^[0-9a-f]{64}$'),
    state text NOT NULL DEFAULT 'queued' CHECK (state IN ('queued','ready','source_changed','storage_unavailable')),
    outcome jsonb,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    expires_at timestamptz NOT NULL DEFAULT clock_timestamp()+interval '90 seconds',
    completed_at timestamptz,
    UNIQUE(workspace_id,checkout_id,id),
    CHECK ((state='queued')=(outcome IS NULL)),
    CHECK ((state='queued')=(completed_at IS NULL)),
    FOREIGN KEY(workspace_id,checkout_id) REFERENCES project_checkouts(workspace_id,id),
    FOREIGN KEY(host_runner_id) REFERENCES runners(id)
);
CREATE INDEX checkout_snapshots_pending ON checkout_snapshots(workspace_id,checkout_id,expires_at) WHERE state='queued';
CREATE UNIQUE INDEX checkout_snapshots_ready_revision ON checkout_snapshots(workspace_id,checkout_id,checkout_revision) WHERE state='ready';

ALTER TABLE checkout_snapshots ENABLE ROW LEVEL SECURITY;
ALTER TABLE checkout_snapshots FORCE ROW LEVEL SECURITY;
CREATE POLICY checkout_snapshot_owner ON checkout_snapshots FOR SELECT USING (
    office_checkout_runner_id() IS NULL AND office_may_capture_checkout(workspace_id,checkout_id)
);
CREATE POLICY checkout_snapshot_create ON checkout_snapshots FOR INSERT WITH CHECK (
    office_checkout_runner_id() IS NULL AND state='queued' AND office_may_capture_checkout(workspace_id,checkout_id)
    AND EXISTS (SELECT 1 FROM project_checkouts c WHERE (c.workspace_id,c.id,c.host_runner_id)=
        (checkout_snapshots.workspace_id,checkout_snapshots.checkout_id,checkout_snapshots.host_runner_id))
);
CREATE POLICY checkout_snapshot_host ON checkout_snapshots FOR SELECT USING (
    workspace_id=office_workspace_id() AND host_runner_id=office_checkout_runner_id() AND office_hosts_checkout(workspace_id,checkout_id)
);
CREATE POLICY checkout_snapshot_report ON checkout_snapshots FOR UPDATE USING (
    workspace_id=office_workspace_id() AND host_runner_id=office_checkout_runner_id() AND office_hosts_checkout(workspace_id,checkout_id)
    AND state='queued' AND expires_at>clock_timestamp()
) WITH CHECK (
    workspace_id=office_workspace_id() AND host_runner_id=office_checkout_runner_id() AND office_hosts_checkout(workspace_id,checkout_id)
    AND state<>'queued'
);
CREATE POLICY checkout_snapshot_gateway ON checkout_snapshots FOR SELECT USING (
    workspace_id=office_workspace_id() AND checkout_id=office_gateway_checkout() AND office_gateway_run() IS NOT NULL
);
