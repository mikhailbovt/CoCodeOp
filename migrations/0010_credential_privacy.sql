ALTER TABLE credentials ENABLE ROW LEVEL SECURITY;
ALTER TABLE credentials FORCE ROW LEVEL SECURITY;

CREATE POLICY credentials_read ON credentials FOR SELECT USING (
    owner_user_id = nullif(current_setting('office.credential_owner', true), '')::uuid
    OR digest = decode(nullif(current_setting('office.credential_digest', true), ''), 'hex')
);

CREATE POLICY credentials_issue ON credentials FOR INSERT WITH CHECK (
    owner_user_id = nullif(current_setting('office.credential_owner', true), '')::uuid
);

-- Authentication needs SELECT FOR SHARE. Only a human owner may update a credential.
CREATE POLICY credentials_lock_or_update ON credentials FOR UPDATE USING (
    owner_user_id = nullif(current_setting('office.credential_owner', true), '')::uuid
    OR digest = decode(nullif(current_setting('office.credential_digest', true), ''), 'hex')
) WITH CHECK (
    owner_user_id = nullif(current_setting('office.credential_owner', true), '')::uuid
);
