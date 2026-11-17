ALTER TABLE decisions ADD UNIQUE (workspace_id, checkout_id, id);
ALTER TABLE access_requests ADD COLUMN decision_id uuid;
ALTER TABLE access_requests ADD CONSTRAINT access_decision_in_checkout
    FOREIGN KEY (workspace_id, checkout_id, decision_id) REFERENCES decisions (workspace_id, checkout_id, id);
CREATE INDEX access_decisions_by_decision ON access_requests (workspace_id, decision_id) WHERE state = 'approved';

CREATE POLICY decision_read_by_approved_access ON decisions FOR SELECT USING (
    workspace_id = office_workspace_id() AND EXISTS (
        SELECT 1 FROM access_requests r WHERE r.workspace_id = decisions.workspace_id
        AND r.decision_id = decisions.id AND r.state = 'approved'
    )
);
