mod config;
mod identity;

use std::{
    future::IntoFuture,
    io,
    path::PathBuf,
    process::ExitCode,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Extension, Json, Router,
    extract::{Request, State},
    http::{HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use clap::Parser;
use office_domain::{Clock, ErrorCode, RequestId, SystemClock};
use office_protocol::{ApiError, Health, Outcome, REQUEST_ID_HEADER, Readiness, Reply};
use office_store::{Store, StoreError};
use tokio::net::TcpListener;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use uuid::Uuid;

#[derive(Parser)]
#[command(version, about = "CoCodeOp coordination server")]
struct Args {
    #[arg(long, value_name = "TOML")]
    config: Option<PathBuf>,
}

#[derive(Clone)]
struct AppState {
    store: Store,
    clock: Arc<dyn Clock>,
    event_retain_count: u32,
}

fn main() -> ExitCode {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .json()
        .with_current_span(true)
        .with_span_list(false)
        .with_target(false)
        .with_writer(io::stderr)
        .init();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(message = %error, "could not start async runtime");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(serve(args));
    // Timed-out PostgreSQL reconnects can leave an operating-system DNS lookup on a blocking thread.
    runtime.shutdown_timeout(Duration::from_secs(1));
    if let Err(error) = result {
        tracing::error!(message = %error, "server stopped");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

async fn serve(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    identity::operations_metrics::start();
    let config = config::Config::load(args.config)?;
    let store = Store::connect(config.postgres, config.max_db_connections).await?;
    let state = AppState {
        store: store.clone(),
        clock: Arc::new(SystemClock),
        event_retain_count: config.event_retain_count,
    };
    let stop = CancellationToken::new();
    let app = Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(readiness))
        .merge(identity::routes(state.clone(), stop.clone()))
        .fallback(not_found)
        .layer(middleware::from_fn(request_context))
        .with_state(state);
    let listener = TcpListener::bind(config.listen).await?;
    tracing::info!(bind = %listener.local_addr()?, "server accepts requests");
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(stop.clone().cancelled_owned())
        .into_future();
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result?,
        _ = shutdown_signal() => {
            tracing::info!("server draining requests");
            stop.cancel();
            timeout(Duration::from_secs(5), &mut server).await??;
        }
    }
    timeout(Duration::from_secs(2), store.close()).await?;
    tracing::info!("server shutdown finished");
    Ok(())
}

async fn request_context(mut request: Request, next: Next) -> Response {
    let id = RequestId(Uuid::new_v4());
    let correlation = id.0.hyphenated().to_string();
    let span = tracing::info_span!("http", trace_id = %correlation, method = %request.method());
    request.extensions_mut().insert(id);
    async move {
        let start = Instant::now();
        let mut response = next.run(request).await;
        response.headers_mut().insert(
            REQUEST_ID_HEADER,
            HeaderValue::from_str(&correlation).expect("UUID is a valid header value"),
        );
        tracing::info!(
            status = response.status().as_u16(),
            elapsed_ms = start.elapsed().as_millis(),
            "request finished"
        );
        response
    }
    .instrument(span)
    .await
}

async fn health(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
) -> Json<Reply<Health>> {
    Json(Reply {
        request_id: id,
        timestamp: state.clock.now(),
        outcome: Outcome::Success {
            result: Health {
                version: env!("CARGO_PKG_VERSION").into(),
            },
        },
    })
}

async fn readiness(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
) -> impl IntoResponse {
    let (status, outcome) = match state.store.readiness().await {
        Ok(result) => (StatusCode::OK, Outcome::Success { result }),
        Err(StoreError::IncompatibleSchema) => unavailable(
            ErrorCode::IncompatibleSchema,
            "PostgreSQL schema does not match this server build",
            false,
        ),
        Err(_) => unavailable(
            ErrorCode::DatabaseUnavailable,
            "PostgreSQL is not ready",
            true,
        ),
    };
    (
        status,
        Json(Reply {
            request_id: id,
            timestamp: state.clock.now(),
            outcome,
        }),
    )
}

fn unavailable(
    code: ErrorCode,
    message: &str,
    retryable: bool,
) -> (StatusCode, Outcome<Readiness>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Outcome::Failure {
            error: ApiError {
                code,
                message: message.to_owned(),
                retryable,
                current_revision: None,
            },
        },
    )
}

async fn not_found(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
) -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        Json(Reply::<()> {
            request_id: id,
            timestamp: state.clock.now(),
            outcome: Outcome::Failure {
                error: ApiError {
                    code: ErrorCode::NotFound,
                    message: "Unknown API route".into(),
                    retryable: false,
                    current_revision: None,
                },
            },
        }),
    )
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut sigterm) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = sigterm.recv() => {},
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
