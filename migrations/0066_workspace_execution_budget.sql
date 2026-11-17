CREATE TABLE workspace_execution_budgets (
    workspace_id uuid PRIMARY KEY REFERENCES workspaces(id),
    revision bigint NOT NULL CHECK (revision>0),
    member_user_ids uuid[] NOT NULL CHECK (cardinality(member_user_ids)<=64 AND array_position(member_user_ids,NULL) IS NULL),
    limit_units bigint NOT NULL CHECK (limit_units BETWEEN 0 AND 1000000000)
);

ALTER TABLE workspace_execution_budgets ENABLE ROW LEVEL SECURITY;
ALTER TABLE workspace_execution_budgets FORCE ROW LEVEL SECURITY;
CREATE POLICY workspace_execution_budget_members ON workspace_execution_budgets FOR SELECT USING (
    workspace_id=office_workspace_id() AND office_user_id()=ANY(member_user_ids)
    AND office_checkout_runner_id() IS NULL
);
CREATE POLICY workspace_execution_budget_administrator ON workspace_execution_budgets FOR ALL USING (
    workspace_id=office_workspace_id() AND current_setting('office.workspace_budget_context',true)='configuration'
) WITH CHECK (
    workspace_id=office_workspace_id() AND current_setting('office.workspace_budget_context',true)='configuration'
);
CREATE POLICY workspace_budget_validate_members ON workspace_memberships FOR SELECT USING (
    workspace_id=office_workspace_id() AND revoked_at IS NULL
    AND current_setting('office.workspace_budget_context',true)='configuration'
);

ALTER TABLE execution_budget_reservations ADD COLUMN uses_workspace_budget boolean NOT NULL DEFAULT false;
CREATE INDEX shared_budget_reservations_by_workspace ON execution_budget_reservations (workspace_id)
    INCLUDE (released_at, units, charged_units) WHERE uses_workspace_budget;
CREATE POLICY workspace_budget_aggregate_reservations ON execution_budget_reservations FOR SELECT USING (
    workspace_id=office_workspace_id() AND uses_workspace_budget
    AND current_setting('office.workspace_budget_context',true) IN ('configuration','accounting')
);
