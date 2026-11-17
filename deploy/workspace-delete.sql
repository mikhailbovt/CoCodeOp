BEGIN;
SET LOCAL lock_timeout='5s';
SET LOCAL statement_timeout='120s';
SELECT set_config('office.remove_workspace', :'workspace', true) AS workspace \gset

CREATE TEMP TABLE workspace_tables ON COMMIT DROP AS
/* WORKSPACE TABLES */;
CREATE TEMP TABLE workspace_counts (rows_deleted bigint) ON COMMIT DROP;

DO $$
DECLARE
    wid uuid := current_setting('office.remove_workspace')::uuid;
    rel record;
    fk record;
    joins text;
    outside text;
    foreign_rows boolean;
    deleted bigint;
BEGIN
    -- Serialize the maintenance operation with every writer, including other operators.
    FOR rel IN SELECT name FROM workspace_tables ORDER BY name LOOP
        EXECUTE format('LOCK TABLE public.%I IN SHARE ROW EXCLUSIVE MODE', rel.name);
    END LOOP;
    IF NOT EXISTS (SELECT 1 FROM workspaces WHERE id=wid) THEN
        RAISE EXCEPTION 'The selected workspace does not exist';
    END IF;
    IF EXISTS (SELECT 1 FROM office_installation WHERE restore_pending)
        OR NOT EXISTS (SELECT 1 FROM execution_controls
                       WHERE workspace_id=wid AND kind='workspace' AND subject_id=wid AND mode='stopped')
        OR EXISTS (SELECT 1 FROM runs WHERE workspace_id=wid AND status IN ('running','cancel_requested','waiting_input'))
        OR EXISTS (SELECT 1 FROM execution_jobs WHERE workspace_id=wid AND state IN ('leased','reconcile_required'))
        OR EXISTS (SELECT 1 FROM checkout_coordinator_sessions
                   WHERE workspace_id=wid AND stopped_at IS NULL AND lease_until>clock_timestamp())
        OR EXISTS (SELECT 1 FROM checkout_content_heads
                   WHERE workspace_id=wid AND (blocked OR applying_change_id IS NOT NULL))
        OR EXISTS (SELECT 1 FROM checkout_releases WHERE workspace_id=wid AND state='applying')
        OR EXISTS (SELECT 1 FROM checkout_commands WHERE workspace_id=wid AND state IN ('authorized','running'))
    THEN
        RAISE EXCEPTION 'Stop the workspace and reconcile active execution, checkout and restore work before deletion';
    END IF;

    -- Inspect incoming foreign keys before any cascading delete can affect a different workspace.
    FOR fk IN
        SELECT c.*, src.key AS source_key, dst.key AS target_key, dst.name AS target_name
        FROM pg_constraint c
        JOIN workspace_tables dst ON dst.oid=c.confrelid
        LEFT JOIN workspace_tables src ON src.oid=c.conrelid
        WHERE c.contype='f'
    LOOP
        SELECT string_agg(format('src.%I=dst.%I', sa.attname, da.attname), ' AND ')
        INTO joins
        FROM generate_subscripts(fk.conkey,1) pos
        JOIN pg_attribute sa ON sa.attrelid=fk.conrelid AND sa.attnum=fk.conkey[pos]
        JOIN pg_attribute da ON da.attrelid=fk.confrelid AND da.attnum=fk.confkey[pos];
        outside := CASE WHEN fk.source_key IS NULL THEN 'true'
                        ELSE format('src.%I IS DISTINCT FROM $1', fk.source_key) END;
        EXECUTE format('SELECT EXISTS (SELECT 1 FROM %s src JOIN public.%I dst ON %s
                        WHERE dst.%I=$1 AND %s)',
                       fk.conrelid::regclass, fk.target_name, joins, fk.target_key, outside)
        INTO foreign_rows USING wid;
        IF foreign_rows THEN
            RAISE EXCEPTION 'Foreign rows outside the selected workspace reference its data';
        END IF;
    END LOOP;

    -- Defer internal NO ACTION constraints to delete the cyclic task and run graph atomically.
    -- Restore the original declarations in this same transaction.
    CREATE TEMP TABLE workspace_deferred ON COMMIT DROP AS
    SELECT c.conrelid,c.conname
    FROM pg_constraint c JOIN workspace_tables src ON c.conrelid=src.oid
    JOIN workspace_tables dst ON c.confrelid=dst.oid
    WHERE c.contype='f' AND NOT c.condeferrable AND c.confdeltype='a';
    FOR fk IN SELECT * FROM workspace_deferred LOOP
        EXECUTE format('ALTER TABLE %s ALTER CONSTRAINT %I DEFERRABLE INITIALLY IMMEDIATE', fk.conrelid::regclass, fk.conname);
    END LOOP;
    SET CONSTRAINTS ALL DEFERRED;

    FOR rel IN SELECT name,key FROM workspace_tables ORDER BY name LOOP
        EXECUTE format('DELETE FROM public.%I WHERE %I=$1', rel.name, rel.key) USING wid;
        GET DIAGNOSTICS deleted=ROW_COUNT;
        INSERT INTO workspace_counts VALUES (deleted);
    END LOOP;
    SET CONSTRAINTS ALL IMMEDIATE;
    FOR fk IN SELECT * FROM workspace_deferred LOOP
        EXECUTE format('ALTER TABLE %s ALTER CONSTRAINT %I NOT DEFERRABLE', fk.conrelid::regclass, fk.conname);
    END LOOP;
END $$;

SELECT jsonb_build_object('workspace_id', :'workspace'::uuid, 'rows_deleted', sum(rows_deleted))
FROM workspace_counts;
COMMIT;
