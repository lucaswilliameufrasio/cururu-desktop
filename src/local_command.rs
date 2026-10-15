use std::{
    ffi::OsString,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, ensure};

use crate::{local_review, repository::RepositorySnapshot};

const MAX_COMMAND_BYTES: usize = 4_096;
const MAX_ARGUMENTS: usize = 64;
const MAX_CAPTURED_STREAM_BYTES: usize = 32 * 1_024;
const COMMAND_TIMEOUT: Duration = Duration::from_mins(10);
const TERMINATION_GRACE: Duration = Duration::from_secs(2);
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(500);
const SUPERVISOR_POLL_INTERVAL: Duration = Duration::from_millis(40);
const SANDBOX_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(target_os = "linux")]
const SANDBOX_WORKSPACE: &str = "/tmp/cururu-workspace";
const SANDBOX_ALTERNATE_WORKSPACE: &str = "/var/tmp/cururu-workspace";
const SANDBOX_HOME: &str = "/tmp/cururu-home";
const SANDBOX_CARGO_HOME: &str = "/tmp/cururu-home/.cargo";
const SANDBOX_RUSTUP_HOME: &str = "/tmp/cururu-rustup";
const SANDBOX_CARGO_BIN: &str = "/tmp/cururu-cargo-bin";
const SANDBOX_TMP: &str = "/tmp/cururu-tmp";
const SANDBOX_PATH: &str = "/usr/local/bin:/usr/bin:/bin";
#[cfg(any(target_os = "macos", test))]
const SEATBELT_READ_PATHS: &[&str] = &[
    "/Applications",
    "/Library/Developer",
    "/Library/Frameworks",
    "/System/Applications",
    "/System/Library",
    "/bin",
    "/opt/homebrew",
    "/private/etc",
    "/sbin",
    "/usr",
    "/usr/local",
    "/usr/sbin",
];
#[cfg(any(target_os = "macos", test))]
const SEATBELT_READ_DEVICES: &[&str] = &[
    "/dev/dtracehelper",
    "/dev/fd",
    "/dev/null",
    "/dev/random",
    "/dev/stderr",
    "/dev/stdin",
    "/dev/stdout",
    "/dev/tty",
    "/dev/urandom",
    "/dev/zero",
];
#[cfg(any(target_os = "macos", test))]
const SEATBELT_WRITE_DEVICES: &[&str] = &["/dev/dtracehelper", "/dev/null", "/dev/zero"];
#[cfg(target_os = "macos")]
const SEATBELT_PATH: &[&str] = &[
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/usr/bin",
    "/bin",
    "/usr/sbin",
    "/sbin",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandContext {
    pub repository_root: PathBuf,
    pub repository_identity: String,
    pub head_oid: Option<String>,
    pub diff_fingerprint: String,
}

impl CommandContext {
    pub fn from_snapshot(snapshot: &RepositorySnapshot) -> anyhow::Result<Self> {
        let repository_root = snapshot
            .root
            .canonicalize()
            .context("could not resolve the local repository path")?;
        Ok(Self {
            repository_identity: local_review::repository_identity(&repository_root)?,
            repository_root,
            head_oid: snapshot.head_oid.clone(),
            diff_fingerprint: local_review::snapshot_fingerprint(snapshot),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCommand {
    pub executable: PathBuf,
    pub canonical_executable: PathBuf,
    pub arguments: Vec<String>,
}

impl ParsedCommand {
    #[must_use]
    pub fn display(&self) -> String {
        let mut words = Vec::with_capacity(self.arguments.len() + 1);
        words.push(self.executable.display().to_string());
        words.extend(self.arguments.iter().cloned());
        shell_words::join(words)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BubblewrapSandbox {
    pub executable: PathBuf,
    pub workspace: PathBuf,
    pub hidden_home: PathBuf,
    pub command_path: PathBuf,
    pub cargo_bin: Option<PathBuf>,
    pub cargo_registry: Option<PathBuf>,
    pub cargo_git: Option<PathBuf>,
    pub rustup_home: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatbeltSandbox {
    pub executable: PathBuf,
    pub hidden_home: PathBuf,
    pub cargo_bin: Option<PathBuf>,
    pub cargo_registry: Option<PathBuf>,
    pub cargo_git: Option<PathBuf>,
    pub rustup_home: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxAvailability {
    Bubblewrap(BubblewrapSandbox),
    Seatbelt(SeatbeltSandbox),
    Unavailable(String),
}

impl SandboxAvailability {
    #[must_use]
    pub const fn is_available(&self) -> bool {
        matches!(self, Self::Bubblewrap(_) | Self::Seatbelt(_))
    }

    #[must_use]
    pub fn description(&self) -> String {
        match self {
            Self::Bubblewrap(_) => {
                "Bubblewrap · checkout read-only · network disabled · user home/credentials hidden"
                    .to_string()
            }
            Self::Seatbelt(_) => {
                "macOS Seatbelt · checkout read-only · network disabled · user home/credentials hidden"
                    .to_string()
            }
            Self::Unavailable(reason) => format!("Unavailable for this command: {reason}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CommandPlan {
    pub context: CommandContext,
    pub command: ParsedCommand,
    pub sandbox: SandboxAvailability,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    Sandboxed,
    Unsandboxed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandOutput {
    pub exit_code: Option<i32>,
    pub termination: Option<CommandTermination>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandTermination {
    Cancelled,
    TimedOut,
    OutputPipeHeldOpen,
}

impl CommandOutput {
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0) && self.termination.is_none()
    }

    #[must_use]
    pub const fn was_cancelled(&self) -> bool {
        matches!(self.termination, Some(CommandTermination::Cancelled))
    }

    #[must_use]
    pub const fn timed_out(&self) -> bool {
        matches!(self.termination, Some(CommandTermination::TimedOut))
    }

    #[must_use]
    pub const fn output_pipe_held_open(&self) -> bool {
        matches!(
            self.termination,
            Some(CommandTermination::OutputPipeHeldOpen)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionResult {
    Completed(CommandOutput),
    Stale,
}

#[derive(Debug)]
struct CapturedStream {
    bytes: Vec<u8>,
    truncated: bool,
}

#[derive(Debug)]
enum CaptureEvent {
    Stdout(io::Result<CapturedStream>),
    Stderr(io::Result<CapturedStream>),
}

struct MonitoredProcess {
    exit_status: Option<std::process::ExitStatus>,
    termination: Option<CommandTermination>,
    stdout_capture: Option<io::Result<CapturedStream>>,
    stderr_capture: Option<io::Result<CapturedStream>>,
}

pub fn prepare(context: CommandContext, input: &str) -> anyhow::Result<CommandPlan> {
    validate_context(&context)?;
    let command = parse_command(input, &context.repository_root)?;
    let sandbox = detect_sandbox(&context.repository_root, &command);
    Ok(CommandPlan {
        context,
        command,
        sandbox,
    })
}

pub fn parse_command(input: &str, repository_root: &Path) -> anyhow::Result<ParsedCommand> {
    ensure!(
        input.len() <= MAX_COMMAND_BYTES,
        "command is longer than {MAX_COMMAND_BYTES} bytes"
    );
    ensure!(
        !input.chars().any(char::is_control),
        "command must be a single line without control characters"
    );

    let mut words = shell_words::split(input).context("command quoting is invalid")?;
    ensure!(!words.is_empty(), "enter an executable and its arguments");
    ensure!(
        words.len() <= MAX_ARGUMENTS,
        "command has more than {MAX_ARGUMENTS} arguments"
    );

    let requested_executable = words.remove(0);
    let executable = resolve_executable(&requested_executable, repository_root)?;
    let canonical_executable = executable
        .canonicalize()
        .with_context(|| format!("could not resolve executable {}", executable.display()))?;
    ensure!(
        canonical_executable.is_file() && is_executable(&canonical_executable),
        "{} is not an executable file",
        executable.display()
    );

    Ok(ParsedCommand {
        executable,
        canonical_executable,
        arguments: words,
    })
}

pub fn run(
    plan: &CommandPlan,
    mode: ExecutionMode,
    cancelled: &AtomicBool,
) -> anyhow::Result<ExecutionResult> {
    validate_context(&plan.context)?;
    if cancelled.load(Ordering::Acquire) {
        return Ok(ExecutionResult::Completed(CommandOutput {
            termination: Some(CommandTermination::Cancelled),
            ..CommandOutput::default()
        }));
    }

    let sandbox = match mode {
        ExecutionMode::Sandboxed => match &plan.sandbox {
            SandboxAvailability::Bubblewrap(_) | SandboxAvailability::Seatbelt(_) => {
                Some(&plan.sandbox)
            }
            SandboxAvailability::Unavailable(reason) => {
                anyhow::bail!("sandbox is unavailable: {reason}");
            }
        },
        ExecutionMode::Unsandboxed => None,
    };

    let output = supervise(
        &plan.context.repository_root,
        &plan.command,
        sandbox,
        cancelled,
    )?;
    if validate_context(&plan.context).is_err() {
        return Ok(ExecutionResult::Stale);
    }

    Ok(ExecutionResult::Completed(output))
}

fn validate_context(context: &CommandContext) -> anyhow::Result<()> {
    let identity = local_review::repository_identity(&context.repository_root)?;
    ensure!(
        identity == context.repository_identity,
        "local repository identity changed; reopen it before running this command"
    );
    let current = RepositorySnapshot::open(&context.repository_root)
        .context("could not re-read the local repository before command execution")?;
    ensure!(
        current.head_oid == context.head_oid
            && local_review::snapshot_fingerprint(&current) == context.diff_fingerprint,
        "the reviewed HEAD or working-tree diff changed; prepare the command again"
    );
    Ok(())
}

fn resolve_executable(requested: &str, repository_root: &Path) -> anyhow::Result<PathBuf> {
    let requested_path = Path::new(requested);
    if requested_path.is_absolute() {
        return resolve_candidate(requested_path);
    }

    if requested_path.components().count() > 1 {
        return resolve_candidate(&repository_root.join(requested_path));
    }

    let path = std::env::var_os("PATH").unwrap_or_default();
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(requested_path);
        if candidate.is_file() && is_executable(&candidate) {
            return resolve_candidate(&candidate);
        }
    }

    anyhow::bail!("could not find executable {requested:?} on the current PATH")
}

fn resolve_candidate(candidate: &Path) -> anyhow::Result<PathBuf> {
    let absolute = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        std::env::current_dir()?.join(candidate)
    };
    ensure!(
        absolute.is_file() && is_executable(&absolute),
        "{} is not an executable file",
        absolute.display()
    );
    Ok(absolute)
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
    }

    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn detect_sandbox(repository_root: &Path, command: &ParsedCommand) -> SandboxAvailability {
    #[cfg(target_os = "linux")]
    {
        detect_bubblewrap(repository_root, command)
    }

    #[cfg(target_os = "macos")]
    {
        detect_seatbelt(repository_root, command)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (repository_root, command);
        SandboxAvailability::Unavailable(
            "this platform does not have a supported sandbox backend".to_string(),
        )
    }
}

#[cfg(target_os = "macos")]
fn detect_seatbelt(repository_root: &Path, command: &ParsedCommand) -> SandboxAvailability {
    let executable = PathBuf::from("/usr/bin/sandbox-exec");
    if !executable.is_file() {
        return SandboxAvailability::Unavailable(
            "the macOS Seatbelt sandbox command is not installed".to_string(),
        );
    }
    let Ok(executable) = executable.canonicalize() else {
        return SandboxAvailability::Unavailable(
            "the macOS Seatbelt sandbox command could not be resolved".to_string(),
        );
    };
    let Ok(repository_root) = repository_root.canonicalize() else {
        return SandboxAvailability::Unavailable(
            "the repository path could not be canonicalized".to_string(),
        );
    };
    let Ok(canonical_executable) = command.canonical_executable.canonicalize() else {
        return SandboxAvailability::Unavailable(
            "the executable path could not be canonicalized".to_string(),
        );
    };
    let Some(home) =
        directories::BaseDirs::new().map(|directories| directories.home_dir().to_path_buf())
    else {
        return SandboxAvailability::Unavailable(
            "could not determine the current user's home directory for credential isolation"
                .to_string(),
        );
    };
    let Ok(home) = home.canonicalize() else {
        return SandboxAvailability::Unavailable(
            "the current user's home directory could not be canonicalized".to_string(),
        );
    };
    if home == Path::new("/") {
        return SandboxAvailability::Unavailable(
            "the current user's home directory resolves to the filesystem root".to_string(),
        );
    }

    let cargo_home = home.join(".cargo");
    let sandbox = SeatbeltSandbox {
        executable,
        hidden_home: home.clone(),
        cargo_bin: existing_home_directory(&cargo_home.join("bin"), &home),
        cargo_registry: existing_home_directory(&cargo_home.join("registry"), &home),
        cargo_git: existing_home_directory(&cargo_home.join("git"), &home),
        rustup_home: existing_home_directory(&home.join(".rustup"), &home),
    };
    if !seatbelt_can_read_path(&canonical_executable, &repository_root, &sandbox) {
        return SandboxAvailability::Unavailable(
            "the executable is outside the system, checkout, and allowlisted Rust toolchain paths"
                .to_string(),
        );
    }

    let Ok(scratch) = tempfile::Builder::new()
        .prefix("cururu-seatbelt-probe-")
        .tempdir()
    else {
        return SandboxAvailability::Unavailable(
            "could not create a temporary directory for the Seatbelt preflight".to_string(),
        );
    };
    let Ok(profile) = build_seatbelt_profile(&repository_root, &sandbox, scratch.path()) else {
        return SandboxAvailability::Unavailable(
            "the Seatbelt profile could not be prepared for this checkout".to_string(),
        );
    };
    if !seatbelt_probe_succeeds(&sandbox.executable, &profile, &repository_root) {
        return SandboxAvailability::Unavailable(
            "macOS Seatbelt could not establish the required isolated profile".to_string(),
        );
    }

    SandboxAvailability::Seatbelt(sandbox)
}

#[cfg(target_os = "macos")]
fn seatbelt_can_read_path(path: &Path, repository_root: &Path, sandbox: &SeatbeltSandbox) -> bool {
    path.starts_with(repository_root)
        || SEATBELT_READ_PATHS
            .iter()
            .any(|root| path.starts_with(root))
        || [
            sandbox.cargo_bin.as_deref(),
            sandbox.cargo_registry.as_deref(),
            sandbox.cargo_git.as_deref(),
            sandbox.rustup_home.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|root| path.starts_with(root))
}

#[cfg(any(target_os = "macos", test))]
fn build_seatbelt_profile(
    repository_root: &Path,
    sandbox: &SeatbeltSandbox,
    scratch_home: &Path,
) -> anyhow::Result<String> {
    let repository_root = repository_root
        .canonicalize()
        .context("could not canonicalize the checkout for the Seatbelt profile")?;
    let scratch_home = scratch_home
        .canonicalize()
        .context("could not canonicalize the temporary command directory")?;
    ensure!(
        !scratch_home.starts_with(&repository_root),
        "the temporary command directory must not be inside the checkout"
    );

    let mut profile = String::from(
        "(version 1)\n\
         (deny default)\n\
         (allow process-exec*)\n\
         (allow process-fork)\n\
         (allow process-info* (target same-sandbox))\n\
         (allow signal (target same-sandbox))\n\
         (allow sysctl-read)\n",
    );
    profile.push_str("(allow file-read* (literal \"/\"))\n");
    for path in SEATBELT_READ_PATHS {
        append_seatbelt_read_path(&mut profile, Path::new(path))?;
    }
    for path in SEATBELT_READ_DEVICES {
        append_seatbelt_read_literal(&mut profile, Path::new(path))?;
    }
    // The default-deny policy blocks unlisted home paths. An explicit home
    // deny would also block a checkout nested below the home directory.
    append_seatbelt_read_path(&mut profile, &repository_root)?;
    for path in [
        sandbox.cargo_bin.as_deref(),
        sandbox.cargo_registry.as_deref(),
        sandbox.cargo_git.as_deref(),
        sandbox.rustup_home.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        let canonical = path
            .canonicalize()
            .context("an allowlisted Rust toolchain path is no longer available")?;
        ensure!(
            canonical.starts_with(&sandbox.hidden_home),
            "an allowlisted Rust toolchain path escaped the user's home directory"
        );
        append_seatbelt_read_path(&mut profile, &canonical)?;
    }
    append_seatbelt_read_path(&mut profile, &scratch_home)?;
    append_seatbelt_write_path(&mut profile, &scratch_home)?;
    for path in SEATBELT_WRITE_DEVICES {
        append_seatbelt_write_literal(&mut profile, Path::new(path))?;
    }
    Ok(profile)
}

#[cfg(any(target_os = "macos", test))]
fn append_seatbelt_read_path(profile: &mut String, path: &Path) -> anyhow::Result<()> {
    let escaped = seatbelt_path_literal(path)?;
    profile.push_str("(allow file-read* (subpath \"");
    profile.push_str(&escaped);
    profile.push_str("\"))\n");
    profile.push_str("(allow file-map-executable (subpath \"");
    profile.push_str(&escaped);
    profile.push_str("\"))\n");
    append_seatbelt_read_metadata_ancestors(profile, path)
}

#[cfg(any(target_os = "macos", test))]
fn append_seatbelt_read_metadata_ancestors(
    profile: &mut String,
    path: &Path,
) -> anyhow::Result<()> {
    for ancestor in path.ancestors() {
        let escaped = seatbelt_path_literal(ancestor)?;
        profile.push_str("(allow file-read-metadata (literal \"");
        profile.push_str(&escaped);
        profile.push_str("\"))\n");
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn append_seatbelt_read_literal(profile: &mut String, path: &Path) -> anyhow::Result<()> {
    let escaped = seatbelt_path_literal(path)?;
    profile.push_str("(allow file-read* (literal \"");
    profile.push_str(&escaped);
    profile.push_str("\"))\n");
    append_seatbelt_read_metadata_ancestors(profile, path)?;
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn append_seatbelt_write_path(profile: &mut String, path: &Path) -> anyhow::Result<()> {
    let escaped = seatbelt_path_literal(path)?;
    profile.push_str("(allow file-write* (subpath \"");
    profile.push_str(&escaped);
    profile.push_str("\"))\n");
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn append_seatbelt_write_literal(profile: &mut String, path: &Path) -> anyhow::Result<()> {
    let escaped = seatbelt_path_literal(path)?;
    profile.push_str("(allow file-write* (literal \"");
    profile.push_str(&escaped);
    profile.push_str("\"))\n");
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn seatbelt_path_literal(path: &Path) -> anyhow::Result<String> {
    let value = path.to_str().context("sandbox paths must be valid UTF-8")?;
    ensure!(
        !value.chars().any(char::is_control),
        "sandbox paths must not contain control characters"
    );
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            _ => escaped.push(character),
        }
    }
    Ok(escaped)
}

#[cfg(target_os = "linux")]
fn detect_bubblewrap(repository_root: &Path, command: &ParsedCommand) -> SandboxAvailability {
    let Some(executable) = find_on_path("bwrap") else {
        return SandboxAvailability::Unavailable("bubblewrap (bwrap) is not installed".to_string());
    };
    let Ok(repository_root) = repository_root.canonicalize() else {
        return SandboxAvailability::Unavailable(
            "the repository path could not be canonicalized".to_string(),
        );
    };
    let Ok(canonical_executable) = command.canonical_executable.canonicalize() else {
        return SandboxAvailability::Unavailable(
            "the executable path could not be canonicalized".to_string(),
        );
    };
    let workspace = sandbox_workspace_for(&repository_root);

    let Some(home) =
        directories::BaseDirs::new().map(|directories| directories.home_dir().to_path_buf())
    else {
        return SandboxAvailability::Unavailable(
            "could not determine the current user's home directory for credential isolation"
                .to_string(),
        );
    };
    let Ok(home) = home.canonicalize() else {
        return SandboxAvailability::Unavailable(
            "the current user's home directory could not be canonicalized".to_string(),
        );
    };
    if home == Path::new("/") {
        return SandboxAvailability::Unavailable(
            "the current user's home directory resolves to the filesystem root".to_string(),
        );
    }
    if !bubblewrap_probe_succeeds(&executable, &home) {
        return SandboxAvailability::Unavailable(
            "bubblewrap could not establish the required isolated namespaces and hide the user's home".to_string(),
        );
    }
    let (command_path, cargo_bin) =
        if let Ok(relative) = canonical_executable.strip_prefix(&repository_root) {
            (workspace.join(relative), None)
        } else {
            let cargo_bin_host = home.join(".cargo/bin");
            let launch_relative = command.executable.strip_prefix(&cargo_bin_host);
            if let Ok(relative) = launch_relative {
                if is_temporary_sandbox_path(&cargo_bin_host) {
                    return SandboxAvailability::Unavailable(
                        "the Rust toolchain is under a temporary path hidden by the sandbox"
                            .to_string(),
                    );
                }
                (
                    PathBuf::from(SANDBOX_CARGO_BIN).join(relative),
                    Some(cargo_bin_host),
                )
            } else if is_hidden_by_sandbox(&canonical_executable, &home) {
                return SandboxAvailability::Unavailable(
                    "the executable is in a hidden user or temporary directory".to_string(),
                );
            } else {
                (canonical_executable, None)
            }
        };
    let cargo_home = home.join(".cargo");
    let rustup = home.join(".rustup");

    SandboxAvailability::Bubblewrap(BubblewrapSandbox {
        executable,
        workspace,
        hidden_home: home,
        command_path,
        cargo_bin,
        cargo_registry: existing_directory(&cargo_home.join("registry")),
        cargo_git: existing_directory(&cargo_home.join("git")),
        rustup_home: existing_directory(&rustup),
    })
}

#[cfg(target_os = "linux")]
fn is_hidden_by_sandbox(path: &Path, home: &Path) -> bool {
    path.starts_with(home) || is_hidden_system_path(path)
}

#[cfg(target_os = "linux")]
fn is_hidden_system_path(path: &Path) -> bool {
    ["/home", "/root", "/tmp", "/var/tmp", "/run"]
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

fn is_masked_by_sandbox(path: &Path) -> bool {
    ["/home", "/root", "/tmp", "/var/tmp", "/run"]
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

#[cfg(target_os = "linux")]
fn is_temporary_sandbox_path(path: &Path) -> bool {
    ["/tmp", "/var/tmp"]
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

#[cfg(target_os = "linux")]
fn existing_directory(path: &Path) -> Option<PathBuf> {
    let canonical = path.canonicalize().ok()?;
    if is_temporary_sandbox_path(&canonical) {
        return None;
    }
    canonical.is_dir().then_some(canonical)
}

#[cfg(target_os = "macos")]
fn existing_home_directory(path: &Path, home: &Path) -> Option<PathBuf> {
    let canonical = path.canonicalize().ok()?;
    if !canonical.starts_with(home) {
        return None;
    }
    canonical.is_dir().then_some(canonical)
}

#[cfg(target_os = "linux")]
fn sandbox_workspace_for(repository_root: &Path) -> PathBuf {
    if repository_root.starts_with("/tmp") {
        PathBuf::from(SANDBOX_ALTERNATE_WORKSPACE)
    } else {
        PathBuf::from(SANDBOX_WORKSPACE)
    }
}

#[cfg(target_os = "linux")]
fn find_on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(program);
        if candidate.is_file() && is_executable(&candidate) {
            return candidate.canonicalize().ok();
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn bubblewrap_probe_succeeds(executable: &Path, hidden_home: &Path) -> bool {
    let mut command = Command::new(executable);
    command.args(["--die-with-parent", "--unshare-all", "--ro-bind", "/", "/"]);
    if !is_masked_by_sandbox(hidden_home) {
        command.arg("--tmpfs").arg(hidden_home);
    }
    command
        .args([
            "--tmpfs",
            "/home",
            "--tmpfs",
            "/root",
            "--tmpfs",
            "/run",
            "--tmpfs",
            "/tmp",
            "--tmpfs",
            "/var/tmp",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--",
            "/usr/bin/true",
        ])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    set_process_group(&mut command);

    let Ok(mut child) = command.spawn() else {
        return false;
    };
    let started_at = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if started_at.elapsed() < SANDBOX_PROBE_TIMEOUT => {
                thread::sleep(SUPERVISOR_POLL_INTERVAL);
            }
            Ok(None) | Err(_) => {
                terminate_child_group(&mut child);
                let _ = child.wait();
                return false;
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn seatbelt_probe_succeeds(executable: &Path, profile: &str, repository_root: &Path) -> bool {
    let mut command = Command::new(executable);
    command
        .arg("-p")
        .arg(profile)
        .arg("/usr/bin/true")
        .current_dir(repository_root)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    set_process_group(&mut command);

    let Ok(mut child) = command.spawn() else {
        return false;
    };
    let started_at = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if started_at.elapsed() < SANDBOX_PROBE_TIMEOUT => {
                thread::sleep(SUPERVISOR_POLL_INTERVAL);
            }
            Ok(None) | Err(_) => {
                terminate_child_group(&mut child);
                let _ = child.wait();
                return false;
            }
        }
    }
}

fn supervise(
    repository_root: &Path,
    parsed: &ParsedCommand,
    sandbox: Option<&SandboxAvailability>,
    cancelled: &AtomicBool,
) -> anyhow::Result<CommandOutput> {
    let scratch = match sandbox {
        Some(SandboxAvailability::Bubblewrap(_)) => None,
        Some(SandboxAvailability::Seatbelt(_)) | None => Some(
            tempfile::Builder::new()
                .prefix("cururu-local-command-")
                .tempdir()
                .context("could not create an isolated temporary home for the command")?,
        ),
        Some(SandboxAvailability::Unavailable(reason)) => {
            anyhow::bail!("sandbox is unavailable: {reason}");
        }
    };
    let scratch_home = scratch.as_ref().map_or_else(
        || Path::new("/tmp"),
        |temporary_directory| temporary_directory.path(),
    );
    let mut command = build_process_command(repository_root, parsed, sandbox, scratch_home)?;
    set_process_group(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("could not start {}", parsed.executable.display()))?;
    let mut process_guard = ProcessGroupGuard::new(child.id());
    let (sender, receiver) = mpsc::channel();

    let Some(stdout) = child.stdout.take() else {
        terminate_child_group(&mut child);
        let _ = child.wait();
        return Err(anyhow::anyhow!("stdout pipe was not created"));
    };
    let Some(stderr) = child.stderr.take() else {
        terminate_child_group(&mut child);
        let _ = child.wait();
        return Err(anyhow::anyhow!("stderr pipe was not created"));
    };
    let stdout_thread = match spawn_capture_thread(stdout, sender.clone(), true) {
        Ok(handle) => handle,
        Err(error) => {
            terminate_child_group(&mut child);
            let _ = child.wait();
            return Err(error).context("could not capture command output");
        }
    };
    let stderr_thread = match spawn_capture_thread(stderr, sender, false) {
        Ok(handle) => handle,
        Err(error) => {
            terminate_child_group(&mut child);
            let _ = child.wait();
            let _ = stdout_thread.join();
            return Err(error).context("could not capture command output");
        }
    };

    let monitored = match monitor_process(&mut child, &receiver, cancelled) {
        Ok(monitored) => monitored,
        Err(error) => {
            kill_child_group(&mut child);
            let _ = child.wait();
            let _ = stdout_thread.join();
            let _ = stderr_thread.join();
            return Err(error);
        }
    };
    if monitored.termination.is_some() {
        kill_child_group(&mut child);
    }
    let status = match monitored.exit_status {
        Some(status) => status,
        None => child.wait().context("could not reap command process")?,
    };
    let output_streams_complete =
        monitored.stdout_capture.is_some() && monitored.stderr_capture.is_some();
    if monitored.stdout_capture.is_some() {
        let _ = stdout_thread.join();
    }
    if monitored.stderr_capture.is_some() {
        let _ = stderr_thread.join();
    }

    let (stdout, stdout_truncated) = captured_text(monitored.stdout_capture);
    let (stderr, stderr_truncated) = captured_text(monitored.stderr_capture);
    if output_streams_complete {
        process_guard.disarm();
    }

    Ok(CommandOutput {
        exit_code: status.code(),
        termination: monitored.termination,
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
    })
}

fn monitor_process(
    child: &mut Child,
    receiver: &mpsc::Receiver<CaptureEvent>,
    cancelled: &AtomicBool,
) -> anyhow::Result<MonitoredProcess> {
    let started_at = Instant::now();
    let deadline = started_at + COMMAND_TIMEOUT;
    let mut exit_status = None;
    let mut stdout_capture = None;
    let mut stderr_capture = None;
    let mut termination: Option<(CommandTermination, Instant)> = None;
    let mut output_drain_started_at = None;
    let mut force_kill_sent = false;
    let mut stop_wait_started_at = None;

    loop {
        let now = Instant::now();
        if termination.is_none() && cancelled.load(Ordering::Acquire) {
            terminate_child_group(child);
            termination = Some((CommandTermination::Cancelled, now));
        } else if termination.is_none() && now >= deadline {
            terminate_child_group(child);
            termination = Some((CommandTermination::TimedOut, now));
        }

        if let Some((_, terminated_at)) = termination
            && !force_kill_sent
            && now.duration_since(terminated_at) >= TERMINATION_GRACE
        {
            kill_child_group(child);
            force_kill_sent = true;
            stop_wait_started_at = Some(now);
        }

        if exit_status.is_none() {
            exit_status = child.try_wait().context("could not check command status")?;
            if exit_status.is_some() {
                output_drain_started_at = Some(now);
            }
        }

        while let Ok(event) = receiver.try_recv() {
            match event {
                CaptureEvent::Stdout(result) => stdout_capture = Some(result),
                CaptureEvent::Stderr(result) => stderr_capture = Some(result),
            }
        }

        if exit_status.is_some() && stdout_capture.is_some() && stderr_capture.is_some() {
            break;
        }

        if force_kill_sent
            && stop_wait_started_at
                .is_some_and(|stopped_at| now.duration_since(stopped_at) >= TERMINATION_GRACE)
        {
            break;
        }

        if let Some(drain_started_at) = output_drain_started_at
            && termination.is_none()
            && Instant::now().duration_since(drain_started_at) >= OUTPUT_DRAIN_GRACE
            && (stdout_capture.is_none() || stderr_capture.is_none())
        {
            terminate_child_group(child);
            termination = Some((CommandTermination::OutputPipeHeldOpen, Instant::now()));
        }

        thread::sleep(SUPERVISOR_POLL_INTERVAL);
    }

    Ok(MonitoredProcess {
        exit_status,
        termination: termination.map(|(reason, _)| reason),
        stdout_capture,
        stderr_capture,
    })
}

fn build_process_command(
    repository_root: &Path,
    parsed: &ParsedCommand,
    sandbox: Option<&SandboxAvailability>,
    scratch_home: &Path,
) -> anyhow::Result<Command> {
    if let Some(sandbox) = sandbox {
        return build_sandboxed_command(repository_root, parsed, sandbox, scratch_home);
    }
    build_unsandboxed_command(repository_root, parsed, scratch_home)
}

fn build_sandboxed_command(
    repository_root: &Path,
    parsed: &ParsedCommand,
    sandbox: &SandboxAvailability,
    scratch_home: &Path,
) -> anyhow::Result<Command> {
    #[cfg(not(target_os = "macos"))]
    let _ = scratch_home;

    match sandbox {
        SandboxAvailability::Bubblewrap(sandbox) => {
            Ok(build_bubblewrap_command(repository_root, parsed, sandbox))
        }
        #[cfg(target_os = "macos")]
        SandboxAvailability::Seatbelt(sandbox) => {
            build_seatbelt_command(repository_root, parsed, sandbox, scratch_home)
        }
        #[cfg(not(target_os = "macos"))]
        SandboxAvailability::Seatbelt(_) => {
            anyhow::bail!("the Seatbelt sandbox is only supported on macOS");
        }
        SandboxAvailability::Unavailable(reason) => {
            anyhow::bail!("sandbox is unavailable: {reason}");
        }
    }
}

fn build_bubblewrap_command(
    repository_root: &Path,
    parsed: &ParsedCommand,
    sandbox: &BubblewrapSandbox,
) -> Command {
    let mut command = Command::new(&sandbox.executable);
    append_sandbox_workspace_mounts(&mut command, repository_root, sandbox);
    append_sandbox_toolchain_mounts(&mut command, sandbox);
    append_sandbox_runtime_options(&mut command, sandbox);
    command.arg("--").arg(&sandbox.command_path);
    command.args(&parsed.arguments);
    command.env_clear().stdin(Stdio::null());
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    command
}

#[cfg(target_os = "macos")]
fn build_seatbelt_command(
    repository_root: &Path,
    parsed: &ParsedCommand,
    sandbox: &SeatbeltSandbox,
    scratch_home: &Path,
) -> anyhow::Result<Command> {
    use std::os::unix::fs::symlink;

    let scratch_home = scratch_home
        .canonicalize()
        .context("could not canonicalize the temporary command directory")?;
    let cargo_home = scratch_home.join(".cargo");
    fs::create_dir_all(&cargo_home).context("could not create an isolated Cargo home")?;
    for (name, source) in [
        ("registry", sandbox.cargo_registry.as_deref()),
        ("git", sandbox.cargo_git.as_deref()),
    ] {
        if let Some(source) = source {
            symlink(source, cargo_home.join(name))
                .context("could not expose a read-only Cargo cache to the sandbox")?;
        }
    }

    let profile = build_seatbelt_profile(repository_root, sandbox, &scratch_home)?;
    let mut path_entries: Vec<PathBuf> = SEATBELT_PATH.iter().map(PathBuf::from).collect();
    if let Some(cargo_bin) = &sandbox.cargo_bin {
        path_entries.insert(0, cargo_bin.clone());
    }
    let path = std::env::join_paths(path_entries).context("could not build sandboxed PATH")?;
    let target_dir = scratch_home.join("target");
    let mut command = Command::new(&sandbox.executable);
    command
        .arg("-p")
        .arg(profile)
        .arg(&parsed.executable)
        .args(&parsed.arguments)
        .current_dir(repository_root)
        .env_clear()
        .env("HOME", &scratch_home)
        .env("TMPDIR", &scratch_home)
        .env("TMP", &scratch_home)
        .env("TEMP", &scratch_home)
        .env("CARGO_HOME", &cargo_home)
        .env("CARGO_TARGET_DIR", target_dir)
        .env("PATH", path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(rustup_home) = &sandbox.rustup_home {
        command.env("RUSTUP_HOME", rustup_home);
    }
    Ok(command)
}

fn append_sandbox_workspace_mounts(
    command: &mut Command,
    repository_root: &Path,
    sandbox: &BubblewrapSandbox,
) {
    let (workspace_parent, other_temporary_parent) =
        if sandbox.workspace == Path::new(SANDBOX_ALTERNATE_WORKSPACE) {
            ("/var/tmp", "/tmp")
        } else {
            ("/tmp", "/var/tmp")
        };
    command
        .args([
            "--die-with-parent",
            "--unshare-all",
            "--ro-bind",
            "/",
            "/",
            "--tmpfs",
            workspace_parent,
            "--dir",
        ])
        .arg(&sandbox.workspace)
        .args(["--ro-bind"])
        .arg(repository_root)
        .arg(&sandbox.workspace)
        .arg("--tmpfs")
        .arg(other_temporary_parent)
        .arg("--dir")
        .arg(SANDBOX_HOME)
        .arg("--dir")
        .arg(SANDBOX_CARGO_HOME);
}

fn append_sandbox_toolchain_mounts(command: &mut Command, sandbox: &BubblewrapSandbox) {
    if let Some(cargo_bin) = &sandbox.cargo_bin {
        command
            .arg("--dir")
            .arg(SANDBOX_CARGO_BIN)
            .arg("--ro-bind")
            .arg(cargo_bin)
            .arg(SANDBOX_CARGO_BIN);
    }
    if let Some(registry) = &sandbox.cargo_registry {
        command
            .arg("--dir")
            .arg(format!("{SANDBOX_CARGO_HOME}/registry"))
            .arg("--ro-bind")
            .arg(registry)
            .arg(format!("{SANDBOX_CARGO_HOME}/registry"));
    }
    if let Some(git) = &sandbox.cargo_git {
        command
            .arg("--dir")
            .arg(format!("{SANDBOX_CARGO_HOME}/git"))
            .arg("--ro-bind")
            .arg(git)
            .arg(format!("{SANDBOX_CARGO_HOME}/git"));
    }
    if let Some(rustup_home) = &sandbox.rustup_home {
        command
            .arg("--dir")
            .arg(SANDBOX_RUSTUP_HOME)
            .arg("--ro-bind")
            .arg(rustup_home)
            .arg(SANDBOX_RUSTUP_HOME);
    }
}

fn append_sandbox_runtime_options(command: &mut Command, sandbox: &BubblewrapSandbox) {
    if !is_masked_by_sandbox(&sandbox.hidden_home) {
        command.arg("--tmpfs").arg(&sandbox.hidden_home);
    }
    let sandbox_path = if sandbox.cargo_bin.is_some() {
        format!("{SANDBOX_CARGO_BIN}:{SANDBOX_PATH}")
    } else {
        SANDBOX_PATH.to_string()
    };
    command
        .args([
            "--tmpfs",
            "/home",
            "--tmpfs",
            "/root",
            "--tmpfs",
            "/run",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--dir",
            SANDBOX_TMP,
            "--chdir",
        ])
        .arg(&sandbox.workspace)
        .args([
            "--setenv",
            "HOME",
            SANDBOX_HOME,
            "--setenv",
            "CARGO_HOME",
            SANDBOX_CARGO_HOME,
            "--setenv",
            "CARGO_TARGET_DIR",
            "/tmp/cururu-target",
            "--setenv",
            "TMPDIR",
            SANDBOX_TMP,
            "--setenv",
            "PATH",
            &sandbox_path,
        ]);
    if sandbox.cargo_bin.is_some() {
        command
            .arg("--setenv")
            .arg("RUSTUP_HOME")
            .arg(SANDBOX_RUSTUP_HOME);
    }
}

fn build_unsandboxed_command(
    repository_root: &Path,
    parsed: &ParsedCommand,
    scratch_home: &Path,
) -> anyhow::Result<Command> {
    let mut command = Command::new(&parsed.executable);
    let host_path = std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin"));
    let rustup_home = directories::BaseDirs::new()
        .map(|directories| directories.home_dir().join(".rustup"))
        .filter(|path| path.is_dir());
    let cargo_home = scratch_home.join(".cargo");
    fs::create_dir_all(&cargo_home).context("could not create an isolated Cargo home")?;

    command
        .args(&parsed.arguments)
        .current_dir(repository_root)
        .env_clear()
        .env("PATH", host_path)
        .env("HOME", scratch_home)
        .env("CARGO_HOME", &cargo_home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(rustup_home) = rustup_home {
        command.env("RUSTUP_HOME", rustup_home);
    }
    Ok(command)
}

fn spawn_capture_thread<R>(
    mut reader: R,
    sender: mpsc::Sender<CaptureEvent>,
    is_stdout: bool,
) -> io::Result<thread::JoinHandle<()>>
where
    R: Read + Send + 'static,
{
    thread::Builder::new()
        .name(if is_stdout {
            "cururu-command-stdout".to_string()
        } else {
            "cururu-command-stderr".to_string()
        })
        .spawn(move || {
            let result = capture_stream(&mut reader);
            let event = if is_stdout {
                CaptureEvent::Stdout(result)
            } else {
                CaptureEvent::Stderr(result)
            };
            let _ = sender.send(event);
        })
}

fn capture_stream(reader: &mut impl Read) -> io::Result<CapturedStream> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8 * 1_024];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        let remaining = MAX_CAPTURED_STREAM_BYTES.saturating_sub(bytes.len());
        let retained = read.min(remaining);
        bytes.extend_from_slice(&chunk[..retained]);
        if retained < read {
            truncated = true;
        }
    }
    Ok(CapturedStream { bytes, truncated })
}

fn captured_text(capture: Option<io::Result<CapturedStream>>) -> (String, bool) {
    match capture {
        Some(Ok(capture)) => (
            String::from_utf8_lossy(&capture.bytes).into_owned(),
            capture.truncated,
        ),
        Some(Err(_)) => ("[output capture failed]".to_string(), false),
        None => ("[output capture did not finish]".to_string(), false),
    }
}

fn set_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }

    #[cfg(not(unix))]
    {
        let _ = command;
    }
}

#[cfg(unix)]
fn terminate_child_group(child: &mut Child) {
    signal_child_group(child, nix::sys::signal::Signal::SIGTERM);
}

#[cfg(unix)]
fn kill_child_group(child: &mut Child) {
    signal_child_group(child, nix::sys::signal::Signal::SIGKILL);
}

#[cfg(unix)]
fn signal_child_group(child: &mut Child, signal: nix::sys::signal::Signal) {
    use nix::{sys::signal::killpg, unistd::Pid};

    let Ok(pid) = i32::try_from(child.id()) else {
        let _ = child.kill();
        return;
    };
    if killpg(Pid::from_raw(pid), signal).is_err() {
        let _ = child.kill();
    }
}

#[cfg(not(unix))]
fn terminate_child_group(child: &mut Child) {
    let _ = child.kill();
}

#[cfg(not(unix))]
fn kill_child_group(child: &mut Child) {
    let _ = child.kill();
}

struct ProcessGroupGuard {
    pid: u32,
    armed: bool,
}

impl ProcessGroupGuard {
    const fn new(pid: u32) -> Self {
        Self { pid, armed: true }
    }

    const fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(pid) = i32::try_from(self.pid) {
            #[cfg(unix)]
            {
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(pid),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::atomic::AtomicBool,
        time::{Duration, Instant},
    };

    #[cfg(target_os = "macos")]
    use super::CommandOutput;
    use super::{
        CommandContext, CommandPlan, ExecutionMode, ParsedCommand, SandboxAvailability,
        SeatbeltSandbox, build_seatbelt_profile, capture_stream, detect_sandbox, parse_command,
        run, seatbelt_path_literal, supervise,
    };
    use crate::repository::RepositorySnapshot;

    #[cfg(target_os = "macos")]
    struct SeatbeltTestFixture {
        _root: tempfile::TempDir,
        repository: PathBuf,
        secret_path: PathBuf,
        checkout_file: PathBuf,
        sandbox: SandboxAvailability,
    }

    #[cfg(target_os = "macos")]
    fn seatbelt_test_fixture() -> SeatbeltTestFixture {
        let root = tempfile::tempdir().unwrap();
        let fake_home = root.path().join("home");
        std::fs::create_dir(&fake_home).unwrap();
        let repository = fake_home.join("checkout");
        std::fs::create_dir(&repository).unwrap();
        let secret_path = fake_home.join("synthetic-credential.txt");
        std::fs::write(&secret_path, "test-only marker").unwrap();
        let checkout_file = repository.join("source.txt");
        std::fs::write(&checkout_file, "checkout is readable").unwrap();
        let fake_home = fake_home.canonicalize().unwrap();
        let repository = repository.canonicalize().unwrap();
        let secret_path = secret_path.canonicalize().unwrap();
        let checkout_file = checkout_file.canonicalize().unwrap();
        let shell_path = PathBuf::from("/bin/sh");
        let shell_command = ParsedCommand {
            canonical_executable: shell_path.canonicalize().unwrap(),
            executable: shell_path,
            arguments: Vec::new(),
        };
        let detected_sandbox = match detect_sandbox(&repository, &shell_command) {
            SandboxAvailability::Seatbelt(sandbox) => sandbox,
            unavailable => {
                panic!(
                    "macOS Seatbelt preflight did not pass: {}",
                    unavailable.description()
                );
            }
        };
        let sandbox = SandboxAvailability::Seatbelt(SeatbeltSandbox {
            hidden_home: fake_home.canonicalize().unwrap(),
            cargo_bin: None,
            cargo_registry: None,
            cargo_git: None,
            rustup_home: None,
            ..detected_sandbox
        });

        SeatbeltTestFixture {
            _root: root,
            repository,
            secret_path,
            checkout_file,
            sandbox,
        }
    }

    #[cfg(target_os = "macos")]
    fn run_seatbelt_command(
        fixture: &SeatbeltTestFixture,
        executable: &str,
        arguments: Vec<String>,
    ) -> CommandOutput {
        let executable = PathBuf::from(executable);
        let command = ParsedCommand {
            canonical_executable: executable.canonicalize().unwrap(),
            executable,
            arguments,
        };
        supervise(
            &fixture.repository,
            &command,
            Some(&fixture.sandbox),
            &AtomicBool::new(false),
        )
        .unwrap()
    }

    #[test]
    fn command_parser_treats_shell_metacharacters_as_literal_arguments() {
        let command = parse_command(
            "/usr/bin/printf '%s' '$(touch /tmp/should-not-exist); *'",
            PathBuf::from(".").as_path(),
        )
        .unwrap();

        assert_eq!(command.executable, PathBuf::from("/usr/bin/printf"));
        assert_eq!(
            command.arguments,
            ["%s", "$(touch /tmp/should-not-exist); *"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn command_context_canonicalizes_the_repository_root() {
        let root = tempfile::tempdir().unwrap();
        let repository = root.path().join("repo");
        std::fs::create_dir(&repository).unwrap();
        gix::init(&repository).unwrap();
        let repository_alias = root.path().join("repo-alias");
        std::os::unix::fs::symlink(&repository, &repository_alias).unwrap();
        let mut snapshot = RepositorySnapshot::open(&repository).unwrap();
        snapshot.root = repository_alias;

        let context = CommandContext::from_snapshot(&snapshot).unwrap();

        assert_eq!(context.repository_root, repository.canonicalize().unwrap());
    }

    #[test]
    fn command_parser_rejects_control_characters_and_unmatched_quotes() {
        assert!(
            parse_command(
                "/usr/bin/true\n/usr/bin/false",
                PathBuf::from(".").as_path()
            )
            .is_err()
        );
        assert!(
            parse_command("/usr/bin/printf 'unfinished", PathBuf::from(".").as_path()).is_err()
        );
    }

    #[test]
    fn output_capture_keeps_only_the_configured_bound() {
        let mut reader = std::io::Cursor::new(vec![b'x'; 40 * 1_024]);
        let captured = capture_stream(&mut reader).unwrap();

        assert_eq!(captured.bytes.len(), 32 * 1_024);
        assert!(captured.truncated);
    }

    #[test]
    fn seatbelt_paths_are_escaped_and_the_profile_scopes_filesystem_access() {
        let path = PathBuf::from("/tmp/quoted\" (allow default) \\");
        assert_eq!(
            seatbelt_path_literal(&path).unwrap(),
            "/tmp/quoted\\\" (allow default) \\\\"
        );
        assert!(seatbelt_path_literal(PathBuf::from("/tmp/line\nbreak").as_path()).is_err());

        let home = tempfile::tempdir().unwrap();
        let repository = home.path().join("checkout");
        std::fs::create_dir(&repository).unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let hidden_home = home.path().canonicalize().unwrap();
        let sandbox = SeatbeltSandbox {
            executable: PathBuf::from("/usr/bin/sandbox-exec"),
            hidden_home: hidden_home.clone(),
            cargo_bin: None,
            cargo_registry: None,
            cargo_git: None,
            rustup_home: None,
        };

        let profile = build_seatbelt_profile(&repository, &sandbox, scratch.path()).unwrap();
        let repository_path = repository.canonicalize().unwrap();
        let scratch_path = scratch.path().canonicalize().unwrap();
        assert!(profile.contains(&format!(
            "(allow file-read* (subpath \"{}\"))",
            repository_path.display()
        )));
        assert!(profile.contains(&format!(
            "(allow file-write* (subpath \"{}\"))",
            scratch_path.display()
        )));
        assert!(!profile.contains(&format!(
            "(allow file-write* (subpath \"{}\"))",
            repository_path.display()
        )));
        assert!(!profile.contains(&format!(
            "(allow file-read* (subpath \"{}\"))",
            hidden_home.display()
        )));
        let home_deny = format!("(deny file-read* (subpath \"{}\"))", hidden_home.display());
        let checkout_allow = format!(
            "(allow file-read* (subpath \"{}\"))",
            repository_path.display()
        );
        assert!(profile.contains("(deny default)"));
        assert!(!profile.contains(&home_deny));
        assert!(profile.contains(&checkout_allow));
        assert!(!profile.contains(&format!(
            "(allow file-read-metadata (subpath \"{}\"))",
            hidden_home.display()
        )));
        assert!(profile.contains("(allow file-write* (literal \"/dev/null\"))"));
        assert!(profile.contains("(allow file-read* (subpath \"/System/Library\"))"));
        assert!(!profile.contains("(allow file-read* (subpath \"/System\"))"));
        assert!(!profile.contains("(allow network"));
    }

    #[test]
    fn direct_process_execution_captures_stdout_and_stderr_without_a_shell() {
        let repository = tempfile::tempdir().unwrap();
        let command = ParsedCommand {
            executable: PathBuf::from("/usr/bin/printf"),
            canonical_executable: PathBuf::from("/usr/bin/printf"),
            arguments: vec!["%s".to_string(), "out;$(touch nope)".to_string()],
        };
        let output = supervise(repository.path(), &command, None, &AtomicBool::new(false)).unwrap();

        assert_eq!(output.stdout, "out;$(touch nope)");
        assert_eq!(output.stderr, "");
        assert!(output.succeeded());
    }

    #[test]
    fn cancellation_terminates_and_reaps_the_command_process_group() {
        let repository = tempfile::tempdir().unwrap();
        let command = ParsedCommand {
            executable: PathBuf::from("/bin/sleep"),
            canonical_executable: PathBuf::from("/bin/sleep"),
            arguments: vec!["30".to_string()],
        };
        let cancellation = std::sync::Arc::new(AtomicBool::new(false));
        let trigger = cancellation.clone();
        let trigger_thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            trigger.store(true, std::sync::atomic::Ordering::Release);
        });
        let started_at = Instant::now();
        let output = supervise(repository.path(), &command, None, cancellation.as_ref()).unwrap();
        trigger_thread.join().unwrap();

        assert!(output.was_cancelled());
        assert!(started_at.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[test]
    fn command_completion_with_a_descendant_holding_output_pipes_stops_the_group() {
        let repository = tempfile::tempdir().unwrap();
        let command = ParsedCommand {
            executable: PathBuf::from("/bin/sh"),
            canonical_executable: PathBuf::from("/bin/sh"),
            arguments: vec!["-c".to_string(), "sleep 30 &".to_string()],
        };
        let started_at = Instant::now();

        let output = supervise(repository.path(), &command, None, &AtomicBool::new(false)).unwrap();

        assert!(output.output_pipe_held_open());
        assert!(started_at.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_stale_confirmation_is_rejected_before_starting_the_command() {
        let repository = tempfile::tempdir().unwrap();
        gix::init(repository.path()).unwrap();
        std::fs::write(repository.path().join("reviewed.txt"), "before\n").unwrap();
        let snapshot = RepositorySnapshot::open(repository.path()).unwrap();
        let context = CommandContext::from_snapshot(&snapshot).unwrap();
        let marker = "command-must-not-run";
        let plan = CommandPlan {
            context,
            command: ParsedCommand {
                executable: PathBuf::from("/usr/bin/touch"),
                canonical_executable: PathBuf::from("/usr/bin/touch"),
                arguments: vec![marker.to_string()],
            },
            sandbox: SandboxAvailability::Unavailable("test".to_string()),
        };
        std::fs::write(repository.path().join("reviewed.txt"), "after\n").unwrap();

        let result = run(&plan, ExecutionMode::Unsandboxed, &AtomicBool::new(false));

        assert!(result.is_err());
        assert!(!repository.path().join(marker).exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bubblewrap_executes_system_commands_with_a_read_only_checkout() {
        let repository = tempfile::tempdir().unwrap();
        let true_command = ParsedCommand {
            executable: PathBuf::from("/usr/bin/true"),
            canonical_executable: PathBuf::from("/usr/bin/true"),
            arguments: Vec::new(),
        };
        let sandbox = detect_sandbox(repository.path(), &true_command);
        let SandboxAvailability::Bubblewrap(_) = &sandbox else {
            return;
        };
        let output = supervise(
            repository.path(),
            &true_command,
            Some(&sandbox),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(output.succeeded());

        let write_marker = "should-not-be-created-by-sandbox-test";
        let touch_command = ParsedCommand {
            executable: PathBuf::from("/usr/bin/touch"),
            canonical_executable: PathBuf::from("/usr/bin/touch"),
            arguments: vec![write_marker.to_string()],
        };
        let touch_sandbox = detect_sandbox(repository.path(), &touch_command);
        let SandboxAvailability::Bubblewrap(_) = &touch_sandbox else {
            return;
        };
        let output = supervise(
            repository.path(),
            &touch_command,
            Some(&touch_sandbox),
            &AtomicBool::new(false),
        )
        .unwrap();

        assert!(!output.succeeded());
        assert!(!repository.path().join(write_marker).exists());

        let sleep_command = ParsedCommand {
            executable: PathBuf::from("/usr/bin/sleep"),
            canonical_executable: PathBuf::from("/usr/bin/sleep"),
            arguments: vec!["5".to_string()],
        };
        let sleep_sandbox = detect_sandbox(repository.path(), &sleep_command);
        let SandboxAvailability::Bubblewrap(_) = &sleep_sandbox else {
            return;
        };
        let cancellation = std::sync::Arc::new(AtomicBool::new(false));
        let trigger = cancellation.clone();
        let trigger_thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            trigger.store(true, std::sync::atomic::Ordering::Release);
        });
        let started_at = Instant::now();
        let output = supervise(
            repository.path(),
            &sleep_command,
            Some(&sleep_sandbox),
            cancellation.as_ref(),
        )
        .unwrap();
        trigger_thread.join().unwrap();

        assert!(output.was_cancelled());
        assert!(started_at.elapsed() < Duration::from_secs(3));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_allows_checkout_reads_and_scratch_writes() {
        let fixture = seatbelt_test_fixture();
        let output = run_seatbelt_command(
            &fixture,
            "/bin/cat",
            vec![fixture.checkout_file.display().to_string()],
        );
        assert!(output.succeeded(), "{}", output.stderr);
        assert_eq!(output.stdout, "checkout is readable");

        let output = run_seatbelt_command(
            &fixture,
            "/bin/sh",
            vec![
                "-c".to_string(),
                "printf marker > \"$TMPDIR/scratch-write\" && test -f \"$TMPDIR/scratch-write\""
                    .to_string(),
            ],
        );
        assert!(output.succeeded(), "{}", output.stderr);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_denies_home_credentials_directly_and_through_symlinks() {
        let fixture = seatbelt_test_fixture();
        let output = run_seatbelt_command(
            &fixture,
            "/bin/cat",
            vec![fixture.secret_path.display().to_string()],
        );
        assert!(!output.succeeded());

        let secret_symlink = fixture.repository.join("synthetic-credential-link");
        std::os::unix::fs::symlink(&fixture.secret_path, &secret_symlink).unwrap();
        let output = run_seatbelt_command(
            &fixture,
            "/bin/cat",
            vec![secret_symlink.display().to_string()],
        );
        assert!(!output.succeeded());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_keeps_the_checkout_read_only() {
        let fixture = seatbelt_test_fixture();
        let write_marker = fixture.repository.join("seatbelt-write-marker");
        let output = run_seatbelt_command(
            &fixture,
            "/usr/bin/touch",
            vec![write_marker.display().to_string()],
        );
        assert!(!output.succeeded());
        assert!(!write_marker.exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_disables_loopback_network_access() {
        let fixture = seatbelt_test_fixture();
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port().to_string();
        let output = run_seatbelt_command(
            &fixture,
            "/usr/bin/nc",
            vec![
                "-z".to_string(),
                "-w".to_string(),
                "1".to_string(),
                "127.0.0.1".to_string(),
                port,
            ],
        );
        assert!(!output.succeeded());
        drop(listener);
    }
}
