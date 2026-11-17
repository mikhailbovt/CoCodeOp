CREATE TABLE artifacts (
    workspace_id uuid NOT NULL,
    id uuid NOT NULL,
    owner_user_id uuid NOT NULL,
    owner_agent_id uuid,
    revision bigint NOT NULL DEFAULT 0 CHECK (revision >= 0),
    PRIMARY KEY (workspace_id,id),
    UNIQUE (workspace_id,id,owner_user_id),
    FOREIGN KEY (workspace_id,id) REFERENCES data_scopes (workspace_id,id),
    FOREIGN KEY (workspace_id,owner_user_id) REFERENCES workspace_memberships (workspace_id,user_id),
    FOREIGN KEY (workspace_id,owner_agent_id,owner_user_id) REFERENCES agents (workspace_id,id,owner_user_id)
);

CREATE TABLE artifact_versions (
    workspace_id uuid NOT NULL,
    upload_id uuid NOT NULL,
    artifact_id uuid NOT NULL,
    revision bigint NOT NULL CHECK (revision > 0),
    owner_user_id uuid NOT NULL,
    author_agent_id uuid,
    name text NOT NULL CHECK (octet_length(name) BETWEEN 1 AND 160),
    mime text NOT NULL CHECK (mime IN ('application/octet-stream','application/json','application/gzip','application/zip','text/plain')),
    size_bytes bigint NOT NULL CHECK (size_bytes BETWEEN 1 AND 67108864),
    sha256 text NOT NULL CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    retention_days integer NOT NULL CHECK (retention_days BETWEEN 1 AND 365),
    task_id uuid,
    run_id uuid,
    state text NOT NULL DEFAULT 'uploading' CHECK (state IN ('uploading','ready','expired')),
    upload_until timestamptz NOT NULL DEFAULT clock_timestamp()+interval '1 hour',
    finalized_at timestamptz,
    expires_at timestamptz,
    PRIMARY KEY (workspace_id,upload_id),
    UNIQUE (workspace_id,artifact_id,revision),
    CHECK (task_id IS NOT NULL OR run_id IS NULL),
    CHECK ((state = 'uploading' AND finalized_at IS NULL AND expires_at IS NULL)
        OR (state <> 'uploading' AND finalized_at IS NOT NULL AND expires_at IS NOT NULL AND expires_at > finalized_at)),
    FOREIGN KEY (workspace_id,artifact_id,owner_user_id) REFERENCES artifacts (workspace_id,id,owner_user_id),
    FOREIGN KEY (workspace_id,author_agent_id,owner_user_id) REFERENCES agents (workspace_id,id,owner_user_id),
    FOREIGN KEY (workspace_id,task_id) REFERENCES tasks (workspace_id,id),
    FOREIGN KEY (workspace_id,task_id,run_id) REFERENCES runs (workspace_id,task_id,id)
);
CREATE UNIQUE INDEX one_open_artifact_upload ON artifact_versions (workspace_id,artifact_id) WHERE state='uploading';
CREATE INDEX artifact_retention_by_owner ON artifact_versions (workspace_id,owner_user_id,upload_until,expires_at) WHERE state<>'expired';

CREATE FUNCTION office_protect_artifact_version() RETURNS trigger LANGUAGE plpgsql
SET search_path = pg_catalog, public AS $$
BEGIN
    IF (to_jsonb(NEW)-ARRAY['state','finalized_at','expires_at']) IS DISTINCT FROM
       (to_jsonb(OLD)-ARRAY['state','finalized_at','expires_at']) OR NOT (
        (OLD.state='uploading' AND NEW.state='ready' AND OLD.upload_until>statement_timestamp()) OR
        (OLD.state='ready' AND NEW.state='expired' AND OLD.expires_at<=clock_timestamp()
         AND (OLD.finalized_at,OLD.expires_at)=(NEW.finalized_at,NEW.expires_at))
    ) THEN
        RAISE EXCEPTION 'immutable artifact revision' USING ERRCODE='23514';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER protect_artifact_version BEFORE UPDATE ON artifact_versions
    FOR EACH ROW EXECUTE FUNCTION office_protect_artifact_version();

CREATE TABLE artifact_chunks (
    workspace_id uuid NOT NULL,
    upload_id uuid NOT NULL,
    ordinal integer NOT NULL CHECK (ordinal BETWEEN 0 AND 63),
    content bytea NOT NULL CHECK (octet_length(content) BETWEEN 1 AND 1048576),
    PRIMARY KEY (workspace_id,upload_id,ordinal),
    FOREIGN KEY (workspace_id,upload_id) REFERENCES artifact_versions (workspace_id,upload_id) ON DELETE CASCADE
);

CREATE TABLE artifact_storage_usage (
    workspace_id uuid NOT NULL,
    owner_user_id uuid NOT NULL,
    occupied_bytes bigint NOT NULL DEFAULT 0 CHECK (occupied_bytes BETWEEN 0 AND 536870912),
    occupied_revisions integer NOT NULL DEFAULT 0 CHECK (occupied_revisions BETWEEN 0 AND 256),
    PRIMARY KEY (workspace_id,owner_user_id),
    FOREIGN KEY (workspace_id,owner_user_id) REFERENCES workspace_memberships (workspace_id,user_id)
);

ALTER TABLE artifacts ENABLE ROW LEVEL SECURITY;
ALTER TABLE artifacts FORCE ROW LEVEL SECURITY;
CREATE POLICY artifact_read ON artifacts FOR SELECT USING (
    workspace_id=office_workspace_id() AND EXISTS (
        SELECT 1 FROM data_scopes s WHERE (s.workspace_id,s.id)=(artifacts.workspace_id,artifacts.id)
    )
);
CREATE POLICY artifact_create ON artifacts FOR INSERT WITH CHECK (
    workspace_id=office_workspace_id() AND owner_user_id=office_user_id()
    AND owner_agent_id IS NOT DISTINCT FROM office_agent_id() AND revision=0
    AND EXISTS (
        SELECT 1 FROM data_scopes s WHERE (s.workspace_id,s.id)=(artifacts.workspace_id,artifacts.id)
            AND office_owns_data(s.owner_user_id,s.author_id)
    )
);
CREATE POLICY artifact_advance ON artifacts FOR UPDATE
    USING (workspace_id=office_workspace_id() AND office_owns_data(owner_user_id,coalesce(owner_agent_id,owner_user_id)))
    WITH CHECK (workspace_id=office_workspace_id() AND office_owns_data(owner_user_id,coalesce(owner_agent_id,owner_user_id)));

ALTER TABLE artifact_versions ENABLE ROW LEVEL SECURITY;
ALTER TABLE artifact_versions FORCE ROW LEVEL SECURITY;
CREATE POLICY artifact_version_read ON artifact_versions FOR SELECT USING (
    workspace_id=office_workspace_id()
    AND (state<>'uploading' OR office_owns_data(owner_user_id,coalesce(author_agent_id,owner_user_id)))
    AND EXISTS (
        SELECT 1 FROM artifacts a WHERE (a.workspace_id,a.id)=(artifact_versions.workspace_id,artifact_versions.artifact_id)
    )
);
CREATE POLICY artifact_version_create ON artifact_versions FOR INSERT WITH CHECK (
    workspace_id=office_workspace_id() AND owner_user_id=office_user_id() AND state='uploading'
    AND author_agent_id IS NOT DISTINCT FROM office_agent_id()
    AND EXISTS (
        SELECT 1 FROM artifacts a WHERE (a.workspace_id,a.id,a.revision+1)=
            (artifact_versions.workspace_id,artifact_versions.artifact_id,artifact_versions.revision)
            AND office_owns_data(a.owner_user_id,coalesce(a.owner_agent_id,a.owner_user_id))
    )
);
CREATE POLICY artifact_version_update ON artifact_versions FOR UPDATE
    USING (workspace_id=office_workspace_id() AND office_owns_data(owner_user_id,coalesce(author_agent_id,owner_user_id)))
    WITH CHECK (workspace_id=office_workspace_id() AND office_owns_data(owner_user_id,coalesce(author_agent_id,owner_user_id)));
CREATE POLICY artifact_version_abandon ON artifact_versions FOR DELETE USING (
    workspace_id=office_workspace_id() AND state='uploading' AND upload_until<=clock_timestamp()
    AND office_owns_data(owner_user_id,coalesce(author_agent_id,owner_user_id))
);

ALTER TABLE artifact_chunks ENABLE ROW LEVEL SECURITY;
ALTER TABLE artifact_chunks FORCE ROW LEVEL SECURITY;
CREATE POLICY artifact_chunk_read ON artifact_chunks FOR SELECT USING (
    workspace_id=office_workspace_id() AND EXISTS (
        SELECT 1 FROM artifact_versions v WHERE (v.workspace_id,v.upload_id)=(artifact_chunks.workspace_id,artifact_chunks.upload_id)
          AND ((v.state='ready' AND v.expires_at>clock_timestamp())
            OR office_owns_data(v.owner_user_id,coalesce(v.author_agent_id,v.owner_user_id)))
    )
);
CREATE POLICY artifact_chunk_upload ON artifact_chunks FOR INSERT WITH CHECK (
    workspace_id=office_workspace_id() AND EXISTS (
        SELECT 1 FROM artifact_versions v WHERE (v.workspace_id,v.upload_id)=(artifact_chunks.workspace_id,artifact_chunks.upload_id)
          AND v.state='uploading' AND v.upload_until>clock_timestamp()
          AND office_owns_data(v.owner_user_id,coalesce(v.author_agent_id,v.owner_user_id))
          AND v.size_bytes>ordinal::bigint*1048576
          AND octet_length(content)=least(1048576,v.size_bytes-ordinal::bigint*1048576)
    )
);
CREATE POLICY artifact_chunk_expire ON artifact_chunks FOR DELETE USING (
    workspace_id=office_workspace_id() AND EXISTS (
        SELECT 1 FROM artifact_versions v WHERE (v.workspace_id,v.upload_id)=(artifact_chunks.workspace_id,artifact_chunks.upload_id)
          AND v.state='expired' AND office_owns_data(v.owner_user_id,coalesce(v.author_agent_id,v.owner_user_id))
    )
);

ALTER TABLE artifact_storage_usage ENABLE ROW LEVEL SECURITY;
ALTER TABLE artifact_storage_usage FORCE ROW LEVEL SECURITY;
CREATE POLICY artifact_storage_owner ON artifact_storage_usage FOR ALL
    USING (workspace_id=office_workspace_id() AND owner_user_id=office_user_id())
    WITH CHECK (workspace_id=office_workspace_id() AND owner_user_id=office_user_id());
