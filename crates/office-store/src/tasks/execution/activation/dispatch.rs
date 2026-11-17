use office_domain::{AgentId, GrantRight, TaskId, Visibility};
use office_protocol::{
    AclGrant, ClaimExecution, ManagedActivation, QueueExecution, Recipient, RunnerIdentity,
    RunnerProfile,
};
use serde_json::json;
use sqlx::{Acquire, Postgres, Transaction, types::Json};
use uuid::Uuid;

use super::super::{JobRow, TaskRow, bind_job, bind_task, enqueue_job, leases, publish};
use crate::{
    data_scope::create_scope,
    events::append_event,
    identity::{IdentityError, WorkspaceContext, active_principal, bind_subject},
};

#[derive(sqlx::FromRow)]
struct SchedulePolicy {
    agent_id: Uuid,
    revision: i64,
    managed: Json<ManagedActivation>,
    start_after_sequence: i64,
}

#[derive(sqlx::FromRow)]
struct SourceEvent {
    id: Uuid,
    scope_id: Uuid,
    event_type: String,
    thread_id: Uuid,
    ready: bool,
    task_revision: Option<i64>,
}

pub(in crate::tasks::execution) async fn admit(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    runner: &RunnerIdentity,
    profile: &RunnerProfile,
    claim: &ClaimExecution,
) -> Result<bool, IdentityError> {
    let policies = sqlx::query_as::<_, SchedulePolicy>(
        "SELECT p.agent_id,p.revision,p.managed,p.start_after_sequence FROM activation_policies p
         JOIN agents a ON (a.workspace_id,a.id,a.owner_user_id)=(p.workspace_id,p.agent_id,p.owner_user_id)
         WHERE p.workspace_id=$1 AND p.owner_user_id=$2 AND a.disabled_at IS NULL
           AND p.managed->>'runner_id'=$3 AND (p.managed->>'runtime_template_id')::uuid=ANY($4)
         ORDER BY (SELECT max(x.created_at) FROM activated_executions x WHERE x.workspace_id=p.workspace_id AND x.agent_id=p.agent_id)
           NULLS FIRST,p.agent_id FOR SHARE OF p",
    ).bind(runner.workspace_id.0).bind(runner.owner_user_id.0).bind(runner.runner_id.0.to_string())
     .bind(&profile.runtime_template_ids).fetch_all(&mut **tx).await?;
    for policy in policies {
        let agent = Recipient {
            user_id: runner.owner_user_id,
            agent_id: Some(AgentId(policy.agent_id)),
        };
        active_principal(tx, context, agent).await?;
        if !policy_idle(tx, runner, &policy).await? {
            continue;
        }
        bind_subject(tx, agent).await?;
        let sources = source_events(tx, runner, &policy).await?;
        bind_subject(tx, context.recipient()).await?;
        if sources.is_empty() {
            continue;
        }
        let mut admission = tx.begin().await?;
        match queue_turn(&mut admission, context, &policy, &sources, claim).await {
            Ok(()) => {
                admission.commit().await?;
                return Ok(true);
            }
            Err(
                IdentityError::BudgetExhausted
                | IdentityError::ExecutionPaused
                | IdentityError::ExecutionCapacityReached
                | IdentityError::AccessRequestRequired
                | IdentityError::Forbidden
                | IdentityError::InvalidTransition
                | IdentityError::DependencyNotReady
                | IdentityError::DelegationInactive
                | IdentityError::DelegationLimit,
            ) => {
                admission.rollback().await?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

async fn policy_idle(
    tx: &mut Transaction<'_, Postgres>,
    runner: &RunnerIdentity,
    policy: &SchedulePolicy,
) -> Result<bool, IdentityError> {
    let active = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM execution_jobs j JOIN activated_executions a
            ON (a.workspace_id,a.job_id)=(j.workspace_id,office_activation_job(j.workspace_id,j.id))
          WHERE j.workspace_id=$1 AND j.worker_agent_id=$2 AND (j.state IN ('queued','leased','reconcile_required')
              OR j.ended_at>clock_timestamp()-make_interval(secs=>$3::integer)))
          OR (SELECT count(*) FROM activated_executions a WHERE a.workspace_id=$1 AND a.agent_id=$2
            AND a.created_at>clock_timestamp()-interval '1 hour') >= $4",
    ).bind(runner.workspace_id.0).bind(policy.agent_id).bind(i32::from(policy.managed.cooldown_seconds))
     .bind(i64::from(policy.managed.max_turns_per_hour)).fetch_one(&mut **tx).await?;
    Ok(!active)
}

async fn source_events(
    tx: &mut Transaction<'_, Postgres>,
    runner: &RunnerIdentity,
    policy: &SchedulePolicy,
) -> Result<Vec<SourceEvent>, IdentityError> {
    let types: Vec<_> = policy
        .managed
        .triggers
        .iter()
        .map(|trigger| trigger.event_type())
        .collect();
    let mut rows = sqlx::query_as::<_, SourceEvent>(
        "SELECT e.id,e.scope_id,e.event_type,thread.id AS thread_id,
          e.created_at<=clock_timestamp()-make_interval(secs=>$6::integer) AS ready,
          (e.payload->>'revision')::bigint AS task_revision FROM events e
        CROSS JOIN LATERAL (SELECT CASE WHEN e.event_type IN ('message.question','message.answer')
          THEN (e.payload->>'thread_id')::uuid ELSE e.scope_id END AS id) thread
        WHERE e.workspace_id=$1 AND (e.recipient_user_id,e.recipient_agent_id)=($2,$3)
          AND e.sequence>$4 AND e.event_type=ANY($5)
          AND e.created_at>clock_timestamp()-interval '1 day'
          AND NOT EXISTS(SELECT 1 FROM activation_events seen WHERE (seen.workspace_id,seen.event_id)=(e.workspace_id,e.id))
          AND (SELECT count(*) FROM activated_executions a WHERE a.workspace_id=e.workspace_id AND a.agent_id=$3
            AND a.thread_id=thread.id)<$7
          AND CASE e.event_type
            WHEN 'message.question' THEN EXISTS(SELECT 1 FROM thread_questions q WHERE q.workspace_id=e.workspace_id
              AND q.question_event_id=e.id AND q.answer_id IS NULL AND q.expires_at>clock_timestamp())
            WHEN 'message.answer' THEN EXISTS(SELECT 1 FROM thread_questions q WHERE q.workspace_id=e.workspace_id AND q.answer_event_id=e.id)
            WHEN 'task.assignment_requested' THEN EXISTS(SELECT 1 FROM tasks t WHERE (t.workspace_id,t.id)=(e.workspace_id,e.scope_id)
              AND t.assignee_user_id=$2 AND t.assignee_agent_id=$3 AND t.accountable_user_id=$2 AND t.status='assigned'
              AND NOT t.awaiting_owner AND t.revision=(e.payload->>'revision')::bigint
              AND NOT EXISTS(SELECT 1 FROM execution_jobs j WHERE (j.workspace_id,j.task_id)=(t.workspace_id,t.id)
                  AND j.state IN ('queued','leased','reconcile_required')))
            WHEN 'task.review_requested' THEN EXISTS(SELECT 1 FROM tasks t WHERE (t.workspace_id,t.id)=(e.workspace_id,e.scope_id)
              AND t.status='in_review' AND t.revision=(e.payload->>'revision')::bigint)
            ELSE false END
        ORDER BY e.sequence LIMIT 16",
    ).bind(runner.workspace_id.0).bind(runner.owner_user_id.0).bind(policy.agent_id).bind(policy.start_after_sequence)
     .bind(types).bind(i32::from(policy.managed.debounce_seconds)).bind(i64::from(policy.managed.max_turns_per_thread))
     .fetch_all(&mut **tx).await?;
    if let Some(first) = rows.iter().find(|source| source.ready) {
        let thread = first.thread_id;
        let event_type = first.event_type.clone();
        rows.retain(|source| source.thread_id == thread && source.event_type == event_type);
        rows.truncate(if event_type.starts_with("message.") {
            4
        } else {
            1
        });
    } else {
        rows.clear();
    }
    Ok(rows)
}

async fn queue_turn(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    policy: &SchedulePolicy,
    sources: &[SourceEvent],
    claim: &ClaimExecution,
) -> Result<(), IdentityError> {
    let first = &sources[0];
    let workspace = context.access.workspace;
    let template = policy.managed.runtime_template_id;
    let assignment = first.event_type == "task.assignment_requested";
    let task = if assignment {
        bind_task(tx, Some(TaskId(first.scope_id))).await?;
        let task = leases::lock_task(tx, workspace, first.scope_id)
            .await?
            .ok_or(IdentityError::InvalidTransition)?;
        let checkout = task.checkout_id;
        let gateway = claim
            .checkout_gateway_templates
            .as_ref()
            .is_some_and(|templates| templates.contains(&template));
        if task.status != "assigned"
            || first.task_revision != Some(task.revision)
            || checkout.is_some()
                && (claim.analysis_only
                    || !gateway
                    || !leases::checkout_live(tx, workspace, checkout, true).await?)
        {
            return Err(IdentityError::InvalidTransition);
        }
        task
    } else {
        coordination_task(tx, context, policy, &first.event_type).await?
    };
    let job_id = Uuid::new_v4();
    sqlx::query("SELECT set_config('office.activation_job_context',$1,true)")
        .bind(job_id.to_string())
        .execute(&mut **tx)
        .await?;
    let row = enqueue_job(
        tx,
        context,
        &task,
        &QueueExecution {
            workspace_id: workspace,
            task_id: TaskId(task.id),
            expected_task_revision: task.revision,
            worker_agent_id: AgentId(policy.agent_id),
            runtime_template_id: template,
            idempotency_key: job_id,
        },
        job_id,
        !assignment,
    )
    .await?;
    record_activation(tx, context, policy, first, sources, row).await
}

async fn coordination_task(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    policy: &SchedulePolicy,
    trigger: &str,
) -> Result<TaskRow, IdentityError> {
    let id = Uuid::new_v4();
    create_scope(
        tx,
        context,
        id,
        Visibility::ExplicitAcl,
        &[AclGrant {
            user_id: context.access.member,
            agent_id: Some(AgentId(policy.agent_id)),
            right: GrantRight::Read,
        }],
    )
    .await?;
    let objective = match trigger {
        "message.question" => {
            "Read the activation references and answer the addressed questions in their original thread."
        }
        "message.answer" => {
            "Read the addressed answers to your questions and summarize the follow-up needed."
        }
        "task.review_requested" => {
            "Read the activation reference and review the submitted task result against its criteria."
        }
        _ => return Err(IdentityError::InvalidInput),
    };
    let task = sqlx::query_as::<_, TaskRow>(
        "INSERT INTO tasks (id,workspace_id,accountable_user_id,assignee_user_id,assignee_agent_id,objective,criteria,status,execution_kind)
         VALUES ($1,$2,$3,$3,$4,$5,$6,'assigned','coordination') RETURNING *",
    ).bind(id).bind(context.access.workspace.0).bind(context.access.member.0).bind(policy.agent_id)
     .bind(objective).bind(Json(vec!["Record the result against the referenced event."])).fetch_one(&mut **tx).await?;
    Ok(task)
}

async fn record_activation(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    policy: &SchedulePolicy,
    first: &SourceEvent,
    sources: &[SourceEvent],
    job: JobRow,
) -> Result<(), IdentityError> {
    if first.event_type != "task.assigned" {
        append_event(
            tx,
            context,
            job.task_id,
            "task.created",
            json!({"task_id":job.task_id,"status":"assigned","revision":job.task_revision}),
            None,
        )
        .await?;
    }
    bind_job(tx, Some(job.id)).await?;
    sqlx::query("INSERT INTO activated_executions (job_id,workspace_id,owner_user_id,agent_id,runner_id,policy_revision,thread_id,trigger)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8)")
        .bind(job.id).bind(job.workspace_id).bind(job.owner_user_id).bind(policy.agent_id).bind(policy.managed.runner_id.0)
        .bind(policy.revision).bind(first.thread_id).bind(&first.event_type).execute(&mut **tx).await?;
    let events: Vec<_> = sources.iter().map(|event| event.id).collect();
    sqlx::query(
        "INSERT INTO activation_events (workspace_id,event_id,job_id)
                 SELECT $1,event,$2 FROM unnest($3::uuid[]) AS events(event)",
    )
    .bind(job.workspace_id)
    .bind(job.id)
    .bind(&events)
    .execute(&mut **tx)
    .await?;
    bind_job(tx, None).await?;
    sqlx::query("SELECT set_config('office.activation_job_context','',true)")
        .execute(&mut **tx)
        .await?;
    publish(tx, context, &job.into_job()?, "execution.queued", None).await?;
    Ok(())
}

pub(in crate::tasks::execution) async fn validate_activation(
    tx: &mut Transaction<'_, Postgres>,
    job: &JobRow,
) -> Result<bool, IdentityError> {
    let row = sqlx::query_as::<_, (bool, bool)>(
        "SELECT a.trigger<>'task.assignment_requested',coalesce(p.revision=a.policy_revision
           AND a.runner_id=office_checkout_runner_id() AND p.managed->>'runner_id'=a.runner_id::text,false)
         FROM activated_executions a JOIN activation_policies p ON (p.workspace_id,p.agent_id)=(a.workspace_id,a.agent_id)
         WHERE (a.workspace_id,a.job_id)=($1,office_activation_job($1,$2)) FOR SHARE OF p",
    ).bind(job.workspace_id).bind(job.id).fetch_optional(&mut **tx).await?;
    match row {
        None => Ok(false),
        Some((coordination, true)) => Ok(coordination),
        Some((_, false)) => Err(IdentityError::Forbidden),
    }
}
