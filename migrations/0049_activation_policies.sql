CREATE TABLE activation_policies (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    owner_user_id uuid NOT NULL,
    agent_id uuid NOT NULL,
    revision bigint NOT NULL CHECK (revision > 0),
    managed jsonb CHECK (managed IS NULL OR jsonb_typeof(managed) = 'object'),
    start_after_sequence bigint NOT NULL CHECK (start_after_sequence >= 0),
    updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (workspace_id, agent_id),
    FOREIGN KEY (workspace_id, id) REFERENCES data_scopes (workspace_id,id),
    FOREIGN KEY (workspace_id, agent_id, owner_user_id) REFERENCES agents (workspace_id,id,owner_user_id)
);
CREATE INDEX activation_policies_by_runner ON activation_policies (workspace_id,owner_user_id,(managed->>'runner_id'))
    WHERE managed IS NOT NULL;

ALTER TABLE activation_policies ENABLE ROW LEVEL SECURITY;
ALTER TABLE activation_policies FORCE ROW LEVEL SECURITY;
CREATE POLICY activation_policy_owner ON activation_policies FOR ALL USING (
    workspace_id=office_workspace_id() AND owner_user_id=office_user_id() AND office_agent_id() IS NULL
) WITH CHECK (
    workspace_id=office_workspace_id() AND owner_user_id=office_user_id() AND office_agent_id() IS NULL
);
CREATE POLICY activation_policy_actor ON activation_policies FOR SELECT USING (
    workspace_id=office_workspace_id() AND owner_user_id=office_user_id() AND agent_id=office_agent_id()
);
