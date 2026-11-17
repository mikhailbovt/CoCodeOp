CREATE TABLE checkout_release_policies (
    workspace_id uuid NOT NULL,
    checkout_id uuid NOT NULL,
    revision bigint NOT NULL CHECK (revision>0),
    required_template_ids uuid[] NOT NULL CHECK (cardinality(required_template_ids) BETWEEN 1 AND 8),
    PRIMARY KEY (workspace_id,checkout_id),
    FOREIGN KEY (workspace_id,checkout_id) REFERENCES project_checkouts (workspace_id,id)
);

CREATE TABLE checkout_releases (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    checkout_id uuid NOT NULL,
    snapshot_id uuid NOT NULL,
    host_runner_id uuid NOT NULL REFERENCES runners(id),
    checkout_revision bigint NOT NULL CHECK (checkout_revision>=0),
    source_sha256 text NOT NULL CHECK (source_sha256 ~ '^[0-9a-f]{64}$'),
    tree_sha256 text NOT NULL CHECK (tree_sha256 ~ '^[0-9a-f]{64}$'),
    expected_commit_sha256 text CHECK (expected_commit_sha256 ~ '^[0-9a-f]{64}$'),
    policy_revision bigint NOT NULL CHECK (policy_revision>0),
    authorization_sha256 text NOT NULL CHECK (authorization_sha256 ~ '^[0-9a-f]{64}$'),
    approval_binding jsonb NOT NULL CHECK (jsonb_typeof(approval_binding)='object'),
    change_count integer NOT NULL CHECK (change_count>=0),
    command_ids uuid[] NOT NULL CHECK (cardinality(command_ids) BETWEEN 1 AND 8),
    state text NOT NULL DEFAULT 'queued' CHECK (state IN ('queued','applying','accepted','source_changed','ref_conflict','storage_unavailable')),
    outcome jsonb,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    expires_at timestamptz NOT NULL DEFAULT clock_timestamp()+interval '90 seconds',
    finalized_at timestamptz,
    CHECK ((state IN ('queued','applying'))=(outcome IS NULL)),
    CHECK ((outcome IS NULL)=(finalized_at IS NULL)),
    FOREIGN KEY(workspace_id,checkout_id,snapshot_id) REFERENCES checkout_snapshots(workspace_id,checkout_id,id)
);
CREATE INDEX checkout_active_releases ON checkout_releases(workspace_id,checkout_id,expires_at) WHERE state IN ('queued','applying');
CREATE UNIQUE INDEX checkout_accepted_versions ON checkout_releases(workspace_id,checkout_id,checkout_revision) WHERE state='accepted';

ALTER TABLE checkout_release_policies ENABLE ROW LEVEL SECURITY;
ALTER TABLE checkout_release_policies FORCE ROW LEVEL SECURITY;
CREATE POLICY checkout_release_policy_owner ON checkout_release_policies FOR ALL
USING (office_checkout_runner_id() IS NULL AND office_may_capture_checkout(workspace_id,checkout_id))
WITH CHECK (office_checkout_runner_id() IS NULL AND office_may_capture_checkout(workspace_id,checkout_id));

ALTER TABLE checkout_releases ENABLE ROW LEVEL SECURITY;
ALTER TABLE checkout_releases FORCE ROW LEVEL SECURITY;
CREATE POLICY checkout_release_owner ON checkout_releases FOR SELECT USING (
    office_checkout_runner_id() IS NULL AND office_may_capture_checkout(workspace_id,checkout_id)
);
CREATE POLICY checkout_release_request ON checkout_releases FOR INSERT WITH CHECK (
    office_checkout_runner_id() IS NULL AND office_may_capture_checkout(workspace_id,checkout_id) AND state='queued'
    AND EXISTS (SELECT 1 FROM checkout_release_policies p WHERE (p.workspace_id,p.checkout_id,p.revision)=
          (checkout_releases.workspace_id,checkout_releases.checkout_id,checkout_releases.policy_revision))
    AND EXISTS (SELECT 1 FROM checkout_snapshots s WHERE (s.workspace_id,s.checkout_id,s.id,s.host_runner_id,s.state)=
          (checkout_releases.workspace_id,checkout_releases.checkout_id,checkout_releases.snapshot_id,
           checkout_releases.host_runner_id,'ready'))
);
CREATE POLICY checkout_release_host ON checkout_releases FOR SELECT USING (
    workspace_id=office_workspace_id() AND host_runner_id=office_checkout_runner_id() AND office_hosts_checkout(workspace_id,checkout_id)
);
CREATE POLICY checkout_release_host_effect ON checkout_releases FOR UPDATE
USING (workspace_id=office_workspace_id() AND host_runner_id=office_checkout_runner_id()
    AND office_hosts_checkout(workspace_id,checkout_id) AND state IN ('queued','applying'))
WITH CHECK (workspace_id=office_workspace_id() AND host_runner_id=office_checkout_runner_id()
    AND office_hosts_checkout(workspace_id,checkout_id) AND state<>'queued');
CREATE POLICY checkout_release_gateway_barrier ON checkout_releases FOR SELECT USING (
    workspace_id=office_workspace_id() AND checkout_id=office_gateway_checkout() AND office_gateway_run() IS NOT NULL
    AND state IN ('queued','applying')
);

CREATE FUNCTION office_is_releasing_checkout(workspace uuid, checkout uuid) RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT office_checkout_runner_id() IS NULL AND office_may_capture_checkout(workspace,checkout)
        AND checkout=nullif(current_setting('office.releasing_checkout_id',true),'')::uuid
$$;
CREATE POLICY checkout_release_command_evidence ON checkout_commands FOR SELECT USING (
    state='finished' AND source_snapshot_id IS NOT NULL AND office_is_releasing_checkout(workspace_id,checkout_id)
);
CREATE POLICY checkout_release_publication_evidence ON checkout_publications FOR SELECT USING (
    state='applied' AND office_is_releasing_checkout(workspace_id,checkout_id)
);
CREATE INDEX checkout_publications_by_revision ON checkout_publications (workspace_id,checkout_id,((outcome->'head'->>'checkout_revision')::bigint))
    WHERE state='applied';
