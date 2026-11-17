CREATE TABLE checkout_areas (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    checkout_id uuid NOT NULL,
    owner_user_id uuid NOT NULL,
    revision bigint NOT NULL DEFAULT 1 CHECK (revision > 0),
    name text NOT NULL CHECK (octet_length(name) BETWEEN 1 AND 128),
    paths jsonb NOT NULL CHECK (jsonb_typeof(paths) = 'object'),
    policy jsonb NOT NULL CHECK (jsonb_typeof(policy) = 'object'),
    FOREIGN KEY (workspace_id, id) REFERENCES data_scopes (workspace_id, id),
    FOREIGN KEY (workspace_id, checkout_id, owner_user_id) REFERENCES project_checkouts (workspace_id, id, owner_user_id)
);

CREATE INDEX checkout_areas_by_checkout ON checkout_areas (workspace_id, checkout_id, id);

ALTER TABLE checkout_areas ENABLE ROW LEVEL SECURITY;
ALTER TABLE checkout_areas FORCE ROW LEVEL SECURITY;
CREATE POLICY checkout_area_read ON checkout_areas FOR SELECT USING (
    workspace_id = office_workspace_id() AND (
        (owner_user_id = office_user_id() AND office_agent_id() IS NULL AND office_checkout_runner_id() IS NULL)
        OR checkout_id = nullif(current_setting('office.validating_checkout_areas', true), '')::uuid
    )
);
CREATE POLICY checkout_area_write ON checkout_areas FOR ALL USING (
    workspace_id = office_workspace_id() AND owner_user_id = office_user_id()
    AND office_agent_id() IS NULL AND office_checkout_runner_id() IS NULL
) WITH CHECK (
    workspace_id = office_workspace_id() AND owner_user_id = office_user_id()
    AND office_agent_id() IS NULL AND office_checkout_runner_id() IS NULL
);
