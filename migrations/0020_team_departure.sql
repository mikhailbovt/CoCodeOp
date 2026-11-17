ALTER TABLE teams ADD COLUMN departed_lead_agent_id uuid;
ALTER TABLE teams ADD CONSTRAINT departed_lead_belongs_to_team_owner
    FOREIGN KEY (workspace_id, departed_lead_agent_id, owner_user_id) REFERENCES agents (workspace_id, id, owner_user_id);

ALTER TABLE tasks ADD COLUMN team_id uuid;
ALTER TABLE tasks ADD COLUMN awaiting_owner boolean NOT NULL DEFAULT false;
ALTER TABLE tasks ADD CONSTRAINT task_team_in_workspace
    FOREIGN KEY (workspace_id, team_id) REFERENCES teams (workspace_id, id);

ALTER TABLE tasks NO FORCE ROW LEVEL SECURITY;
ALTER TABLE data_scopes NO FORCE ROW LEVEL SECURITY;
UPDATE tasks SET team_id = data_scopes.team_id
FROM data_scopes
WHERE data_scopes.workspace_id = tasks.workspace_id AND data_scopes.id = tasks.id
  AND data_scopes.visibility = 'team';
ALTER TABLE data_scopes FORCE ROW LEVEL SECURITY;
ALTER TABLE tasks FORCE ROW LEVEL SECURITY;

CREATE INDEX task_team_queue ON tasks (workspace_id, team_id, status) WHERE team_id IS NOT NULL;

CREATE FUNCTION office_active_team_member(workspace uuid, team uuid) RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT EXISTS (SELECT 1 FROM team_memberships membership
        WHERE membership.workspace_id = workspace AND membership.team_id = team
          AND membership.user_id = office_user_id() AND membership.agent_id IS NOT DISTINCT FROM office_agent_id()
          AND membership.revoked_at IS NULL)
$$;

ALTER POLICY data_scope_read ON data_scopes USING (
    workspace_id = office_workspace_id() AND (
        (visibility <> 'team' AND office_owns_data(owner_user_id, author_id))
        OR visibility = 'workspace'
        OR (visibility = 'team' AND office_active_team_member(workspace_id, team_id))
        OR (visibility = 'explicit_acl' AND EXISTS (
            SELECT 1 FROM data_acl grant_row
            WHERE grant_row.workspace_id = data_scopes.workspace_id AND grant_row.scope_id = data_scopes.id
              AND grant_row.grantee_user_id = office_user_id()
              AND grant_row.grantee_agent_id IS NOT DISTINCT FROM office_agent_id()
        ))
    )
);

CREATE POLICY team_self_departure ON teams FOR UPDATE USING (
    workspace_id = office_workspace_id()
    AND id = nullif(current_setting('office.team_leaving_id', true), '')::uuid
    AND office_active_team_member(workspace_id, id)
) WITH CHECK (
    workspace_id = office_workspace_id()
    AND id = nullif(current_setting('office.team_leaving_id', true), '')::uuid
    AND office_active_team_member(workspace_id, id)
);

-- Bound only by the custody transaction after verifying authority over the exact team.
CREATE FUNCTION office_team_custody_id() RETURNS uuid LANGUAGE sql STABLE AS $$
    SELECT nullif(current_setting('office.team_custody_id', true), '')::uuid
$$;

CREATE POLICY task_custody_read ON tasks FOR SELECT USING (
    workspace_id = office_workspace_id() AND team_id = office_team_custody_id()
);
CREATE POLICY task_custody_update ON tasks FOR UPDATE USING (
    workspace_id = office_workspace_id() AND team_id = office_team_custody_id()
) WITH CHECK (
    workspace_id = office_workspace_id() AND team_id = office_team_custody_id()
);
