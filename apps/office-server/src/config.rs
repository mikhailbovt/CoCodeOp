use std::{env, fs, net::SocketAddr, path::PathBuf};

use serde::Deserialize;
use sqlx::postgres::PgConnectOptions;
use thiserror::Error;

#[derive(Debug, Error)]
#[error("{0}")]
pub struct ConfigError(&'static str);

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    listen: Option<SocketAddr>,
    allow_private_http: Option<bool>,
    database_url: Option<String>,
    database_password_file: Option<PathBuf>,
    max_db_connections: Option<u32>,
    event_retain_count: Option<u32>,
}

pub struct Config {
    pub listen: SocketAddr,
    pub postgres: PgConnectOptions,
    pub max_db_connections: u32,
    pub event_retain_count: u32,
}

impl Config {
    pub fn load(path: Option<PathBuf>) -> Result<Self, ConfigError> {
        let file: FileConfig = if let Some(path) = path {
            let content = fs::read_to_string(path)
                .map_err(|_| ConfigError("could not read server configuration file"))?;
            toml::from_str(&content)
                .map_err(|_| ConfigError("invalid server TOML configuration"))?
        } else {
            FileConfig::default()
        };
        let listen = optional_env("OFFICE_LISTEN")?
            .map(|value| {
                value
                    .parse()
                    .map_err(|_| ConfigError("invalid OFFICE_LISTEN"))
            })
            .transpose()?
            .or(file.listen)
            .unwrap_or(SocketAddr::from(([127, 0, 0, 1], 8400)));
        let private_http = optional_env("OFFICE_ALLOW_PRIVATE_HTTP")?
            .map(|value| {
                value
                    .parse::<bool>()
                    .map_err(|_| ConfigError("invalid OFFICE_ALLOW_PRIVATE_HTTP"))
            })
            .transpose()?
            .or(file.allow_private_http)
            .unwrap_or(false);
        if !listen.ip().is_loopback() && !private_http {
            return Err(ConfigError(
                "non-loopback HTTP requires OFFICE_ALLOW_PRIVATE_HTTP=true on a private network",
            ));
        }
        let url = optional_env("OFFICE_DATABASE_URL")?
            .or(file.database_url)
            .ok_or(ConfigError(
                "set OFFICE_DATABASE_URL or database_url in the server configuration",
            ))?;
        let password_file = env::var_os("OFFICE_DATABASE_PASSWORD_FILE")
            .map(PathBuf::from)
            .or(file.database_password_file);
        let postgres = office_store::database_options(&url, password_file.as_deref())
            .map_err(|_| ConfigError("could not configure the PostgreSQL connection"))?;
        let max_db_connections = optional_env("OFFICE_MAX_DB_CONNECTIONS")?
            .map(|value| {
                value
                    .parse()
                    .map_err(|_| ConfigError("invalid OFFICE_MAX_DB_CONNECTIONS"))
            })
            .transpose()?
            .or(file.max_db_connections)
            .unwrap_or(12);
        if !(1..=128).contains(&max_db_connections) {
            return Err(ConfigError("max_db_connections must be between 1 and 128"));
        }
        let event_retain_count = optional_env("OFFICE_EVENT_RETAIN_COUNT")?
            .map(|value| {
                value
                    .parse()
                    .map_err(|_| ConfigError("invalid OFFICE_EVENT_RETAIN_COUNT"))
            })
            .transpose()?
            .or(file.event_retain_count)
            .unwrap_or(100_000);
        if !(1..=1_000_000).contains(&event_retain_count) {
            return Err(ConfigError(
                "event_retain_count must be between 1 and 1000000",
            ));
        }
        Ok(Self {
            listen,
            postgres,
            max_db_connections,
            event_retain_count,
        })
    }
}

fn optional_env(name: &str) -> Result<Option<String>, ConfigError> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(ConfigError(
            "server environment contains non-UTF-8 configuration",
        )),
    }
}
