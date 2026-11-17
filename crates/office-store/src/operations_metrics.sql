WITH observation AS (
    SELECT statement_timestamp() AS observed_at,
           statement_timestamp() - make_interval(secs => $3) AS since
), gauges AS (
    SELECT
        count(*) FILTER (WHERE state = 'queued') AS queued_jobs,
        count(*) FILTER (WHERE state = 'reconcile_required') AS reconciling_jobs,
        min(enqueued_at) FILTER (WHERE state = 'queued') AS oldest_queued
    FROM execution_jobs WHERE workspace_id = $1 AND owner_user_id = $2
      AND state IN ('queued', 'reconcile_required')
), attempt_gauges AS (
    SELECT count(*) FILTER (WHERE status = 'running') AS running,
           count(*) FILTER (WHERE status = 'waiting_input') AS waiting
    FROM runs WHERE workspace_id = $1 AND executor_user_id = $2
      AND status IN ('running', 'waiting_input')
), first_attempts AS (
    SELECT extract(epoch FROM (r.started_at - j.enqueued_at)) * 1000 AS lag_ms
    FROM runs r JOIN execution_jobs j ON (j.workspace_id, j.id) = (r.workspace_id, r.managed_job_id)
    WHERE r.workspace_id = $1 AND r.executor_user_id = $2 AND j.owner_user_id = $2
      AND r.managed_job_id IS NOT NULL AND r.started_at >= (SELECT since FROM observation)
      AND NOT EXISTS (
          SELECT 1 FROM runs earlier WHERE earlier.workspace_id = $1
            AND earlier.managed_job_id = r.managed_job_id AND earlier.started_at < r.started_at
      )
), finished AS (
    SELECT backend_evidence->'usage' AS usage,
           backend_evidence->'cost' AS cost,
           backend_evidence->'cost'->>'quality' AS quality
    FROM runs WHERE workspace_id = $1 AND executor_user_id = $2
      AND managed_job_id IS NOT NULL AND ended_at >= (SELECT since FROM observation)
), delivery_lag AS (
    SELECT count(*) AS pending, min(e.created_at) AS oldest
    FROM events e
    WHERE e.workspace_id = $1 AND e.recipient_user_id = $2 AND e.recipient_agent_id IS NULL
      AND NOT EXISTS (
          SELECT 1 FROM inbox_receipts i WHERE (i.workspace_id, i.event_id, i.user_id) =
              (e.workspace_id, e.id, e.recipient_user_id) AND i.agent_id IS NULL
      )
), publication_latency AS (
    SELECT extract(epoch FROM (finalized_at - created_at)) * 1000 AS lag_ms
    FROM checkout_publications WHERE workspace_id = $1 AND executor_user_id = $2
      AND finalized_at >= (SELECT since FROM observation) AND state = 'applied'
), claim_latency AS (
    SELECT extract(epoch FROM (resolved.created_at - request.created_at)) * 1000 AS lag_ms
    FROM path_claim_handoffs h
    JOIN events resolved ON resolved.workspace_id = h.workspace_id AND resolved.scope_id = h.id
      AND resolved.event_type = 'path.handoff_resolved' AND resolved.payload->>'state' = 'accepted'
    JOIN events request ON request.workspace_id = h.workspace_id AND request.scope_id = h.id
      AND request.event_type = 'path.handoff_requested'
    WHERE h.workspace_id = $1 AND h.requesting_user_id = $2 AND h.state = 'accepted'
      AND resolved.created_at >= (SELECT since FROM observation)
)
SELECT jsonb_build_object(
    'workspace_id', $1::uuid,
    'observed_at', observed_at,
    'since', since,
    'executions', jsonb_build_object(
        'queued_jobs', gauges.queued_jobs,
        'running_attempts', attempt_gauges.running,
        'waiting_input_attempts', attempt_gauges.waiting,
        'blocked_tasks', (SELECT count(*) FROM tasks
            WHERE workspace_id = $1 AND accountable_user_id = $2 AND status = 'blocked'),
        'reconciling_jobs', gauges.reconciling_jobs,
        'oldest_dispatch_wait_ms', (extract(epoch FROM (observed_at - gauges.oldest_queued)) * 1000)::bigint,
        'first_dispatch', (
            SELECT jsonb_build_object('samples', count(*), 'p95_ms',
                percentile_cont(0.95) WITHIN GROUP (ORDER BY lag_ms)::bigint) FROM first_attempts
        ),
        'retries', (
            SELECT count(*) FROM events e
            JOIN execution_jobs j ON j.workspace_id = e.workspace_id AND j.id::text = e.payload->>'job_id'
            WHERE e.workspace_id = $1 AND j.owner_user_id = $2
              AND e.event_type = 'execution.requeued' AND e.created_at >= since
        )
    ),
    'usage', (
        SELECT jsonb_build_object(
            'finished_managed_attempts', count(*),
            'measured_attempts', count(*) FILTER (WHERE jsonb_typeof(usage) = 'object'),
            'input_tokens', coalesce(sum((usage->>'input_tokens')::numeric), 0)::text,
            'output_tokens', coalesce(sum((usage->>'output_tokens')::numeric), 0)::text,
            'cached_input_tokens', coalesce(sum((usage->>'cached_input_tokens')::numeric), 0)::text,
            'reasoning_output_tokens', coalesce(sum((usage->>'reasoning_output_tokens')::numeric), 0)::text,
            'reported_cost_attempts', count(*) FILTER (WHERE quality = 'reported'),
            'reported_usd_micros', coalesce(sum((cost->>'usd_micros')::numeric)
                FILTER (WHERE quality = 'reported'), 0)::text,
            'estimated_cost_attempts', count(*) FILTER (WHERE quality = 'estimated'),
            'estimated_usd_micros', coalesce(sum((cost->>'usd_micros')::numeric)
                FILTER (WHERE quality = 'estimated'), 0)::text,
            'unknown_cost_attempts', count(*) FILTER (WHERE quality IS NULL)
        ) FROM finished
    ),
    'checkout', jsonb_build_object(
        'reconciling_checkouts', (SELECT count(*) FROM checkout_content_heads WHERE workspace_id = $1 AND blocked),
        'newest_snapshot_age_ms', (SELECT (extract(epoch FROM (observed_at - max(created_at))) * 1000)::bigint
            FROM checkout_snapshots WHERE workspace_id = $1 AND state = 'ready'),
        'publication', (
            SELECT jsonb_build_object('samples', count(*), 'p95_ms',
                percentile_cont(0.95) WITHIN GROUP (ORDER BY lag_ms)::bigint) FROM publication_latency
        ),
        'claim_handoff', (
            SELECT jsonb_build_object('samples', count(*), 'p95_ms',
                percentile_cont(0.95) WITHIN GROUP (ORDER BY lag_ms)::bigint) FROM claim_latency
        )
    ),
    'inbox', jsonb_build_object(
        'pending_addressed', delivery_lag.pending,
        'oldest_pending_ms', (extract(epoch FROM (observed_at - delivery_lag.oldest)) * 1000)::bigint
    )
)
FROM observation CROSS JOIN gauges CROSS JOIN attempt_gauges CROSS JOIN delivery_lag;
