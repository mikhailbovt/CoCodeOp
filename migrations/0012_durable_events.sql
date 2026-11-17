CREATE TABLE workspace_event_counters (
    workspace_id uuid PRIMARY KEY REFERENCES workspaces (id),
    last_sequence bigint NOT NULL CHECK (last_sequence > 0)
);

CREATE TABLE events (
    workspace_id uuid NOT NULL,
    sequence bigint NOT NULL CHECK (sequence > 0),
    id uuid NOT NULL UNIQUE,
    scope_id uuid NOT NULL,
    event_type text NOT NULL CHECK (octet_length(event_type) BETWEEN 1 AND 64),
    payload jsonb NOT NULL CHECK (jsonb_typeof(payload) = 'object'),
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    PRIMARY KEY (workspace_id, sequence),
    FOREIGN KEY (workspace_id, scope_id) REFERENCES data_scopes (workspace_id, id)
);

CREATE TABLE outbox (
    workspace_id uuid NOT NULL,
    sequence bigint NOT NULL,
    PRIMARY KEY (workspace_id, sequence),
    FOREIGN KEY (workspace_id, sequence) REFERENCES events (workspace_id, sequence) ON DELETE CASCADE
);

CREATE TABLE idempotency_records (
    workspace_id uuid NOT NULL,
    user_id uuid NOT NULL,
    agent_id uuid,
    operation text NOT NULL CHECK (octet_length(operation) BETWEEN 1 AND 64),
    idempotency_key uuid NOT NULL,
    request_hash bytea NOT NULL CHECK (octet_length(request_hash) = 32),
    result_scope_id uuid NOT NULL,
    result jsonb NOT NULL CHECK (jsonb_typeof(result) = 'object'),
    UNIQUE NULLS NOT DISTINCT (workspace_id, user_id, agent_id, operation, idempotency_key),
    FOREIGN KEY (workspace_id, user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    FOREIGN KEY (workspace_id, agent_id, user_id) REFERENCES agents (workspace_id, id, owner_user_id),
    FOREIGN KEY (workspace_id, result_scope_id) REFERENCES data_scopes (workspace_id, id)
);

ALTER TABLE workspace_event_counters ENABLE ROW LEVEL SECURITY;
ALTER TABLE workspace_event_counters FORCE ROW LEVEL SECURITY;
CREATE POLICY workspace_event_counter ON workspace_event_counters FOR ALL
    USING (workspace_id = office_workspace_id()) WITH CHECK (workspace_id = office_workspace_id());

ALTER TABLE events ENABLE ROW LEVEL SECURITY;
ALTER TABLE events FORCE ROW LEVEL SECURITY;
CREATE POLICY event_read ON events FOR SELECT USING (
    workspace_id = office_workspace_id() AND EXISTS (
        SELECT 1 FROM data_scopes scope WHERE scope.workspace_id = events.workspace_id AND scope.id = events.scope_id
    )
);
CREATE POLICY event_append ON events FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND EXISTS (
        SELECT 1 FROM data_scopes scope WHERE scope.workspace_id = events.workspace_id AND scope.id = events.scope_id
    )
);

ALTER TABLE outbox ENABLE ROW LEVEL SECURITY;
ALTER TABLE outbox FORCE ROW LEVEL SECURITY;
CREATE POLICY outbox_read ON outbox FOR SELECT USING (
    workspace_id = office_workspace_id() AND EXISTS (
        SELECT 1 FROM events event WHERE event.workspace_id = outbox.workspace_id AND event.sequence = outbox.sequence
    )
);
CREATE POLICY outbox_append ON outbox FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND EXISTS (
        SELECT 1 FROM events event WHERE event.workspace_id = outbox.workspace_id AND event.sequence = outbox.sequence
    )
);

ALTER TABLE idempotency_records ENABLE ROW LEVEL SECURITY;
ALTER TABLE idempotency_records FORCE ROW LEVEL SECURITY;
CREATE POLICY idempotency_read ON idempotency_records FOR SELECT USING (
    workspace_id = office_workspace_id() AND user_id = office_user_id() AND agent_id IS NOT DISTINCT FROM office_agent_id()
    AND EXISTS (
        SELECT 1 FROM data_scopes scope
        WHERE scope.workspace_id = idempotency_records.workspace_id AND scope.id = idempotency_records.result_scope_id
    )
);
CREATE POLICY idempotency_record ON idempotency_records FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND user_id = office_user_id() AND agent_id IS NOT DISTINCT FROM office_agent_id()
    AND EXISTS (
        SELECT 1 FROM data_scopes scope
        WHERE scope.workspace_id = idempotency_records.workspace_id AND scope.id = idempotency_records.result_scope_id
    )
);
