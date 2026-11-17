BEGIN;

UPDATE office_installation SET restore_pending=true, restored_backup_id=:'backup_id'::uuid
WHERE singleton;

WITH quarantined AS (
    UPDATE execution_jobs SET state='dead_letter', ended_at=clock_timestamp(),
        terminal_reason='restore_review_required', revision=revision+1, retry_not_before=NULL
    WHERE state IN ('queued','waiting_input')
    RETURNING id
), released AS (
    UPDATE execution_budget_reservations r SET released_at=clock_timestamp()
    FROM quarantined q WHERE r.job_id=q.id AND r.released_at IS NULL
    RETURNING r.budget_id,r.units
), totals AS (
    SELECT budget_id,sum(units) AS units FROM released GROUP BY budget_id
), updated AS (
    UPDATE execution_budgets b SET reserved_units=b.reserved_units-t.units
    FROM totals t WHERE b.id=t.budget_id RETURNING b.id
)
SELECT coalesce(jsonb_agg(id ORDER BY id),'[]'::jsonb) FROM quarantined;

DELETE FROM runner_execution_profiles;
DELETE FROM checkout_execution_presence;
UPDATE checkout_coordinator_sessions SET stopped_at=clock_timestamp() WHERE stopped_at IS NULL;
UPDATE checkout_snapshots SET expires_at=least(expires_at,clock_timestamp()) WHERE state='queued';
UPDATE checkout_releases SET expires_at=least(expires_at,clock_timestamp()) WHERE state='queued';

COMMIT;
