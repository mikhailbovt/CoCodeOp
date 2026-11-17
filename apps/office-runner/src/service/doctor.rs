use std::{fs, path::Path};

use office_protocol::{DoctorReport, ManagedRuntime};
use uuid::Uuid;

use super::{Config, Coordinator, Journal, Manifest, Podman, RunSockets, read_config};
use crate::{
    DoctorOptions,
    inference::provider_secret,
    transport::{endpoint, print_json, request},
};

pub(crate) async fn probe_checkout_isolation(
    state_dir: &Path,
    root: &Path,
    image: &str,
) -> Result<String, String> {
    let root = root
        .to_str()
        .ok_or("The checkout probe requires a UTF-8 path")?;
    let manifest = Manifest {
        image_id: image.into(),
        adapter_id: "checkout-isolation-probe.v1".into(),
        entrypoint: "/bin/sh".into(),
        args: vec![
            "-ec".into(),
            r#"
test -r /task.json
test ! -e /snapshot
test ! -e "$1"
test ! -w /task.json
if ( printf 'probe\n' > /task.json ) 2>/dev/null; then exit 13; fi
printf 'isolated\n'
"#
            .into(),
            "checkout-isolation".into(),
            root.into(),
        ],
        timeout_seconds: 10,
        memory_mib: 128,
        cpus: 1,
        pids_limit: 32,
        run_mcp: false,
        checkout_gateway: false,
        provider_profile_sha256: None,
    };
    manifest.validate()?;
    let probe = tempfile::Builder::new()
        .prefix("checkout-doctor-")
        .tempdir_in(state_dir)
        .map_err(|_| "Could not create private diagnostic workspace")?;
    let task = probe.path().join("task.json");
    fs::write(&task, b"{}").map_err(|_| "Could not create the disposable task brief")?;
    let scratch = probe.path().join("scratch");
    fs::create_dir(&scratch).map_err(|_| "Could not create the diagnostic scratch mount")?;
    let engine = Podman::local(probe.path()).await?;
    let container = engine
        .create(
            Uuid::new_v4(),
            &scratch,
            &task,
            &manifest,
            RunSockets::default(),
            None,
        )
        .await?;
    let result = engine.verify_probe(&container).await;
    let stopped = engine.stop(&container).await;
    let removed = engine.remove(&container).await;
    stopped.and(removed)?;
    result?;
    Ok("Worker OCI cannot see the checkout or modify its mounted read-only task brief".into())
}

#[tokio::main]
pub(crate) async fn run(options: DoctorOptions, json: bool) -> Result<(), String> {
    let mut report = DoctorReport::default();
    let config = read_config::<Config>(&options.file).and_then(|config| {
        config.validate()?;
        Ok(config)
    });
    report.add(
        "configuration",
        config
            .as_ref()
            .map(|item| format!("{} configured templates", item.runtimes.len()))
            .map_err(Clone::clone),
    );

    let journal = Journal::open(&options.state_dir);
    report.add(
        "state",
        match &journal {
            Ok(journal) => match journal.load() {
                Ok(None) => Ok("Owner-only journal and exclusive controller lock".into()),
                Ok(Some(_)) => Err("A durable pending attempt requires reconciliation".into()),
                Err(error) => Err(error),
            },
            Err(error) => Err(error.clone()),
        },
    );
    let podman = Podman::local(&options.state_dir).await;
    report.add(
        "oci",
        match &podman {
            Ok(engine) => engine.version().await,
            Err(error) => Err(error.clone()),
        },
    );
    let coordinator = Coordinator::local(&options.connection);
    report.add(
        "runner",
        match &coordinator {
            Ok(office) => match office.profile().await {
                Ok(Some(profile)) => Ok(format!(
                    "Authenticated runner {} · advertised capacity {} through {}",
                    profile.runner_id.0, profile.capacity, profile.expires_at
                )),
                Ok(None) => Ok("Authenticated runner with no advertised profile".into()),
                Err(error) => Err(error),
            },
            Err(error) => Err(error.clone()),
        },
    );

    if let Ok(config) = &config {
        for runtime in &config.runtimes {
            let label = format!("template/{}", runtime.template_id);
            match &coordinator {
                Ok(office) => {
                    let result = async {
                        let path = format!("v1/runner-scheduler/templates/{}", runtime.template_id);
                        let url = endpoint(&office.base, &path)?;
                        let approved: ManagedRuntime =
                            request(office.client.get(url), &office.token).await?;
                        runtime.manifest.verify(&approved)?;
                        if !approved
                            .required_capabilities
                            .iter()
                            .all(|capability| config.capabilities.contains(capability))
                        {
                            return Err(
                                "The local runner omits a required template capability".into()
                            );
                        }
                        Ok(format!("Approved manifest {}", runtime.manifest.digest()?))
                    }
                    .await;
                    report.add(&label, result);
                }
                Err(_) => report.skip(&label, "Connect to the server to verify this template"),
            }
            let label = format!("backend/{}", runtime.template_id);
            match &podman {
                Ok(engine) => report.add(&label, engine.probe_runtime(&runtime.manifest).await),
                Err(_) => report.skip(&label, "A local rootless Podman engine is required"),
            }
        }
        for provider in &config.providers {
            let checked = provider
                .bearer_file
                .as_deref()
                .map(provider_secret)
                .transpose();
            report.add(
                format!("provider/{}", provider.profile.name),
                checked.map(|_| {
                    format!(
                        "{} · {} · local credential permissions checked",
                        provider.profile.api.name(),
                        provider.profile.model
                    )
                }),
            );
        }
    }

    let passed = report.passed();
    if json {
        print_json(&report)?;
    } else {
        for check in report.checks {
            println!(
                "{:?} · {} · {}",
                check.status, check.component, check.detail
            );
        }
    }
    if passed {
        Ok(())
    } else {
        Err("Runner diagnostics reported a failure".into())
    }
}
