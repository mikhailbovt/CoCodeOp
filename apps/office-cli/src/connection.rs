use std::{
    env,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::ExitCode,
};

use clap::Args;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use uuid::Uuid;

use crate::{doctor, identity, print_receipt};

#[derive(Args)]
pub(crate) struct ConnectOptions {
    #[arg(long, default_value = "http://127.0.0.1:8400")]
    url: Url,
    #[arg(long)]
    token_file: PathBuf,
    #[arg(long)]
    agent: Option<Uuid>,
    #[arg(long)]
    json: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Connection {
    // Persist the path rather than the bearer, so the same profile supports rotation.
    url: String,
    token_file: PathBuf,
    agent_id: Uuid,
}

impl Connection {
    pub(crate) fn read() -> Result<Self, String> {
        let path = profile_path()?;
        let file = File::open(path)
            .map_err(|_| "CoCodeOp is not configured. Run office connect and reopen the client")?;
        let mut bytes = Vec::new();
        file.take(8193)
            .read_to_end(&mut bytes)
            .map_err(|_| "Could not read the local CoCodeOp connection profile")?;
        if bytes.len() > 8192 {
            return Err("CoCodeOp connection profile exceeds 8192 bytes".into());
        }
        serde_json::from_slice(&bytes).map_err(|_| "Invalid CoCodeOp connection profile".into())
    }

    pub(crate) fn endpoint(&self) -> Result<Url, String> {
        let base = Url::parse(&self.url).map_err(|_| "Invalid CoCodeOp server URL")?;
        validate_url(&base)?;
        crate::server_url(&base, "/mcp").map_err(str::to_owned)
    }

    pub(crate) fn bearer(&self) -> Result<String, String> {
        identity::read_local_token(&self.token_file)
    }

    pub(crate) fn agent_id(&self) -> Uuid {
        self.agent_id
    }
}

#[derive(Serialize)]
struct ConnectionReceipt {
    agent_id: Uuid,
    workspace_id: Uuid,
    profile: PathBuf,
}

pub(crate) async fn run(options: ConnectOptions) -> ExitCode {
    let json = options.json;
    print_receipt(save(options).await, json, |receipt| {
        format!(
            "Connected agent {} · workspace {} · {}",
            receipt.agent_id,
            receipt.workspace_id,
            receipt.profile.display()
        )
    })
}

async fn save(options: ConnectOptions) -> Result<ConnectionReceipt, String> {
    validate_url(&options.url)?;
    let token_file = fs::canonicalize(&options.token_file)
        .map_err(|_| "Could not resolve the local agent credential file")?;
    let token = identity::read_local_token(&token_file)?;
    doctor::negotiate(&options.url, &token).await?;
    let agent = doctor::bootstrap(&options.url, &token).await?;
    if options
        .agent
        .is_some_and(|expected| expected != agent.agent_id.0)
    {
        return Err("CoCodeOp returned a different agent; the profile was not saved".into());
    }
    let profile = profile_path()?;
    write_profile(
        &profile,
        &Connection {
            url: options.url.into(),
            token_file,
            agent_id: agent.agent_id.0,
        },
    )
    .map_err(|error| format!("Could not save the CoCodeOp connection profile: {error}"))?;
    Ok(ConnectionReceipt {
        agent_id: agent.agent_id.0,
        workspace_id: agent.workspace_id.0,
        profile,
    })
}

fn validate_url(url: &Url) -> Result<(), String> {
    identity::require_confidential(url)?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err("Use the CoCodeOp server origin, without a path or credentials".into());
    }
    Ok(())
}

fn profile_path() -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("COCODEOP_CLIENT_CONFIG") {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err("COCODEOP_CLIENT_CONFIG must be an absolute path".into());
        }
        return Ok(path);
    }
    if cfg!(windows) {
        return env::var_os("LOCALAPPDATA")
            .map(|parent| PathBuf::from(parent).join("CoCodeOp/client.json"))
            .ok_or_else(|| "Set COCODEOP_CLIENT_CONFIG to a local profile path".into());
    }
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| env::home_dir().map(|home| home.join(".config")))
        .map(|parent| parent.join("cocodeop/client.json"))
        .ok_or_else(|| "Set COCODEOP_CLIENT_CONFIG to a local profile path".into())
}

fn write_profile(path: &Path, profile: &Connection) -> std::io::Result<()> {
    let directory = path
        .parent()
        .ok_or(std::io::Error::other("Invalid profile path"))?;
    fs::create_dir_all(directory)?;
    let mut staged = NamedTempFile::new_in(directory)?;
    let bytes = serde_json::to_vec_pretty(profile)?;
    if bytes.len() > 8192 {
        return Err(std::io::Error::other(
            "Connection profile exceeds 8192 bytes",
        ));
    }
    staged.write_all(&bytes)?;
    staged.as_file().sync_all()?;
    staged.persist(path).map_err(|error| error.error)?;
    Ok(())
}
