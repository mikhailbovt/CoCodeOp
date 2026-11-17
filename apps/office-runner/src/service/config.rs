use std::path::Path;

use office_protocol::{ManagedRuntime, is_sha256, packaged_backend, valid_capabilities};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::inference::ProviderInstallation;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Config {
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub runtimes: Vec<ConfiguredRuntime>,
    #[serde(default)]
    pub providers: Vec<ProviderInstallation>,
}

impl Config {
    pub fn validate(&self) -> Result<(), String> {
        if self.runtimes.is_empty()
            || self.runtimes.len() > 32
            || self.providers.len() > 16
            || !valid_capabilities(&self.capabilities, 32)
        {
            return Err(
                "Configure 1–32 runtime templates, at most 16 inference profiles and at most 32 unique capabilities".into(),
            );
        }
        for (index, provider) in self.providers.iter().enumerate() {
            provider.profile.validate()?;
            if self.providers[..index]
                .iter()
                .any(|item| item.profile.name == provider.profile.name)
                || provider
                    .bearer_file
                    .as_ref()
                    .is_some_and(|path| !path.is_absolute())
            {
                return Err(
                    "Inference profiles require unique names and absolute credential file paths"
                        .into(),
                );
            }
        }
        for (index, runtime) in self.runtimes.iter().enumerate() {
            runtime.manifest.validate()?;
            let installation = self.provider_for(&runtime.manifest)?;
            let expected_api = match packaged_backend(&runtime.manifest.adapter_id) {
                Some("codex") => Some("responses"),
                Some("opencode") => Some("chat_completions"),
                _ => None,
            };
            if expected_api.is_some_and(|api| {
                installation.is_none_or(|profile| profile.profile.api.name() != api)
            }) {
                return Err(
                    "The native worker needs a provider profile for its supported API".into(),
                );
            }
            if self.runtimes[..index]
                .iter()
                .any(|item| item.template_id == runtime.template_id)
            {
                return Err("The local runner configuration repeats a runtime template ID".into());
            }
        }
        Ok(())
    }

    pub fn provider_for(
        &self,
        manifest: &Manifest,
    ) -> Result<Option<&ProviderInstallation>, String> {
        let Some(expected) = &manifest.provider_profile_sha256 else {
            return Ok(None);
        };
        self.providers
            .iter()
            .find(|provider| {
                provider
                    .profile
                    .digest()
                    .is_ok_and(|digest| &digest == expected)
            })
            .map(Some)
            .ok_or_else(|| {
                "The runtime has no installed inference profile matching its approved digest".into()
            })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ConfiguredRuntime {
    pub template_id: Uuid,
    pub manifest: Manifest,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Manifest {
    pub adapter_id: String,
    pub image_id: String,
    pub entrypoint: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub timeout_seconds: u16,
    pub memory_mib: u16,
    pub cpus: u8,
    pub pids_limit: u16,
    #[serde(default, skip_serializing_if = "disabled")]
    pub run_mcp: bool,
    #[serde(default, skip_serializing_if = "disabled")]
    pub checkout_gateway: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_profile_sha256: Option<String>,
}

fn disabled(value: &bool) -> bool {
    !*value
}

impl Manifest {
    pub fn validate(&self) -> Result<(), String> {
        if !valid_capabilities(std::slice::from_ref(&self.adapter_id), 1)
            || self.image_id.len() != 64
            || !self
                .image_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
            || !Path::new(&self.entrypoint).is_absolute()
            || self.entrypoint.len() > 512
            || self
                .entrypoint
                .split('/')
                .any(|part| matches!(part, "." | ".."))
            || self.entrypoint.contains('\0')
            || self.args.len() > 64
            || self
                .args
                .iter()
                .any(|arg| arg.len() > 8192 || arg.contains('\0'))
            || !(5..=3600).contains(&self.timeout_seconds)
            || !(64..=16_384).contains(&self.memory_mib)
            || !(1..=16).contains(&self.cpus)
            || !(16..=1024).contains(&self.pids_limit)
            || (self.checkout_gateway && !self.run_mcp)
            || self
                .provider_profile_sha256
                .as_deref()
                .is_some_and(|value| !is_sha256(value))
        {
            return Err("Invalid OCI runtime manifest. Use a pinned local image ID and bounded executable, arguments and limits".into());
        }
        if let Some(backend) = packaged_backend(&self.adapter_id)
            && (!self.run_mcp
                || self.entrypoint != "/usr/local/bin/office-runner"
                || !self.args.starts_with(&[
                    "worker-agent".into(),
                    "--backend".into(),
                    backend.into(),
                ])
                || (backend != "jsonl"
                    && (self.args.len() != 3 || self.provider_profile_sha256.is_none())))
        {
            return Err("The approved agent manifest requires its packaged worker-agent entrypoint and run MCP".into());
        }
        Ok(())
    }

    pub fn digest(&self) -> Result<String, String> {
        let bytes =
            serde_json::to_vec(self).map_err(|_| "Could not encode the runtime manifest")?;
        let mut hash = Sha256::new();
        hash.update(b"cocodeop/oci-runtime/v1\0");
        hash.update(bytes);
        Ok(hex::encode(hash.finalize()))
    }

    pub fn verify(&self, approved: &ManagedRuntime) -> Result<(), String> {
        if self.adapter_id != approved.adapter_id
            || approved.manifest_sha256.as_deref() != Some(&self.digest()?)
        {
            return Err(
                "The installed runtime manifest differs from the owner-approved digest".into(),
            );
        }
        Ok(())
    }
}
