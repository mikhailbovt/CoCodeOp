CREATE TABLE notes (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    current_revision bigint NOT NULL CHECK (current_revision > 0),
    UNIQUE (workspace_id, id),
    FOREIGN KEY (workspace_id, id) REFERENCES data_scopes (workspace_id, id)
);

CREATE TABLE note_revisions (
    workspace_id uuid NOT NULL,
    note_id uuid NOT NULL,
    revision bigint NOT NULL CHECK (revision > 0),
    editor_user_id uuid NOT NULL,
    editor_agent_id uuid,
    title text NOT NULL CHECK (octet_length(title) BETWEEN 1 AND 128),
    body text NOT NULL CHECK (octet_length(body) BETWEEN 1 AND 8192),
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    document tsvector GENERATED ALWAYS AS (
        setweight(to_tsvector('simple', title), 'A') || setweight(to_tsvector('simple', body), 'B')
    ) STORED,
    PRIMARY KEY (workspace_id, note_id, revision),
    FOREIGN KEY (workspace_id, note_id) REFERENCES notes (workspace_id, id),
    FOREIGN KEY (workspace_id, editor_user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    FOREIGN KEY (workspace_id, editor_agent_id, editor_user_id) REFERENCES agents (workspace_id, id, owner_user_id)
);

ALTER TABLE notes ADD CONSTRAINT note_current_revision
    FOREIGN KEY (workspace_id, id, current_revision)
    REFERENCES note_revisions (workspace_id, note_id, revision) DEFERRABLE INITIALLY DEFERRED;

CREATE INDEX note_content_search ON note_revisions USING GIN (document);

ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
ALTER TABLE notes FORCE ROW LEVEL SECURITY;
CREATE POLICY note_read ON notes FOR SELECT USING (
    workspace_id = office_workspace_id() AND EXISTS (
        SELECT 1 FROM data_scopes scope WHERE scope.workspace_id = notes.workspace_id AND scope.id = notes.id
    )
);
CREATE POLICY note_create ON notes FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND EXISTS (
        SELECT 1 FROM data_scopes scope
        WHERE scope.workspace_id = notes.workspace_id AND scope.id = notes.id
          AND office_owns_data(scope.owner_user_id, scope.author_id)
    )
);
CREATE POLICY note_update ON notes FOR UPDATE USING (
    workspace_id = office_workspace_id() AND EXISTS (
        SELECT 1 FROM data_scopes scope WHERE scope.workspace_id = notes.workspace_id AND scope.id = notes.id
    )
);

ALTER TABLE note_revisions ENABLE ROW LEVEL SECURITY;
ALTER TABLE note_revisions FORCE ROW LEVEL SECURITY;
CREATE POLICY note_revision_read ON note_revisions FOR SELECT USING (
    workspace_id = office_workspace_id() AND EXISTS (
        SELECT 1 FROM notes note WHERE note.workspace_id = note_revisions.workspace_id AND note.id = note_revisions.note_id
    )
);
CREATE POLICY note_revision_append ON note_revisions FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id()
    AND editor_user_id = office_user_id() AND editor_agent_id IS NOT DISTINCT FROM office_agent_id()
    AND EXISTS (
        SELECT 1 FROM notes note WHERE note.workspace_id = note_revisions.workspace_id AND note.id = note_revisions.note_id
    )
);
