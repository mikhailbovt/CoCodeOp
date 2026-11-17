CREATE TABLE managed_workers (
    workspace_id uuid NOT NULL,
    agent_id uuid NOT NULL,
    owner_user_id uuid NOT NULL,
    lead_agent_id uuid NOT NULL,
    display_name text NOT NULL CHECK (octet_length(display_name) BETWEEN 1 AND 128),
    name_key text GENERATED ALWAYS AS (lower(display_name)) STORED,
    revision bigint NOT NULL DEFAULT 1 CHECK (revision>=1),
    PRIMARY KEY (workspace_id,agent_id),
    UNIQUE (workspace_id,agent_id,owner_user_id),
    UNIQUE (workspace_id,owner_user_id,lead_agent_id,name_key),
    FOREIGN KEY (workspace_id,agent_id,owner_user_id) REFERENCES agents (workspace_id,id,owner_user_id),
    FOREIGN KEY (workspace_id,lead_agent_id,owner_user_id) REFERENCES agents (workspace_id,id,owner_user_id),
    FOREIGN KEY (workspace_id,agent_id) REFERENCES data_scopes (workspace_id,id)
);

CREATE TABLE managed_worker_revisions (
    workspace_id uuid NOT NULL,
    agent_id uuid NOT NULL,
    owner_user_id uuid NOT NULL,
    revision bigint NOT NULL CHECK (revision>=1),
    updated_by_agent_id uuid,
    instructions text NOT NULL CHECK (octet_length(instructions)<=6000),
    updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (workspace_id,agent_id,revision),
    FOREIGN KEY (workspace_id,agent_id,owner_user_id) REFERENCES managed_workers (workspace_id,agent_id,owner_user_id),
    FOREIGN KEY (workspace_id,updated_by_agent_id,owner_user_id) REFERENCES agents (workspace_id,id,owner_user_id)
);

CREATE TABLE managed_worker_run_profiles (
    workspace_id uuid NOT NULL,
    task_id uuid NOT NULL,
    run_id uuid NOT NULL,
    owner_user_id uuid NOT NULL,
    agent_id uuid NOT NULL,
    profile_revision bigint NOT NULL,
    PRIMARY KEY (workspace_id,run_id),
    FOREIGN KEY (workspace_id,task_id,run_id) REFERENCES runs (workspace_id,task_id,id),
    FOREIGN KEY (workspace_id,agent_id,owner_user_id) REFERENCES managed_workers (workspace_id,agent_id,owner_user_id),
    FOREIGN KEY (workspace_id,agent_id,profile_revision) REFERENCES managed_worker_revisions (workspace_id,agent_id,revision)
);

ALTER TABLE managed_workers ENABLE ROW LEVEL SECURITY;
ALTER TABLE managed_workers FORCE ROW LEVEL SECURITY;
CREATE POLICY managed_worker_read ON managed_workers FOR SELECT USING (
    workspace_id=office_workspace_id() AND owner_user_id=office_user_id()
    AND (office_agent_id() IS NULL OR office_agent_id() IN (agent_id,lead_agent_id))
);
CREATE POLICY managed_worker_create ON managed_workers FOR INSERT WITH CHECK (
    workspace_id=office_workspace_id() AND owner_user_id=office_user_id() AND lead_agent_id=office_agent_id() AND revision=1
    AND EXISTS (SELECT 1 FROM agents a WHERE (a.workspace_id,a.id,a.owner_user_id)=
        (managed_workers.workspace_id,managed_workers.lead_agent_id,managed_workers.owner_user_id)
        AND a.role='lead' AND a.disabled_at IS NULL)
    AND EXISTS (SELECT 1 FROM agents a WHERE (a.workspace_id,a.id,a.owner_user_id)=
        (managed_workers.workspace_id,managed_workers.agent_id,managed_workers.owner_user_id)
        AND a.role='worker' AND a.disabled_at IS NULL)
);
CREATE POLICY managed_worker_update ON managed_workers FOR UPDATE
USING (workspace_id=office_workspace_id() AND office_owns_data(owner_user_id,lead_agent_id))
WITH CHECK (workspace_id=office_workspace_id() AND office_owns_data(owner_user_id,lead_agent_id));

ALTER TABLE managed_worker_revisions ENABLE ROW LEVEL SECURITY;
ALTER TABLE managed_worker_revisions FORCE ROW LEVEL SECURITY;
CREATE POLICY managed_worker_revision_read ON managed_worker_revisions FOR SELECT USING (
    workspace_id=office_workspace_id() AND owner_user_id=office_user_id()
    AND EXISTS (SELECT 1 FROM managed_workers a WHERE (a.workspace_id,a.agent_id)=(managed_worker_revisions.workspace_id,managed_worker_revisions.agent_id))
);
CREATE POLICY managed_worker_revision_create ON managed_worker_revisions FOR INSERT WITH CHECK (
    workspace_id=office_workspace_id() AND owner_user_id=office_user_id()
    AND updated_by_agent_id IS NOT DISTINCT FROM office_agent_id()
    AND EXISTS (SELECT 1 FROM managed_workers a
        WHERE (a.workspace_id,a.agent_id,a.revision)=(managed_worker_revisions.workspace_id,managed_worker_revisions.agent_id,managed_worker_revisions.revision)
          AND office_owns_data(a.owner_user_id,a.lead_agent_id))
);

ALTER TABLE managed_worker_run_profiles ENABLE ROW LEVEL SECURITY;
ALTER TABLE managed_worker_run_profiles FORCE ROW LEVEL SECURITY;
CREATE POLICY managed_worker_run_profile_read ON managed_worker_run_profiles FOR SELECT USING (
    workspace_id=office_workspace_id() AND owner_user_id=office_user_id()
    AND (office_agent_id() IS NULL OR agent_id=office_agent_id())
);
CREATE POLICY managed_worker_run_profile_bind ON managed_worker_run_profiles FOR INSERT WITH CHECK (
    workspace_id=office_workspace_id() AND owner_user_id=office_user_id() AND office_runner_planning()
    AND EXISTS (SELECT 1 FROM runs r WHERE (r.workspace_id,r.task_id,r.id)=
        (managed_worker_run_profiles.workspace_id,managed_worker_run_profiles.task_id,managed_worker_run_profiles.run_id)
        AND r.status='starting' AND (r.executor_user_id,r.executor_agent_id)=(owner_user_id,agent_id)
        AND r.managed_runner_id=office_checkout_runner_id() AND r.managed_job_id=office_execution_job())
    AND EXISTS (SELECT 1 FROM managed_workers a
        WHERE (a.workspace_id,a.agent_id,a.revision)=(managed_worker_run_profiles.workspace_id,managed_worker_run_profiles.agent_id,profile_revision))
);

CREATE POLICY execution_jobs_for_managed_worker_status ON execution_jobs FOR SELECT USING (
    workspace_id=office_workspace_id() AND worker_agent_id=nullif(current_setting('office.managed_worker_status_id',true),'')::uuid
    AND EXISTS (SELECT 1 FROM managed_workers a WHERE (a.workspace_id,a.agent_id)=(execution_jobs.workspace_id,execution_jobs.worker_agent_id)
        AND office_owns_data(a.owner_user_id,a.lead_agent_id))
    AND EXISTS (SELECT 1 FROM tasks t WHERE (t.workspace_id,t.id)=(execution_jobs.workspace_id,execution_jobs.task_id))
);
