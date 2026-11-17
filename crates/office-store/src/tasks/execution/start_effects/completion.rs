use office_domain::{BearerDigest, RunStatus, TaskStatus};
use office_protocol::{
    CODEX_EXEC_ADAPTER, CompleteExecution, ExecutionCompletionReceipt, ExecutionTerminalReason,
    OPENCODE_RUN_ADAPTER, ProcessResult, Recipient, Run, packaged_backend,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction};

use super::super::{
    activation::missing_activation_effect, managed_input, recovery, runs, select_managed_runtime,
    settle_budget,
};
use super::{StartEffectRow, StartTarget, publish, require_effect, validate_external_id};
use crate::{
    Store,
    data_scope::subject,
    events::append_workspace_event,
    identity::IdentityError,
    tasks::{results, task_text},
};

impl Store {
    pub async fn complete_execution(
        &self,
        credential: &BearerDigest,
        input: CompleteExecution,
    ) -> Result<ExecutionCompletionReceipt, IdentityError> {
        validate_external_id(&input.external_id)?;
        let encoded = serde_json::to_vec(&input).map_err(|_| IdentityError::InvalidInput)?;
        let hash = hex::encode(Sha256::digest(encoded));
        let mut tx = self.pool.begin().await?;
        let target = StartTarget::load(&mut tx, credential, &input.effect.lease).await?;
        let previous = require_effect(&mut tx, &target.run, input.effect.effect_id).await?;
        if let Some(key) = previous.completion_key {
            if key != input.idempotency_key || previous.completion_sha256.as_deref() != Some(&hash)
            {
                return Err(IdentityError::IdempotencyConflict);
            }
            let receipt = previous.completion(target.run)?;
            tx.commit().await?;
            return Ok(receipt);
        }
        target.verify_live(&mut tx).await?;
        if target.run.status != RunStatus::Running
            || previous.state != "started"
            || previous
                .runner_credential_id
                .is_some_and(|id| id != target.runner.credential_id.0)
            || previous.external_id.as_deref() != Some(&input.external_id)
            || (!target.job.gateway_required
                && !target.verifies_approval(&previous, input.effect.lease.run_fence))
        {
            return Err(IdentityError::RunLeaseStale);
        }
        let (declared, requested_detail) = outcome(&input.result, input.exit_code)?;
        let runtime = select_managed_runtime(
            &mut tx,
            target.run.workspace_id,
            target.run.executor.user_id,
            target.job.runtime_template_id,
        )
        .await?;
        if input.backend.as_ref().is_some_and(|evidence| {
            !evidence.valid()
                || evidence.backend != runtime.adapter_id
                || !input.result.matches_backend_state(evidence.terminal_state)
        }) || (declared == RunStatus::Succeeded
            && packaged_backend(&runtime.adapter_id).is_some()
            && input.backend.is_none())
            || (declared == RunStatus::Paused && input.backend.is_none())
        {
            return Err(IdentityError::InvalidInput);
        }
        let intent = if matches!(
            runtime.adapter_id.as_str(),
            CODEX_EXEC_ADAPTER | OPENCODE_RUN_ADAPTER
        ) && matches!(declared, RunStatus::Paused | RunStatus::Succeeded)
        {
            let intent = managed_input::recorded_intent(&mut tx, &target.run).await?;
            if declared == RunStatus::Paused && intent.as_deref() != Some(requested_detail) {
                return Err(IdentityError::InvalidInput);
            }
            intent
        } else {
            None
        };
        let result = if declared == RunStatus::Succeeded && intent.is_some() {
            ProcessResult::Failed {
                reason: "The model requested input but the controller did not verify a checkpoint"
                    .into(),
            }
        } else if declared == RunStatus::Succeeded
            && let Some(reason) = missing_activation_effect(&mut tx, &target.run).await?
        {
            ProcessResult::Failed {
                reason: reason.into(),
            }
        } else if declared == RunStatus::Paused
            && !managed_input::can_pause(
                &mut tx,
                target.job.workspace_id,
                target.job.task_id,
                requested_detail,
            )
            .await?
        {
            ProcessResult::Failed {
                reason:
                    "The question was already answered or the task reached its input wait limit"
                        .into(),
            }
        } else {
            input.result
        };
        let (status, detail) = outcome(&result, input.exit_code)?;
        let run =
            runs::finish_managed(&mut tx, &target.run, status, detail, input.backend.as_ref())
                .await?;
        let task_revision = apply_completion(&mut tx, &target, &run, detail, &result).await?;
        let effect = sqlx::query_as::<_, StartEffectRow>(
            "UPDATE execution_start_effects SET state='stopped',reconciled_at=clock_timestamp(),
                 completion_key=$2,completion_sha256=$3,process_exit_code=$4,result_task_revision=$5
             WHERE id=$1 AND state='started' RETURNING *",
        )
        .bind(previous.id)
        .bind(input.idempotency_key)
        .bind(hash)
        .bind(i16::from(input.exit_code))
        .bind(task_revision)
        .fetch_one(&mut *tx)
        .await?;
        publish(&mut tx, &run, &effect, "execution.completed").await?;
        let receipt = effect.completion(run)?;
        tx.commit().await?;
        Ok(receipt)
    }
}

fn outcome(result: &ProcessResult, exit_code: u8) -> Result<(RunStatus, &str), IdentityError> {
    match result {
        ProcessResult::Succeeded { summary, .. } if exit_code == 0 => {
            Ok((RunStatus::Succeeded, task_text(summary, 4096)?))
        }
        ProcessResult::Succeeded { .. } => Err(IdentityError::InvalidInput),
        ProcessResult::Failed { reason } => Ok((RunStatus::Failed, task_text(reason, 4096)?)),
        ProcessResult::WaitingInput { question } if exit_code == 0 => {
            Ok((RunStatus::Paused, task_text(question, 3000)?))
        }
        ProcessResult::WaitingInput { .. } => Err(IdentityError::InvalidInput),
    }
}

impl StartEffectRow {
    fn completion(self, run: Run) -> Result<ExecutionCompletionReceipt, IdentityError> {
        let exit_code = self
            .process_exit_code
            .and_then(|code| u8::try_from(code).ok())
            .ok_or(IdentityError::InvalidRole)?;
        let task_revision = self
            .result_task_revision
            .ok_or(IdentityError::InvalidRole)?;
        Ok(ExecutionCompletionReceipt {
            execution: self.receipt(run)?,
            exit_code,
            task_revision,
        })
    }
}

async fn apply_completion(
    tx: &mut Transaction<'_, Postgres>,
    target: &StartTarget,
    run: &Run,
    detail: &str,
    result: &ProcessResult,
) -> Result<i64, IdentityError> {
    let job = &target.job;
    let (task_status, event) = match result {
        ProcessResult::Succeeded { report, .. } => {
            sqlx::query("SELECT set_config('office.managed_result_run',$1,true)")
                .bind(run.run_id.0.to_string())
                .execute(&mut **tx)
                .await?;
            results::capture(tx, &target.task, run, detail, report.as_ref()).await?;
            let updated = sqlx::query("UPDATE execution_jobs SET state='completed',ended_at=clock_timestamp(),revision=revision+1
                WHERE workspace_id=$1 AND id=$2 AND state='leased' AND current_run_id=$3")
                .bind(job.workspace_id).bind(job.id).bind(run.run_id.0).execute(&mut **tx).await?;
            if updated.rows_affected() != 1 {
                return Err(IdentityError::RunLeaseStale);
            }
            settle_budget(tx, job, job.reserved_units).await?;
            (TaskStatus::InReview, "task.result_submitted")
        }
        ProcessResult::Failed { .. } => {
            recovery::close_failed(
                tx,
                job,
                ExecutionTerminalReason::ExecutionFailed,
                job.reserved_units,
            )
            .await?;
            (TaskStatus::Failed, "task.failed")
        }
        ProcessResult::WaitingInput { .. } => {
            managed_input::begin_pause(tx, &target.task, job, run, detail).await?;
            (TaskStatus::Blocked, "task.input_requested")
        }
    };
    let revision: i64 = sqlx::query_scalar(
        "UPDATE tasks SET status=$3,revision=revision+1,updated_at=clock_timestamp()
         WHERE workspace_id=$1 AND id=$2 AND status='in_progress' RETURNING revision",
    )
    .bind(job.workspace_id)
    .bind(job.task_id)
    .bind(task_status.as_str())
    .fetch_one(&mut **tx)
    .await?;
    if task_status == TaskStatus::InReview {
        results::notify_reviewer(tx, &target.task, target.context.recipient(), run, revision)
            .await?;
    }
    let actor = subject(
        run.executor.user_id,
        run.executor.agent_id.map(|item| item.0),
    );
    let address = (event == "task.input_requested").then_some((
        target.context.recipient(),
        Recipient {
            user_id: office_domain::UserId(target.task.accountable_user_id),
            agent_id: target.task.accountable_agent_id.map(office_domain::AgentId),
        },
    ));
    append_workspace_event(
        tx,
        run.workspace_id,
        run.task_id.0,
        event,
        json!({"task_id":run.task_id,"status":task_status,"revision":revision,"actor":actor,
               "run_id":run.run_id,"detail":detail}),
        address,
    )
    .await?;
    Ok(revision)
}
