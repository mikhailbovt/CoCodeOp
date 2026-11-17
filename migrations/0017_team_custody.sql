ALTER TABLE teams
    ADD COLUMN revision bigint NOT NULL DEFAULT 1 CHECK (revision > 0),
    ADD COLUMN state text NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'awaiting_owner'));

CREATE FUNCTION office_team_view_id() RETURNS uuid LANGUAGE sql STABLE AS $$
    SELECT nullif(current_setting('office.team_view_id', true), '')::uuid
$$;

CREATE FUNCTION office_team_manage_id() RETURNS uuid LANGUAGE sql STABLE AS $$
    SELECT nullif(current_setting('office.team_manage_id', true), '')::uuid
$$;

-- Roster permissions are bound after checking the actual owner/lead or member in the store.
-- Always test membership explicitly in data visibility policies, including inside a roster transaction.
ALTER POLICY visible_teams ON teams USING (
    workspace_id = office_workspace_id() AND (
        office_owns_data(owner_user_id, coalesce(lead_agent_id, owner_user_id))
        OR EXISTS (
            SELECT 1 FROM team_memberships membership
            WHERE membership.workspace_id = teams.workspace_id AND membership.team_id = teams.id
              AND membership.user_id = office_user_id() AND membership.agent_id IS NOT DISTINCT FROM office_agent_id()
              AND membership.revoked_at IS NULL
        )
    )
);

ALTER POLICY data_scope_read ON data_scopes USING (
    workspace_id = office_workspace_id() AND (
        office_owns_data(owner_user_id, author_id)
        OR visibility = 'workspace'
        OR (visibility = 'team' AND EXISTS (
            SELECT 1 FROM team_memberships membership
            WHERE membership.workspace_id = data_scopes.workspace_id AND membership.team_id = data_scopes.team_id
              AND membership.user_id = office_user_id() AND membership.agent_id IS NOT DISTINCT FROM office_agent_id()
              AND membership.revoked_at IS NULL
        ))
        OR (visibility = 'explicit_acl' AND EXISTS (
            SELECT 1 FROM data_acl grant_row
            WHERE grant_row.workspace_id = data_scopes.workspace_id AND grant_row.scope_id = data_scopes.id
              AND grant_row.grantee_user_id = office_user_id()
              AND grant_row.grantee_agent_id IS NOT DISTINCT FROM office_agent_id()
        ))
    )
);

CREATE POLICY team_create ON teams FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND owner_user_id = office_user_id()
    AND lead_agent_id IS NOT DISTINCT FROM office_agent_id()
);

CREATE POLICY team_manage ON teams FOR UPDATE USING (
    workspace_id = office_workspace_id() AND office_owns_data(owner_user_id, coalesce(lead_agent_id, owner_user_id))
) WITH CHECK (
    workspace_id = office_workspace_id() AND office_owns_data(owner_user_id, coalesce(lead_agent_id, owner_user_id))
);

CREATE POLICY team_roster_read ON team_memberships FOR SELECT USING (
    workspace_id = office_workspace_id() AND team_id = office_team_view_id()
);

CREATE POLICY team_roster_insert ON team_memberships FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND team_id = office_team_manage_id()
);

CREATE POLICY team_roster_update ON team_memberships FOR UPDATE USING (
    workspace_id = office_workspace_id() AND team_id = office_team_manage_id()
) WITH CHECK (
    workspace_id = office_workspace_id() AND team_id = office_team_manage_id()
);

CREATE INDEX own_team_directory ON team_memberships (workspace_id, user_id, agent_id, team_id)
    WHERE revoked_at IS NULL;
