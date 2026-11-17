use std::{
    env,
    fs::File,
    io::{ErrorKind, Read, Write},
    path::Path,
};

use office_domain::{AgentRole, BearerDigest, CredentialId, WorkspaceId};
use office_protocol::{
    AcceptInvitation, AgentIdentity, CreateAgent, CreateInvitation, CreateWorkspace,
    CredentialSubject, Identity, Invitation, IssueCredential, IssuedCredential, Outcome, Reply,
    RevokeCredential, RevokedCredential, WorkspaceMembership,
};
use office_store::Store;
use reqwest::{Method, RequestBuilder, Url};
use tempfile::NamedTempFile;
use uuid::Uuid;

pub async fn bootstrap(
    user: &str,
    workspace: &str,
    token_path: &Path,
) -> Result<Identity, Box<dyn std::error::Error>> {
    let url = env::var("OFFICE_DATABASE_URL")
        .map_err(|_| "set OFFICE_DATABASE_URL for the local operator CLI")?;
    let password_file = env::var_os("OFFICE_DATABASE_PASSWORD_FILE").map(std::path::PathBuf::from);
    let config = office_store::database_options(&url, password_file.as_deref())?;
    let store = Store::connect(config, 2).await?;
    let digest = BearerDigest::of(&local_token(token_path, true)?).expect("validated token file");
    let identity = store
        .bootstrap_installation(user, workspace, &digest)
        .await?;
    store.close().await;
    Ok(identity)
}

fn local_token(path: &Path, create: bool) -> Result<String, Box<dyn std::error::Error>> {
    match File::open(path) {
        Ok(file) => {
            let mut contents = String::new();
            file.take(67).read_to_string(&mut contents)?;
            if contents.len() > 66 {
                return Err("invalid credential file".into());
            }
            let token = contents.trim_end_matches(['\r', '\n']);
            BearerDigest::of(token).ok_or("invalid credential file")?;
            Ok(token.to_owned())
        }
        Err(error) if create && error.kind() == ErrorKind::NotFound => {
            let mut random = [0u8; 32];
            getrandom::fill(&mut random).map_err(|_| "could not generate the bearer credential")?;
            let secret = hex::encode(random);
            let directory = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let mut staged = NamedTempFile::new_in(directory)?;
            writeln!(staged, "{secret}")?;
            staged.as_file().sync_all()?;
            staged
                .persist_noclobber(path)
                .map_err(|error| error.error)?;
            Ok(secret)
        }
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn read_local_token(path: &Path) -> Result<String, String> {
    local_token(path, false)
        .map_err(|_| "Could not read a valid local agent credential file".into())
}

pub async fn whoami(url: Url) -> Result<Identity, String> {
    require_confidential(&url)?;
    let token = user_token()?;
    reply(super::request(Method::GET, url, "/v1/me")?, &token).await
}

pub async fn create_workspace(
    url: Url,
    name: String,
    idempotency_key: Uuid,
) -> Result<WorkspaceMembership, String> {
    require_confidential(&url)?;
    let owner = user_token()?;
    let body = CreateWorkspace {
        name,
        idempotency_key,
    };
    reply(
        super::request(Method::POST, url, "/v1/workspaces")?.json(&body),
        &owner,
    )
    .await
}

pub async fn invite(
    url: Url,
    workspace: WorkspaceId,
    name: String,
    path: &Path,
) -> Result<Invitation, String> {
    require_confidential(&url)?;
    let owner = user_token()?;
    let local = local_token(path, true).map_err(|error| error.to_string())?;
    let digest = BearerDigest::of(&local).expect("validated token file");
    let body = CreateInvitation {
        idempotency_key: invitation_key(&digest),
        display_name: name,
        invitation_digest: hex::encode(digest.as_bytes()),
    };
    let path = format!("/v1/workspaces/{}/invitations", workspace.0);
    reply(
        super::request(Method::POST, url, &path)?.json(&body),
        &owner,
    )
    .await
}

pub async fn create_agent(
    url: Url,
    workspace: WorkspaceId,
    name: String,
    role: AgentRole,
    token_file: &Path,
) -> Result<AgentIdentity, String> {
    require_confidential(&url)?;
    let owner = user_token()?;
    let token = local_token(token_file, true).map_err(|error| error.to_string())?;
    let digest = BearerDigest::of(&token).expect("validated token file");
    let body = CreateAgent {
        idempotency_key: invitation_key(&digest),
        display_name: name,
        role,
        credential_digest: hex::encode(digest.as_bytes()),
    };
    let path = format!("/v1/workspaces/{}/agents", workspace.0);
    reply(
        super::request(Method::POST, url, &path)?.json(&body),
        &owner,
    )
    .await
}

pub async fn join(url: Url, invitation: &Path, token_file: &Path) -> Result<Identity, String> {
    require_confidential(&url)?;
    let invitation = local_token(invitation, false).map_err(|error| error.to_string())?;
    let token = local_token(token_file, true).map_err(|error| error.to_string())?;
    let digest = BearerDigest::of(&invitation).expect("validated token file");
    let body = AcceptInvitation {
        idempotency_key: invitation_key(&digest),
        invitation_token: invitation,
    };
    reply(
        super::request(Method::POST, url, "/v1/invitations/accept")?.json(&body),
        &token,
    )
    .await
}

fn invitation_key(digest: &BearerDigest) -> Uuid {
    Uuid::from_slice(&digest.as_bytes()[..16]).expect("digest contains 16 bytes")
}

pub async fn issue_credential(
    url: Url,
    subject: CredentialSubject,
    token_file: &Path,
) -> Result<IssuedCredential, String> {
    require_confidential(&url)?;
    let owner = user_token()?;
    let token = local_token(token_file, true).map_err(|error| error.to_string())?;
    let digest = BearerDigest::of(&token).expect("validated token file");
    let body = IssueCredential {
        idempotency_key: invitation_key(&digest),
        subject,
        credential_digest: hex::encode(digest.as_bytes()),
    };
    reply(
        super::request(Method::POST, url, "/v1/credentials")?.json(&body),
        &owner,
    )
    .await
}

pub async fn revoke_credential(
    url: Url,
    credential: CredentialId,
) -> Result<RevokedCredential, String> {
    require_confidential(&url)?;
    let owner = user_token()?;
    let path = format!("/v1/credentials/{}/revoke", credential.0);
    let body = RevokeCredential {
        idempotency_key: credential.0,
    };
    reply(
        super::request(Method::POST, url, &path)?.json(&body),
        &owner,
    )
    .await
}

pub async fn register_runner(
    url: Url,
    workspace: WorkspaceId,
    name: String,
    token_file: &Path,
) -> Result<office_protocol::RunnerIdentity, String> {
    require_confidential(&url)?;
    let owner = user_token()?;
    let token = local_token(token_file, true).map_err(|error| error.to_string())?;
    let digest = BearerDigest::of(&token).expect("validated token file");
    let body = office_protocol::CreateRunner {
        idempotency_key: invitation_key(&digest),
        name,
        credential_digest: hex::encode(digest.as_bytes()),
    };
    let path = format!("/v1/workspaces/{}/runners", workspace.0);
    reply(
        super::request(Method::POST, url, &path)?.json(&body),
        &owner,
    )
    .await
}

pub async fn whoami_runner(url: Url) -> Result<office_protocol::RunnerIdentity, String> {
    require_confidential(&url)?;
    let token = user_token()?;
    reply(
        super::request(Method::GET, url, "/v1/runners/self")?,
        &token,
    )
    .await
}

pub async fn authenticated_post<T: serde::de::DeserializeOwned>(
    url: Url,
    path: &str,
    input: &impl serde::Serialize,
) -> Result<T, String> {
    require_confidential(&url)?;
    let token = user_token()?;
    reply(super::request(Method::POST, url, path)?.json(input), &token).await
}

pub(crate) async fn post_file<
    Input: serde::de::DeserializeOwned + serde::Serialize,
    Output: serde::de::DeserializeOwned,
>(
    file: &Path,
    url: Url,
    path: &str,
) -> Result<Output, String> {
    let bytes = tokio::fs::read(file)
        .await
        .map_err(|_| "could not read request file")?;
    let input: Input = serde_json::from_slice(&bytes).map_err(|_| "invalid request JSON")?;
    authenticated_post(url, path, &input).await
}

pub(crate) fn require_confidential(url: &Url) -> Result<(), String> {
    let confidential = url.scheme() == "https"
        || (url.scheme() == "http"
            && url.host_str().is_some_and(|host| {
                host == "localhost"
                    || host
                        .trim_matches(['[', ']'])
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            }));
    if !confidential {
        return Err("Use HTTPS or the loopback endpoint for credentials".into());
    }
    Ok(())
}

pub(crate) fn user_token() -> Result<String, String> {
    let token =
        env::var("OFFICE_TOKEN").map_err(|_| "set OFFICE_TOKEN from the local credential file")?;
    if BearerDigest::of(&token).is_none() {
        return Err("invalid OFFICE_TOKEN format".into());
    }
    Ok(token)
}

pub(crate) async fn reply<T: serde::de::DeserializeOwned>(
    request: RequestBuilder,
    token: &str,
) -> Result<T, String> {
    let reply = request
        .bearer_auth(token)
        .send()
        .await
        .map_err(|_| "could not reach the CoCodeOp server")?;
    let status = reply.status();
    let body: Reply<T> = reply
        .json()
        .await
        .map_err(|_| "invalid CoCodeOp response")?;
    match body.outcome {
        Outcome::Success { result } if status.is_success() => Ok(result),
        Outcome::Failure { error } => Err(error.message),
        _ => Err("unexpected CoCodeOp response status".into()),
    }
}
