use chrono::{DateTime, Duration, Utc};
use office_domain::{Actor, AgentId, BearerDigest, GrantRight, UserId, Visibility, WorkspaceId};
use office_protocol::{
    AclGrant, Area, AreaDecision, AreaPolicy, Decision, GetAreaDecision, GetDecision, GetProposal,
    Proposal, ProposalContent, ProposalDecision, ProposalOption, ProposalReceipt, ProposalResponse,
    ProposalState, ProposalTarget, ProposeDecision, Recipient, RespondProposal, ReviseProposal,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction, types::Json};
use uuid::Uuid;

use crate::{
    Store,
    checkouts::{areas::policy_area, lock_policy},
    data_scope::create_scope,
    events::{Idempotency, append_event},
    identity::{IdentityError, WorkspaceContext, active_principal, valid_name, workspace_context},
    inbox::save_cursor,
    tasks::task_text,
};

const MAX_ROUNDS: i64 = 3;

#[derive(Clone, Copy)]
struct AreaBinding {
    checkout: Uuid,
    id: Uuid,
    revision: i64,
    base: i64,
}

#[derive(sqlx::FromRow)]
struct ProposalRow {
    workspace_id: Uuid,
    proposal_id: Uuid,
    revision: i64,
    owner_user_id: Uuid,
    author_agent_id: Option<Uuid>,
    topic: String,
    checkout_id: Option<Uuid>,
    area_id: Option<Uuid>,
    area_revision: Option<i64>,
    base_generation: Option<i64>,
    required_approvers: Json<Vec<Recipient>>,
    content: Json<ProposalContent>,
    deadline: DateTime<Utc>,
    state: String,
}

impl ProposalRow {
    fn binding(&self) -> Option<AreaBinding> {
        Some(AreaBinding {
            checkout: self.checkout_id?,
            id: self.area_id?,
            revision: self.area_revision?,
            base: self.base_generation?,
        })
    }

    fn author(&self) -> Recipient {
        Recipient {
            user_id: UserId(self.owner_user_id),
            agent_id: self.author_agent_id.map(AgentId),
        }
    }

    fn public_state(&self, now: DateTime<Utc>) -> Result<ProposalState, IdentityError> {
        Ok(match self.state.as_str() {
            "open" | "changes_requested" if self.deadline <= now => ProposalState::Expired,
            "open" => ProposalState::Open,
            "changes_requested" => ProposalState::ChangesRequested,
            "accepted" => ProposalState::Accepted,
            "rejected" => ProposalState::Rejected,
            "superseded" => ProposalState::Superseded,
            "escalated" => ProposalState::Escalated,
            _ => return Err(IdentityError::InvalidRole),
        })
    }

    fn target(&self) -> ProposalTarget {
        match self.binding() {
            Some(area) => ProposalTarget::SharedArea {
                checkout_id: area.checkout,
                area_id: area.id,
                expected_area_revision: area.revision,
                expected_generation: area.base,
            },
            None => ProposalTarget::Workspace {
                topic: self.topic.clone(),
                required_approvers: self.required_approvers.0.clone(),
            },
        }
    }
}

#[derive(sqlx::FromRow)]
struct DecisionRow {
    id: Uuid,
    workspace_id: Uuid,
    proposal_id: Uuid,
    proposal_revision: i64,
    checkout_id: Option<Uuid>,
    area_id: Option<Uuid>,
    area_revision: Option<i64>,
    generation: Option<i64>,
    topic: String,
    selected_option: Json<ProposalOption>,
    content_sha256: String,
    accepted_at: DateTime<Utc>,
}

impl From<DecisionRow> for Decision {
    fn from(row: DecisionRow) -> Self {
        Self {
            decision_id: row.id,
            workspace_id: WorkspaceId(row.workspace_id),
            proposal_id: row.proposal_id,
            proposal_revision: row.proposal_revision,
            topic: row.topic,
            checkout_id: row.checkout_id,
            area_id: row.area_id,
            area_revision: row.area_revision,
            generation: row.generation,
            selected_option: row.selected_option.0,
            content_sha256: row.content_sha256,
            accepted_at: row.accepted_at,
        }
    }
}

impl Store {
    pub async fn propose_decision(
        &self,
        digest: &BearerDigest,
        input: ProposeDecision,
    ) -> Result<ProposalReceipt, IdentityError> {
        let content = validated_content(&input.content)?;
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "proposal.open",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        let (topic, people, binding, checkout_owner) = match input.target {
            ProposalTarget::Workspace {
                topic,
                required_approvers,
            } => (
                valid_name(&topic)?.to_owned(),
                required_approvers,
                None,
                None,
            ),
            ProposalTarget::SharedArea {
                checkout_id,
                area_id,
                expected_area_revision,
                expected_generation,
            } => {
                lock_policy(&mut tx, checkout_id).await?;
                let area = shared_area(&mut tx, context, checkout_id, area_id).await?;
                require_revision(expected_area_revision, area.revision)?;
                let tip = area_decision(&mut tx, input.workspace_id, area_id).await?;
                let base = tip.and_then(|row| row.generation).unwrap_or(0);
                require_revision(expected_generation, base)?;
                let binding = Some(AreaBinding {
                    checkout: checkout_id,
                    id: area_id,
                    revision: area.revision,
                    base,
                });
                (
                    area.name,
                    area.policy.approvers().to_vec(),
                    binding,
                    Some(area.owner_user_id),
                )
            }
        };
        verify_approvers(&mut tx, context, &people, binding.is_some()).await?;
        let now = clock(&mut tx).await?;
        verify_deadline(now, input.deadline)?;
        let id = Uuid::new_v4();
        let mut readers = people.clone();
        if let Some(owner) = checkout_owner {
            let person = Recipient {
                user_id: owner,
                agent_id: None,
            };
            if !readers.contains(&person) {
                readers.push(person);
            }
        }
        let grants: Vec<_> = readers
            .into_iter()
            .filter(|person| *person != context.recipient())
            .map(|person| AclGrant {
                user_id: person.user_id,
                agent_id: person.agent_id,
                right: GrantRight::Read,
            })
            .collect();
        create_scope(&mut tx, context, id, Visibility::ExplicitAcl, &grants).await?;
        let row = insert_revision(
            &mut tx,
            ProposalRow {
                workspace_id: input.workspace_id.0,
                proposal_id: id,
                revision: 1,
                owner_user_id: context.access.member.0,
                author_agent_id: context.agent_id().map(|id| id.0),
                topic,
                checkout_id: binding.map(|value| value.checkout),
                area_id: binding.map(|value| value.id),
                area_revision: binding.map(|value| value.revision),
                base_generation: binding.map(|value| value.base),
                required_approvers: Json(people),
                content: Json(content),
                deadline: input.deadline,
                state: "open".into(),
            },
        )
        .await?;
        let receipt = record(&mut tx, context, &idem, row, None, "proposal.opened").await?;
        tx.commit().await?;
        Ok(receipt)
    }

    pub async fn revise_proposal(
        &self,
        digest: &BearerDigest,
        input: ReviseProposal,
    ) -> Result<ProposalReceipt, IdentityError> {
        let content = validated_content(&input.content)?;
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "proposal.revise",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        let mut row = lock_proposal(&mut tx, context, input.proposal_id).await?;
        if context.recipient() != row.author()
            && context.actor != Actor::Human(UserId(row.owner_user_id))
        {
            return Err(IdentityError::Forbidden);
        }
        require_revision(input.expected_revision, row.revision)?;
        verify_bound_area(&mut tx, context, row.binding()).await?;
        let now = clock(&mut tx).await?;
        if !matches!(
            row.public_state(now)?,
            ProposalState::Open | ProposalState::ChangesRequested
        ) || row.revision >= MAX_ROUNDS
        {
            return Err(IdentityError::InvalidTransition);
        }
        verify_deadline(now, input.deadline)?;
        if row.state == "open" {
            update_state(&mut tx, &row, "superseded").await?;
        }
        row.revision += 1;
        row.content = Json(content);
        row.deadline = input.deadline;
        let row = insert_revision(&mut tx, row).await?;
        let receipt = record(&mut tx, context, &idem, row, None, "proposal.revised").await?;
        tx.commit().await?;
        Ok(receipt)
    }

    pub async fn respond_proposal(
        &self,
        digest: &BearerDigest,
        input: RespondProposal,
    ) -> Result<ProposalReceipt, IdentityError> {
        let reason = task_text(&input.reason, 2048)?;
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "proposal.respond",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        let mut row = lock_proposal(&mut tx, context, input.proposal_id).await?;
        if !row.required_approvers.0.contains(&context.recipient()) {
            return Err(IdentityError::Forbidden);
        }
        require_revision(input.expected_revision, row.revision)?;
        if row.state != "open" {
            return Err(IdentityError::InvalidTransition);
        }
        verify_bound_area(&mut tx, context, row.binding()).await?;
        verify_approvers(
            &mut tx,
            context,
            &row.required_approvers.0,
            row.area_id.is_some(),
        )
        .await?;
        if row.deadline <= clock(&mut tx).await? {
            return Err(IdentityError::InvalidTransition);
        }
        let inserted = sqlx::query("INSERT INTO proposal_responses
           (workspace_id,proposal_id,proposal_revision,responder_user_id,responder_agent_id,decision,reason)
           VALUES ($1,$2,$3,$4,$5,$6,$7) ON CONFLICT DO NOTHING")
            .bind(input.workspace_id.0).bind(row.proposal_id).bind(row.revision)
            .bind(context.access.member.0).bind(context.agent_id().map(|id| id.0))
            .bind(decision_name(input.decision)).bind(reason).execute(&mut *tx).await?;
        if inserted.rows_affected() != 1 {
            return Err(IdentityError::InvalidTransition);
        }
        let state = match input.decision {
            ProposalDecision::Approve => {
                let count = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM proposal_responses
                    WHERE workspace_id=$1 AND proposal_id=$2 AND proposal_revision=$3 AND decision='approve'")
                    .bind(row.workspace_id).bind(row.proposal_id).bind(row.revision).fetch_one(&mut *tx).await?;
                if count == row.required_approvers.0.len() as i64 {
                    "accepted"
                } else {
                    "open"
                }
            }
            ProposalDecision::Reject => "rejected",
            ProposalDecision::RequestChanges if row.revision == MAX_ROUNDS => "escalated",
            ProposalDecision::RequestChanges => "changes_requested",
        };
        let mut decision = None;
        if state != "open" {
            update_state(&mut tx, &row, state).await?;
            row.state = state.into();
            if state == "accepted" {
                decision = Some(accept(&mut tx, &row).await?);
            }
        }
        let event = match state {
            "accepted" => "decision.accepted",
            "escalated" => "proposal.escalated",
            _ => "proposal.responded",
        };
        let receipt = record(&mut tx, context, &idem, row, decision, event).await?;
        tx.commit().await?;
        Ok(receipt)
    }

    pub async fn get_proposal(
        &self,
        digest: &BearerDigest,
        input: GetProposal,
    ) -> Result<Proposal, IdentityError> {
        let mut tx = self.pool.begin().await?;
        workspace_context(&mut tx, digest, input.workspace_id).await?;
        lock_proposal_key(&mut tx, input.workspace_id, input.proposal_id, true).await?;
        let row = select_proposal(
            &mut tx,
            input.workspace_id,
            input.proposal_id,
            input.revision,
        )
        .await?;
        let decision_id = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM decisions
            WHERE workspace_id=$1 AND proposal_id=$2 AND proposal_revision=$3",
        )
        .bind(row.workspace_id)
        .bind(row.proposal_id)
        .bind(row.revision)
        .fetch_optional(&mut *tx)
        .await?;
        let proposal = materialize(&mut tx, row, decision_id).await?;
        tx.commit().await?;
        Ok(proposal)
    }

    pub async fn get_decision(
        &self,
        digest: &BearerDigest,
        input: GetDecision,
    ) -> Result<Decision, IdentityError> {
        let mut tx = self.pool.begin().await?;
        workspace_context(&mut tx, digest, input.workspace_id).await?;
        let row = sqlx::query_as::<_, DecisionRow>(
            "SELECT * FROM decisions WHERE workspace_id=$1 AND id=$2",
        )
        .bind(input.workspace_id.0)
        .bind(input.decision_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(IdentityError::Forbidden)?;
        tx.commit().await?;
        Ok(row.into())
    }

    pub async fn get_area_decision(
        &self,
        digest: &BearerDigest,
        input: GetAreaDecision,
    ) -> Result<AreaDecision, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        lock_policy(&mut tx, input.checkout_id).await?;
        let area = shared_area(&mut tx, context, input.checkout_id, input.area_id).await?;
        let last = area_decision(&mut tx, input.workspace_id, input.area_id).await?;
        let generation = last.as_ref().and_then(|row| row.generation).unwrap_or(0);
        let decision = last
            .filter(|row| row.area_revision == Some(area.revision))
            .map(Decision::from);
        tx.commit().await?;
        Ok(AreaDecision {
            area_id: area.area_id,
            area_revision: area.revision,
            generation,
            decision,
        })
    }
}

async fn shared_area(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    checkout: Uuid,
    id: Uuid,
) -> Result<Area, IdentityError> {
    let area = policy_area(tx, context.access.workspace, checkout, id).await?;
    if !matches!(area.policy, AreaPolicy::Shared { .. })
        || (context.actor != Actor::Human(area.owner_user_id)
            && !area.policy.approvers().contains(&context.recipient()))
    {
        return Err(IdentityError::Forbidden);
    }
    Ok(area)
}

async fn area_decision(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    area: Uuid,
) -> Result<Option<DecisionRow>, IdentityError> {
    sqlx::query("SELECT set_config('office.reading_area_decisions',$1,true)")
        .bind(area.to_string())
        .execute(&mut **tx)
        .await?;
    let row = sqlx::query_as::<_, DecisionRow>(
        "SELECT * FROM decisions WHERE workspace_id=$1 AND area_id=$2
        ORDER BY generation DESC LIMIT 1",
    )
    .bind(workspace.0)
    .bind(area)
    .fetch_optional(&mut **tx)
    .await?;
    sqlx::query("SELECT set_config('office.reading_area_decisions','',true)")
        .execute(&mut **tx)
        .await?;
    Ok(row)
}

pub(crate) async fn current_decision_covers_access(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    areas: &[Area],
    requested: Option<Uuid>,
) -> Result<bool, IdentityError> {
    let Some(requested) = requested else {
        return Ok(true);
    };
    for area in areas
        .iter()
        .filter(|area| matches!(area.policy, AreaPolicy::Shared { .. }))
    {
        if area_decision(tx, workspace, area.area_id)
            .await?
            .is_some_and(|decision| {
                decision.id == requested && decision.area_revision == Some(area.revision)
            })
        {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn verify_bound_area(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    binding: Option<AreaBinding>,
) -> Result<(), IdentityError> {
    if let Some(area) = binding {
        lock_policy(tx, area.checkout).await?;
        let policy = shared_area(tx, context, area.checkout, area.id).await?;
        require_revision(area.revision, policy.revision)?;
        let last = area_decision(tx, context.access.workspace, area.id).await?;
        require_revision(area.base, last.and_then(|row| row.generation).unwrap_or(0))?;
    }
    Ok(())
}

async fn verify_approvers(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    people: &[Recipient],
    requires_two_owners: bool,
) -> Result<(), IdentityError> {
    if !(1..=8).contains(&people.len())
        || people
            .iter()
            .enumerate()
            .any(|(index, person)| people[..index].contains(person))
    {
        return Err(IdentityError::InvalidInput);
    }
    let mut lead_owners = Vec::new();
    for person in people {
        if active_principal(tx, context, *person).await? == Some(office_domain::AgentRole::Lead)
            && !lead_owners.contains(&person.user_id)
        {
            lead_owners.push(person.user_id);
        }
    }
    if requires_two_owners && lead_owners.len() < 2 {
        return Err(IdentityError::InvalidInput);
    }
    Ok(())
}

async fn lock_proposal(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    id: Uuid,
) -> Result<ProposalRow, IdentityError> {
    lock_proposal_key(tx, context.access.workspace, id, false).await?;
    let row = select_proposal(tx, context.access.workspace, id, None).await?;
    sqlx::query("SELECT set_config('office.proposal_context',$1,true)")
        .bind(id.to_string())
        .execute(&mut **tx)
        .await?;
    Ok(row)
}

async fn lock_proposal_key(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    id: Uuid,
    read_only: bool,
) -> Result<(), IdentityError> {
    let statement = if read_only {
        "SELECT pg_advisory_xact_lock_shared(hashtextextended($1, 0))"
    } else {
        "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))"
    };
    sqlx::query(statement)
        .bind(format!("proposal/{}/{}", workspace.0, id))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn select_proposal(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    id: Uuid,
    revision: Option<i64>,
) -> Result<ProposalRow, IdentityError> {
    sqlx::query_as(
        "SELECT * FROM proposal_revisions WHERE workspace_id=$1 AND proposal_id=$2
        AND ($3::bigint IS NULL OR revision=$3) ORDER BY revision DESC LIMIT 1",
    )
    .bind(workspace.0)
    .bind(id)
    .bind(revision)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(IdentityError::Forbidden)
}

async fn insert_revision(
    tx: &mut Transaction<'_, Postgres>,
    row: ProposalRow,
) -> Result<ProposalRow, IdentityError> {
    Ok(sqlx::query_as(
        "INSERT INTO proposal_revisions
        (workspace_id,proposal_id,revision,owner_user_id,author_agent_id,topic,checkout_id,area_id,
         area_revision,base_generation,required_approvers,content,deadline)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) RETURNING *",
    )
    .bind(row.workspace_id)
    .bind(row.proposal_id)
    .bind(row.revision)
    .bind(row.owner_user_id)
    .bind(row.author_agent_id)
    .bind(row.topic)
    .bind(row.checkout_id)
    .bind(row.area_id)
    .bind(row.area_revision)
    .bind(row.base_generation)
    .bind(row.required_approvers)
    .bind(row.content)
    .bind(row.deadline)
    .fetch_one(&mut **tx)
    .await?)
}

async fn update_state(
    tx: &mut Transaction<'_, Postgres>,
    row: &ProposalRow,
    state: &str,
) -> Result<(), IdentityError> {
    sqlx::query("UPDATE proposal_revisions SET state=$4 WHERE workspace_id=$1 AND proposal_id=$2 AND revision=$3")
        .bind(row.workspace_id).bind(row.proposal_id).bind(row.revision).bind(state).execute(&mut **tx).await?;
    Ok(())
}

async fn accept(
    tx: &mut Transaction<'_, Postgres>,
    row: &ProposalRow,
) -> Result<Decision, IdentityError> {
    let content = &row.content.0;
    let chosen = content
        .options
        .iter()
        .find(|option| option.key == content.recommendation)
        .ok_or(IdentityError::InvalidInput)?;
    let serialized = serde_json::to_vec(content).map_err(|_| IdentityError::InvalidInput)?;
    let generation = row
        .base_generation
        .map(|base| base.checked_add(1).ok_or(IdentityError::InvalidTransition))
        .transpose()?;
    let result = sqlx::query_as::<_, DecisionRow>("INSERT INTO decisions
         (id,workspace_id,proposal_id,proposal_revision,checkout_id,area_id,area_revision,generation,
          topic,selected_option,content_sha256)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) RETURNING *")
        .bind(Uuid::new_v4()).bind(row.workspace_id).bind(row.proposal_id).bind(row.revision)
        .bind(row.checkout_id).bind(row.area_id).bind(row.area_revision).bind(generation)
        .bind(&row.topic).bind(Json(chosen)).bind(hex::encode(Sha256::digest(serialized)))
        .fetch_one(&mut **tx).await?;
    Ok(result.into())
}

async fn materialize(
    tx: &mut Transaction<'_, Postgres>,
    row: ProposalRow,
    decision_id: Option<Uuid>,
) -> Result<Proposal, IdentityError> {
    let state = row.public_state(clock(tx).await?)?;
    let responses = sqlx::query_as::<_, (Uuid, Option<Uuid>, String, String, DateTime<Utc>)>(
        "SELECT responder_user_id,responder_agent_id,decision,reason,recorded_at FROM proposal_responses
         WHERE workspace_id=$1 AND proposal_id=$2 AND proposal_revision=$3 ORDER BY recorded_at,responder_user_id,responder_agent_id",
    ).bind(row.workspace_id).bind(row.proposal_id).bind(row.revision).fetch_all(&mut **tx).await?
        .into_iter().map(|(user, agent, decision, reason, recorded_at)| {
            let decision = match decision.as_str() {
                "approve" => ProposalDecision::Approve,
                "reject" => ProposalDecision::Reject,
                "request_changes" => ProposalDecision::RequestChanges,
                _ => return Err(IdentityError::InvalidRole),
            };
            Ok(ProposalResponse { responder: Recipient { user_id: UserId(user), agent_id: agent.map(AgentId) },
                decision, reason, recorded_at })
        }).collect::<Result<_, _>>()?;
    let author = row.author();
    let target = row.target();
    Ok(Proposal {
        proposal_id: row.proposal_id,
        workspace_id: WorkspaceId(row.workspace_id),
        revision: row.revision,
        author,
        target,
        content: row.content.0,
        required_approvers: row.required_approvers.0,
        responses,
        deadline: row.deadline,
        state,
        escalated_to: (state == ProposalState::Escalated).then_some(author.user_id),
        decision_id,
    })
}

async fn record(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    idem: &Idempotency,
    row: ProposalRow,
    decision: Option<Decision>,
    event: &str,
) -> Result<ProposalReceipt, IdentityError> {
    let proposal = materialize(tx, row, decision.as_ref().map(|item| item.decision_id)).await?;
    let sequence = append_event(tx, context, proposal.proposal_id, event, json!({
        "proposal_id": proposal.proposal_id, "revision": proposal.revision, "state": proposal.state,
        "decision_id": proposal.decision_id, "escalated_to": proposal.escalated_to,
    }), None).await?;
    let result = ProposalReceipt {
        proposal,
        decision,
        event_cursor: save_cursor(tx, context, sequence).await?,
    };
    idem.record(tx, result.proposal.proposal_id, &result)
        .await?;
    Ok(result)
}

fn validated_content(content: &ProposalContent) -> Result<ProposalContent, IdentityError> {
    if !(2..=6).contains(&content.options.len())
        || !content
            .options
            .iter()
            .any(|option| option.key == content.recommendation)
    {
        return Err(IdentityError::InvalidInput);
    }
    let mut options: Vec<ProposalOption> = Vec::new();
    for option in &content.options {
        if !(1..=32).contains(&option.key.len())
            || !option
                .key
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            || options.iter().any(|other| other.key == option.key)
        {
            return Err(IdentityError::InvalidInput);
        }
        options.push(ProposalOption {
            key: option.key.clone(),
            description: task_text(&option.description, 1024)?.into(),
        });
    }
    Ok(ProposalContent {
        title: valid_name(&content.title)?.into(),
        problem: task_text(&content.problem, 2048)?.into(),
        options,
        recommendation: content.recommendation.clone(),
    })
}

fn verify_deadline(now: DateTime<Utc>, deadline: DateTime<Utc>) -> Result<(), IdentityError> {
    if deadline <= now || deadline > now + Duration::days(7) {
        return Err(IdentityError::InvalidInput);
    }
    Ok(())
}

fn require_revision(expected: i64, current: i64) -> Result<(), IdentityError> {
    if expected != current {
        return Err(IdentityError::RevisionMismatch { current });
    }
    Ok(())
}

fn decision_name(decision: ProposalDecision) -> &'static str {
    match decision {
        ProposalDecision::Approve => "approve",
        ProposalDecision::Reject => "reject",
        ProposalDecision::RequestChanges => "request_changes",
    }
}

async fn clock(tx: &mut Transaction<'_, Postgres>) -> Result<DateTime<Utc>, IdentityError> {
    Ok(sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut **tx)
        .await?)
}
