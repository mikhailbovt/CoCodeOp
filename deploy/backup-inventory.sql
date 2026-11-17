SELECT jsonb_build_object(
    'installation_id', installation_id,
    'schema_version', (SELECT max(version) FROM _sqlx_migrations),
    'restore_pending', restore_pending,
    'restored_backup_id', restored_backup_id,
    'workspaces_stopped', NOT EXISTS (
        SELECT 1 FROM workspaces w
        LEFT JOIN execution_controls c ON c.workspace_id=w.id AND c.kind='workspace' AND c.subject_id=w.id
        WHERE c.mode IS DISTINCT FROM 'stopped'
    ),
    'unresolved_executions', (SELECT count(*) FROM execution_jobs WHERE state IN ('leased','reconcile_required'))
        + (SELECT count(*) FROM runs WHERE managed_job_id IS NULL AND status IN ('running','waiting_input','cancel_requested')),
    'unresolved_checkouts', (SELECT count(*) FROM checkout_content_heads WHERE blocked OR applying_change_id IS NOT NULL)
        + (SELECT count(*) FROM checkout_releases WHERE state='applying')
        + (SELECT count(*) FROM checkout_commands WHERE state IN ('authorized','running')),
    'live_coordinators', (
        SELECT count(*) FROM checkout_coordinator_sessions
        WHERE stopped_at IS NULL AND lease_until>clock_timestamp()
    ),
    'checkouts', coalesce((
        SELECT jsonb_agg(jsonb_build_object(
            'checkout_id', c.id, 'workspace_id', c.workspace_id,
            'host_runner_id', c.host_runner_id, 'manifest', c.manifest,
            'root', jsonb_build_object('host_fingerprint', r.host_fingerprint, 'directory_id', r.directory_id,
                                        'ancestors', r.ancestor_ids),
            'head', jsonb_build_object('checkout_revision', coalesce(h.checkout_revision,0),
                                       'source_sha256', coalesce(h.source_sha256,c.baseline->>'manifest_sha256')),
            'state_id', h.state_id,
            'epoch', coalesce(latest.epoch,0),
            'ready_epoch', CASE WHEN latest.stopped_at IS NULL AND latest.ready_at IS NOT NULL
                                  AND latest.lease_until>clock_timestamp() THEN latest.epoch END,
            'ready_state_id', latest.publication_state_id,
            'accepted_commit', (SELECT outcome->>'commit_sha256' FROM checkout_releases a
                                  WHERE a.checkout_id=c.id AND a.state='accepted' ORDER BY a.checkout_revision DESC LIMIT 1),
            'snapshots', coalesce((SELECT jsonb_agg(jsonb_build_object(
                    'snapshot_id', s.id, 'checkout_revision', s.checkout_revision,
                    'source_sha256', s.source_sha256, 'proof', s.outcome->'proof'
                ) ORDER BY s.checkout_revision) FROM checkout_snapshots s WHERE s.checkout_id=c.id AND s.state='ready'), '[]'::jsonb)
        ) ORDER BY c.id)
        FROM project_checkouts c
        JOIN checkout_roots r ON r.id=c.root_ref
        LEFT JOIN checkout_content_heads h ON h.checkout_id=c.id
        LEFT JOIN LATERAL (
            SELECT epoch,stopped_at,ready_at,lease_until,publication_state_id FROM checkout_coordinator_sessions
            WHERE checkout_id=c.id ORDER BY epoch DESC LIMIT 1
        ) latest ON true
        WHERE c.state='registered'
    ), '[]'::jsonb)
) FROM office_installation WHERE singleton;
