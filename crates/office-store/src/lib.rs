use std::{fs, path::Path, str::FromStr, sync::Arc, time::Duration};

use office_domain::InstallationId;
use office_protocol::{API_VERSION, Readiness};
use sqlx::{
    ConnectOptions, PgPool,
    migrate::Migrator,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use thiserror::Error;
use uuid::Uuid;

mod artifacts;
mod checkouts;
mod data_scope;
mod decisions;
mod events;
mod execution_control;
mod identity;
mod inbox;
mod messages;
mod notes;
mod operations_metrics;
mod scheduler;
mod search;
mod tasks;
mod teams;
mod workspace_budget;

pub use identity::IdentityError;

static MIGRATIONS: Migrator = sqlx::migrate!("../../migrations");

#[derive(Debug, Error)]
#[error("{0}")]
pub struct DatabaseConfigError(&'static str);

pub fn database_options(
    url: &str,
    password_file: Option<&Path>,
) -> Result<PgConnectOptions, DatabaseConfigError> {
    let mut options = PgConnectOptions::from_str(url)
        .map_err(|_| DatabaseConfigError("invalid PostgreSQL connection configuration"))?;
    if let Some(path) = password_file {
        let password = fs::read_to_string(path)
            .map_err(|_| DatabaseConfigError("could not read PostgreSQL password file"))?;
        let password = password.trim_end_matches(['\r', '\n']);
        if password.is_empty() {
            return Err(DatabaseConfigError("PostgreSQL password file is empty"));
        }
        options = options.password(password);
    }
    Ok(options)
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("PostgreSQL is unavailable")]
    Database(#[from] sqlx::Error),
    #[error("database migration failed")]
    Migration(#[from] sqlx::migrate::MigrateError),
    #[error("database health check timed out")]
    Timeout,
    #[error("database role must lack SUPERUSER and BYPASSRLS")]
    InsecureRole,
    #[error("database schema or API version is incompatible")]
    IncompatibleSchema,
}

#[derive(Clone)]
pub struct Store {
    pool: PgPool,
    publication_wakes: Arc<tasks::publications::PublicationWakes>,
}

#[derive(sqlx::FromRow)]
struct SchemaStatus {
    installation_id: Uuid,
    api_version: i16,
    latest: Option<i64>,
    count: i64,
    clean: Option<bool>,
}

impl Store {
    pub async fn connect(
        options: PgConnectOptions,
        max_connections: u32,
    ) -> Result<Self, StoreError> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(options.disable_statement_logging())
            .await?;
        let bounded_role: bool = sqlx::query_scalar(
            "SELECT NOT rolsuper AND NOT rolbypassrls FROM pg_roles WHERE rolname = current_user",
        )
        .fetch_one(&pool)
        .await?;
        if !bounded_role {
            return Err(StoreError::InsecureRole);
        }
        MIGRATIONS.run(&pool).await?;
        let store = Self {
            pool,
            publication_wakes: Arc::default(),
        };
        store.readiness().await?;
        Ok(store)
    }

    pub async fn readiness(&self) -> Result<Readiness, StoreError> {
        tokio::time::timeout(Duration::from_secs(2), self.check_schema())
            .await
            .map_err(|_| StoreError::Timeout)?
    }

    async fn check_schema(&self) -> Result<Readiness, StoreError> {
        let row: Option<SchemaStatus> = sqlx::query_as(
            "SELECT installation_id, api_version, versions.latest, versions.count, versions.clean
             FROM office_installation
             CROSS JOIN (
                 SELECT max(version) AS latest, count(*) AS count, bool_and(success) AS clean
                 FROM _sqlx_migrations
             ) AS versions
             WHERE singleton",
        )
        .fetch_optional(&self.pool)
        .await?;
        let Some(SchemaStatus {
            installation_id,
            api_version,
            latest: Some(schema_version),
            count,
            clean: Some(true),
        }) = row
        else {
            return Err(StoreError::IncompatibleSchema);
        };
        if api_version != API_VERSION
            || MIGRATIONS.iter().last().map(|entry| entry.version) != Some(schema_version)
            || count as usize != MIGRATIONS.iter().count()
        {
            return Err(StoreError::IncompatibleSchema);
        }
        Ok(Readiness {
            installation_id: InstallationId(installation_id),
            api_version,
            schema_version,
        })
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }
}
