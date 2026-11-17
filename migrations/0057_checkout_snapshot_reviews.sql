CREATE FUNCTION office_verified_review_task() RETURNS uuid LANGUAGE sql STABLE AS $$
    SELECT nullif(current_setting('office.checkout_review_task_id',true),'')::uuid
$$;

CREATE FUNCTION office_reviews_checkout(workspace uuid, checkout uuid) RETURNS boolean LANGUAGE sql STABLE
SET search_path = pg_catalog, public AS $$
    SELECT workspace=office_workspace_id() AND EXISTS (
        SELECT 1 FROM tasks t WHERE (t.workspace_id,t.checkout_id,t.id)=(workspace,checkout,office_verified_review_task())
          AND t.status IN ('in_review','done') AND NOT t.awaiting_owner AND NOT t.review_escalated
          AND (CASE WHEN t.reviewer_user_id IS NOT NULL THEN
              (t.reviewer_user_id,t.reviewer_agent_id) IS NOT DISTINCT FROM (office_user_id(),office_agent_id())
           ELSE office_owns_data(t.accountable_user_id,coalesce(t.accountable_agent_id,t.accountable_user_id)) END)
    )
$$;

CREATE POLICY checkout_snapshot_for_review ON checkout_snapshots FOR SELECT USING (
    state='ready' AND office_reviews_checkout(workspace_id,checkout_id)
);
CREATE POLICY checkout_publication_for_review ON checkout_publications FOR SELECT USING (
    state='applied' AND task_id=office_verified_review_task() AND office_reviews_checkout(workspace_id,checkout_id)
);
CREATE POLICY checkout_command_for_review ON checkout_commands FOR SELECT USING (
    state='finished' AND source_snapshot_id IS NOT NULL AND office_reviews_checkout(workspace_id,checkout_id)
);

CREATE TABLE checkout_snapshot_reviews (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    checkout_id uuid NOT NULL,
    snapshot_id uuid NOT NULL,
    task_id uuid NOT NULL,
    result_id uuid NOT NULL REFERENCES task_result_reviews(result_id),
    reviewer_user_id uuid NOT NULL,
    reviewer_agent_id uuid,
    approval_sha256 text NOT NULL CHECK (approval_sha256 ~ '^[0-9a-f]{64}$'),
    definition jsonb NOT NULL CHECK (jsonb_typeof(definition)='object'),
    approved_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (workspace_id,snapshot_id,task_id,approval_sha256),
    FOREIGN KEY(workspace_id,checkout_id,snapshot_id) REFERENCES checkout_snapshots(workspace_id,checkout_id,id),
    FOREIGN KEY(workspace_id,task_id,result_id) REFERENCES task_result_revisions(workspace_id,task_id,id),
    FOREIGN KEY(workspace_id,reviewer_user_id) REFERENCES workspace_memberships(workspace_id,user_id),
    FOREIGN KEY(workspace_id,reviewer_agent_id,reviewer_user_id) REFERENCES agents(workspace_id,id,owner_user_id)
);

ALTER TABLE checkout_snapshot_reviews ENABLE ROW LEVEL SECURITY;
ALTER TABLE checkout_snapshot_reviews FORCE ROW LEVEL SECURITY;
CREATE POLICY checkout_snapshot_review_read ON checkout_snapshot_reviews FOR SELECT USING (
    workspace_id=office_workspace_id() AND (
        (task_id=office_verified_review_task() AND office_reviews_checkout(workspace_id,checkout_id))
        OR office_may_capture_checkout(workspace_id,checkout_id)
        OR office_hosts_checkout(workspace_id,checkout_id)
    )
);
CREATE POLICY checkout_snapshot_review_write ON checkout_snapshot_reviews FOR INSERT WITH CHECK (
    workspace_id=office_workspace_id() AND office_reviews_checkout(workspace_id,checkout_id)
    AND task_id=office_verified_review_task()
    AND (reviewer_user_id,reviewer_agent_id) IS NOT DISTINCT FROM (office_user_id(),office_agent_id())
    AND EXISTS (SELECT 1 FROM task_result_reviews r
         WHERE (r.workspace_id,r.task_id,r.result_id)=(checkout_snapshot_reviews.workspace_id,
           checkout_snapshot_reviews.task_id,checkout_snapshot_reviews.result_id)
           AND r.decision='accept' AND (r.reviewer_user_id,r.reviewer_agent_id) IS NOT DISTINCT FROM (office_user_id(),office_agent_id()))
);
