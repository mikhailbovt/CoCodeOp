ALTER TABLE events ADD CONSTRAINT event_workspace_identity UNIQUE (workspace_id, id);

CREATE TABLE inbox_cursors (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    user_id uuid NOT NULL,
    agent_id uuid,
    position bigint NOT NULL CHECK (position >= 0),
    expires_at timestamptz NOT NULL DEFAULT transaction_timestamp() + interval '7 days',
    UNIQUE NULLS NOT DISTINCT (workspace_id, user_id, agent_id, position),
    FOREIGN KEY (workspace_id, user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    FOREIGN KEY (workspace_id, agent_id, user_id) REFERENCES agents (workspace_id, id, owner_user_id)
);

CREATE INDEX inbox_cursors_expiry ON inbox_cursors (workspace_id, user_id, agent_id, expires_at);

CREATE TABLE inbox_receipts (
    workspace_id uuid NOT NULL,
    event_id uuid NOT NULL,
    user_id uuid NOT NULL,
    agent_id uuid,
    acknowledged_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    UNIQUE NULLS NOT DISTINCT (workspace_id, event_id, user_id, agent_id),
    FOREIGN KEY (workspace_id, event_id) REFERENCES events (workspace_id, id) ON DELETE CASCADE,
    FOREIGN KEY (workspace_id, user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    FOREIGN KEY (workspace_id, agent_id, user_id) REFERENCES agents (workspace_id, id, owner_user_id)
);

ALTER TABLE inbox_cursors ENABLE ROW LEVEL SECURITY;
ALTER TABLE inbox_cursors FORCE ROW LEVEL SECURITY;
CREATE POLICY scoped_inbox_cursor ON inbox_cursors FOR ALL
    USING (workspace_id = office_workspace_id() AND user_id = office_user_id() AND agent_id IS NOT DISTINCT FROM office_agent_id())
    WITH CHECK (workspace_id = office_workspace_id() AND user_id = office_user_id() AND agent_id IS NOT DISTINCT FROM office_agent_id());

ALTER TABLE inbox_receipts ENABLE ROW LEVEL SECURITY;
ALTER TABLE inbox_receipts FORCE ROW LEVEL SECURITY;
CREATE POLICY scoped_inbox_receipt ON inbox_receipts FOR ALL USING (
    workspace_id = office_workspace_id() AND user_id = office_user_id() AND agent_id IS NOT DISTINCT FROM office_agent_id()
    AND EXISTS (
        SELECT 1 FROM events event WHERE event.workspace_id = inbox_receipts.workspace_id AND event.id = inbox_receipts.event_id
    )
) WITH CHECK (
    workspace_id = office_workspace_id() AND user_id = office_user_id() AND agent_id IS NOT DISTINCT FROM office_agent_id()
    AND EXISTS (
        SELECT 1 FROM events event WHERE event.workspace_id = inbox_receipts.workspace_id AND event.id = inbox_receipts.event_id
    )
);
