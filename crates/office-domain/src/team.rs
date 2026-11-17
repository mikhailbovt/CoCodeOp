use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TeamRole {
    Lead,
    Member,
    Worker,
    Reviewer,
}

impl TeamRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lead => "lead",
            Self::Member => "member",
            Self::Worker => "worker",
            Self::Reviewer => "reviewer",
        }
    }
}

impl FromStr for TeamRole {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        [Self::Lead, Self::Member, Self::Worker, Self::Reviewer]
            .into_iter()
            .find(|role| role.as_str() == value)
            .ok_or("invalid team role")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TeamState {
    Active,
    AwaitingOwner,
}

impl FromStr for TeamState {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "active" => Ok(Self::Active),
            "awaiting_owner" => Ok(Self::AwaitingOwner),
            _ => Err("invalid team state"),
        }
    }
}
