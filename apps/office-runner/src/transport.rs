use std::{fs::File, io::Read, os::unix::fs::PermissionsExt, path::Path, time::Duration};

use office_domain::{BearerDigest, ErrorCode};
use office_protocol::{Outcome, Reply};
use reqwest::{Client, RequestBuilder, Url, redirect::Policy};
use rustix::{
    fs::{self, FileType, Mode, OFlags},
    process::geteuid,
};
use serde::de::DeserializeOwned;

pub(super) fn client() -> Result<Client, String> {
    Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(Policy::none())
        .no_proxy()
        .build()
        .map_err(|_| "Could not initialize the runner HTTP client".into())
}

pub(super) fn endpoint(base: &Url, path: &str) -> Result<Url, String> {
    base.join(path)
        .map_err(|_| "Invalid CoCodeOp endpoint".into())
}

pub(super) fn confidential_url(input: &str) -> Result<Url, String> {
    let mut url = Url::parse(input).map_err(|_| "Invalid CoCodeOp endpoint")?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "CoCodeOp endpoint cannot include user information, a query or a fragment".into(),
        );
    }
    let secure = url.scheme() == "https"
        || (url.scheme() == "http"
            && url.host_str().is_some_and(|host| {
                host.trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
            }));
    if !secure || url.host().is_none() {
        return Err("Use HTTPS or an explicit loopback IP to send runner credentials".into());
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}

pub(super) fn read_token(path: &Path) -> Result<String, String> {
    let fd = fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|_| "Could not open the local runner token file")?;
    let stat = fs::fstat(&fd).map_err(|_| "Could not inspect the runner token file")?;
    let file = File::from(fd);
    let permissions = file
        .metadata()
        .map_err(|_| "Could not inspect the runner token file")?
        .permissions();
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || permissions.mode() & 0o077 != 0
        || stat.st_uid != geteuid().as_raw()
        || stat.st_nlink != 1
    {
        return Err("The runner token must be stored in a regular owner-only file".into());
    }
    let mut token = String::new();
    file.take(128)
        .read_to_string(&mut token)
        .map_err(|_| "Could not read the runner token file")?;
    let token = token.trim().to_owned();
    if BearerDigest::of(&token).is_none() {
        return Err("Invalid runner token format".into());
    }
    Ok(token)
}

pub(super) async fn request<T: DeserializeOwned>(
    request: RequestBuilder,
    token: &str,
) -> Result<T, String> {
    request_typed(request, token)
        .await
        .map_err(|error| error.to_string())
}

pub(super) enum ControlError {
    Rejected(ErrorCode, String),
    Transport(&'static str),
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(code, message) => write!(formatter, "{code:?} · {message}"),
            Self::Transport(message) => formatter.write_str(message),
        }
    }
}

pub(super) async fn request_typed<T: DeserializeOwned>(
    request: RequestBuilder,
    token: &str,
) -> Result<T, ControlError> {
    let response = request
        .bearer_auth(token)
        .send()
        .await
        .map_err(|_| ControlError::Transport("Could not reach the CoCodeOp server"))?;
    let status = response.status();
    let body: Reply<T> = response
        .json()
        .await
        .map_err(|_| ControlError::Transport("CoCodeOp returned an unreadable response"))?;
    match body.outcome {
        Outcome::Success { result } if status.is_success() => Ok(result),
        Outcome::Failure { error } => Err(ControlError::Rejected(error.code, error.message)),
        _ => Err(ControlError::Transport(
            "CoCodeOp returned an inconsistent response",
        )),
    }
}

pub(super) fn print_json(output: &impl serde::Serialize) -> Result<(), String> {
    let payload = serde_json::to_string(output).map_err(|_| "Could not encode the receipt")?;
    println!("{payload}");
    Ok(())
}
