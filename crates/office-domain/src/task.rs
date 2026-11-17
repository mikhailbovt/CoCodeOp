use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct TaskId(pub Uuid);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct RunId(pub Uuid);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Draft,
    Ready,
    Assigned,
    InProgress,
    Blocked,
    InReview,
    ChangesRequested,
    Done,
    Cancelled,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskTransition {
    AddDependency,
    ReviseCriteria,
    ResolveReviewEscalation,
    Prepare,
    Assign,
    Claim,
    Progress,
    RequestHelp,
    Resume,
    Submit,
    Accept,
    RequestChanges,
    Fail,
    Cancel,
    AcknowledgeCancel,
}

impl TaskStatus {
    const ALL: [Self; 10] = [
        Self::Draft,
        Self::Ready,
        Self::Assigned,
        Self::InProgress,
        Self::Blocked,
        Self::InReview,
        Self::ChangesRequested,
        Self::Done,
        Self::Cancelled,
        Self::Failed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Ready => "ready",
            Self::Assigned => "assigned",
            Self::InProgress => "in_progress",
            Self::Blocked => "blocked",
            Self::InReview => "in_review",
            Self::ChangesRequested => "changes_requested",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }

    pub fn transition(self, command: TaskTransition) -> Option<Self> {
        use TaskStatus as State;
        use TaskTransition as Command;
        match (self, command) {
            (State::Draft, Command::AddDependency) => Some(State::Draft),
            (
                state @ (State::Draft | State::Ready | State::Assigned | State::ChangesRequested),
                Command::ReviseCriteria,
            ) => Some(state),
            (State::Blocked, Command::ResolveReviewEscalation) => Some(State::ChangesRequested),
            (State::Draft | State::ChangesRequested, Command::Prepare) => Some(State::Ready),
            (State::Ready, Command::Assign) => Some(State::Assigned),
            (State::Ready | State::Assigned, Command::Claim) => Some(State::InProgress),
            (State::InProgress, Command::Progress) => Some(State::InProgress),
            (State::InProgress, Command::RequestHelp) => Some(State::Blocked),
            (State::Blocked, Command::Resume) => Some(State::InProgress),
            (State::InProgress, Command::Submit) => Some(State::InReview),
            (State::InReview, Command::Accept) => Some(State::Done),
            (State::InReview, Command::RequestChanges) => Some(State::ChangesRequested),
            (State::InProgress | State::Blocked, Command::Fail) => Some(State::Failed),
            (State::Cancelled, Command::AcknowledgeCancel) => Some(State::Cancelled),
            (State::Done | State::Cancelled | State::Failed, _) => None,
            (_, Command::Cancel) => Some(State::Cancelled),
            _ => None,
        }
    }
}

impl FromStr for TaskStatus {
    type Err = &'static str;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|status| status.as_str() == name)
            .ok_or("invalid task status")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Starting,
    StartUnknown,
    Running,
    WaitingInput,
    Paused,
    Succeeded,
    Failed,
    CancelRequested,
    Cancelled,
    Lost,
    Unknown,
}

impl RunStatus {
    const ALL: [Self; 11] = [
        Self::Starting,
        Self::StartUnknown,
        Self::Running,
        Self::WaitingInput,
        Self::Paused,
        Self::Succeeded,
        Self::Failed,
        Self::CancelRequested,
        Self::Cancelled,
        Self::Lost,
        Self::Unknown,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::StartUnknown => "start_unknown",
            Self::Running => "running",
            Self::WaitingInput => "waiting_input",
            Self::Paused => "paused",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::CancelRequested => "cancel_requested",
            Self::Cancelled => "cancelled",
            Self::Lost => "lost",
            Self::Unknown => "unknown",
        }
    }
}

impl FromStr for RunStatus {
    type Err = &'static str;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|status| status.as_str() == name)
            .ok_or("invalid run status")
    }
}
