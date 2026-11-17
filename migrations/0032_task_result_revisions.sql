ALTER TABLE tasks ADD COLUMN criteria_revision bigint NOT NULL DEFAULT 1 CHECK (criteria_revision > 0);
ALTER TABLE tasks ADD COLUMN review_escalated boolean NOT NULL DEFAULT false;
ALTER TABLE tasks ADD COLUMN review_cycle_after bigint NOT NULL DEFAULT 0 CHECK (review_cycle_after >= 0);
ALTER TABLE tasks ADD CONSTRAINT task_review_escalation_is_blocked CHECK (NOT review_escalated OR status = 'blocked');

CREATE FUNCTION office_review_digest(document jsonb) RETURNS text
LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE AS $$
    SELECT encode(sha256(convert_to(document::text, 'UTF8')), 'hex')
$$;

CREATE TABLE task_result_revisions (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    task_id uuid NOT NULL,
    run_id uuid NOT NULL,
    revision bigint NOT NULL CHECK (revision > 0),
    author_user_id uuid NOT NULL,
    author_agent_id uuid,
    definition jsonb NOT NULL CHECK (jsonb_typeof(definition) = 'object'),
    explicit_binding boolean NOT NULL,
    result_sha256 text GENERATED ALWAYS AS (office_review_digest(definition)) STORED,
    criteria_sha256 text GENERATED ALWAYS AS (office_review_digest(definition->'acceptance_criteria')) STORED NOT NULL,
    submitted_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (workspace_id, task_id, id),
    UNIQUE (workspace_id, task_id, revision),
    UNIQUE (workspace_id, task_id, run_id),
    FOREIGN KEY (workspace_id, task_id, run_id) REFERENCES runs (workspace_id, task_id, id),
    FOREIGN KEY (workspace_id, author_user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    FOREIGN KEY (workspace_id, author_agent_id, author_user_id) REFERENCES agents (workspace_id, id, owner_user_id)
);

-- Recover only the summaries and criteria actually recorded for earlier submissions.
ALTER TABLE tasks NO FORCE ROW LEVEL SECURITY;
ALTER TABLE runs NO FORCE ROW LEVEL SECURITY;
INSERT INTO task_result_revisions (id, workspace_id, task_id, run_id, revision, author_user_id,
    author_agent_id, definition, explicit_binding, submitted_at)
SELECT gen_random_uuid(), r.workspace_id, r.task_id, r.id, r.attempt_number, r.executor_user_id,
    r.executor_agent_id, jsonb_build_object('summary', r.summary, 'criteria_revision', t.criteria_revision,
        'acceptance_criteria', t.criteria, 'artifacts', '[]'::jsonb, 'agent_reported_checks', '[]'::jsonb),
    false, r.ended_at
FROM runs r JOIN tasks t ON (t.workspace_id,t.id) = (r.workspace_id,r.task_id)
WHERE r.status = 'succeeded' AND r.summary IS NOT NULL;
ALTER TABLE runs FORCE ROW LEVEL SECURITY;
ALTER TABLE tasks FORCE ROW LEVEL SECURITY;

CREATE TABLE task_result_reviews (
    result_id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    task_id uuid NOT NULL,
    reviewer_user_id uuid NOT NULL,
    reviewer_agent_id uuid,
    decision text NOT NULL CHECK (decision IN ('accept', 'request_changes')),
    reason text NOT NULL CHECK (octet_length(reason) BETWEEN 1 AND 4096),
    reviewed_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    FOREIGN KEY (workspace_id, task_id, result_id) REFERENCES task_result_revisions (workspace_id, task_id, id),
    FOREIGN KEY (workspace_id, reviewer_user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    FOREIGN KEY (workspace_id, reviewer_agent_id, reviewer_user_id) REFERENCES agents (workspace_id, id, owner_user_id)
);
CREATE INDEX task_result_review_by_task ON task_result_reviews (workspace_id, task_id, decision);

ALTER TABLE task_result_revisions ENABLE ROW LEVEL SECURITY;
ALTER TABLE task_result_revisions FORCE ROW LEVEL SECURITY;
CREATE POLICY task_result_read ON task_result_revisions FOR SELECT USING (
    workspace_id = office_workspace_id() AND EXISTS (
        SELECT 1 FROM tasks t WHERE t.workspace_id = task_result_revisions.workspace_id
        AND t.id = task_result_revisions.task_id
    )
);
CREATE POLICY task_result_submit ON task_result_revisions FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND author_user_id = office_user_id()
    AND author_agent_id IS NOT DISTINCT FROM office_agent_id() AND office_checkout_runner_id() IS NULL
    AND EXISTS (SELECT 1 FROM runs r WHERE r.workspace_id = task_result_revisions.workspace_id
          AND r.task_id = task_result_revisions.task_id AND r.id = task_result_revisions.run_id
          AND r.attempt_number = task_result_revisions.revision AND r.status = 'succeeded'
          AND r.executor_user_id = task_result_revisions.author_user_id
          AND r.executor_agent_id IS NOT DISTINCT FROM task_result_revisions.author_agent_id)
);

ALTER TABLE task_result_reviews ENABLE ROW LEVEL SECURITY;
ALTER TABLE task_result_reviews FORCE ROW LEVEL SECURITY;
CREATE POLICY task_result_review_read ON task_result_reviews FOR SELECT USING (
    workspace_id = office_workspace_id() AND EXISTS (
        SELECT 1 FROM task_result_revisions result WHERE result.workspace_id = task_result_reviews.workspace_id
        AND result.task_id = task_result_reviews.task_id AND result.id = task_result_reviews.result_id
    )
);
CREATE POLICY task_result_review_record ON task_result_reviews FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND reviewer_user_id = office_user_id()
    AND reviewer_agent_id IS NOT DISTINCT FROM office_agent_id() AND office_checkout_runner_id() IS NULL
    AND result_id = nullif(current_setting('office.review_result_context', true), '')::uuid
);
