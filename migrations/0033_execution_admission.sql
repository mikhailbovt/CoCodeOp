ALTER TABLE runtime_templates DROP CONSTRAINT runtime_templates_kind_check;
ALTER TABLE runtime_templates ADD COLUMN managed jsonb;
ALTER TABLE runtime_templates ADD CHECK (kind IN ('interactive', 'managed'));
ALTER TABLE runtime_templates ADD CHECK ((kind = 'managed') = (managed IS NOT NULL));
ALTER TABLE runtime_templates ADD CHECK (jsonb_typeof(managed) = 'object');

CREATE TABLE runner_execution_profiles (
    runner_id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    owner_user_id uuid NOT NULL,
    revision bigint NOT NULL DEFAULT 1 CHECK (revision > 0),
    capacity smallint NOT NULL CHECK (capacity BETWEEN 0 AND 64),
    runtime_template_ids uuid[] NOT NULL CHECK (cardinality(runtime_template_ids) <= 64),
    capabilities text[] NOT NULL CHECK (cardinality(capabilities) <= 32),
    expires_at timestamptz NOT NULL,
    FOREIGN KEY (workspace_id, runner_id, owner_user_id) REFERENCES runners (workspace_id, id, owner_user_id)
);
CREATE INDEX runner_execution_profiles_owner ON runner_execution_profiles (workspace_id, owner_user_id, expires_at);

CREATE TABLE execution_budgets (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    owner_user_id uuid NOT NULL,
    revision bigint NOT NULL DEFAULT 1 CHECK (revision > 0),
    limit_units bigint NOT NULL CHECK (limit_units BETWEEN 0 AND 1000000000000),
    reserved_units bigint NOT NULL DEFAULT 0 CHECK (reserved_units >= 0),
    charged_units bigint NOT NULL DEFAULT 0 CHECK (charged_units >= 0),
    CHECK (reserved_units + charged_units <= limit_units),
    UNIQUE (workspace_id, owner_user_id),
    FOREIGN KEY (workspace_id, id) REFERENCES data_scopes (workspace_id, id),
    FOREIGN KEY (workspace_id, owner_user_id) REFERENCES workspace_memberships (workspace_id, user_id)
);

ALTER TABLE runner_execution_profiles ENABLE ROW LEVEL SECURITY;
ALTER TABLE runner_execution_profiles FORCE ROW LEVEL SECURITY;
CREATE POLICY execution_profile_read ON runner_execution_profiles FOR SELECT USING (
    workspace_id = office_workspace_id() AND owner_user_id = office_user_id()
    AND (office_checkout_runner_id() IS NULL OR runner_id = office_checkout_runner_id())
);
CREATE POLICY execution_profile_insert ON runner_execution_profiles FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND owner_user_id = office_user_id()
    AND runner_id = office_checkout_runner_id()
);
CREATE POLICY execution_profile_update ON runner_execution_profiles FOR UPDATE USING (
    workspace_id = office_workspace_id() AND owner_user_id = office_user_id()
    AND runner_id = office_checkout_runner_id()
) WITH CHECK (
    workspace_id = office_workspace_id() AND owner_user_id = office_user_id()
    AND runner_id = office_checkout_runner_id()
);

ALTER TABLE execution_budgets ENABLE ROW LEVEL SECURITY;
ALTER TABLE execution_budgets FORCE ROW LEVEL SECURITY;
CREATE POLICY execution_budget_owner ON execution_budgets FOR ALL USING (
    workspace_id = office_workspace_id() AND owner_user_id = office_user_id()
    AND office_agent_id() IS NULL AND office_checkout_runner_id() IS NULL
) WITH CHECK (
    workspace_id = office_workspace_id() AND owner_user_id = office_user_id()
    AND office_agent_id() IS NULL AND office_checkout_runner_id() IS NULL
);
