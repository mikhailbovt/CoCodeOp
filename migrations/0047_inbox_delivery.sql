ALTER TABLE inbox_receipts ADD COLUMN delivered_at timestamptz;
ALTER TABLE inbox_receipts NO FORCE ROW LEVEL SECURITY;
UPDATE inbox_receipts SET delivered_at = acknowledged_at;
ALTER TABLE inbox_receipts FORCE ROW LEVEL SECURITY;
ALTER TABLE inbox_receipts ALTER COLUMN delivered_at SET DEFAULT clock_timestamp();
ALTER TABLE inbox_receipts ALTER COLUMN delivered_at SET NOT NULL;
ALTER TABLE inbox_receipts ALTER COLUMN acknowledged_at DROP DEFAULT;
ALTER TABLE inbox_receipts ALTER COLUMN acknowledged_at DROP NOT NULL;
ALTER TABLE inbox_receipts ADD CHECK (acknowledged_at IS NULL OR acknowledged_at >= delivered_at);

CREATE POLICY inbox_sender_delivery ON inbox_receipts FOR SELECT USING (
    workspace_id = office_workspace_id() AND EXISTS (
        SELECT 1 FROM events e WHERE (e.workspace_id,e.id) = (inbox_receipts.workspace_id,inbox_receipts.event_id)
        AND (e.sender_user_id,e.sender_agent_id) IS NOT DISTINCT FROM (office_user_id(),office_agent_id())
        AND (e.recipient_user_id,e.recipient_agent_id) IS NOT DISTINCT FROM (inbox_receipts.user_id,inbox_receipts.agent_id)
    )
);
