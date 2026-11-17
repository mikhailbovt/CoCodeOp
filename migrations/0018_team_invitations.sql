CREATE TABLE team_invitations (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    team_id uuid NOT NULL,
    issuer_user_id uuid NOT NULL,
    issuer_agent_id uuid,
    recipient_user_id uuid NOT NULL,
    recipient_agent_id uuid,
    requested_role text NOT NULL CHECK (requested_role IN ('member', 'worker', 'reviewer')),
    state text NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'accepted', 'declined')),
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    expires_at timestamptz NOT NULL DEFAULT transaction_timestamp() + interval '24 hours',
    decided_at timestamptz,
    decided_by_user_id uuid,
    decided_by_agent_id uuid,
    CHECK (expires_at > created_at),
    CHECK ((state = 'pending') = (decided_at IS NULL)),
    CHECK ((decided_at IS NULL) = (decided_by_user_id IS NULL)),
    CHECK (decided_by_agent_id IS NULL OR decided_by_user_id IS NOT NULL),
    FOREIGN KEY (workspace_id, id) REFERENCES data_scopes (workspace_id, id),
    FOREIGN KEY (workspace_id, team_id) REFERENCES teams (workspace_id, id),
    FOREIGN KEY (workspace_id, issuer_user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    FOREIGN KEY (workspace_id, issuer_agent_id, issuer_user_id) REFERENCES agents (workspace_id, id, owner_user_id),
    FOREIGN KEY (workspace_id, recipient_user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    FOREIGN KEY (workspace_id, recipient_agent_id, recipient_user_id) REFERENCES agents (workspace_id, id, owner_user_id),
    FOREIGN KEY (workspace_id, decided_by_user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    FOREIGN KEY (workspace_id, decided_by_agent_id, decided_by_user_id) REFERENCES agents (workspace_id, id, owner_user_id)
);

CREATE INDEX team_invites_by_recipient ON team_invitations (workspace_id, recipient_user_id, recipient_agent_id, id);
CREATE INDEX team_invites_by_issuer ON team_invitations (workspace_id, issuer_user_id, issuer_agent_id, id);
CREATE INDEX team_invites_by_expiry ON team_invitations (expires_at) WHERE state = 'pending';

ALTER TABLE team_invitations ENABLE ROW LEVEL SECURITY;
ALTER TABLE team_invitations FORCE ROW LEVEL SECURITY;

CREATE POLICY team_invitation_read ON team_invitations FOR SELECT USING (
    workspace_id = office_workspace_id() AND (
        office_owns_data(issuer_user_id, coalesce(issuer_agent_id, issuer_user_id))
        OR office_owns_data(recipient_user_id, coalesce(recipient_agent_id, recipient_user_id))
    )
);
CREATE POLICY team_invitation_issue ON team_invitations FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND issuer_user_id = office_user_id()
    AND issuer_agent_id IS NOT DISTINCT FROM office_agent_id() AND team_id = office_team_manage_id()
);
CREATE POLICY team_invitation_decide ON team_invitations FOR UPDATE USING (
    workspace_id = office_workspace_id()
    AND office_owns_data(recipient_user_id, coalesce(recipient_agent_id, recipient_user_id))
) WITH CHECK (
    workspace_id = office_workspace_id() AND decided_by_user_id = office_user_id()
    AND decided_by_agent_id IS NOT DISTINCT FROM office_agent_id()
);

CREATE FUNCTION office_pending_team_invitation(workspace uuid, team uuid) RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT EXISTS (
        SELECT 1 FROM team_invitations invite
        WHERE invite.id = nullif(current_setting('office.team_invitation_id', true), '')::uuid
          AND invite.workspace_id = workspace AND invite.team_id = team
          AND invite.state = 'pending' AND invite.expires_at > transaction_timestamp()
          AND office_owns_data(invite.recipient_user_id, coalesce(invite.recipient_agent_id, invite.recipient_user_id))
    )
$$;

CREATE POLICY team_offer_read ON teams FOR SELECT USING (
    workspace_id = office_workspace_id() AND office_pending_team_invitation(workspace_id, id)
);
CREATE POLICY team_offer_join ON teams FOR UPDATE
    USING (workspace_id = office_workspace_id() AND office_pending_team_invitation(workspace_id, id))
    WITH CHECK (workspace_id = office_workspace_id() AND office_pending_team_invitation(workspace_id, id));
