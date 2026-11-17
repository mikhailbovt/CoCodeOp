CREATE TABLE path_claim_handoffs (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    checkout_id uuid NOT NULL,
    requested_task_id uuid NOT NULL,
    requested_run_id uuid NOT NULL,
    requested_scope_id uuid NOT NULL,
    scope_revision bigint NOT NULL,
    requesting_user_id uuid NOT NULL,
    requesting_agent_id uuid NOT NULL,
    holder_user_id uuid NOT NULL,
    holder_agent_id uuid NOT NULL,
    blocking_claim_id uuid NOT NULL REFERENCES path_claims (id),
    paths jsonb NOT NULL CHECK (jsonb_typeof(paths) = 'array' AND jsonb_array_length(paths) BETWEEN 1 AND 32),
    bounds office_path_multirange GENERATED ALWAYS AS (office_claim_ranges(paths)) STORED NOT NULL,
    reason text NOT NULL CHECK (octet_length(reason) BETWEEN 1 AND 2048),
    state text NOT NULL DEFAULT 'requested'
        CHECK (state IN ('requested', 'offered', 'accepted', 'rejected', 'cancelled', 'expired')),
    revision bigint NOT NULL DEFAULT 1 CHECK (revision > 0),
    expires_at timestamptz NOT NULL,
    accepted_fence bigint REFERENCES path_claims (fence),
    CHECK ((accepted_fence IS NOT NULL) = (state = 'accepted')),
    CHECK (requesting_agent_id <> holder_agent_id),
    FOREIGN KEY (workspace_id, id) REFERENCES data_scopes (workspace_id, id),
    FOREIGN KEY (workspace_id, requested_task_id, requested_run_id) REFERENCES runs (workspace_id, task_id, id),
    FOREIGN KEY (workspace_id, checkout_id, requested_scope_id) REFERENCES work_scopes (workspace_id, checkout_id, id),
    FOREIGN KEY (workspace_id, requested_scope_id, scope_revision) REFERENCES work_scope_revisions (workspace_id, scope_id, revision),
    FOREIGN KEY (workspace_id, requesting_agent_id, requesting_user_id) REFERENCES agents (workspace_id, id, owner_user_id),
    FOREIGN KEY (workspace_id, holder_agent_id, holder_user_id) REFERENCES agents (workspace_id, id, owner_user_id)
);

CREATE UNIQUE INDEX one_pending_handoff_per_run ON path_claim_handoffs (workspace_id, requested_run_id)
    WHERE state IN ('requested', 'offered');
CREATE INDEX handoffs_waiting_for_claim ON path_claim_handoffs (workspace_id, blocking_claim_id, expires_at)
    WHERE state = 'requested';
CREATE INDEX handoffs_live_checkout ON path_claim_handoffs (workspace_id, checkout_id, expires_at)
    WHERE state IN ('requested', 'offered');
CREATE INDEX handoff_reservation_conflict ON path_claim_handoffs USING gist (bounds)
    WHERE state = 'offered';

ALTER TABLE path_claim_handoffs ENABLE ROW LEVEL SECURITY;
ALTER TABLE path_claim_handoffs FORCE ROW LEVEL SECURITY;
CREATE POLICY path_handoff_read ON path_claim_handoffs FOR SELECT USING (
    workspace_id = office_workspace_id() AND (
        checkout_id = office_claim_checkout()
        OR EXISTS (SELECT 1 FROM data_scopes scope
                   WHERE scope.workspace_id = path_claim_handoffs.workspace_id AND scope.id = path_claim_handoffs.id)
    )
);
CREATE POLICY path_handoff_insert ON path_claim_handoffs FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND checkout_id = office_claim_checkout()
    AND state = 'requested' AND requesting_user_id = office_user_id() AND requesting_agent_id = office_agent_id()
    AND EXISTS (SELECT 1 FROM path_claims claim WHERE claim.id = blocking_claim_id
        AND claim.workspace_id = path_claim_handoffs.workspace_id AND claim.checkout_id = path_claim_handoffs.checkout_id
        AND claim.worker_user_id = holder_user_id AND claim.worker_agent_id = holder_agent_id
        AND claim.state = 'active')
);
CREATE POLICY path_handoff_update ON path_claim_handoffs FOR UPDATE USING (
    workspace_id = office_workspace_id() AND checkout_id = office_claim_checkout()
) WITH CHECK (
    workspace_id = office_workspace_id() AND checkout_id = office_claim_checkout()
);
