use chrono::{DateTime, Utc};
use office_domain::{
    AgentId, AgentRole, CredentialId, InstanceId, InvitationId, RunnerId, UserId, WorkspaceId,
    WorkspaceRole,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceMembership {
    pub workspace_id: WorkspaceId,
    pub name: String,
    pub role: WorkspaceRole,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateWorkspace {
    pub idempotency_key: Uuid,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Identity {
    pub user_id: UserId,
    pub display_name: String,
    pub credential_id: CredentialId,
    pub memberships: Vec<WorkspaceMembership>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateInvitation {
    pub idempotency_key: Uuid,
    pub display_name: String,
    pub invitation_digest: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AcceptInvitation {
    pub idempotency_key: Uuid,
    pub invitation_token: String,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Invitation {
    pub invitation_id: InvitationId,
    pub workspace_id: WorkspaceId,
    pub display_name: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateAgent {
    pub idempotency_key: Uuid,
    pub display_name: String,
    pub role: AgentRole,
    pub credential_digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentIdentity {
    pub agent_id: AgentId,
    pub owner_user_id: UserId,
    pub credential_id: CredentialId,
    pub display_name: String,
    pub workspace_id: WorkspaceId,
    pub workspace_name: String,
    pub role: AgentRole,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttachAgentInstance {
    pub idempotency_key: Uuid,
    pub label: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetAgentInstance {
    pub instance_id: InstanceId,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentInstance {
    pub instance_id: InstanceId,
    pub agent_id: AgentId,
    pub workspace_id: WorkspaceId,
    pub credential_id: CredentialId,
    pub label: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Clone, Serialize)]
#[serde(untagged)]
pub enum McpIdentity {
    Human(Identity),
    Agent(AgentIdentity),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialSubject {
    User,
    Agent {
        workspace_id: WorkspaceId,
        agent_id: AgentId,
    },
    Runner {
        workspace_id: WorkspaceId,
        runner_id: RunnerId,
    },
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateRunner {
    pub idempotency_key: Uuid,
    pub name: String,
    pub credential_digest: String,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RunnerIdentity {
    pub runner_id: RunnerId,
    pub workspace_id: WorkspaceId,
    pub owner_user_id: UserId,
    pub credential_id: CredentialId,
    pub name: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IssueCredential {
    pub idempotency_key: Uuid,
    pub subject: CredentialSubject,
    pub credential_digest: String,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IssuedCredential {
    pub credential_id: CredentialId,
    pub owner_user_id: UserId,
    pub subject: CredentialSubject,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RevokeCredential {
    pub idempotency_key: Uuid,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RevokedCredential {
    pub credential_id: CredentialId,
    pub revoked_at: DateTime<Utc>,
}
