ALTER TABLE credentials ADD CONSTRAINT credentials_owner UNIQUE (id, owner_user_id);

-- SELECT FOR SHARE needs an applicable UPDATE policy. The check keeps these rows immutable.
CREATE POLICY workspace_memberships_lock_self ON workspace_memberships FOR UPDATE
    USING (user_id = nullif(current_setting('office.user_id', true), '')::uuid AND revoked_at IS NULL)
    WITH CHECK (false);

CREATE TABLE workspace_invitations (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    inviter_user_id uuid NOT NULL,
    idempotency_key uuid NOT NULL,
    display_name text NOT NULL CHECK (octet_length(display_name) BETWEEN 1 AND 128),
    digest bytea NOT NULL UNIQUE CHECK (octet_length(digest) = 32),
    expires_at timestamptz NOT NULL DEFAULT transaction_timestamp() + interval '24 hours',
    accepted_user_id uuid,
    accepted_credential_id uuid,
    acceptance_key uuid,
    accepted_at timestamptz,
    UNIQUE (inviter_user_id, idempotency_key),
    FOREIGN KEY (workspace_id, inviter_user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    FOREIGN KEY (workspace_id, accepted_user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    FOREIGN KEY (accepted_credential_id, accepted_user_id) REFERENCES credentials (id, owner_user_id),
    CHECK (num_nonnulls(accepted_user_id, accepted_credential_id, acceptance_key, accepted_at) IN (0, 4))
);

ALTER TABLE workspace_invitations ENABLE ROW LEVEL SECURITY;
ALTER TABLE workspace_invitations FORCE ROW LEVEL SECURITY;

CREATE POLICY invitation_read ON workspace_invitations FOR SELECT USING (
    inviter_user_id = nullif(current_setting('office.user_id', true), '')::uuid
    OR digest = decode(nullif(current_setting('office.invitation_digest', true), ''), 'hex')
);

CREATE POLICY invitation_issue ON workspace_invitations FOR INSERT WITH CHECK (
    inviter_user_id = nullif(current_setting('office.user_id', true), '')::uuid
);

CREATE POLICY invitation_accept ON workspace_invitations FOR UPDATE
    USING (digest = decode(nullif(current_setting('office.invitation_digest', true), ''), 'hex'))
    WITH CHECK (digest = decode(nullif(current_setting('office.invitation_digest', true), ''), 'hex'));
