CREATE TABLE channels (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    name text NOT NULL CHECK (octet_length(name) BETWEEN 1 AND 128),
    kind text NOT NULL CHECK (kind IN ('workspace', 'team', 'direct')),
    recipient_user_id uuid,
    recipient_agent_id uuid,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    UNIQUE (workspace_id, id),
    CHECK ((kind = 'direct') = (recipient_user_id IS NOT NULL)),
    CHECK (recipient_agent_id IS NULL OR recipient_user_id IS NOT NULL),
    FOREIGN KEY (workspace_id, id) REFERENCES data_scopes (workspace_id, id),
    FOREIGN KEY (workspace_id, recipient_user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    FOREIGN KEY (workspace_id, recipient_agent_id, recipient_user_id) REFERENCES agents (workspace_id, id, owner_user_id)
);

CREATE TABLE messages (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    channel_id uuid NOT NULL,
    thread_id uuid NOT NULL,
    reply_to uuid,
    author_user_id uuid NOT NULL,
    author_agent_id uuid,
    body text NOT NULL CHECK (octet_length(body) BETWEEN 1 AND 4096),
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    document tsvector GENERATED ALWAYS AS (to_tsvector('simple', body)) STORED,
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, channel_id, id),
    CHECK ((reply_to IS NULL) = (thread_id = id)),
    FOREIGN KEY (workspace_id, channel_id) REFERENCES channels (workspace_id, id),
    FOREIGN KEY (workspace_id, channel_id, thread_id) REFERENCES messages (workspace_id, channel_id, id),
    FOREIGN KEY (workspace_id, channel_id, reply_to) REFERENCES messages (workspace_id, channel_id, id),
    FOREIGN KEY (workspace_id, author_user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    FOREIGN KEY (workspace_id, author_agent_id, author_user_id) REFERENCES agents (workspace_id, id, owner_user_id)
);

CREATE INDEX channel_threads_by_start ON messages (workspace_id, channel_id, created_at, id) WHERE reply_to IS NULL;
CREATE INDEX thread_messages_by_time ON messages (workspace_id, channel_id, thread_id, created_at, id);
CREATE INDEX message_content_search ON messages USING GIN (document);

CREATE TABLE message_mentions (
    workspace_id uuid NOT NULL,
    message_id uuid NOT NULL,
    user_id uuid NOT NULL,
    agent_id uuid,
    ordinal smallint NOT NULL CHECK (ordinal BETWEEN 0 AND 15),
    UNIQUE (workspace_id, message_id, ordinal),
    UNIQUE NULLS NOT DISTINCT (workspace_id, message_id, user_id, agent_id),
    FOREIGN KEY (workspace_id, message_id) REFERENCES messages (workspace_id, id),
    FOREIGN KEY (workspace_id, user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    FOREIGN KEY (workspace_id, agent_id, user_id) REFERENCES agents (workspace_id, id, owner_user_id)
);

CREATE TABLE message_note_links (
    workspace_id uuid NOT NULL,
    message_id uuid NOT NULL,
    note_id uuid NOT NULL,
    revision bigint NOT NULL,
    ordinal smallint NOT NULL CHECK (ordinal BETWEEN 0 AND 15),
    UNIQUE (workspace_id, message_id, ordinal),
    PRIMARY KEY (workspace_id, message_id, note_id, revision),
    FOREIGN KEY (workspace_id, message_id) REFERENCES messages (workspace_id, id),
    FOREIGN KEY (workspace_id, note_id, revision) REFERENCES note_revisions (workspace_id, note_id, revision)
);

ALTER TABLE channels ENABLE ROW LEVEL SECURITY;
ALTER TABLE channels FORCE ROW LEVEL SECURITY;
CREATE POLICY channel_visible ON channels FOR SELECT USING (
    workspace_id = office_workspace_id() AND EXISTS (
        SELECT 1 FROM data_scopes scope WHERE scope.workspace_id = channels.workspace_id AND scope.id = channels.id
    )
);
CREATE POLICY channel_create ON channels FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND EXISTS (
        SELECT 1 FROM data_scopes scope
        WHERE scope.workspace_id = channels.workspace_id AND scope.id = channels.id
          AND office_owns_data(scope.owner_user_id, scope.author_id)
    )
);

ALTER TABLE messages ENABLE ROW LEVEL SECURITY;
ALTER TABLE messages FORCE ROW LEVEL SECURITY;
CREATE POLICY message_visible ON messages FOR SELECT USING (
    workspace_id = office_workspace_id() AND EXISTS (
        SELECT 1 FROM channels channel WHERE channel.workspace_id = messages.workspace_id AND channel.id = messages.channel_id
    )
);
CREATE POLICY message_append ON messages FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id()
    AND author_user_id = office_user_id() AND author_agent_id IS NOT DISTINCT FROM office_agent_id()
    AND EXISTS (
        SELECT 1 FROM channels channel WHERE channel.workspace_id = messages.workspace_id AND channel.id = messages.channel_id
    )
);

ALTER TABLE message_mentions ENABLE ROW LEVEL SECURITY;
ALTER TABLE message_mentions FORCE ROW LEVEL SECURITY;
CREATE POLICY mention_visible ON message_mentions FOR SELECT USING (
    workspace_id = office_workspace_id() AND EXISTS (
        SELECT 1 FROM messages message WHERE message.workspace_id = message_mentions.workspace_id AND message.id = message_mentions.message_id
    )
);
CREATE POLICY mention_append ON message_mentions FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND EXISTS (
        SELECT 1 FROM messages message
        WHERE message.workspace_id = message_mentions.workspace_id AND message.id = message_mentions.message_id
          AND message.author_user_id = office_user_id() AND message.author_agent_id IS NOT DISTINCT FROM office_agent_id()
    )
);

ALTER TABLE message_note_links ENABLE ROW LEVEL SECURITY;
ALTER TABLE message_note_links FORCE ROW LEVEL SECURITY;
CREATE POLICY visible_message_note_links ON message_note_links FOR SELECT USING (
    workspace_id = office_workspace_id() AND EXISTS (
        SELECT 1 FROM messages message
        WHERE message.workspace_id = message_note_links.workspace_id AND message.id = message_note_links.message_id
    ) AND EXISTS (
        SELECT 1 FROM note_revisions revision
        WHERE revision.workspace_id = message_note_links.workspace_id AND revision.note_id = message_note_links.note_id
          AND revision.revision = message_note_links.revision
    )
);
CREATE POLICY append_message_note_links ON message_note_links FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND EXISTS (
        SELECT 1 FROM messages message
        WHERE message.workspace_id = message_note_links.workspace_id AND message.id = message_note_links.message_id
          AND message.author_user_id = office_user_id() AND message.author_agent_id IS NOT DISTINCT FROM office_agent_id()
    ) AND EXISTS (
        SELECT 1 FROM note_revisions revision
        WHERE revision.workspace_id = message_note_links.workspace_id AND revision.note_id = message_note_links.note_id
          AND revision.revision = message_note_links.revision
    )
);

ALTER TABLE events
    ADD COLUMN recipient_user_id uuid,
    ADD COLUMN recipient_agent_id uuid,
    ADD COLUMN sender_user_id uuid,
    ADD COLUMN sender_agent_id uuid,
    ADD CONSTRAINT events_direct_scope CHECK (
        (recipient_user_id IS NULL) = (sender_user_id IS NULL)
        AND (recipient_user_id IS NOT NULL OR num_nonnulls(recipient_agent_id, sender_agent_id) = 0)
    ),
    ADD FOREIGN KEY (workspace_id, sender_user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    ADD FOREIGN KEY (workspace_id, sender_agent_id, sender_user_id) REFERENCES agents (workspace_id, id, owner_user_id),
    ADD FOREIGN KEY (workspace_id, recipient_user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    ADD FOREIGN KEY (workspace_id, recipient_agent_id, recipient_user_id) REFERENCES agents (workspace_id, id, owner_user_id);

ALTER POLICY event_read ON events USING (
    workspace_id = office_workspace_id()
    AND (recipient_user_id IS NULL
         OR (recipient_user_id = office_user_id() AND recipient_agent_id IS NOT DISTINCT FROM office_agent_id())
         OR (sender_user_id = office_user_id() AND sender_agent_id IS NOT DISTINCT FROM office_agent_id()))
    AND EXISTS (SELECT 1 FROM data_scopes scope WHERE scope.workspace_id = events.workspace_id AND scope.id = events.scope_id)
);

ALTER POLICY event_append ON events WITH CHECK (
    workspace_id = office_workspace_id()
    AND (recipient_user_id IS NULL
         OR (sender_user_id = office_user_id() AND sender_agent_id IS NOT DISTINCT FROM office_agent_id()))
    AND EXISTS (SELECT 1 FROM data_scopes scope WHERE scope.workspace_id = events.workspace_id AND scope.id = events.scope_id)
);
