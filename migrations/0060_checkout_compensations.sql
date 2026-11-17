ALTER TABLE checkout_publications ADD COLUMN compensates_change_id uuid REFERENCES checkout_publications(id);
ALTER TABLE checkout_publications ADD CHECK (compensates_change_id IS DISTINCT FROM id);
CREATE UNIQUE INDEX applied_compensation_by_change ON checkout_publications(workspace_id,checkout_id,compensates_change_id)
    WHERE state='applied' AND compensates_change_id IS NOT NULL;

CREATE TABLE checkout_change_paths (
    workspace_id uuid NOT NULL,
    checkout_id uuid NOT NULL,
    change_id uuid NOT NULL REFERENCES checkout_publications(id),
    checkout_revision bigint NOT NULL CHECK (checkout_revision>0),
    path text NOT NULL CHECK (octet_length(path) BETWEEN 1 AND 1024),
    PRIMARY KEY (workspace_id,checkout_id,change_id,path),
    FOREIGN KEY(workspace_id,checkout_id) REFERENCES project_checkouts(workspace_id,id)
);
CREATE INDEX checkout_path_change_order ON checkout_change_paths(workspace_id,checkout_id,path,checkout_revision);

ALTER TABLE checkout_publications NO FORCE ROW LEVEL SECURITY;
INSERT INTO checkout_change_paths (workspace_id,checkout_id,change_id,checkout_revision,path)
SELECT p.workspace_id,p.checkout_id,p.id,(p.outcome->'head'->>'checkout_revision')::bigint,f->>'path'
FROM checkout_publications p CROSS JOIN LATERAL jsonb_array_elements(p.outcome->'files') f
WHERE p.state='applied';
ALTER TABLE checkout_publications FORCE ROW LEVEL SECURITY;

ALTER TABLE checkout_change_paths ENABLE ROW LEVEL SECURITY;
ALTER TABLE checkout_change_paths FORCE ROW LEVEL SECURITY;
CREATE POLICY checkout_path_change_host ON checkout_change_paths FOR INSERT WITH CHECK (
    workspace_id=office_workspace_id() AND office_hosts_checkout(workspace_id,checkout_id)
    AND EXISTS (SELECT 1 FROM checkout_publications p WHERE (p.workspace_id,p.checkout_id,p.id)=
        (checkout_change_paths.workspace_id,checkout_change_paths.checkout_id,checkout_change_paths.change_id)
        AND p.host_runner_id=office_checkout_runner_id() AND p.state='applied'
        AND (p.outcome->'head'->>'checkout_revision')::bigint=checkout_change_paths.checkout_revision)
);
CREATE POLICY checkout_path_change_compensation ON checkout_change_paths FOR SELECT USING (
    workspace_id=office_workspace_id() AND checkout_id=office_gateway_checkout()
    AND office_gateway_run() IS NOT NULL AND nullif(current_setting('office.compensated_change_id',true),'') IS NOT NULL
);
CREATE POLICY checkout_change_compensation_source ON checkout_publications FOR SELECT USING (
    workspace_id=office_workspace_id() AND checkout_id=office_gateway_checkout() AND office_gateway_run() IS NOT NULL
    AND id=nullif(current_setting('office.compensated_change_id',true),'')::uuid AND state='applied'
    AND (executor_user_id,worker_agent_id)=(office_user_id(),office_agent_id())
);
