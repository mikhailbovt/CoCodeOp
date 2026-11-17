CREATE TYPE office_path_range AS RANGE (
    subtype = text,
    collation = "C",
    multirange_type_name = office_path_multirange
);

-- A directory covers its own name and the strings beginning with its name and '/'.
-- In byte order '/' and '0' are adjacent. Keep these ranges separate so 'src/ab'
-- doesn't conflict with a claim for 'src/a'.
CREATE FUNCTION office_claim_ranges(paths jsonb) RETURNS office_path_multirange
LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE AS $$
    SELECT range_agg(bounds)
    FROM (
        SELECT office_path_range(rule->>'path', rule->>'path', '[]') AS bounds
        FROM jsonb_array_elements(paths) AS rule
        WHERE rule->>'kind' IN ('file', 'directory')
        UNION ALL
        SELECT office_path_range((rule->>'path') || '/', (rule->>'path') || '0', '[)')
        FROM jsonb_array_elements(paths) AS rule
        WHERE rule->>'kind' = 'directory'
    ) AS ranges
$$;

CREATE TABLE path_claims (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    checkout_id uuid NOT NULL,
    task_id uuid NOT NULL,
    run_id uuid NOT NULL,
    scope_id uuid NOT NULL,
    scope_revision bigint NOT NULL,
    worker_user_id uuid NOT NULL,
    worker_agent_id uuid NOT NULL,
    fence bigint GENERATED ALWAYS AS IDENTITY UNIQUE,
    paths jsonb NOT NULL CHECK (jsonb_typeof(paths) = 'array' AND jsonb_array_length(paths) BETWEEN 1 AND 32),
    bounds office_path_multirange GENERATED ALWAYS AS (office_claim_ranges(paths)) STORED NOT NULL,
    state text NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'released', 'expired', 'fenced')),
    started_at timestamptz NOT NULL,
    expires_at timestamptz NOT NULL,
    renewals smallint NOT NULL DEFAULT 0 CHECK (renewals BETWEEN 0 AND 2),
    CHECK (expires_at > started_at AND expires_at <= started_at + interval '5 minutes'),
    FOREIGN KEY (workspace_id, task_id, run_id) REFERENCES runs (workspace_id, task_id, id),
    FOREIGN KEY (workspace_id, checkout_id, scope_id) REFERENCES work_scopes (workspace_id, checkout_id, id),
    FOREIGN KEY (workspace_id, scope_id, scope_revision) REFERENCES work_scope_revisions (workspace_id, scope_id, revision),
    FOREIGN KEY (workspace_id, worker_agent_id, worker_user_id) REFERENCES agents (workspace_id, id, owner_user_id)
);

CREATE UNIQUE INDEX one_active_path_claim_per_run ON path_claims (workspace_id, task_id, run_id) WHERE state = 'active';
CREATE INDEX path_claim_active_checkout ON path_claims (workspace_id, checkout_id, expires_at) WHERE state = 'active';
CREATE INDEX path_claim_active_scope ON path_claims (workspace_id, scope_id) WHERE state = 'active';
CREATE INDEX path_claim_conflict ON path_claims USING gist (bounds) WHERE state = 'active';
CREATE INDEX path_claim_checkout_history ON path_claims (workspace_id, checkout_id, fence DESC);

CREATE FUNCTION office_claim_checkout() RETURNS uuid LANGUAGE sql STABLE AS $$
    SELECT nullif(current_setting('office.claim_index_context', true), '')::uuid
$$;

ALTER TABLE path_claims ENABLE ROW LEVEL SECURITY;
ALTER TABLE path_claims FORCE ROW LEVEL SECURITY;
CREATE POLICY path_claim_read ON path_claims FOR SELECT USING (
    workspace_id = office_workspace_id() AND (
        checkout_id = office_claim_checkout()
        OR EXISTS (SELECT 1 FROM data_scopes scope WHERE scope.workspace_id = path_claims.workspace_id
                   AND scope.id = path_claims.scope_id)
    )
);
CREATE POLICY path_claim_create ON path_claims FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND checkout_id = office_claim_checkout()
    AND state = 'active' AND worker_user_id = office_user_id() AND worker_agent_id = office_agent_id()
    AND EXISTS (SELECT 1 FROM work_scopes scope WHERE scope.workspace_id = path_claims.workspace_id
                    AND scope.id = path_claims.scope_id AND scope.task_id = path_claims.task_id
                    AND scope.worker_user_id = path_claims.worker_user_id
                    AND scope.worker_agent_id = path_claims.worker_agent_id)
    AND EXISTS (SELECT 1 FROM runs WHERE runs.workspace_id = path_claims.workspace_id
                    AND runs.task_id = path_claims.task_id AND runs.id = path_claims.run_id
                    AND runs.executor_user_id = path_claims.worker_user_id
                    AND runs.executor_agent_id = path_claims.worker_agent_id AND runs.status = 'running')
);
CREATE POLICY path_claim_change ON path_claims FOR UPDATE USING (
    workspace_id = office_workspace_id() AND checkout_id = office_claim_checkout()
) WITH CHECK (
    workspace_id = office_workspace_id() AND checkout_id = office_claim_checkout()
);
