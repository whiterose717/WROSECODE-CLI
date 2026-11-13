use anyhow::Result;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Live tail ring size kept for interleaved stdout+stderr while a command
/// runs (the dashboard renders the last slice of this buffer).
const LIVE_TAIL_BYTES: usize = 64 * 1024;
/// Final stdout/stderr kept in the registry after a command finishes, so a
/// long scan cannot pin megabytes of history for the dashboard.
const DONE_RETAIN_BYTES: usize = 256 * 1024;
/// Registry capacity: the oldest finished entries are dropped first.
const PROC_CAP: usize = 50;

/// Keep only the newest `cap` bytes. Applied while the command still runs
/// (see [`spawn_reader`]) so a `yes`-style flood cannot pin gigabytes before
/// its timeout hits, and again at exit by [`ProcEntry::retain_tail`].
fn keep_tail(buffer: &mut Vec<u8>, cap: usize) {
    if buffer.len() > cap {
        let excess = buffer.len() - cap;
        buffer.drain(..excess);
    }
}

/// One tracked shell command. Shared between the reader tasks that stream
/// its output and the dashboard that samples it.
pub struct ProcEntry {
    pub pid: u32,
    pub command: String,
    pub cwd: String,
    pub started: Instant,
    pub running: AtomicBool,
    pub exit_code: Mutex<Option<i32>>,
    pub timed_out: AtomicBool,
    stdout: Mutex<Vec<u8>>,
    stderr: Mutex<Vec<u8>>,
    live: Mutex<Vec<u8>>,
}

impl ProcEntry {
    fn new(pid: u32, command: &str, cwd: &str) -> Self {
        Self {
            pid,
            command: command.to_string(),
            cwd: cwd.to_string(),
            started: Instant::now(),
            running: AtomicBool::new(true),
            exit_code: Mutex::new(None),
            timed_out: AtomicBool::new(false),
            stdout: Mutex::new(Vec::new()),
            stderr: Mutex::new(Vec::new()),
            live: Mutex::new(Vec::new()),
        }
    }

    fn push_live(&self, bytes: &[u8]) {
        let mut live = match self.live.lock() {
            Ok(live) => live,
            Err(poisoned) => poisoned.into_inner(),
        };
        live.extend_from_slice(bytes);
        if live.len() > LIVE_TAIL_BYTES {
            let excess = live.len() - LIVE_TAIL_BYTES;
            live.drain(..excess);
        }
    }

    fn finish(&self, exit_code: Option<i32>, timed_out: bool) {
        *match self.exit_code.lock() {
            Ok(code) => code,
            Err(poisoned) => poisoned.into_inner(),
        } = exit_code;
        self.timed_out.store(timed_out, Ordering::Relaxed);
        self.running.store(false, Ordering::Release);
    }

    fn retain_tail(&self) {
        for buffer in [&self.stdout, &self.stderr] {
            let mut guard = match buffer.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            keep_tail(&mut guard, DONE_RETAIN_BYTES);
        }
    }
}

/// Newest-last view of the registry for the dashboard's Processes panel.
#[derive(Clone, Debug)]
pub struct ProcSnapshot {
    pub pid: u32,
    pub command: String,
    pub cwd: String,
    pub started: Instant,
    pub running: bool,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub cpu_pct: Option<f64>,
    pub rss_kb: Option<u64>,
    pub tail: String,
}

impl ProcSnapshot {
    fn of(entry: &ProcEntry) -> Self {
        let running = entry.running.load(Ordering::Acquire);
        // Sampling an exited PID could show stats for an unrelated process
        // after the kernel reuses its PID. It also wastes two procfs reads.
        let (cpu_pct, rss_kb) = if running {
            // Include active script subprocesses rather than only the
            // sh -c wrapper (which often uses nearly zero CPU itself).
            proc_usage_tree(entry.pid)
        } else {
            (None, None)
        };
        let live = entry
            .live
            .lock()
            .map(|live| String::from_utf8_lossy(&live).into_owned())
            .unwrap_or_default();
        Self {
            pid: entry.pid,
            command: entry.command.clone(),
            cwd: entry.cwd.clone(),
            started: entry.started,
            running,
            exit_code: *entry.exit_code.lock().unwrap_or_else(|p| p.into_inner()),
            timed_out: entry.timed_out.load(Ordering::Relaxed),
            cpu_pct,
            rss_kb,
            tail: live,
        }
    }
}

fn registry() -> &'static Mutex<Vec<Arc<ProcEntry>>> {
    static PROCS: OnceLock<Mutex<Vec<Arc<ProcEntry>>>> = OnceLock::new();
    PROCS.get_or_init(|| Mutex::new(Vec::new()))
}

fn register(entry: Arc<ProcEntry>) {
    let mut procs = registry().lock().unwrap_or_else(|p| p.into_inner());
    procs.push(entry);
    // Retain the newest commands. Remove the oldest *finished* entry first,
    // and never lose a running child that the user may need to cancel.
    while procs.len() > PROC_CAP {
        let Some(oldest_finished) = procs
            .iter()
            .position(|entry| !entry.running.load(Ordering::Relaxed))
        else {
            break;
        };
        procs.remove(oldest_finished);
    }
}

/// Newest-last snapshot of every tracked command (bounded by [`PROC_CAP`]).
pub fn proc_snapshots() -> Vec<ProcSnapshot> {
    // Read process status outside the registry lock: procfs calls and text
    // conversion should never block other commands from registering.
    let entries = registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    entries
        .iter()
        .map(|entry| ProcSnapshot::of(entry))
        .collect()
}

/// Send SIGINT (or SIGTERM when `kill` is set) to a tracked process.
///
/// Tracked commands run as process-group leaders (see [`run_with_timeout`]),
/// so the signal goes to the whole group first: grandchildren must not
/// outlive a dashboard kill. A plain-pid delivery is the fallback when the
/// group signal is refused (foreign pid, platform without group signalling).
pub fn signal(pid: u32, kill: bool) -> Result<String> {
    if pid == 0 {
        return Err(anyhow::anyhow!("cannot signal an unknown pid"));
    }
    let tracked = registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .find(|entry| entry.pid == pid)
        .cloned();
    if tracked
        .as_ref()
        .is_some_and(|entry| !entry.running.load(Ordering::Acquire))
    {
        return Ok(format!("process {pid} already exited"));
    }

    #[cfg(unix)]
    {
        let signal = if kill { libc::SIGTERM } else { libc::SIGINT };
        // The supervisor owns the process group. ESRCH is a normal race with
        // a child that exited between the state check and the signal. A
        // manually attached dashboard row may be a plain child, so fall back
        // to its pid without invoking an external `kill` utility.
        let group_result = unsafe { libc::kill(-(pid as libc::pid_t), signal) };
        let group_errno = std::io::Error::last_os_error().raw_os_error();
        let result = if group_result == 0 {
            0
        } else if group_errno == Some(libc::ESRCH) {
            unsafe { libc::kill(pid as libc::pid_t, signal) }
        } else {
            group_result
        };
        let errno = std::io::Error::last_os_error().raw_os_error();
        if result == 0 || errno == Some(libc::ESRCH) {
            if let Some(entry) = tracked {
                entry.running.store(false, Ordering::Release);
            }
            return Ok(format!(
                "SIG{} sent to pid {pid}",
                if kill { "TERM" } else { "INT" }
            ));
        }
        Err(std::io::Error::last_os_error().into())
    }
    #[cfg(not(unix))]
    {
        let _ = (kill, tracked);
        Err(anyhow::anyhow!(
            "process cancellation is unavailable on this platform"
        ))
    }
}

/// Average CPU percent since spawn and resident set size, read from procfs.
/// Non-Linux platforms get `(None, None)` and the panel prints `n/a`.
#[cfg(test)]
pub fn proc_usage(pid: u32) -> (Option<f64>, Option<u64>) {
    #[cfg(target_os = "linux")]
    {
        use std::io::Read as _;
        let mut stat = String::new();
        if std::fs::File::open(format!("/proc/{pid}/stat"))
            .and_then(|mut file| file.read_to_string(&mut stat))
            .is_err()
        {
            return (None, None);
        }
        // Fields after the last ')' are 1-indexed from `state`; utime and
        // stime sit at 14 and 15 (1-based) => indices 11 and 12 here.
        let rest = stat.rsplit(')').next().unwrap_or_default();
        let fields: Vec<&str> = rest.split_whitespace().collect();
        let ticks = fields
            .get(11)
            .zip(fields.get(12))
            .and_then(|(u, s)| Some(u.parse::<u64>().ok()? + s.parse::<u64>().ok()?));
        let cpu = ticks.map(|ticks| {
            let elapsed = pid_started(pid)
                .map(|start| start.elapsed().as_secs_f64())
                .filter(|secs| *secs > 0.0)
                .unwrap_or(1.0);
            ticks as f64 / 100.0 / elapsed * 100.0
        });
        let mut statm = String::new();
        let rss_kb = std::fs::File::open(format!("/proc/{pid}/statm"))
            .and_then(|mut file| file.read_to_string(&mut statm))
            .ok()
            .and_then(|_| statm.split_whitespace().nth(1).map(str::to_owned))
            .and_then(|pages| pages.parse::<u64>().ok())
            .map(|pages| pages * 4);
        (cpu, rss_kb)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        (None, None)
    }
}

/// Approximate CPU and resident memory of the tracked command's active
/// process tree. The procfs task children file avoids scanning all OS
/// processes each time a WROSECODE tool live status refreshes.
#[cfg(target_os = "linux")]
fn proc_usage_tree(root_pid: u32) -> (Option<f64>, Option<u64>) {
    use std::collections::HashSet;

    let mut pending = vec![root_pid];
    let mut seen = HashSet::new();
    let mut cpu_ticks = 0u64;
    let mut rss_kb = 0u64;
    let mut sampled = 0u32;
    let page_kb = (unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1024) as u64) / 1024;

    while let Some(pid) = pending.pop() {
        // Hard cap limits procfs work under fork-heavy scripts while leaving
        // the live rendering responsive.
        if seen.len() >= 96 || !seen.insert(pid) {
            continue;
        }
        let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => stat,
            Err(_) => continue,
        };
        let fields: Vec<&str> = stat
            .rsplit(')')
            .next()
            .unwrap_or_default()
            .split_whitespace()
            .collect();
        if fields.len() < 13 {
            continue;
        }
        let ticks = fields[11]
            .parse::<u64>()
            .ok()
            .zip(fields[12].parse::<u64>().ok())
            .map(|(user, system)| user.saturating_add(system));
        if let Some(ticks) = ticks {
            cpu_ticks = cpu_ticks.saturating_add(ticks);
            sampled += 1;
        }
        if let Ok(mem) = std::fs::read_to_string(format!("/proc/{pid}/statm")) {
            if let Some(pages) = mem
                .split_whitespace()
                .nth(1)
                .and_then(|pages| pages.parse::<u64>().ok())
            {
                rss_kb = rss_kb.saturating_add(pages.saturating_mul(page_kb));
            }
        }
        if let Ok(children) = std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")) {
            pending.extend(
                children
                    .split_whitespace()
                    .filter_map(|value| value.parse::<u32>().ok()),
            );
        }
    }
    if sampled == 0 {
        return (None, None);
    }
    let hz = (unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).max(1) as f64;
    let elapsed = pid_started(root_pid)
        .map(|start| start.elapsed().as_secs_f64())
        .filter(|seconds| *seconds > 0.0)
        .unwrap_or(1.0);
    // CPU is a process-lifetime average; RSS sums current processes and
    // may include shared pages more than once. Both are approximate.
    (Some(cpu_ticks as f64 / hz / elapsed * 100.0), Some(rss_kb))
}

// On platforms without Linux procfs, the dashboard reports unavailable
// process metrics rather than failing to compile or reporting invented data.
#[cfg(not(target_os = "linux"))]
fn proc_usage_tree(_root_pid: u32) -> (Option<f64>, Option<u64>) {
    (None, None)
}

#[cfg(target_os = "linux")]
fn pid_started(pid: u32) -> Option<Instant> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // starttime is field 22 (1-based); `rest` starts at field 3 (state), so
    // its index is 22 - 3 = 19. Age comes from boot-relative ticks.
    let rest = stat.rsplit(')').next()?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let start_ticks: u64 = fields.get(19)?.parse().ok()?;
    let uptime: f64 = std::fs::read_to_string("/proc/uptime")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    let age = uptime - start_ticks as f64 / 100.0;
    if age < 0.0 {
        return None;
    }
    Some(Instant::now() - Duration::from_secs_f64(age))
}

pub fn destructive(command: &str) -> bool {
    let c = command.to_ascii_lowercase();
    [
        "rm -rf",
        "rm -fr",
        "mkfs",
        "dd if=",
        ":(){",
        "chmod -r 777 /",
        "> /dev/sd",
        ">/dev/sd",
        "> /dev/nvme",
        ">/dev/nvme",
    ]
    .iter()
    .any(|needle| c.contains(needle))
}

pub fn read_only(command: &str) -> bool {
    let trimmed = command.trim();
    let first = trimmed.split_whitespace().next().unwrap_or("");
    let simple = ![";", "&&", "||", "|", ">", "<", "`", "$(", "\n"]
        .iter()
        .any(|token| trimmed.contains(token));
    simple
        && matches!(
            first,
            "pwd"
                | "ls"
                | "cat"
                | "rg"
                | "grep"
                | "find"
                | "head"
                | "tail"
                | "wc"
                | "git"
                | "file"
                | "strings"
        )
        && !(first == "git"
            && !trimmed.starts_with("git status")
            && !trimmed.starts_with("git diff")
            && !trimmed.starts_with("git log")
            && !trimmed.starts_with("git show"))
}

pub async fn run(command: &str, cwd: &Path) -> Result<String> {
    run_with_timeout(command, cwd, crate::config::DEFAULT_SHELL_TIMEOUT_SECONDS).await
}

/// Validate a command's syntax without running it. Returns the first stderr
/// line of `bash -n` on failure, `None` when the command parses or when
/// `bash` itself cannot be spawned.
fn bash_syntax_error(command: &str) -> Option<String> {
    let checked = std::process::Command::new("bash")
        .arg("--noprofile")
        .arg("--norc")
        .arg("-n")
        .arg("-c")
        .arg(command)
        .output()
        .ok()?;
    if checked.status.success() {
        return None;
    }
    let stderr = String::from_utf8_lossy(&checked.stderr);
    Some(stderr.lines().next().unwrap_or("syntax error").to_string())
}

/// SIGKILL an entire process group; a no-op once nothing remains (ESRCH is
/// ignored). `pid` is a group leader's id (see `process_group`), never 0 —
/// `kill(-0)` would target our *own* group, so 0 is refused outright.
#[cfg(unix)]
fn kill_group(pid: u32) {
    if pid == 0 {
        return;
    }
    // Do not shell out to `kill`: its diagnostic stderr used to leak into the
    // managed TUI when the process group had already exited. ESRCH is the
    // expected outcome of a cancellation race and is intentionally ignored.
    let _ = unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
}

/// Kills the command's whole process group on drop unless disarmed.
///
/// Spawned commands are group leaders, so this covers the grandchildren a
/// plain kill of `sh` would leave behind — both on a timeout *and* on a
/// cancelled turn (the future is dropped mid-await and `kill_on_drop` only
/// reaches the shell itself). Disarmed after a normal exit: a background
/// daemon the model started on purpose (`cmd &`, exit 0) keeps running,
/// exactly as before this guard existed.
struct GroupGuard {
    pid: u32,
    armed: bool,
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if self.armed {
            #[cfg(unix)]
            kill_group(self.pid);
            #[cfg(not(unix))]
            let _ = self.pid;
        }
    }
}

/// Await a reader without risking a hang: after the shell exits, a
/// background grandchild may still hold the pipe open. Give it a second to
/// flush what is already buffered, then abort the reader task.
async fn reap(mut reader: tokio::task::JoinHandle<()>) {
    if tokio::time::timeout(Duration::from_secs(1), &mut reader)
        .await
        .is_err()
    {
        reader.abort();
    }
}

pub async fn run_with_timeout(command: &str, cwd: &Path, timeout_seconds: u64) -> Result<String> {
    // Rectifier step (SWE-agent pattern): reject generated commands that
    // cannot parse before spawning anything. `bash -n` validates syntax
    // without executing, so a hallucinated one-liner fails here with the
    // compiler's own message instead of burning a turn on `sh: syntax error`
    // output. A missing `bash` binary skips the check rather than breaking
    // execution on exotic hosts.
    if let Some(error) = bash_syntax_error(command) {
        return Err(anyhow::anyhow!(
            "command failed the `bash -n` syntax check: {error} — fix the syntax instead of retrying"
        ));
    }
    let mut process = tokio::process::Command::new("sh");
    process
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // Own process group: one id for the whole tree, so a timeout or the
    // dashboard kill button can take the tree down together.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        process.as_std_mut().process_group(0);
    }
    let mut child = process.spawn()?;
    let pid = child.id().unwrap_or(0);
    let mut group = GroupGuard {
        pid,
        armed: pid != 0,
    };
    let entry = Arc::new(ProcEntry::new(pid, command, &cwd.display().to_string()));
    register(entry.clone());
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stdout_reader = spawn_reader(entry.clone(), stdout_pipe, true);
    let stderr_reader = spawn_reader(entry.clone(), stderr_pipe, false);
    let outcome = tokio::time::timeout(Duration::from_secs(timeout_seconds), child.wait()).await;
    let exit_code = match outcome {
        Ok(Ok(status)) => status.code(),
        Ok(Err(error)) => {
            entry.finish(None, false);
            reap(stdout_reader).await;
            reap(stderr_reader).await;
            return Err(error.into());
        }
        Err(_) => {
            // A SIGCHLD notification can be coalesced while another runtime in
            // the same process is reaping, or arrive before the wait future is
            // registered. If the child has already exited, report its real
            // status instead of a false timeout; only kill when it is genuinely
            // still running.
            match child.try_wait() {
                Ok(Some(status)) => status.code(),
                _ => {
                    // Kill the whole tree *before* awaiting the readers:
                    // grandchildren inheriting the pipes would otherwise keep
                    // them open forever.
                    #[cfg(unix)]
                    kill_group(pid);
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    entry.finish(None, true);
                    reap(stdout_reader).await;
                    reap(stderr_reader).await;
                    entry.retain_tail();
                    return Err(anyhow::anyhow!(
                        "command timed out after {timeout_seconds} seconds and was terminated; \
                         for a legitimately long command, retry with a larger timeout \
                         in seconds (up to 3600), or raise [agent] shell_timeout_seconds"
                    ));
                }
            }
        }
    };
    reap(stdout_reader).await;
    reap(stderr_reader).await;
    let text = {
        let stdout = entry
            .stdout
            .lock()
            .map(|buffer| String::from_utf8_lossy(&buffer).into_owned())
            .unwrap_or_default();
        let stderr = entry
            .stderr
            .lock()
            .map(|buffer| String::from_utf8_lossy(&buffer).into_owned())
            .unwrap_or_default();
        format!("exit={}\n{stdout}{stderr}", exit_code.unwrap_or(-1))
    };
    entry.finish(exit_code, false);
    entry.retain_tail();
    group.armed = false;
    Ok(text)
}

fn spawn_reader(
    entry: Arc<ProcEntry>,
    pipe: Option<impl tokio::io::AsyncRead + Unpin + Send + 'static>,
    is_stdout: bool,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let Some(mut pipe) = pipe else { return };
        use tokio::io::AsyncReadExt as _;
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    let bytes = &chunk[..read];
                    entry.push_live(bytes);
                    let buffer = if is_stdout {
                        &entry.stdout
                    } else {
                        &entry.stderr
                    };
                    if let Ok(mut guard) = buffer.lock() {
                        guard.extend_from_slice(bytes);
                        // Cap while the command runs: flooding output must
                        // not pin memory for the whole timeout window.
                        keep_tail(&mut guard, DONE_RETAIN_BYTES);
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destructive_commands_always_match() {
        assert!(destructive("rm -rf /tmp/data"));
        assert!(destructive("dd if=/dev/zero of=/dev/sda"));
        assert!(!read_only("git status && rm -rf /tmp/data"));
    }

    fn tmp() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wrose-shell-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn active_process_tree_cpu_and_ram_sampling_is_available() {
        let (cpu, memory_kb) = proc_usage_tree(std::process::id());
        assert!(cpu.is_some(), "own procfs CPU counters should be available");
        assert!(
            memory_kb.unwrap_or(0) > 0,
            "own procfs RAM should be visible"
        );
        assert_eq!(proc_usage_tree(u32::MAX), (None, None));
    }

    #[tokio::test]
    async fn malformed_commands_fail_the_bash_n_check_before_spawn() {
        let dir = tmp();
        let error = run_with_timeout("echo \"unterminated", &dir, 5)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("bash -n"), "unexpected error: {error}");
        // A well-formed multi-line command still passes the check.
        let text = run_with_timeout("echo a\necho b", &dir, 5).await.unwrap();
        assert_eq!(text, "exit=0\na\nb\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn streaming_keeps_stdout_before_stderr_and_registers() {
        let dir = tmp();
        let text = run_with_timeout("echo hi; echo bad >&2", &dir, 5)
            .await
            .unwrap();
        assert_eq!(text, "exit=0\nhi\nbad\n");
        let snapshots = proc_snapshots();
        let mine = snapshots
            .iter()
            .find(|proc| proc.command == "echo hi; echo bad >&2")
            .expect("process registered");
        assert!(!mine.running);
        assert_eq!(mine.exit_code, Some(0));
        assert!(mine.tail.contains("hi"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn timeouts_are_reported_and_marked() {
        let dir = tmp();
        let error = run_with_timeout("sleep 30", &dir, 1).await.unwrap_err();
        assert!(
            error.to_string().contains("timed out after 1 seconds"),
            "unexpected error: {error}"
        );
        let snapshots = proc_snapshots();
        let mine = snapshots
            .iter()
            .find(|proc| proc.command == "sleep 30")
            .expect("process registered");
        assert!(!mine.running);
        assert!(mine.timed_out);
        assert_eq!(mine.exit_code, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn registry_stays_within_its_cap() {
        let dir = tmp();
        for index in 0..(PROC_CAP + 10) {
            run_with_timeout(&format!("true # {index}"), &dir, 30)
                .await
                .unwrap_or_else(|error| panic!("iteration {index} failed: {error}"));
        }
        assert!(proc_snapshots().len() <= PROC_CAP);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn registry_keeps_newest_finished_commands() {
        let tag = format!(
            "wrose-registry-retention-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        for index in 0..(PROC_CAP + 3) {
            let entry = Arc::new(ProcEntry::new(0, &format!("{tag}-{index}"), "."));
            entry.finish(Some(0), false);
            register(entry);
        }
        let latest = format!("{tag}-{}", PROC_CAP + 2);
        assert!(
            proc_snapshots().iter().any(|entry| entry.command == latest),
            "the newest finished command disappeared from the dashboard"
        );
    }

    #[tokio::test]
    async fn a_timeout_kills_the_whole_process_group() {
        let dir = tmp();
        let marker = dir.join("heartbeat");
        // A grandchild that keeps writing: if the timeout killed only `sh`,
        // this loop would outlive the command and the marker would grow.
        let command = format!(
            "(while true; do echo x >> '{}'; sleep 0.1; done) & sleep 60",
            marker.display()
        );
        let error = run_with_timeout(&command, &dir, 1).await.unwrap_err();
        assert!(
            error.to_string().contains("timed out"),
            "unexpected: {error}"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        let first = std::fs::metadata(&marker).map(|m| m.len()).unwrap_or(0);
        tokio::time::sleep(Duration::from_millis(600)).await;
        let second = std::fs::metadata(&marker).map(|m| m.len()).unwrap_or(0);
        assert_eq!(first, second, "grandchild survived the timeout");
        assert!(first > 0, "heartbeat never started");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_background_daemon_does_not_hold_the_result_open() {
        let dir = tmp();
        // The daemon inherits the stdout pipe and stays alive until a
        // `release` file appears (bounded at 300s so a regression still
        // finishes). A reader that awaited EOF would not return until the
        // daemon exits; the bounded reader returns while the daemon is
        // still running. Liveness — not wall-clock time — is the assertion,
        // so a stalled or overloaded host cannot flake it.
        let command =
            "(i=0; while [ ! -e release ] && [ $i -lt 300 ]; do sleep 1; i=$((i+1)); done & echo pid=$!) ; echo ok";
        let text = run_with_timeout(command, &dir, 30).await.unwrap();
        assert!(text.contains("ok"));
        let pid: u32 = text
            .lines()
            .find_map(|line| line.trim().strip_prefix("pid="))
            .and_then(|raw| raw.trim().parse().ok())
            .expect("the daemon echoes its pid");
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        assert!(
            alive,
            "the reader held the result open until the daemon (pid {pid}) exited"
        );
        let _ = std::process::Command::new("kill")
            .arg(pid.to_string())
            .status();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_flooding_command_keeps_only_the_newest_output() {
        let dir = tmp();
        let text = run_with_timeout("yes | head -c 4000000", &dir, 30)
            .await
            .unwrap();
        assert!(
            text.len() < 600 * 1024,
            "buffers were not capped: {} bytes",
            text.len()
        );
        assert!(text.contains('y'));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn finished_process_snapshot_does_not_sample_reused_pid() {
        // The PID belongs to this still-running test process, while the
        // dashboard entry deliberately represents an exited child. Reading
        // /proc for the entry would display this unrelated process's RSS.
        let pid = std::process::id();
        assert!(proc_usage(pid).1.is_some(), "test process should have RSS");
        let entry = ProcEntry::new(pid, "already completed", ".");
        entry.finish(Some(0), false);
        let snapshot = ProcSnapshot::of(&entry);
        assert!(!snapshot.running);
        assert_eq!(snapshot.exit_code, Some(0));
        assert_eq!(snapshot.cpu_pct, None);
        assert_eq!(snapshot.rss_kb, None);
    }

    #[test]
    fn signalling_pid_zero_is_refused() {
        // `kill(-0)` would signal our own process group — the terminal
        // session running the tests. The guard must reject it outright.
        assert!(signal(0, true).is_err());
        assert!(signal(0, false).is_err());
    }

    #[test]
    fn keep_tail_drops_only_the_oldest_bytes() {
        let mut buffer = b"hello world".to_vec();
        keep_tail(&mut buffer, 5);
        assert_eq!(buffer, b"world");
        keep_tail(&mut buffer, 100);
        assert_eq!(buffer, b"world");
        keep_tail(&mut buffer, 3);
        assert_eq!(buffer, b"rld");
    }
}
