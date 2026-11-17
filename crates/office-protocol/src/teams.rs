use chrono::{DateTime, Utc};
use office_domain::{AgentId, TeamId, TeamRole, TeamState, UserId, WorkspaceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::Recipient;

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateTeam {
    pub workspace_id: WorkspaceId,
    pub idempotency_key: Uuid,
    pub name: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetTeam {
    pub workspace_id: WorkspaceId,
    pub team_id: TeamId,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListTeams {
    pub workspace_id: WorkspaceId,
    pub after: Option<TeamId>,
    pub limit: Option<u16>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetTeamMember {
    pub workspace_id: WorkspaceId,
    pub team_id: TeamId,
    pub expected_revision: i64,
    pub idempotency_key: Uuid,
    pub agent_id: AgentId,
    pub role: TeamRole,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateTeam {
    pub workspace_id: WorkspaceId,
    pub team_id: TeamId,
    pub expected_revision: i64,
    pub idempotency_key: Uuid,
    pub change: TeamChange,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum TeamChange {
    Leave,
    RemoveMember { member: Recipient },
    AppointLead { agent_id: Option<AgentId> },
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Team {
    pub workspace_id: WorkspaceId,
    pub team_id: TeamId,
    pub owner_user_id: UserId,
    pub lead_agent_id: Option<AgentId>,
    pub state: TeamState,
    pub name: String,
    pub revision: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TeamMember {
    pub member: Recipient,
    pub role: TeamRole,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct TeamView {
    pub team: Team,
    pub members: Vec<TeamMember>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct TeamMutation {
    pub team: Team,
    pub member: Option<TeamMember>,
    pub event_cursor: Uuid,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct TeamPage {
    pub teams: Vec<Team>,
    pub next_after: Option<TeamId>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InviteTeamMember {
    pub workspace_id: WorkspaceId,
    pub team_id: TeamId,
    pub expected_revision: i64,
    pub idempotency_key: Uuid,
    pub recipient: Recipient,
    pub role: TeamRole,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TeamInviteDecision {
    Accept,
    Decline,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DecideTeamInvite {
    pub workspace_id: WorkspaceId,
    pub invitation_id: Uuid,
    pub idempotency_key: Uuid,
    pub decision: TeamInviteDecision,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListTeamInvites {
    pub workspace_id: WorkspaceId,
    pub after: Option<Uuid>,
    pub limit: Option<u16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TeamInviteState {
    Pending,
    Accepted,
    Declined,
    Expired,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct TeamInvitation {
    pub invitation_id: Uuid,
    pub workspace_id: WorkspaceId,
    pub team_id: TeamId,
    pub issued_by: Recipient,
    pub recipient: Recipient,
    pub role: TeamRole,
    pub state: TeamInviteState,
    pub expires_at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct TeamInviteMutation {
    pub invitation: TeamInvitation,
    pub joined_team_revision: Option<i64>,
    pub event_cursor: Uuid,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct TeamInvitePage {
    pub invitations: Vec<TeamInvitation>,
    pub next_after: Option<Uuid>,
}
