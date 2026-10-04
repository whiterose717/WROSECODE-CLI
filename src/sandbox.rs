use anyhow::{bail, Result};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use tokio::sync::Mutex;

const UNKNOWN: u8 = 0;
const READY: u8 = 1;
const UNAVAILABLE: u8 = 2;

/// How `shell` commands are executed.
///
/// `engine = "none"` runs the command directly on the host. `engine = "docker"`
/// runs it inside a container, which keeps host-side damage and dependency
/// pollution out of the project directory.
#[derive(Clone, Debug)]
pub struct SandboxPolicy {
    pub engine: String,
    pub image: String,
    /// Keep the container alive between commands so later calls skip the cold start.
    pub persistent: bool,
    pub name: String,
    pub network: String,
    pub memory: String,
    pub cpus: String,
    /// Pull the image if it is missing locally instead of failing.
    pub auto_build: bool,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self {
            engine: "none".into(),
            image: "wrosecode-sandbox:latest".into(),
            persistent: true,
            name: "wrosecode-sandbox".into(),
            network: "bridge".into(),
            memory: "2g".into(),
            cpus: "2.0".into(),
            auto_build: false,
        }
    }
}

pub struct Sandbox {
    policy: SandboxPolicy,
    root: PathBuf,
    state: AtomicU8,
    boot_lock: Mutex<()>,
}

impl Sandbox {
    pub fn new(policy: SandboxPolicy, root: PathBuf) -> Self {
        Self {
            policy,
            root,
            state: AtomicU8::new(UNKNOWN),
            boot_lock: Mutex::new(()),
        }
    }

    pub fn enabled(&self) -> bool {
        self.policy.engine.eq_ignore_ascii_case("docker")
    }

    pub fn describe(&self) -> String {
        if !self.enabled() {
            return "host".into();
        }
        format!(
            "{} {}",
            self.policy.name,
            if self.policy.persistent {
                "(persistent)"
            } else {
                "(fresh per call)"
            }
        )
    }

    /// Resolve once whether the container runtime is usable.
    async fn ready(&self) -> bool {
        match self.state.load(Ordering::Relaxed) {
            READY => return true,
            UNAVAILABLE => return false,
            _ => {}
        }
        let _guard = self.boot_lock.lock().await;
        match self.state.load(Ordering::Relaxed) {
            READY => return true,
            UNAVAILABLE => return false,
            _ => {}
        }
        let ready = self.bootstrap().await;
        self.state
            .store(if ready { READY } else { UNAVAILABLE }, Ordering::Relaxed);
        ready
    }

    async fn bootstrap(&self) -> bool {
        if !self.enabled() {
            return false;
        }
        if !probe("docker", &["info", "--format", "{{.ServerVersion}}"]).await {
            return false;
        }
        let image = self.policy.image.as_str();
        if !probe("docker", &["image", "inspect", image]).await && !self.provision().await {
            return false;
        }
        if !self.policy.persistent {
            return true;
        }
        let name = self.policy.name.as_str();
        match docker_output(&["inspect", "-f", "{{.State.Running}}", name]).await {
            Ok(state) if state.trim() == "true" => true,
            Ok(_) => probe("docker", &["start", name]).await,
            Err(_) => self.spawn().await,
        }
    }

    /// Make `policy.image` available: build it locally when `auto_build` is on,
    /// otherwise (or as a fallback) pull it from the registry.
    async fn provision(&self) -> bool {
        let image = self.policy.image.as_str();
        if self.policy.auto_build {
            let root = self.root.to_string_lossy().to_string();
            let dockerfile = self.root.join("Dockerfile.sandbox");
            if dockerfile.is_file() {
                let dockerfile = dockerfile.to_string_lossy().to_string();
                let built = exec(
                    "docker",
                    &["build", "-t", image, "-f", &dockerfile, &root],
                    900,
                )
                .await;
                if built.is_ok_and(|out| out.starts_with("exit=0")) {
                    return true;
                }
            }
        }
        probe("docker", &["pull", image]).await
    }

    async fn spawn(&self) -> bool {
        let root = self.root.to_string_lossy();
        let args: Vec<String> = vec![
            "run".into(),
            "-d".into(),
            "--name".into(),
            self.policy.name.clone(),
            "-v".into(),
            format!("{root}:/workspace"),
            "-w".into(),
            "/workspace".into(),
            "--network".into(),
            self.policy.network.clone(),
            "--memory".into(),
            self.policy.memory.clone(),
            "--cpus".into(),
            self.policy.cpus.clone(),
            self.policy.image.clone(),
            "sleep".into(),
            "infinity".into(),
        ];
        probe("docker", &as_refs(&args)).await
    }

    /// Run `command`, returning `exit=N` plus combined output.
    pub async fn run(&self, command: &str, timeout_seconds: u64) -> Result<String> {
        if !self.enabled() {
            return crate::tools::shell::run_with_timeout(command, &self.root, timeout_seconds)
                .await;
        }
        if !self.ready().await {
            bail!(
                "sandbox engine=docker is unavailable. Build the image with \
                 `docker build -f Dockerfile.sandbox -t {} .`, start the Docker daemon, \
                 or set engine = \"none\" under [sandbox] in config.toml",
                self.policy.image
            );
        }
        // In-container deadline: the host-side timeout kills only the docker
        // *client*, which would leave an ephemeral container running (or an
        // exec'd process straggling inside the persistent one). `command -v`
        // degrades to a plain `sh -c` on custom images without coreutils.
        // The command travels as `$1` (an extra argv slot), so it is never
        // interpolated into the script text.
        let shell: Vec<String> = vec![
            "sh".into(),
            "-c".into(),
            format!(
                "if command -v timeout >/dev/null 2>&1; then \
                 timeout -k 5 {timeout_seconds}s sh -c \"$1\"; else sh -c \"$1\"; fi"
            ),
            "sh".into(),
            command.to_string(),
        ];
        let args: Vec<String> = if self.policy.persistent {
            let mut args = vec![
                "exec".into(),
                "-w".into(),
                "/workspace".into(),
                self.policy.name.clone(),
            ];
            args.extend(shell);
            args
        } else {
            let root = self.root.to_string_lossy();
            let mut args = vec![
                "run".into(),
                "--rm".into(),
                "-v".into(),
                format!("{root}:/workspace"),
                "-w".into(),
                "/workspace".into(),
                "--network".into(),
                self.policy.network.clone(),
                "--memory".into(),
                self.policy.memory.clone(),
                "--cpus".into(),
                self.policy.cpus.clone(),
                self.policy.image.clone(),
            ];
            args.extend(shell);
            args
        };
        exec("docker", &as_refs(&args), timeout_seconds).await
    }

    /// Stop and remove the persistent container so the next run starts fresh.
    pub async fn reset(&self) -> Result<String> {
        if !self.enabled() {
            return Ok("sandbox disabled".into());
        }
        self.state.store(UNKNOWN, Ordering::Relaxed);
        let name = self.policy.name.as_str();
        exec("docker", &["rm", "-f", name], 30).await
    }

    /// Warm the container up so the first real command pays no cold start.
    pub async fn warm(&self) -> Result<String> {
        if !self.enabled() {
            return Ok("sandbox disabled".into());
        }
        if self.ready().await {
            return Ok(format!("ready · {}", self.describe()));
        }
        bail!(
            "sandbox unavailable · build {} with Dockerfile.sandbox",
            self.policy.image
        )
    }
}

fn as_refs(values: &[String]) -> Vec<&str> {
    values.iter().map(String::as_str).collect()
}

async fn probe(program: &str, args: &[&str]) -> bool {
    exec(program, args, 120)
        .await
        .is_ok_and(|out| out.starts_with("exit=0"))
}

async fn docker_output(args: &[&str]) -> Result<String> {
    exec("docker", args, 60).await
}

async fn exec(program: &str, args: &[&str], timeout_seconds: u64) -> Result<String> {
    let mut process = tokio::process::Command::new(program);
    process
        .args(args)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_seconds),
        process.output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("{program} timed out after {timeout_seconds} seconds"))??;
    Ok(format!(
        "exit={}\n{}{}",
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn host_engine_falls_back_to_local_shell() {
        let sandbox = Sandbox::new(SandboxPolicy::default(), std::env::temp_dir());
        let out = sandbox.run("echo wrose", 10).await.unwrap();
        assert!(out.starts_with("exit=0"));
        assert!(out.contains("wrose"));
    }

    #[test]
    fn disabled_engine_does_not_report_ready() {
        let sandbox = Sandbox::new(SandboxPolicy::default(), PathBuf::from("/tmp"));
        assert!(!sandbox.enabled());
        assert_eq!(sandbox.describe(), "host");
    }
}
