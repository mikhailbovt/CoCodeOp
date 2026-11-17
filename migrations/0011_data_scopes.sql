CREATE FUNCTION office_workspace_id() RETURNS uuid LANGUAGE sql STABLE AS $$
    SELECT nullif(current_setting('office.workspace_id', true), '')::uuid
$$;

CREATE FUNCTION office_user_id() RETURNS uuid LANGUAGE sql STABLE AS $$
    SELECT nullif(current_setting('office.user_id', true), '')::uuid
$$;

CREATE FUNCTION office_agent_id() RETURNS uuid LANGUAGE sql STABLE AS $$
    SELECT nullif(current_setting('office.agent_id', true), '')::uuid
$$;

CREATE FUNCTION office_owns_data(owner_user_id uuid, author_id uuid) RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT owner_user_id = office_user_id() AND (office_agent_id() IS NULL OR author_id = office_agent_id())
$$;

CREATE TABLE teams (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    owner_user_id uuid NOT NULL,
    lead_agent_id uuid,
    name text NOT NULL CHECK (octet_length(name) BETWEEN 1 AND 128),
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    UNIQUE (workspace_id, id),
    FOREIGN KEY (workspace_id, owner_user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    FOREIGN KEY (workspace_id, lead_agent_id, owner_user_id) REFERENCES agents (workspace_id, id, owner_user_id)
);

CREATE TABLE team_memberships (
    workspace_id uuid NOT NULL,
    team_id uuid NOT NULL,
    user_id uuid NOT NULL,
    agent_id uuid,
    role text NOT NULL CHECK (role IN ('lead', 'member', 'worker', 'reviewer')),
    revoked_at timestamptz,
    UNIQUE NULLS NOT DISTINCT (workspace_id, team_id, user_id, agent_id),
    FOREIGN KEY (workspace_id, team_id) REFERENCES teams (workspace_id, id),
    FOREIGN KEY (workspace_id, user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    FOREIGN KEY (workspace_id, agent_id, user_id) REFERENCES agents (workspace_id, id, owner_user_id)
);

ALTER TABLE teams ENABLE ROW LEVEL SECURITY;
ALTER TABLE teams FORCE ROW LEVEL SECURITY;
ALTER TABLE team_memberships ENABLE ROW LEVEL SECURITY;
ALTER TABLE team_memberships FORCE ROW LEVEL SECURITY;

CREATE POLICY own_team_memberships ON team_memberships FOR SELECT USING (
    workspace_id = office_workspace_id()
    AND user_id = office_user_id() AND agent_id IS NOT DISTINCT FROM office_agent_id()
    AND revoked_at IS NULL
);

CREATE POLICY visible_teams ON teams FOR SELECT USING (
    workspace_id = office_workspace_id() AND (
        office_owns_data(owner_user_id, coalesce(lead_agent_id, owner_user_id)) OR EXISTS (
            SELECT 1 FROM team_memberships membership
            WHERE membership.workspace_id = teams.workspace_id AND membership.team_id = teams.id
        )
    )
);

CREATE TABLE data_scopes (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    owner_user_id uuid NOT NULL,
    author_agent_id uuid,
    author_id uuid GENERATED ALWAYS AS (coalesce(author_agent_id, owner_user_id)) STORED,
    visibility text NOT NULL,
    team_id uuid,
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, id, owner_user_id, author_id),
    CHECK (visibility IN ('owner_private', 'team', 'workspace', 'explicit_acl')),
    CHECK ((team_id IS NOT NULL) = (visibility = 'team')),
    FOREIGN KEY (workspace_id, owner_user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    FOREIGN KEY (workspace_id, author_agent_id, owner_user_id) REFERENCES agents (workspace_id, id, owner_user_id),
    FOREIGN KEY (workspace_id, team_id) REFERENCES teams (workspace_id, id)
);

CREATE TABLE data_acl (
    workspace_id uuid NOT NULL,
    scope_id uuid NOT NULL,
    scope_owner_user_id uuid NOT NULL,
    scope_author_id uuid NOT NULL,
    grantee_user_id uuid NOT NULL,
    grantee_agent_id uuid,
    access_right text NOT NULL CHECK (access_right IN ('read', 'write')),
    UNIQUE NULLS NOT DISTINCT (workspace_id, scope_id, grantee_user_id, grantee_agent_id),
    FOREIGN KEY (workspace_id, scope_id, scope_owner_user_id, scope_author_id)
        REFERENCES data_scopes (workspace_id, id, owner_user_id, author_id) ON DELETE CASCADE,
    FOREIGN KEY (workspace_id, grantee_user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    FOREIGN KEY (workspace_id, grantee_agent_id, grantee_user_id) REFERENCES agents (workspace_id, id, owner_user_id)
);

ALTER TABLE data_scopes ENABLE ROW LEVEL SECURITY;
ALTER TABLE data_scopes FORCE ROW LEVEL SECURITY;
ALTER TABLE data_acl ENABLE ROW LEVEL SECURITY;
ALTER TABLE data_acl FORCE ROW LEVEL SECURITY;

CREATE POLICY data_acl_visible ON data_acl FOR SELECT USING (
    workspace_id = office_workspace_id() AND (
        office_owns_data(scope_owner_user_id, scope_author_id) OR (
            grantee_user_id = office_user_id() AND grantee_agent_id IS NOT DISTINCT FROM office_agent_id()
        )
    )
);

CREATE POLICY data_acl_grant_owner ON data_acl FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND office_owns_data(scope_owner_user_id, scope_author_id)
);

CREATE POLICY data_acl_change_owner ON data_acl FOR UPDATE
    USING (workspace_id = office_workspace_id() AND office_owns_data(scope_owner_user_id, scope_author_id))
    WITH CHECK (workspace_id = office_workspace_id() AND office_owns_data(scope_owner_user_id, scope_author_id));

CREATE POLICY data_acl_revoke_owner ON data_acl FOR DELETE USING (
    workspace_id = office_workspace_id() AND office_owns_data(scope_owner_user_id, scope_author_id)
);

CREATE POLICY data_scope_create ON data_scopes FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id()
    AND owner_user_id = office_user_id() AND author_agent_id IS NOT DISTINCT FROM office_agent_id()
);

CREATE POLICY data_scope_read ON data_scopes FOR SELECT USING (
    workspace_id = office_workspace_id() AND (
        office_owns_data(owner_user_id, author_id)
        OR visibility = 'workspace'
        OR (visibility = 'team' AND EXISTS (
            SELECT 1 FROM team_memberships membership
            WHERE membership.workspace_id = data_scopes.workspace_id AND membership.team_id = data_scopes.team_id
        ))
        OR (visibility = 'explicit_acl' AND EXISTS (
            SELECT 1 FROM data_acl grant_row
            WHERE grant_row.workspace_id = data_scopes.workspace_id AND grant_row.scope_id = data_scopes.id
              AND grant_row.grantee_user_id = office_user_id()
              AND grant_row.grantee_agent_id IS NOT DISTINCT FROM office_agent_id()
        ))
    )
);
