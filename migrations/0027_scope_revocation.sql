ALTER TABLE work_scope_revisions ADD COLUMN operation text NOT NULL DEFAULT 'apply' CHECK (operation IN ('apply', 'revoke'));
CREATE SEQUENCE checkout_authorization_order AS bigint;
ALTER TABLE work_scope_revisions ADD COLUMN authorization_order bigint NOT NULL DEFAULT nextval('checkout_authorization_order');
ALTER TABLE work_scope_revisions ADD COLUMN requires_new_grant boolean NOT NULL DEFAULT false
    CHECK (NOT requires_new_grant OR operation = 'revoke');
ALTER TABLE checkout_grants ADD COLUMN authorization_order bigint NOT NULL DEFAULT nextval('checkout_authorization_order');
DROP INDEX one_pending_work_scope;
CREATE UNIQUE INDEX one_pending_work_scope ON work_scope_revisions (workspace_id, scope_id, operation)
    WHERE state = 'pending_activation';

ALTER POLICY work_scope_revision_create ON work_scope_revisions WITH CHECK (
    workspace_id = office_workspace_id() AND office_checkout_runner_id() IS NULL
    AND state = 'pending_activation' AND operation = 'apply'
    AND EXISTS (SELECT 1 FROM checkout_grants WHERE workspace_id = work_scope_revisions.workspace_id
        AND id = grant_id AND office_owns_data(recipient_user_id, recipient_agent_id))
);

ALTER POLICY approved_work_scope_revision_create ON work_scope_revisions WITH CHECK (
    workspace_id = office_workspace_id() AND office_checkout_runner_id() IS NULL
    AND access_request_id = office_access_request_context() AND state = 'pending_activation' AND operation = 'apply'
    AND EXISTS (SELECT 1 FROM access_requests WHERE id = access_request_id
       AND workspace_id = work_scope_revisions.workspace_id
       AND scope_id = work_scope_revisions.scope_id AND checkout_id = work_scope_revisions.checkout_id
       AND state = 'requested')
);

CREATE POLICY work_scope_revocation_create ON work_scope_revisions FOR INSERT WITH CHECK (
    workspace_id = office_workspace_id() AND office_checkout_runner_id() IS NULL
    AND state = 'pending_activation' AND operation = 'revoke' AND access_request_id IS NULL
    AND scope_id = nullif(current_setting('office.scope_revocation_context', true), '')::uuid
);
