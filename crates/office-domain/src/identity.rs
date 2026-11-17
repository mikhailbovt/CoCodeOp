use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::str::FromStr;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct UserId(pub Uuid);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct WorkspaceId(pub Uuid);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct CredentialId(pub Uuid);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct InvitationId(pub Uuid);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct AgentId(pub Uuid);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct InstanceId(pub Uuid);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct RunnerId(pub Uuid);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct TeamId(pub Uuid);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct NoteId(pub Uuid);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct ChannelId(pub Uuid);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct MessageId(pub Uuid);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    Lead,
    Worker,
    Reviewer,
}

impl AgentRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lead => "lead",
            Self::Worker => "worker",
            Self::Reviewer => "reviewer",
        }
    }
}

impl FromStr for AgentRole {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "lead" => Ok(Self::Lead),
            "worker" => Ok(Self::Worker),
            "reviewer" => Ok(Self::Reviewer),
            _ => Err("expected lead, worker, or reviewer"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceRole {
    Owner,
    Admin,
    Member,
}

impl FromStr for WorkspaceRole {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "owner" => Ok(Self::Owner),
            "admin" => Ok(Self::Admin),
            "member" => Ok(Self::Member),
            _ => Err("expected owner, admin, or member"),
        }
    }
}

/// The digest of a pre-issued 256-bit opaque bearer. This value is never included in API replies.
#[derive(Clone)]
pub struct BearerDigest([u8; 32]);

impl BearerDigest {
    pub fn of(token: &str) -> Option<Self> {
        if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        Some(Self(Sha256::digest(token.as_bytes()).into()))
    }

    pub fn of_run(token: &str) -> Option<Self> {
        let body = token.strip_prefix("crun_")?;
        if body.len() != 64 || !body.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        Some(Self(Sha256::digest(token.as_bytes()).into()))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn decode(value: &str) -> Option<Self> {
        let mut digest = [0; 32];
        hex::decode_to_slice(value, &mut digest).ok()?;
        Some(Self(digest))
    }
}
