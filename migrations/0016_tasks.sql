ALTER TABLE agent_instances ADD CONSTRAINT instances_workspace_actor UNIQUE (workspace_id, agent_id, owner_user_id, id);

CREATE TABLE tasks (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    parent_task_id uuid,
    accountable_user_id uuid NOT NULL,
    accountable_agent_id uuid,
    assignee_user_id uuid,
    assignee_agent_id uuid,
    objective text NOT NULL CHECK (octet_length(objective) BETWEEN 1 AND 4096),
    criteria jsonb NOT NULL CHECK (jsonb_typeof(criteria) = 'array' AND jsonb_array_length(criteria) BETWEEN 1 AND 16),
    status text NOT NULL DEFAULT 'draft' CHECK (status IN (
        'draft', 'ready', 'assigned', 'in_progress', 'blocked', 'in_review',
        'changes_requested', 'done', 'cancelled', 'failed'
    )),
    revision bigint NOT NULL DEFAULT 1 CHECK (revision > 0),
    execution_kind text NOT NULL CHECK (execution_kind IN ('coordination', 'coding')),
    checkout_id uuid,
    work_scope_id uuid,
    work_scope_revision bigint CHECK (work_scope_revision > 0),
    reviewer_user_id uuid,
    reviewer_agent_id uuid,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    UNIQUE (workspace_id, id),
    CHECK (parent_task_id IS DISTINCT FROM id),
    CHECK (assignee_agent_id IS NULL OR assignee_user_id IS NOT NULL),
    CHECK (reviewer_agent_id IS NULL OR reviewer_user_id IS NOT NULL),
    CHECK ((work_scope_id IS NULL) = (work_scope_revision IS NULL)),
    CHECK (work_scope_id IS NULL OR checkout_id IS NOT NULL),
    CHECK (execution_kind = 'coding' OR checkout_id IS NULL),
    FOREIGN KEY (workspace_id, id) REFERENCES data_scopes (workspace_id, id),
    FOREIGN KEY (workspace_id, parent_task_id) REFERENCES tasks (workspace_id, id),
    FOREIGN KEY (workspace_id, accountable_user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    FOREIGN KEY (workspace_id, accountable_agent_id, accountable_user_id) REFERENCES agents (workspace_id, id, owner_user_id),
    FOREIGN KEY (workspace_id, assignee_user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    FOREIGN KEY (workspace_id, assignee_agent_id, assignee_user_id) REFERENCES agents (workspace_id, id, owner_user_id),
    FOREIGN KEY (workspace_id, reviewer_user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    FOREIGN KEY (workspace_id, reviewer_agent_id, reviewer_user_id) REFERENCES agents (workspace_id, id, owner_user_id)
);

CREATE INDEX task_queue_by_state ON tasks (workspace_id, status, id);
CREATE INDEX task_assignee_queue ON tasks (workspace_id, assignee_user_id, assignee_agent_id, status);
CREATE INDEX task_by_parent ON tasks (workspace_id, parent_task_id);

CREATE TABLE task_dependencies (
    workspace_id uuid NOT NULL,
    task_id uuid NOT NULL,
    prerequisite_id uuid NOT NULL,
    PRIMARY KEY (workspace_id, task_id, prerequisite_id),
    CHECK (task_id <> prerequisite_id),
    FOREIGN KEY (workspace_id, task_id) REFERENCES tasks (workspace_id, id),
    FOREIGN KEY (workspace_id, prerequisite_id) REFERENCES tasks (workspace_id, id)
);

CREATE INDEX task_dependency_consumers ON task_dependencies (workspace_id, prerequisite_id, task_id);

CREATE TABLE runs (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    task_id uuid NOT NULL,
    attempt_number bigint NOT NULL CHECK (attempt_number > 0),
    executor_user_id uuid NOT NULL,
    executor_agent_id uuid,
    instance_id uuid,
    status text NOT NULL CHECK (status IN ('running', 'waiting_input', 'succeeded', 'failed', 'cancel_requested', 'cancelled')),
    summary text CHECK (octet_length(summary) BETWEEN 1 AND 4096),
    waiting_until timestamptz,
    started_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    ended_at timestamptz,
    UNIQUE (workspace_id, task_id, id),
    UNIQUE (workspace_id, task_id, attempt_number),
    CHECK ((ended_at IS NOT NULL) = (status IN ('succeeded', 'failed', 'cancelled'))),
    CHECK ((waiting_until IS NOT NULL) = (status = 'waiting_input')),
    CHECK ((instance_id IS NULL) = (executor_agent_id IS NULL)),
    FOREIGN KEY (workspace_id, task_id) REFERENCES tasks (workspace_id, id),
    FOREIGN KEY (workspace_id, executor_user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    FOREIGN KEY (workspace_id, executor_agent_id, executor_user_id) REFERENCES agents (workspace_id, id, owner_user_id),
    FOREIGN KEY (workspace_id, executor_agent_id, executor_user_id, instance_id)
        REFERENCES agent_instances (workspace_id, agent_id, owner_user_id, id)
);

CREATE UNIQUE INDEX one_active_run_per_task ON runs (workspace_id, task_id)
    WHERE status IN ('running', 'waiting_input', 'cancel_requested');

ALTER TABLE tasks ENABLE ROW LEVEL SECURITY;
ALTER TABLE tasks FORCE ROW LEVEL SECURITY;
CREATE POLICY task_visible ON tasks FOR SELECT USING (
    workspace_id = office_workspace_id() AND EXISTS (
        SELECT 1 FROM data_scopes scope WHERE scope.workspace_id = tasks.workspace_id AND scope.id = tasks.id
    )
);
CREATE POLICY task_create ON tasks FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND accountable_user_id = office_user_id()
    AND accountable_agent_id IS NOT DISTINCT FROM office_agent_id()
    AND EXISTS (
        SELECT 1 FROM data_scopes scope
        WHERE scope.workspace_id = tasks.workspace_id AND scope.id = tasks.id
          AND office_owns_data(scope.owner_user_id, scope.author_id)
    )
);
CREATE POLICY task_change ON tasks FOR UPDATE USING (
    workspace_id = office_workspace_id() AND EXISTS (
        SELECT 1 FROM data_scopes scope WHERE scope.workspace_id = tasks.workspace_id AND scope.id = tasks.id
    )
);

ALTER TABLE task_dependencies ENABLE ROW LEVEL SECURITY;
ALTER TABLE task_dependencies FORCE ROW LEVEL SECURITY;
CREATE POLICY dependency_read ON task_dependencies FOR SELECT USING (
    workspace_id = office_workspace_id() AND EXISTS (
        SELECT 1 FROM tasks task WHERE task.workspace_id = task_dependencies.workspace_id AND task.id = task_dependencies.task_id
    )
);
CREATE POLICY dependency_create ON task_dependencies FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND EXISTS (
        SELECT 1 FROM tasks task WHERE task.workspace_id = task_dependencies.workspace_id AND task.id = task_dependencies.task_id
    )
);

ALTER TABLE runs ENABLE ROW LEVEL SECURITY;
ALTER TABLE runs FORCE ROW LEVEL SECURITY;
CREATE POLICY run_visible ON runs FOR SELECT USING (
    workspace_id = office_workspace_id() AND EXISTS (
        SELECT 1 FROM tasks task WHERE task.workspace_id = runs.workspace_id AND task.id = runs.task_id
    )
);
CREATE POLICY run_start ON runs FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND executor_user_id = office_user_id()
    AND executor_agent_id IS NOT DISTINCT FROM office_agent_id()
    AND EXISTS (SELECT 1 FROM tasks task WHERE task.workspace_id = runs.workspace_id AND task.id = runs.task_id)
);
CREATE POLICY run_change ON runs FOR UPDATE USING (
    workspace_id = office_workspace_id()
    AND EXISTS (SELECT 1 FROM tasks task WHERE task.workspace_id = runs.workspace_id AND task.id = runs.task_id)
);
