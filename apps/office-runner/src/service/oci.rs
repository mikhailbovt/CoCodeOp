use std::{
    path::{Path, PathBuf},
    process::Output,
    time::Duration,
};

use office_protocol::CommandResources;
use serde::Deserialize;
use tokio::{process::Command, time::timeout};
use uuid::Uuid;

use super::config::Manifest;
use crate::inference::ProviderProfile;

pub(super) struct Podman {
    root: PathBuf,
    home: PathBuf,
    runtime_dir: PathBuf,
}

pub(super) struct Container {
    pub id: String,
    pub state: ContainerState,
}

pub(super) struct CommandMount<'a> {
    pub snapshot: &'a Path,
    pub writable_staging: bool,
    pub resources: &'a CommandResources,
}

#[derive(Default)]
pub(super) struct RunSockets<'a> {
    pub mcp: Option<&'a Path>,
    pub inference: Option<InferenceMount<'a>>,
}

pub(super) struct InferenceMount<'a> {
    pub socket: &'a Path,
    pub profile: &'a ProviderProfile,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct ContainerState {
    pub status: String,
    pub running: bool,
    pub exit_code: i64,
    pub started_at: String,
}

impl ContainerState {
    pub fn started(&self) -> bool {
        !self.started_at.is_empty() && !self.started_at.starts_with("0001-")
    }

    pub fn stopped(&self) -> bool {
        !self.running && matches!(self.status.as_str(), "exited" | "stopped")
    }
}

impl Podman {
    pub async fn local(root: &Path) -> Result<Self, String> {
        let uid = rustix::process::geteuid().as_raw();
        if uid == 0 {
            return Err("Run the controller as a non-root owner account".into());
        }
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or("The owner account must have an absolute HOME")?;
        let runtime_dir = PathBuf::from(format!("/run/user/{uid}"));
        let engine = Self {
            root: root.to_owned(),
            home,
            runtime_dir,
        };
        let security = engine
            .checked(
                "detect rootless Podman",
                &["info", "--format={{.Host.Security.Rootless}}"],
            )
            .await?;
        if security.trim() != "true" {
            return Err("A local rootless Podman service is required".into());
        }
        Ok(engine)
    }

    pub async fn version(&self) -> Result<String, String> {
        self.checked("report its version", &["--version"])
            .await
            .map(|text| text.trim().to_owned())
    }

    pub async fn probe_runtime(&self, manifest: &Manifest) -> Result<String, String> {
        let image = format!("sha256:{}", manifest.image_id);
        let observed = self
            .checked(
                "inspect the locally approved image",
                &["image", "inspect", "--format={{.Id}}", &image],
            )
            .await?;
        if observed.trim().trim_start_matches("sha256:") != manifest.image_id {
            return Err("The inspected image ID differs from the approved runtime".into());
        }
        let backend = office_protocol::packaged_backend(&manifest.adapter_id);
        let native = backend.and_then(crate::worker_agent::backend_executable);
        let entrypoint = native.unwrap_or("/bin/sh");
        let mut args = vec![
            "run",
            "--rm",
            "--pull=never",
            "--network=none",
            "--read-only",
            "--cap-drop=all",
            "--security-opt=no-new-privileges",
            "--user=1000:1000",
            "--pids-limit=32",
            "--memory=256m",
            "--cpus=1",
            "--timeout=8",
            "--entrypoint",
            entrypoint,
            &image,
        ];
        if native.is_some() {
            args.push("--version");
        } else {
            let bridge = manifest.args.windows(2).find(|pair| pair[0] == "--bridge");
            let executable = bridge.map_or(manifest.entrypoint.as_str(), |pair| pair[1].as_str());
            args.extend([
                "-c",
                "test -x \"$1\" && printf '%s\\n' 'executable present'",
                "office-doctor",
                executable,
            ]);
        }
        let output = self.checked("verify the backend executable", &args).await?;
        let observed = output
            .lines()
            .find(|line| !line.trim().is_empty())
            .filter(|line| line.len() <= 160 && !line.chars().any(char::is_control))
            .ok_or("The backend probe returned no bounded version or executable receipt")?;
        Ok(format!("{} · {observed}", manifest.adapter_id))
    }

    pub async fn create(
        &self,
        effect: Uuid,
        scratch: &Path,
        task_file: &Path,
        manifest: &Manifest,
        sockets: RunSockets<'_>,
        command: Option<CommandMount<'_>>,
    ) -> Result<Container, String> {
        let source = scratch
            .to_str()
            .filter(|path| !path.contains(':'))
            .ok_or("The workspace mount needs a UTF-8 path without a colon")?;
        let name = name(effect);
        let task_file = task_file
            .to_str()
            .filter(|path| !path.contains(':'))
            .ok_or("The task mount needs a UTF-8 path without a colon")?;
        let owner_label = format!("--label=io.cocodeop.effect={effect}");
        let volume = format!("--volume={source}:/work:rw,nosuid,nodev");
        let task = format!("--volume={task_file}:/task.json:ro,nosuid,nodev");
        let memory = format!("--memory={}m", manifest.memory_mib);
        let cpu = format!("--cpus={}", manifest.cpus);
        let pids = format!("--pids-limit={}", manifest.pids_limit);
        let duration = format!("--timeout={}", manifest.timeout_seconds);
        let entrypoint = format!("--entrypoint={}", manifest.entrypoint);
        let socket_mount = sockets
            .mcp
            .map(|path| {
                path.to_str()
                    .filter(|path| !path.contains(':'))
                    .map(|path| format!("--volume={path}:/run/cocodeop/mcp.sock:ro,nosuid,nodev"))
                    .ok_or("The MCP socket needs a UTF-8 path without a colon")
            })
            .transpose()?;
        let inference = sockets
            .inference
            .as_ref()
            .map(|mount| {
                let socket = mount
                    .socket
                    .to_str()
                    .filter(|path| !path.contains(':'))
                    .ok_or("The inference socket needs a UTF-8 path without a colon")?;
                Ok::<_, &str>([
                    format!("--volume={socket}:/run/cocodeop/inference.sock:ro,nosuid,nodev"),
                    format!("--env=COCODEOP_MODEL={}", mount.profile.model),
                    format!("--env=COCODEOP_INFERENCE_API={}", mount.profile.api.name()),
                    format!(
                        "--env=COCODEOP_MODEL_OUTPUT_LIMIT={}",
                        mount.profile.max_output_tokens
                    ),
                    "--env=COCODEOP_INFERENCE_SOCKET=/run/cocodeop/inference.sock".into(),
                ])
            })
            .transpose()?;
        let snapshot_mount = command
            .as_ref()
            .map(|mount| {
                mount
                    .snapshot
                    .to_str()
                    .filter(|path| !path.contains(':'))
                    .map(|path| format!("--volume={path}:/snapshot:ro,nosuid,nodev"))
                    .ok_or("The command snapshot needs a UTF-8 path without a colon")
            })
            .transpose()?;
        let workdir = match command.as_ref() {
            Some(mount) if mount.writable_staging => "--workdir=/work/source",
            Some(_) => "--workdir=/snapshot",
            None => "--workdir=/work",
        };
        let mut args = vec![
            "create",
            "--name",
            &name,
            &owner_label,
            "--pull=never",
            "--network=none",
            "--pid=private",
            "--ipc=private",
            "--uts=private",
            "--read-only",
            "--cap-drop=all",
            "--security-opt=no-new-privileges",
            "--http-proxy=false",
            "--image-volume=ignore",
            "--restart=no",
            "--stop-timeout=2",
            "--userns=keep-id:uid=1000,gid=1000",
            "--user=1000:1000",
            "--log-driver=k8s-file",
            "--log-opt=max-size=262144",
            workdir,
            "--env=HOME=/work",
            "--env=COCODEOP_TASK=/task.json",
            "--env=COCODEOP_RESULT=/work/result.json",
            &volume,
            &task,
            &memory,
            &cpu,
            &pids,
            &duration,
            &entrypoint,
        ];
        if let Some(socket) = &socket_mount {
            args.extend([
                socket.as_str(),
                "--env=COCODEOP_MCP_SOCKET=/run/cocodeop/mcp.sock",
            ]);
        }
        if let Some(inference) = &inference {
            args.extend(inference.iter().map(String::as_str));
        }
        if let (Some(command), Some(snapshot)) = (&command, &snapshot_mount) {
            args.extend([snapshot.as_str(), "--env=COCODEOP_OUTPUT=/work"]);
            if command.resources.build_cache {
                args.extend([
                    "--env=CARGO_TARGET_DIR=/work/build-cache",
                    "--env=COCODEOP_BUILD_CACHE=/work/build-cache",
                ]);
            } else {
                args.push("--env=CARGO_TARGET_DIR=/work/target");
            }
            if command.resources.package_cache {
                args.push("--env=COCODEOP_PACKAGE_CACHE=/work/package-cache");
            }
            if command.resources.test_database {
                args.push("--env=COCODEOP_TEST_DB_DIR=/work/test-db");
            }
        }
        args.push(&manifest.image_id);
        args.extend(manifest.args.iter().map(String::as_str));
        let id = self.checked("create the isolated container", &args).await?;
        let container = self.require(effect).await?;
        if container.id != id.trim() || container.state.started() {
            return Err("The created container does not match the requested run".into());
        }
        Ok(container)
    }

    pub async fn inspect(&self, effect: Uuid) -> Result<Option<Container>, String> {
        let name = name(effect);
        let exists = self.command(&["container", "exists", &name]).await?;
        match exists.status.code() {
            Some(1) => return Ok(None),
            Some(0) => (),
            _ => return Err("Podman could not determine if the recorded container exists".into()),
        }
        let inspected = self.checked("inspect the isolated container", &[
            "inspect", "--type=container", "--format={{.Id}} {{index .Config.Labels \"io.cocodeop.effect\"}} {{json .State}}", &name,
        ]).await?;
        let mut values = inspected.trim().splitn(3, ' ');
        let id = values
            .next()
            .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or("Podman did not return a valid container ID")?;
        if values.next() != Some(effect.to_string().as_str()) {
            return Err("The container label does not match the durable start effect".into());
        }
        let state = serde_json::from_str(
            values
                .next()
                .ok_or("Podman did not return a container state")?,
        )
        .map_err(|_| "Podman returned an unrecognized container state")?;
        Ok(Some(Container {
            id: id.into(),
            state,
        }))
    }

    pub async fn require(&self, effect: Uuid) -> Result<Container, String> {
        self.inspect(effect)
            .await?
            .ok_or_else(|| "The recorded container is missing".into())
    }

    pub async fn start(&self, container: &Container) -> Result<(), String> {
        self.checked("start the isolated container", &["start", &container.id])
            .await
            .map(|_| ())
    }

    pub async fn stop(&self, container: &Container) -> Result<(), String> {
        self.checked(
            "stop the isolated container",
            &["stop", "--ignore", "--time=2", &container.id],
        )
        .await
        .map(|_| ())
    }

    pub async fn remove(&self, container: &Container) -> Result<(), String> {
        self.checked(
            "remove the stopped container",
            &["rm", "--ignore", &container.id],
        )
        .await
        .map(|_| ())
    }

    pub async fn logs(&self, container: &Container) -> Result<Output, String> {
        let result = self.command(&["logs", &container.id]).await?;
        if result.status.success() {
            Ok(result)
        } else {
            Err("Podman could not read the recorded command output".into())
        }
    }

    pub async fn verify_probe(&self, container: &Container) -> Result<(), String> {
        self.start(container).await?;
        let code = self
            .checked("wait for the isolation probe", &["wait", &container.id])
            .await?;
        if code.trim() != "0" || self.logs(container).await?.stdout != b"isolated\n" {
            return Err(
                "The worker isolation probe did not confirm a protected filesystem view".into(),
            );
        }
        Ok(())
    }

    async fn checked(&self, operation: &str, args: &[&str]) -> Result<String, String> {
        let result = self.command(args).await?;
        if !result.status.success() {
            return Err(format!("Podman could not {operation} ({})", result.status));
        }
        String::from_utf8(result.stdout).map_err(|_| "Podman returned invalid UTF-8".into())
    }

    async fn command(&self, args: &[&str]) -> Result<Output, String> {
        let mut command = Command::new("/usr/bin/podman");
        command
            .env_clear()
            .env("HOME", &self.home)
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}/bus", self.runtime_dir.display()),
            )
            .env("PATH", "/usr/bin:/bin")
            .current_dir(&self.root)
            .kill_on_drop(true)
            .arg("--remote=false")
            .args(args);
        timeout(Duration::from_secs(12), command.output())
            .await
            .map_err(|_| "Podman did not respond within 12 seconds")?
            .map_err(|_| "Could not execute the local Podman client".into())
    }
}

fn name(effect: Uuid) -> String {
    format!("cocodeop-{effect}")
}
