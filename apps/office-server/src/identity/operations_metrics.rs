use std::{
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
    time::{Duration, Instant},
};

use axum::{Extension, Json, extract::State, response::Response};
use office_domain::{BearerDigest, ErrorCode, RequestId};
use office_protocol::{InstanceRejections, ReadOperationsMetrics};
use tokio::time::timeout;

use super::mutations::reply;
use crate::AppState;

static START: OnceLock<Instant> = OnceLock::new();
static DENIED_ACTIONS: AtomicU64 = AtomicU64::new(0);
static SCOPE_DENIALS: AtomicU64 = AtomicU64::new(0);
static STALE_FENCES: AtomicU64 = AtomicU64::new(0);
static CONTENT_CONFLICTS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn start() {
    START.get_or_init(Instant::now);
}

pub(super) fn record(code: ErrorCode) {
    match code {
        ErrorCode::Forbidden | ErrorCode::Unauthenticated | ErrorCode::DelegationInactive => {
            DENIED_ACTIONS.fetch_add(1, Relaxed);
        }
        ErrorCode::PathForbidden | ErrorCode::AccessRequestRequired => {
            DENIED_ACTIONS.fetch_add(1, Relaxed);
            SCOPE_DENIALS.fetch_add(1, Relaxed);
        }
        ErrorCode::ClaimFenceStale
        | ErrorCode::RunLeaseStale
        | ErrorCode::CoordinatorStale
        | ErrorCode::ScopeStale => {
            STALE_FENCES.fetch_add(1, Relaxed);
        }
        ErrorCode::ContentConflict | ErrorCode::ContextStale => {
            CONTENT_CONFLICTS.fetch_add(1, Relaxed);
        }
        _ => {}
    }
}

pub(super) async fn read(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(digest): Extension<BearerDigest>,
    Json(input): Json<ReadOperationsMetrics>,
) -> Response {
    reply(
        &state,
        id,
        timeout(Duration::from_secs(4), async {
            let (mut metrics, operator) = state.store.operations_metrics(&digest, input).await?;
            if operator {
                metrics.instance_rejections = Some(InstanceRejections {
                    uptime_seconds: START.get_or_init(Instant::now).elapsed().as_secs(),
                    denied_actions: DENIED_ACTIONS.load(Relaxed),
                    scope_denials: SCOPE_DENIALS.load(Relaxed),
                    stale_fences: STALE_FENCES.load(Relaxed),
                    content_conflicts: CONTENT_CONFLICTS.load(Relaxed),
                });
            }
            Ok(metrics)
        })
        .await,
    )
}
