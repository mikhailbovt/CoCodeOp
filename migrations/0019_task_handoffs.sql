CREATE TABLE task_handoffs (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    task_id uuid NOT NULL,
    expected_revision bigint NOT NULL CHECK (expected_revision > 0),
    sender_user_id uuid NOT NULL,
    sender_agent_id uuid,
    recipient_user_id uuid NOT NULL,
    recipient_agent_id uuid,
    state text NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'accepted', 'declined')),
    reason text NOT NULL CHECK (octet_length(reason) BETWEEN 1 AND 2048),
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
    FOREIGN KEY (workspace_id, task_id) REFERENCES tasks (workspace_id, id),
    FOREIGN KEY (workspace_id, sender_user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    FOREIGN KEY (workspace_id, sender_agent_id, sender_user_id) REFERENCES agents (workspace_id, id, owner_user_id),
    FOREIGN KEY (workspace_id, recipient_user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    FOREIGN KEY (workspace_id, recipient_agent_id, recipient_user_id) REFERENCES agents (workspace_id, id, owner_user_id),
    FOREIGN KEY (workspace_id, decided_by_user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    FOREIGN KEY (workspace_id, decided_by_agent_id, decided_by_user_id) REFERENCES agents (workspace_id, id, owner_user_id)
);

CREATE INDEX task_handoffs_by_recipient ON task_handoffs (workspace_id, recipient_user_id, recipient_agent_id, id);
CREATE INDEX task_handoffs_by_sender ON task_handoffs (workspace_id, sender_user_id, sender_agent_id, id);

ALTER TABLE task_handoffs ENABLE ROW LEVEL SECURITY;
ALTER TABLE task_handoffs FORCE ROW LEVEL SECURITY;

CREATE POLICY task_handoff_read ON task_handoffs FOR SELECT USING (
    workspace_id = office_workspace_id() AND (
        office_owns_data(sender_user_id, coalesce(sender_agent_id, sender_user_id))
        OR office_owns_data(recipient_user_id, coalesce(recipient_agent_id, recipient_user_id))
    )
);
CREATE POLICY task_handoff_create ON task_handoffs FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND sender_user_id = office_user_id()
    AND sender_agent_id IS NOT DISTINCT FROM office_agent_id()
    AND EXISTS (SELECT 1 FROM tasks WHERE workspace_id = task_handoffs.workspace_id AND id = task_handoffs.task_id
         AND revision = task_handoffs.expected_revision
         AND office_owns_data(accountable_user_id, coalesce(accountable_agent_id, accountable_user_id)))
);
CREATE POLICY task_handoff_decide ON task_handoffs FOR UPDATE USING (
    workspace_id = office_workspace_id()
    AND office_owns_data(recipient_user_id, coalesce(recipient_agent_id, recipient_user_id))
) WITH CHECK (
    workspace_id = office_workspace_id() AND decided_by_user_id = office_user_id()
    AND decided_by_agent_id IS NOT DISTINCT FROM office_agent_id()
);
