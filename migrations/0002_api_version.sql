ALTER TABLE office_installation
    ADD COLUMN api_version smallint NOT NULL DEFAULT 1 CHECK (api_version > 0);
