ALTER TABLE credentials
    ADD COLUMN issuance_key uuid,
    ADD COLUMN revocation_key uuid,
    ADD CONSTRAINT credential_issuance_by_owner UNIQUE (owner_user_id, issuance_key),
    ADD CONSTRAINT credential_revocation_by_owner UNIQUE (owner_user_id, revocation_key);
