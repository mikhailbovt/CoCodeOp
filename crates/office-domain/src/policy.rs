use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    AgentId, AgentRole, CredentialId, RunnerId, TeamId, TeamRole, UserId, WorkspaceId,
    WorkspaceRole,
};

/// Identities and membership evidence are built by the application from locked database rows.
/// These types are intentionally unavailable for deserialization from a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Actor {
    Human(UserId),
    Agent {
        owner: UserId,
        agent: AgentId,
        workspace: WorkspaceId,
        credential: CredentialId,
        role: AgentRole,
    },
    Runner {
        owner: UserId,
        runner: RunnerId,
        workspace: WorkspaceId,
    },
}

impl Actor {
    pub fn owner(self) -> UserId {
        match self {
            Self::Human(user)
            | Self::Agent { owner: user, .. }
            | Self::Runner { owner: user, .. } => user,
        }
    }

    pub fn subject(self) -> Option<Subject> {
        match self {
            Self::Human(user_id) => Some(Subject::Human { user_id }),
            Self::Agent { agent, .. } => Some(Subject::Agent { agent_id: agent }),
            Self::Runner { .. } => None,
        }
    }

    fn in_workspace(self, access: WorkspaceAccess) -> bool {
        if self.owner() != access.member {
            return false;
        }
        match self {
            Self::Human(_) => true,
            Self::Agent { workspace, .. } => workspace == access.workspace,
            Self::Runner { .. } => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Subject {
    Human { user_id: UserId },
    Agent { agent_id: AgentId },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Visibility {
    #[default]
    OwnerPrivate,
    Team {
        team_id: TeamId,
    },
    Workspace,
    ExplicitAcl,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GrantRight {
    Read,
    Write,
}

impl GrantRight {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataGrant {
    pub grantee: Subject,
    pub right: GrantRight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkspaceAccess {
    pub workspace: WorkspaceId,
    pub member: UserId,
    pub role: WorkspaceRole,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataAccess {
    pub workspace: WorkspaceAccess,
    pub owner: UserId,
    pub author: Subject,
    pub visibility: Visibility,
    pub active_team: Option<TeamId>,
    pub grant: Option<DataGrant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskAuthority {
    pub workspace: WorkspaceAccess,
    pub accountable_owner: UserId,
    pub accountable: Subject,
    pub assignee: Option<Subject>,
    pub executor: Option<Subject>,
    pub explicit_reviewer: Option<Subject>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TeamAuthority {
    pub workspace: WorkspaceAccess,
    pub owner: UserId,
    pub lead_agent: Option<AgentId>,
    pub member_role: Option<TeamRole>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    CreateWorkspace,
    ReadWorkspace,
    InviteMember,
    RegisterAgent,
    RegisterRunner,
    ManageCredentials,
    AccessInstance,
    UseRunnerIdentity,
    ReadData,
    WriteData,
    PostMessage,
    CreateTask,
    ManageTask,
    ClaimTask,
    UpdateRun,
    ReviewTask,
    CreateTeam,
    ReadTeam,
    ManageTeam,
    AppointTeamLead,
    LeaveTeam,
    DecideOffer,
    CreateRuntimeTemplate,
    ManageExecutionBudget,
    IssueDelegation,
    UseDelegation,
    RevokeDelegation,
    CreateProject,
    RegisterCheckout,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resource {
    Account(UserId),
    Workspace(WorkspaceAccess),
    Instance {
        workspace: WorkspaceId,
        agent: AgentId,
        credential: CredentialId,
    },
    Runner {
        workspace: WorkspaceId,
        runner: RunnerId,
        owner: UserId,
    },
    Data(DataAccess),
    Task(TaskAuthority),
    Team(TeamAuthority),
    DirectedOffer {
        workspace: WorkspaceAccess,
        recipient_owner: UserId,
        recipient_agent: Option<AgentId>,
    },
    Delegation {
        workspace: WorkspaceAccess,
        owner: UserId,
        issuer: Option<AgentId>,
        grantee: AgentId,
    },
}

pub fn authorize(actor: Actor, action: Action, resource: Resource) -> bool {
    match (action, resource) {
        (
            Action::CreateWorkspace
            | Action::ManageCredentials
            | Action::CreateRuntimeTemplate
            | Action::ManageExecutionBudget
            | Action::IssueDelegation
            | Action::CreateProject
            | Action::RegisterCheckout,
            Resource::Account(owner),
        ) => actor == Actor::Human(owner),
        (Action::ReadWorkspace, Resource::Workspace(access)) => actor.in_workspace(access),
        (Action::CreateTask | Action::CreateTeam, Resource::Workspace(access)) => {
            actor.in_workspace(access)
                && matches!(
                    actor,
                    Actor::Human(_)
                        | Actor::Agent {
                            role: AgentRole::Lead,
                            ..
                        }
                )
        }
        (Action::RegisterAgent | Action::RegisterRunner, Resource::Workspace(access)) => {
            actor == Actor::Human(access.member)
        }
        (Action::InviteMember, Resource::Workspace(access)) => {
            actor == Actor::Human(access.member)
                && matches!(access.role, WorkspaceRole::Owner | WorkspaceRole::Admin)
        }
        (
            Action::AccessInstance,
            Resource::Instance {
                workspace,
                agent,
                credential,
            },
        ) => matches!(
            actor,
            Actor::Agent {
                workspace: scope,
                agent: id,
                credential: active,
                ..
            } if workspace == scope && agent == id && credential == active
        ),
        (
            Action::UseRunnerIdentity,
            Resource::Runner {
                workspace,
                runner,
                owner,
            },
        ) => {
            actor
                == Actor::Runner {
                    owner,
                    runner,
                    workspace,
                }
        }
        (Action::ReadData | Action::WriteData | Action::PostMessage, Resource::Data(access)) => {
            authorize_data(actor, action, access)
        }
        (
            Action::ManageTask | Action::ClaimTask | Action::UpdateRun | Action::ReviewTask,
            Resource::Task(access),
        ) => authorize_task(actor, action, access),
        (
            Action::ReadTeam | Action::ManageTeam | Action::AppointTeamLead | Action::LeaveTeam,
            Resource::Team(access),
        ) => authorize_team(actor, action, access),
        (
            Action::DecideOffer,
            Resource::DirectedOffer {
                workspace,
                recipient_owner,
                recipient_agent,
            },
        ) => {
            actor.in_workspace(workspace)
                && (actor == Actor::Human(recipient_owner)
                    || matches!(actor, Actor::Agent { owner, agent, .. }
                    if owner == recipient_owner && Some(agent) == recipient_agent))
        }
        (
            Action::UseDelegation | Action::RevokeDelegation,
            Resource::Delegation {
                workspace,
                owner,
                issuer,
                grantee,
            },
        ) => {
            actor.in_workspace(workspace)
                && actor.owner() == owner
                && match (action, actor) {
                    (Action::UseDelegation, Actor::Agent { agent, .. }) => agent == grantee,
                    (Action::RevokeDelegation, Actor::Agent { agent, .. }) => Some(agent) == issuer,
                    (Action::RevokeDelegation, Actor::Human(_)) => true,
                    _ => false,
                }
        }
        _ => false,
    }
}

fn authorize_team(actor: Actor, action: Action, access: TeamAuthority) -> bool {
    if !actor.in_workspace(access.workspace) {
        return false;
    }
    let owns_team = actor == Actor::Human(access.owner);
    match action {
        Action::ReadTeam => owns_team || access.member_role.is_some(),
        Action::LeaveTeam => access.member_role.is_some() && !owns_team,
        Action::AppointTeamLead => owns_team,
        Action::ManageTeam => {
            owns_team
                || matches!(actor, Actor::Agent { owner, agent, .. }
                if owner == access.owner && Some(agent) == access.lead_agent
                   && access.member_role == Some(TeamRole::Lead))
        }
        _ => false,
    }
}

fn authorize_task(actor: Actor, action: Action, access: TaskAuthority) -> bool {
    if !actor.in_workspace(access.workspace) {
        return false;
    }
    let principal = actor.subject();
    let controls_task =
        actor == Actor::Human(access.accountable_owner) || principal == Some(access.accountable);
    match action {
        Action::ManageTask => controls_task,
        Action::ClaimTask => {
            !matches!(
                actor,
                Actor::Agent {
                    role: AgentRole::Reviewer,
                    ..
                }
            ) && access
                .assignee
                .is_none_or(|assigned| principal == Some(assigned))
        }
        Action::UpdateRun => principal.is_some() && principal == access.executor,
        Action::ReviewTask => {
            !matches!(
                actor,
                Actor::Agent {
                    role: AgentRole::Worker,
                    ..
                }
            ) && access
                .explicit_reviewer
                .map_or(controls_task, |reviewer| principal == Some(reviewer))
        }
        _ => false,
    }
}

fn authorize_data(actor: Actor, action: Action, access: DataAccess) -> bool {
    if !actor.in_workspace(access.workspace) {
        return false;
    }
    if let Visibility::Team { team_id } = access.visibility
        && access.active_team != Some(team_id)
    {
        return false;
    }
    if actor.owner() == access.owner
        && (actor == Actor::Human(access.owner) || actor.subject() == Some(access.author))
    {
        return true;
    }
    match access.visibility {
        Visibility::OwnerPrivate => false,
        Visibility::Workspace => matches!(action, Action::ReadData | Action::PostMessage),
        Visibility::Team { team_id } => {
            matches!(action, Action::ReadData | Action::PostMessage)
                && access.active_team == Some(team_id)
        }
        Visibility::ExplicitAcl => access.grant.is_some_and(|grant| {
            actor.subject() == Some(grant.grantee)
                && (action == Action::ReadData || grant.right == GrantRight::Write)
        }),
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    const ALICE: UserId = UserId(Uuid::from_u128(1));
    const BOB: UserId = UserId(Uuid::from_u128(2));
    const WORKSPACE: WorkspaceId = WorkspaceId(Uuid::from_u128(3));
    const OTHER_WORKSPACE: WorkspaceId = WorkspaceId(Uuid::from_u128(4));
    const LEAD: AgentId = AgentId(Uuid::from_u128(5));
    const SIBLING: AgentId = AgentId(Uuid::from_u128(6));
    const TEAM: TeamId = TeamId(Uuid::from_u128(7));
    const TOKEN: CredentialId = CredentialId(Uuid::from_u128(8));
    const RUNNER: RunnerId = RunnerId(Uuid::from_u128(9));

    fn alice_agent(agent: AgentId, workspace: WorkspaceId) -> Actor {
        Actor::Agent {
            owner: ALICE,
            agent,
            workspace,
            credential: TOKEN,
            role: AgentRole::Lead,
        }
    }

    fn alice_data(member: UserId, visibility: Visibility) -> DataAccess {
        DataAccess {
            workspace: WorkspaceAccess {
                workspace: WORKSPACE,
                member,
                role: WorkspaceRole::Owner,
            },
            owner: ALICE,
            author: Subject::Agent { agent_id: LEAD },
            visibility,
            active_team: None,
            grant: None,
        }
    }

    #[test]
    fn author_and_owner_are_distinct_from_other_agents_and_administrators() {
        let access = Resource::Data(alice_data(ALICE, Visibility::OwnerPrivate));
        assert!(authorize(Actor::Human(ALICE), Action::ReadData, access));
        assert!(authorize(
            alice_agent(LEAD, WORKSPACE),
            Action::WriteData,
            access
        ));
        assert!(!authorize(
            alice_agent(SIBLING, WORKSPACE),
            Action::ReadData,
            access
        ));
        assert!(!authorize(
            alice_agent(LEAD, OTHER_WORKSPACE),
            Action::ReadData,
            access
        ));
        assert!(!authorize(
            Actor::Human(BOB),
            Action::ReadData,
            Resource::Data(alice_data(BOB, Visibility::OwnerPrivate))
        ));
    }

    #[test]
    fn team_and_explicit_grants_apply_to_exact_subject_and_right() {
        let mut data = alice_data(BOB, Visibility::Team { team_id: TEAM });
        let bob = Actor::Human(BOB);
        assert!(!authorize(bob, Action::ReadData, Resource::Data(data)));
        data.active_team = Some(TEAM);
        assert!(authorize(bob, Action::ReadData, Resource::Data(data)));
        assert!(!authorize(bob, Action::WriteData, Resource::Data(data)));

        data.visibility = Visibility::ExplicitAcl;
        assert!(!authorize(bob, Action::ReadData, Resource::Data(data)));
        data.grant = Some(DataGrant {
            grantee: Subject::Human { user_id: BOB },
            right: GrantRight::Read,
        });
        assert!(authorize(bob, Action::ReadData, Resource::Data(data)));
        assert!(!authorize(bob, Action::WriteData, Resource::Data(data)));
        data.grant.as_mut().unwrap().right = GrantRight::Write;
        assert!(authorize(bob, Action::WriteData, Resource::Data(data)));
        let bob_agent = Actor::Agent {
            owner: BOB,
            agent: SIBLING,
            workspace: WORKSPACE,
            credential: TOKEN,
            role: AgentRole::Lead,
        };
        assert!(!authorize(
            bob_agent,
            Action::ReadData,
            Resource::Data(data)
        ));
    }

    #[test]
    fn service_identity_cannot_use_member_permissions() {
        let runner = Actor::Runner {
            owner: ALICE,
            runner: RUNNER,
            workspace: WORKSPACE,
        };
        let data = alice_data(ALICE, Visibility::Workspace);
        assert!(!authorize(
            runner,
            Action::ReadWorkspace,
            Resource::Workspace(data.workspace)
        ));
        assert!(!authorize(runner, Action::ReadData, Resource::Data(data)));
        assert!(!authorize(
            runner,
            Action::ManageCredentials,
            Resource::Account(ALICE)
        ));
        let owned_runner = Resource::Runner {
            owner: ALICE,
            runner: RUNNER,
            workspace: WORKSPACE,
        };
        assert!(authorize(runner, Action::UseRunnerIdentity, owned_runner));
        assert!(!authorize(
            alice_agent(LEAD, WORKSPACE),
            Action::UseRunnerIdentity,
            owned_runner
        ));
        let another_runner = Resource::Runner {
            owner: ALICE,
            runner: RUNNER,
            workspace: OTHER_WORKSPACE,
        };
        assert!(!authorize(
            runner,
            Action::UseRunnerIdentity,
            another_runner
        ));
    }
}
