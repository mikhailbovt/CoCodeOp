CREATE TABLE projects (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    owner_user_id uuid NOT NULL,
    name text NOT NULL CHECK (octet_length(name) BETWEEN 1 AND 128),
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (workspace_id, id, owner_user_id),
    FOREIGN KEY (workspace_id, id) REFERENCES data_scopes (workspace_id, id),
    FOREIGN KEY (workspace_id, owner_user_id) REFERENCES workspace_memberships (workspace_id, user_id)
);

ALTER TABLE runners ADD CONSTRAINT runner_workspace_owner UNIQUE (workspace_id, id, owner_user_id);

CREATE TABLE project_checkouts (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    project_id uuid NOT NULL,
    owner_user_id uuid NOT NULL,
    host_runner_id uuid NOT NULL,
    state text NOT NULL DEFAULT 'pending_registration' CHECK (state IN ('pending_registration', 'registered')),
    mode text NOT NULL DEFAULT 'shared' CHECK (mode = 'shared'),
    write_strategy text NOT NULL DEFAULT 'mediated' CHECK (write_strategy = 'mediated'),
    revision bigint NOT NULL DEFAULT 1 CHECK (revision > 0),
    manifest jsonb NOT NULL CHECK (jsonb_typeof(manifest) = 'object'),
    root_ref uuid,
    baseline jsonb CHECK (baseline IS NULL OR jsonb_typeof(baseline) = 'object'),
    UNIQUE (workspace_id, project_id),
    UNIQUE (workspace_id, id),
    CHECK ((state = 'registered') = (root_ref IS NOT NULL)),
    CHECK ((root_ref IS NULL) = (baseline IS NULL)),
    FOREIGN KEY (workspace_id, project_id, owner_user_id) REFERENCES projects (workspace_id, id, owner_user_id),
    FOREIGN KEY (workspace_id, host_runner_id, owner_user_id) REFERENCES runners (workspace_id, id, owner_user_id)
);

CREATE TABLE checkout_roots (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    checkout_id uuid NOT NULL,
    host_runner_id uuid NOT NULL,
    host_fingerprint text NOT NULL CHECK (host_fingerprint ~ '^[a-f0-9]{64}$'),
    directory_id text NOT NULL CHECK (directory_id ~ '^[a-f0-9]{64}$'),
    ancestor_ids text[] NOT NULL CHECK (cardinality(ancestor_ids) BETWEEN 1 AND 64),
    CHECK (array_position(ancestor_ids, NULL) IS NULL AND NOT directory_id = ANY(ancestor_ids)),
    UNIQUE (host_fingerprint, directory_id),
    UNIQUE (workspace_id, checkout_id, id),
    FOREIGN KEY (workspace_id, checkout_id) REFERENCES project_checkouts (workspace_id, id)
);

ALTER TABLE project_checkouts ADD CONSTRAINT checkout_root_matches
    FOREIGN KEY (workspace_id, id, root_ref) REFERENCES checkout_roots (workspace_id, checkout_id, id);

CREATE FUNCTION office_checkout_runner_id() RETURNS uuid LANGUAGE sql STABLE AS $$
    SELECT nullif(current_setting('office.checkout_runner_id', true), '')::uuid
$$;

ALTER TABLE projects ENABLE ROW LEVEL SECURITY;
ALTER TABLE projects FORCE ROW LEVEL SECURITY;
CREATE POLICY project_read ON projects FOR SELECT USING (
    workspace_id = office_workspace_id()
    AND EXISTS (SELECT 1 FROM data_scopes WHERE workspace_id = projects.workspace_id AND id = projects.id)
);
CREATE POLICY project_create ON projects FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND owner_user_id = office_user_id() AND office_agent_id() IS NULL
);

ALTER TABLE project_checkouts ENABLE ROW LEVEL SECURITY;
ALTER TABLE project_checkouts FORCE ROW LEVEL SECURITY;
CREATE POLICY checkout_read ON project_checkouts FOR SELECT USING (
    workspace_id = office_workspace_id() AND (
        host_runner_id = office_checkout_runner_id()
        OR (office_checkout_runner_id() IS NULL AND EXISTS (
            SELECT 1 FROM projects WHERE workspace_id = project_checkouts.workspace_id AND id = project_id
        ))
    )
);
CREATE POLICY checkout_request ON project_checkouts FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND owner_user_id = office_user_id()
    AND office_agent_id() IS NULL AND office_checkout_runner_id() IS NULL AND state = 'pending_registration'
);
CREATE POLICY checkout_enroll ON project_checkouts FOR UPDATE USING (
    workspace_id = office_workspace_id() AND host_runner_id = office_checkout_runner_id()
);

ALTER TABLE checkout_roots ENABLE ROW LEVEL SECURITY;
ALTER TABLE checkout_roots FORCE ROW LEVEL SECURITY;
CREATE POLICY root_overlap_probe ON checkout_roots FOR SELECT USING (
    office_checkout_runner_id() IS NOT NULL
    AND host_fingerprint = nullif(current_setting('office.checkout_host_fingerprint', true), '')
);
CREATE POLICY root_reserve ON checkout_roots FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND host_runner_id = office_checkout_runner_id()
    AND host_fingerprint = nullif(current_setting('office.checkout_host_fingerprint', true), '')
    AND EXISTS (SELECT 1 FROM project_checkouts WHERE workspace_id = checkout_roots.workspace_id
        AND id = checkout_id AND host_runner_id = checkout_roots.host_runner_id)
);
