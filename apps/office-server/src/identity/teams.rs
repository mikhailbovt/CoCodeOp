use std::time::Duration;

use axum::{
    Extension, Json, Router,
    extract::{State, rejection::JsonRejection},
    response::Response,
    routing::post,
};
use office_domain::{BearerDigest, RequestId};
use office_protocol::{
    CreateTeam, DecideTeamInvite, GetTeam, InviteTeamMember, ListTeamInvites, ListTeams,
    SetTeamMember, UpdateTeam,
};
use office_store::IdentityError;
use rmcp::{
    ErrorData, RoleServer, handler::server::wrapper::Parameters, model::CallToolResult,
    service::RequestContext, tool, tool_router,
};
use tokio::time::timeout;

use super::{
    OfficeMcp, current_digest,
    mutations::{error_reply, reply},
    tool_reply,
};
use crate::AppState;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/teams", post(create_team))
        .route("/v1/teams/read", post(get_team))
        .route("/v1/teams/list", post(list_teams))
        .route("/v1/teams/member", post(set_team_member))
        .route("/v1/teams/update", post(update_team))
        .route("/v1/teams/invite", post(invite_team_member))
        .route("/v1/team-invitations/respond", post(decide_team_invite))
        .route("/v1/team-invitations/list", post(list_team_invites))
}

#[tool_router(router = team_tools, vis = "pub(super)")]
impl OfficeMcp {
    #[tool(
        description = "Leave a team, remove a team member, or appoint a lead using the current team revision. Only the human team owner can appoint a lead.",
        annotations(
            idempotent_hint = true,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn office_update_team(
        &self,
        Parameters(input): Parameters<UpdateTeam>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(timeout(Duration::from_secs(5), self.store.update_team(actor, input)).await)
    }

    #[tool(
        description = "Offer an explicit, expiring membership to an outside owner or their agent.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_invite_member(
        &self,
        Parameters(input): Parameters<InviteTeamMember>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.invite_team_member(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Accept or decline an invitation addressed to your identity.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_respond_invitation(
        &self,
        Parameters(input): Parameters<DecideTeamInvite>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.decide_team_invite(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "List team invitations issued by or addressed to your identity.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_list_team_invitations(
        &self,
        Parameters(input): Parameters<ListTeamInvites>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.list_team_invites(actor, input),
            )
            .await,
        )
    }
    #[tool(
        description = "Create a team owned by the authenticated owner, with your lead binding when using an agent credential.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_create_team(
        &self,
        Parameters(input): Parameters<CreateTeam>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(timeout(Duration::from_secs(5), self.store.create_team(actor, input)).await)
    }

    #[tool(
        description = "Read an accessible team's roster and roles.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_get_team(
        &self,
        Parameters(input): Parameters<GetTeam>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(timeout(Duration::from_secs(5), self.store.get_team(actor, input)).await)
    }

    #[tool(
        description = "List the teams visible to your identity.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_list_teams(
        &self,
        Parameters(input): Parameters<ListTeams>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(timeout(Duration::from_secs(5), self.store.list_teams(actor, input)).await)
    }

    #[tool(
        description = "Add a registered agent of your owner to your team or change its team role.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_set_team_member(
        &self,
        Parameters(input): Parameters<SetTeamMember>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.set_team_member(actor, input),
            )
            .await,
        )
    }
}

async fn create_team(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<CreateTeam>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.create_team(&actor, body),
    )
    .await;
    reply(&state, id, result)
}

async fn get_team(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<GetTeam>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(Duration::from_secs(5), state.store.get_team(&actor, body)).await;
    reply(&state, id, result)
}

async fn list_teams(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<ListTeams>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(Duration::from_secs(5), state.store.list_teams(&actor, body)).await;
    reply(&state, id, result)
}

async fn set_team_member(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<SetTeamMember>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.set_team_member(&actor, body),
    )
    .await;
    reply(&state, id, result)
}

async fn update_team(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<UpdateTeam>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.update_team(&actor, body),
    )
    .await;
    reply(&state, id, result)
}

async fn invite_team_member(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<InviteTeamMember>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.invite_team_member(&actor, body),
    )
    .await;
    reply(&state, id, result)
}

async fn decide_team_invite(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<DecideTeamInvite>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.decide_team_invite(&actor, body),
    )
    .await;
    reply(&state, id, result)
}

async fn list_team_invites(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<ListTeamInvites>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.list_team_invites(&actor, body),
    )
    .await;
    reply(&state, id, result)
}
