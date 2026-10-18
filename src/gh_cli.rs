//! Opt-in, bounded access to an existing github.com CLI credential. No login,
//! network requests, persistence, environment-token fallback, or token logging.

use std::{
    fmt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context as _, ensure};

#[derive(Clone)]
pub struct GhToken(String);

impl fmt::Debug for GhToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("GhToken([REDACTED])")
    }
}

impl GhToken {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub fn read_existing_token() -> anyhow::Result<GhToken> {
    let home = directories::BaseDirs::new()
        .context("could not locate the user home for gh")?
        .home_dir()
        .canonicalize()
        .context("could not resolve the user home for gh")?;
    let candidates = ["/usr/bin/gh", "/usr/local/bin/gh", "/opt/homebrew/bin/gh"]
        .into_iter()
        .map(PathBuf::from)
        .chain([home.join(".local/bin/gh")]);
    let executable = candidates.into_iter().find(|path| path.is_file()).context(
        "gh was not found in supported installation directories; install/sign in to gh separately",
    )?;
    let mut command = token_command(&executable, &home);
    // CLI configuration and OS keyring transport locators only; provider/token
    // environment variables are deliberately absent from the child process.
    for name in [
        "GH_CONFIG_DIR",
        "XDG_CONFIG_HOME",
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    read_token(command, Duration::from_secs(5))
}

fn token_command(executable: &Path, home: &Path) -> Command {
    let mut command = Command::new(executable);
    command
        .args(["auth", "token", "--hostname", "github.com"])
        .env_clear()
        .env("HOME", home)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command
}

#[cfg(unix)]
fn read_token(mut command: Command, timeout: Duration) -> anyhow::Result<GhToken> {
    use nix::fcntl::{FcntlArg, OFlag, fcntl};
    use std::{io::Read as _, os::unix::process::CommandExt as _, time::Instant};

    command.process_group(0);
    let child = command
        .spawn()
        .context("could not start gh; no credential was loaded")?;
    let mut guard = CredentialChild(child);
    let mut stdout = guard
        .0
        .stdout
        .take()
        .context("gh stdout pipe was unavailable")?;
    fcntl(&stdout, FcntlArg::F_SETFL(OFlag::O_NONBLOCK))
        .context("could not bound gh output reads")?;
    let deadline = Instant::now() + timeout;
    let mut output = Vec::new();
    let mut eof = false;
    let mut status = None;
    while Instant::now() < deadline {
        let mut buffer = [0_u8; 1_024];
        match stdout.read(&mut buffer) {
            Ok(0) => eof = true,
            Ok(count) => {
                ensure!(
                    output.len() + count <= 16_384,
                    "gh output exceeded the credential size limit"
                );
                output.extend_from_slice(&buffer[..count]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => anyhow::bail!("could not read gh output"),
        }
        if status.is_none() {
            status = guard
                .0
                .try_wait()
                .context("could not inspect gh exit status")?;
        }
        if let Some(status) = status {
            ensure!(
                status.success(),
                "gh did not return a credential; sign in to gh separately"
            );
            if eof {
                return validate_token(output);
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    anyhow::bail!("gh credential read timed out; no credential was loaded")
}

#[cfg(unix)]
struct CredentialChild(std::process::Child);

#[cfg(unix)]
impl Drop for CredentialChild {
    fn drop(&mut self) {
        use nix::{
            sys::signal::{Signal, killpg},
            unistd::Pid,
        };
        if let Ok(pid) = i32::try_from(self.0.id()) {
            let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(not(unix))]
fn read_token(_command: Command, _timeout: Duration) -> anyhow::Result<GhToken> {
    anyhow::bail!("the gh session fallback is currently supported only on Linux and macOS")
}

pub fn validate_token(bytes: Vec<u8>) -> anyhow::Result<GhToken> {
    let token = String::from_utf8(bytes)
        .map_err(|_| anyhow::anyhow!("gh returned a non-text credential"))?;
    let token = token.trim_end_matches(['\r', '\n']);
    ensure!(
        !token.is_empty()
            && token
                .bytes()
                .all(|byte| byte.is_ascii_graphic() && !b"\"\\".contains(&byte)),
        "gh returned a missing or malformed credential"
    );
    Ok(GhToken(token.to_string()))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt as _, time::Instant};

    fn fixture(script: &str) -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("fake-gh");
        fs::write(&executable, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        (root, executable)
    }

    #[test]
    fn explicit_command_is_host_scoped_and_token_is_redacted() {
        let (root, executable) = fixture(
            "test \"$*\" = 'auth token --hostname github.com' || exit 2; test -z \"$GH_TOKEN$GITHUB_TOKEN$GH_ENTERPRISE_TOKEN$GITHUB_ENTERPRISE_TOKEN$LLM_API_KEY$TYPESAFE_API_KEY\" || exit 3; test \"$GH_PROMPT_DISABLED\" = 1 || exit 4; printf 'synthetic_test_token\\n'",
        );
        let command = token_command(&executable, root.path());
        assert!(
            command
                .get_envs()
                .all(|(name, _)| name != "GH_TOKEN" && name != "GITHUB_TOKEN")
        );
        let token = read_token(command, Duration::from_secs(2)).unwrap();
        assert_eq!(token.as_str(), "synthetic_test_token");
        assert!(!format!("{token:?}").contains("synthetic_test_token"));
    }

    #[test]
    fn failures_and_invalid_output_never_echo_credential_or_stderr() {
        for script in [
            "printf 'synthetic-secret' >&2; exit 2",
            "printf 'synthetic-secret\\nextra'",
            "exit 0",
        ] {
            let (root, executable) = fixture(script);
            let error = read_token(
                token_command(&executable, root.path()),
                Duration::from_secs(2),
            )
            .unwrap_err();
            assert!(!format!("{error:#}").contains("synthetic-secret"));
        }
    }

    #[test]
    fn oversized_output_hits_the_size_bound_without_echoing_the_buffer() {
        let (root, executable) = fixture(
            "while :; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; done",
        );
        let error = read_token(
            token_command(&executable, root.path()),
            Duration::from_secs(2),
        )
        .unwrap_err();
        assert!(error.to_string().contains("size limit"));
        assert!(!error.to_string().contains("xxxxxxxx"));
    }

    #[test]
    fn oversized_output_hangs_and_inherited_pipes_are_bounded() {
        for script in [
            "while :; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; done",
            "/bin/sleep 30",
            "/bin/sleep 30 & printf synthetic_test_token",
        ] {
            let (root, executable) = fixture(script);
            let started = Instant::now();
            assert!(
                read_token(
                    token_command(&executable, root.path()),
                    Duration::from_millis(100)
                )
                .is_err()
            );
            assert!(started.elapsed() < Duration::from_secs(2));
        }
    }
}
