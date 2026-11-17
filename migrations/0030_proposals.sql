ALTER TABLE checkout_areas ADD UNIQUE (workspace_id, checkout_id, id);

CREATE TABLE proposal_revisions (
    workspace_id uuid NOT NULL,
    proposal_id uuid NOT NULL,
    revision bigint NOT NULL CHECK (revision BETWEEN 1 AND 3),
    owner_user_id uuid NOT NULL,
    author_agent_id uuid,
    topic text NOT NULL CHECK (octet_length(topic) BETWEEN 1 AND 128),
    checkout_id uuid,
    area_id uuid,
    area_revision bigint,
    base_generation bigint,
    required_approvers jsonb NOT NULL
        CHECK (jsonb_typeof(required_approvers) = 'array' AND jsonb_array_length(required_approvers) BETWEEN 1 AND 8),
    content jsonb NOT NULL CHECK (jsonb_typeof(content) = 'object'),
    deadline timestamptz NOT NULL,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    state text NOT NULL DEFAULT 'open'
        CHECK (state IN ('open', 'changes_requested', 'accepted', 'rejected', 'superseded', 'escalated')),
    CHECK (num_nonnulls(area_id, checkout_id, area_revision, base_generation) IN (0, 4)),
    CHECK (area_revision > 0 AND base_generation >= 0),
    PRIMARY KEY (workspace_id, proposal_id, revision),
    FOREIGN KEY (workspace_id, proposal_id) REFERENCES data_scopes (workspace_id, id),
    FOREIGN KEY (workspace_id, owner_user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    FOREIGN KEY (workspace_id, author_agent_id, owner_user_id) REFERENCES agents (workspace_id, id, owner_user_id),
    FOREIGN KEY (workspace_id, checkout_id, area_id) REFERENCES checkout_areas (workspace_id, checkout_id, id)
);

CREATE TABLE proposal_responses (
    workspace_id uuid NOT NULL,
    proposal_id uuid NOT NULL,
    proposal_revision bigint NOT NULL,
    responder_user_id uuid NOT NULL,
    responder_agent_id uuid,
    decision text NOT NULL CHECK (decision IN ('approve', 'reject', 'request_changes')),
    reason text NOT NULL CHECK (octet_length(reason) BETWEEN 1 AND 2048),
    recorded_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    UNIQUE NULLS NOT DISTINCT (workspace_id, proposal_id, proposal_revision, responder_user_id, responder_agent_id),
    FOREIGN KEY (workspace_id, proposal_id, proposal_revision)
        REFERENCES proposal_revisions (workspace_id, proposal_id, revision),
    FOREIGN KEY (workspace_id, responder_user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    FOREIGN KEY (workspace_id, responder_agent_id, responder_user_id) REFERENCES agents (workspace_id, id, owner_user_id)
);

CREATE TABLE decisions (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    proposal_id uuid NOT NULL,
    proposal_revision bigint NOT NULL,
    checkout_id uuid,
    area_id uuid,
    area_revision bigint,
    generation bigint,
    topic text NOT NULL,
    selected_option jsonb NOT NULL CHECK (jsonb_typeof(selected_option) = 'object'),
    content_sha256 text NOT NULL CHECK (content_sha256 ~ '^[a-f0-9]{64}$'),
    accepted_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    CHECK (num_nonnulls(area_id, checkout_id, area_revision, generation) IN (0, 4)),
    CHECK (area_revision > 0 AND generation > 0),
    UNIQUE (workspace_id, proposal_id),
    UNIQUE (workspace_id, area_id, generation),
    FOREIGN KEY (workspace_id, proposal_id, proposal_revision)
        REFERENCES proposal_revisions (workspace_id, proposal_id, revision),
    FOREIGN KEY (workspace_id, checkout_id, area_id) REFERENCES checkout_areas (workspace_id, checkout_id, id)
);

CREATE FUNCTION office_proposal_context() RETURNS uuid LANGUAGE sql STABLE AS $$
    SELECT nullif(current_setting('office.proposal_context', true), '')::uuid
$$;

ALTER TABLE proposal_revisions ENABLE ROW LEVEL SECURITY;
ALTER TABLE proposal_revisions FORCE ROW LEVEL SECURITY;
CREATE POLICY proposal_read ON proposal_revisions FOR SELECT USING (
    workspace_id = office_workspace_id() AND EXISTS (
        SELECT 1 FROM data_scopes scope WHERE scope.workspace_id = proposal_revisions.workspace_id
            AND scope.id = proposal_revisions.proposal_id
    )
);
CREATE POLICY proposal_create ON proposal_revisions FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND office_checkout_runner_id() IS NULL AND state = 'open'
    AND EXISTS (SELECT 1 FROM data_scopes scope WHERE scope.workspace_id = proposal_revisions.workspace_id
        AND scope.id = proposal_revisions.proposal_id AND scope.owner_user_id = proposal_revisions.owner_user_id
        AND scope.author_agent_id IS NOT DISTINCT FROM proposal_revisions.author_agent_id
        AND office_owns_data(scope.owner_user_id, coalesce(scope.author_agent_id, scope.owner_user_id)))
);
CREATE POLICY proposal_state_change ON proposal_revisions FOR UPDATE USING (
    workspace_id = office_workspace_id() AND proposal_id = office_proposal_context()
) WITH CHECK (workspace_id = office_workspace_id() AND proposal_id = office_proposal_context());

ALTER TABLE proposal_responses ENABLE ROW LEVEL SECURITY;
ALTER TABLE proposal_responses FORCE ROW LEVEL SECURITY;
CREATE POLICY proposal_response_read ON proposal_responses FOR SELECT USING (
    workspace_id = office_workspace_id() AND EXISTS (
        SELECT 1 FROM data_scopes scope WHERE scope.workspace_id = proposal_responses.workspace_id
            AND scope.id = proposal_responses.proposal_id
    )
);
CREATE POLICY proposal_response_create ON proposal_responses FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND proposal_id = office_proposal_context()
    AND responder_user_id = office_user_id() AND responder_agent_id IS NOT DISTINCT FROM office_agent_id()
    AND EXISTS (SELECT 1 FROM proposal_revisions p WHERE p.workspace_id = proposal_responses.workspace_id
        AND p.proposal_id = proposal_responses.proposal_id
        AND p.revision = proposal_responses.proposal_revision AND p.state = 'open'
        AND p.required_approvers @> jsonb_build_array(jsonb_build_object('user_id', office_user_id(), 'agent_id', office_agent_id())))
);

ALTER TABLE decisions ENABLE ROW LEVEL SECURITY;
ALTER TABLE decisions FORCE ROW LEVEL SECURITY;
CREATE POLICY decision_read ON decisions FOR SELECT USING (
    workspace_id = office_workspace_id() AND (
        area_id = nullif(current_setting('office.reading_area_decisions', true), '')::uuid
        OR EXISTS (SELECT 1 FROM data_scopes scope WHERE scope.workspace_id = decisions.workspace_id
                   AND scope.id = decisions.proposal_id)
    )
);
CREATE POLICY decision_insert ON decisions FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND proposal_id = office_proposal_context()
    AND EXISTS (SELECT 1 FROM proposal_revisions p WHERE p.workspace_id = decisions.workspace_id
        AND p.proposal_id = decisions.proposal_id AND p.revision = decisions.proposal_revision AND p.state = 'accepted')
);
