use std::{path::PathBuf, time::Duration};

use axum::{
    Json,
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::{fs::File, io::AsyncReadExt, time::timeout};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Actor {
    LeadA,
    LeadB,
}

impl Actor {
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::LeadA => "lead-a",
            Self::LeadB => "lead-b",
        }
    }

    pub(crate) fn fixture_file(self) -> &'static str {
        match self {
            Self::LeadA => "lead-a.txt",
            Self::LeadB => "lead-b.txt",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialHashes {
    lead_a: Option<String>,
    lead_b: Option<String>,
}

impl CredentialHashes {
    async fn load(path: PathBuf) -> Option<Self> {
        const MAX_SIZE: u64 = 1_024;
        let mut bytes = Vec::new();
        let file = File::open(path).await.ok()?;
        file.take(MAX_SIZE + 1).read_to_end(&mut bytes).await.ok()?;
        if bytes.len() > MAX_SIZE as usize {
            return None;
        }
        serde_json::from_slice(&bytes).ok()
    }

    fn authenticate(self, token: &str) -> Result<Option<Actor>, ()> {
        let hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        let a = decode_hash(self.lead_a)?;
        let b = decode_hash(self.lead_b)?;
        if a.is_some() && a == b {
            return Err(());
        }

        let matches_a = a.is_some_and(|candidate| bool::from(candidate.ct_eq(&hash)));
        let matches_b = b.is_some_and(|candidate| bool::from(candidate.ct_eq(&hash)));
        Ok(match (matches_a, matches_b) {
            (true, false) => Some(Actor::LeadA),
            (false, true) => Some(Actor::LeadB),
            _ => None,
        })
    }
}

fn decode_hash(encoded: Option<String>) -> Result<Option<[u8; 32]>, ()> {
    encoded
        .map(|value| {
            let mut bytes = [0; 32];
            hex::decode_to_slice(value, &mut bytes).map_err(|_| ())?;
            Ok(bytes)
        })
        .transpose()
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let (scheme, token) = values.next()?.to_str().ok()?.split_once(' ')?;
    if values.next().is_some() || !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    // The probe launcher generates 32 random bytes encoded as lowercase hex.
    (token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(token)
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        Json(json!({"code": "UNAUTHENTICATED", "retryable": false})),
    )
        .into_response()
}

pub(crate) async fn authenticate(
    State(path): State<PathBuf>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(token) = bearer(request.headers()) else {
        return unauthorized();
    };

    let identity = timeout(Duration::from_secs(1), CredentialHashes::load(path))
        .await
        .ok()
        .flatten()
        .ok_or(())
        .and_then(|credentials| credentials.authenticate(token));

    let actor = match identity {
        Ok(Some(actor)) => actor,
        Ok(None) => return unauthorized(),
        Err(()) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"code": "CREDENTIAL_STORE_UNAVAILABLE", "retryable": true})),
            )
                .into_response();
        }
    };
    request.headers_mut().remove(header::AUTHORIZATION);
    request.extensions_mut().insert(actor);
    next.run(request).await
}
