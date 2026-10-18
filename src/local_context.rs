//! Explicitly loaded local evidence. No read is performed during config parsing.

use anyhow::{Context as _, ensure};
use cururu_core::ContextFile;
use cururu_engine::{
    config::ContextToml,
    review_context::{MAX_CONTEXT_BYTES, MAX_CONTEXT_FILE_BYTES, ReviewContext},
};
use sha2::{Digest as _, Sha256};
use std::{io::Read as _, path::Path};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextPath {
    pub label: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextSpec {
    pub focus: Vec<String>,
    pub paths: Vec<ContextPath>,
    pub max_bytes: usize,
}

impl Default for ContextSpec {
    fn default() -> Self {
        Self {
            focus: Vec::new(),
            paths: Vec::new(),
            max_bytes: MAX_CONTEXT_BYTES,
        }
    }
}

impl ContextSpec {
    pub fn parse(context: Option<ContextToml>, focus: Vec<String>) -> anyhow::Result<Self> {
        let mut spec = Self {
            focus,
            ..Self::default()
        };
        if let Some(context) = context {
            if let Some(auto) = context.auto {
                ensure!(
                    auto.enabled == Some(false)
                        && auto.max_bytes.is_none()
                        && auto.max_files.is_none()
                        && auto.per_file_bytes.is_none()
                        && auto.include.is_none()
                        && auto.exclude.is_none(),
                    "automatic context discovery is unsupported; only context.auto.enabled = false is accepted"
                );
            }
            spec.max_bytes = context
                .max_bytes
                .unwrap_or(MAX_CONTEXT_BYTES)
                .min(MAX_CONTEXT_BYTES);
            ensure!(spec.max_bytes > 0, "context.max_bytes must be positive");
            for (label, paths) in [
                ("Conventions", context.conventions),
                ("Specifications", context.specifications),
                ("Skills (evidence only)", context.skills),
                ("Additional", context.additional),
            ] {
                for path in paths.unwrap_or_default() {
                    ensure!(
                        !path.contains(['*', '?', '[', ']']),
                        "desktop context currently requires exact paths, not globs"
                    );
                    ensure!(
                        !path.eq_ignore_ascii_case(".cururu.toml")
                            && path.split('/').all(|part| {
                                let lower = part.to_ascii_lowercase();
                                !matches!(
                                    lower.as_str(),
                                    ".git"
                                        | ".ssh"
                                        | ".aws"
                                        | ".gnupg"
                                        | ".config"
                                        | ".npmrc"
                                        | ".netrc"
                                        | "id_rsa"
                                        | "id_ed25519"
                                        | "credentials"
                                        | "credentials.json"
                                ) && lower != ".env"
                                    && !lower.starts_with(".env.")
                                    && !Path::new(part).extension().is_some_and(|extension| {
                                        extension.eq_ignore_ascii_case("pem")
                                            || extension.eq_ignore_ascii_case("key")
                                    })
                            }),
                        "context references a credential/configuration location that cannot be sent"
                    );
                    spec.paths.push(ContextPath {
                        label: label.to_string(),
                        path,
                    });
                }
            }
        }
        // Share the portable evidence path/count/focus validators; do not copy
        // the CLI's environment/profile/base-loading behavior.
        ReviewContext {
            focus: spec.focus.clone(),
            files: spec
                .paths
                .iter()
                .map(|entry| ContextFile {
                    label: entry.label.clone(),
                    path: entry.path.clone(),
                    content: String::new(),
                })
                .collect(),
        }
        .validate()?;
        Ok(spec)
    }

    pub const fn requires_choice(&self) -> bool {
        !self.focus.is_empty() || !self.paths.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct PreparedContext {
    pub snapshot_fingerprint: String,
    pub fingerprint: String,
    pub evidence: ReviewContext,
}

pub fn load(snapshot: &crate::repository::RepositorySnapshot) -> anyhow::Result<PreparedContext> {
    let policy = snapshot
        .review_policy
        .as_ref()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let mut evidence = ReviewContext {
        focus: policy.context.focus.clone(),
        files: Vec::new(),
    };
    let mut total = 0_usize;
    for entry in &policy.context.paths {
        let mut file = open_file(&snapshot.root, &entry.path)
            .context("could not safely open a declared context file")?;
        ensure!(
            file.metadata()?.is_file(),
            "declared context must be a regular file"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            ensure!(
                file.metadata()?.nlink() == 1,
                "hard-linked context files are not supported"
            );
        }
        let limit = MAX_CONTEXT_FILE_BYTES.min(policy.context.max_bytes.saturating_sub(total));
        let mut bytes = Vec::new();
        file.by_ref()
            .take(u64::try_from(limit + 1)?)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= limit,
            "declared context exceeds its per-file or aggregate budget; no text was truncated"
        );
        total += bytes.len();
        let content = String::from_utf8(bytes)
            .map_err(|_| anyhow::anyhow!("declared context must be UTF-8 text"))?;
        evidence.files.push(ContextFile {
            label: entry.label.clone(),
            path: entry.path.clone(),
            content,
        });
    }
    evidence.validate()?;
    let fingerprint = format!("{:x}", Sha256::digest(serde_json::to_vec(&evidence)?));
    Ok(PreparedContext {
        snapshot_fingerprint: scope(snapshot)?,
        fingerprint,
        evidence,
    })
}

pub fn scope(snapshot: &crate::repository::RepositorySnapshot) -> anyhow::Result<String> {
    let mut digest = Sha256::new();
    digest.update(crate::local_review::repository_identity(&snapshot.root)?.as_bytes());
    digest.update([0]);
    digest.update(crate::local_review::snapshot_fingerprint(snapshot).as_bytes());
    Ok(format!("{:x}", digest.finalize()))
}

pub fn validate(
    snapshot: &crate::repository::RepositorySnapshot,
    approved: Option<&PreparedContext>,
) -> anyhow::Result<()> {
    if let Some(approved) = approved {
        approved.evidence.validate()?;
        ensure!(
            format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&approved.evidence)?)
            ) == approved.fingerprint,
            "approved context no longer matches its fingerprint"
        );
        ensure!(
            approved.snapshot_fingerprint == scope(snapshot)?,
            "approved context belongs to a different repository snapshot"
        );
        let current = load(snapshot)?;
        ensure!(
            current.fingerprint == approved.fingerprint,
            "declared context changed after approval; prepare it again"
        );
    }
    Ok(())
}

#[cfg(unix)]
fn open_file(root: &Path, path: &str) -> anyhow::Result<std::fs::File> {
    use nix::{
        fcntl::{OFlag, open, openat},
        sys::stat::Mode,
    };
    let mut directory = open(
        root,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )?;
    let mut parts = path.split('/').peekable();
    while let Some(part) = parts.next() {
        let mut flags = OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK;
        if parts.peek().is_some() {
            flags |= OFlag::O_DIRECTORY;
        }
        let next = openat(&directory, part, flags, Mode::empty())?;
        if parts.peek().is_none() {
            return Ok(std::fs::File::from(next));
        }
        directory = next;
    }
    anyhow::bail!("declared context path is empty")
}

#[cfg(not(unix))]
fn open_file(_root: &Path, _path: &str) -> anyhow::Result<std::fs::File> {
    anyhow::bail!("safe context reads are supported only on Linux and macOS")
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn fixture() -> (tempfile::TempDir, crate::repository::RepositorySnapshot) {
        let root = tempfile::tempdir().unwrap();
        gix::init(root.path()).unwrap();
        std::fs::write(root.path().join(".gitignore"), "CONTEXT.md\n").unwrap();
        std::fs::write(root.path().join("source.rs"), "changed\n").unwrap();
        std::fs::write(root.path().join("CONTEXT.md"), "Check error handling\n").unwrap();
        std::fs::write(root.path().join(".cururu.toml"), "version = 1\n[policy]\nfocus = ['correctness']\n[context]\nconventions = ['CONTEXT.md']\nmax_bytes = 100\n[context.auto]\nenabled = false").unwrap();
        let snapshot = crate::repository::RepositorySnapshot::open(root.path()).unwrap();
        (root, snapshot)
    }

    #[test]
    fn parsing_and_opening_never_reads_declared_files_until_explicit_load() {
        let (root, _) = fixture();
        std::fs::remove_file(root.path().join("CONTEXT.md")).unwrap();
        let snapshot = crate::repository::RepositorySnapshot::open(root.path()).unwrap();
        assert!(
            snapshot
                .review_policy
                .as_ref()
                .unwrap()
                .context
                .requires_choice()
        );
        assert!(load(&snapshot).is_err());
        assert!(validate(&snapshot, None).is_ok());
    }

    #[test]
    fn ignored_context_changes_are_detected_even_when_the_diff_is_identical() {
        let (root, snapshot) = fixture();
        let approved = load(&snapshot).unwrap();
        assert!(validate(&snapshot, Some(&approved)).is_ok());
        assert_eq!(approved.evidence.files[0].content, "Check error handling\n");
        std::fs::write(root.path().join("CONTEXT.md"), "Changed conventions\n").unwrap();
        let changed = crate::repository::RepositorySnapshot::open(root.path()).unwrap();
        assert_eq!(
            crate::local_review::snapshot_fingerprint(&snapshot),
            crate::local_review::snapshot_fingerprint(&changed)
        );
        assert!(validate(&changed, Some(&approved)).is_err());
        let (_, other) = fixture();
        assert!(validate(&other, Some(&approved)).is_err());
    }

    #[test]
    fn limits_and_binary_content_are_rejected_without_truncation() {
        let (root, snapshot) = fixture();
        for bytes in [vec![b'x'; 101], vec![0xff], vec![0]] {
            std::fs::write(root.path().join("CONTEXT.md"), bytes).unwrap();
            assert!(load(&snapshot).is_err());
        }
    }

    #[test]
    fn unsupported_or_sensitive_declarations_fail_closed_without_echoing_them() {
        for path in [
            "../secret",
            ".git/config",
            ".ENV.local",
            "key.PEM",
            "key.key",
            "id_rsa",
            "credentials.json",
            "docs/**",
            "/etc/passwd",
        ] {
            let raw = format!("version = 1\n[context]\nadditional = ['{path}']");
            assert!(
                crate::repository_policy::RepositoryPolicy::parse(&raw).is_err(),
                "{path}"
            );
        }
        assert!(
            crate::repository_policy::RepositoryPolicy::parse(
                "version = 1\n[context.auto]\nenabled = true"
            )
            .is_err()
        );
        assert!(
            crate::repository_policy::RepositoryPolicy::parse(
                "version = 1\n[context]\nadditional = ['a', 'a']"
            )
            .is_err()
        );
        assert!(
            crate::repository_policy::RepositoryPolicy::parse(&format!(
                "version = 1\n[policy]\nfocus = ['{}']",
                "x".repeat(257)
            ))
            .is_err()
        );
    }

    #[test]
    #[cfg(unix)]
    fn descriptor_relative_reads_reject_symlink_parents_leaves_fifos_and_hardlinks() {
        use std::os::unix::fs::symlink;
        let (root, snapshot) = fixture();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("outside.md"), "synthetic marker").unwrap();
        std::fs::remove_file(root.path().join("CONTEXT.md")).unwrap();
        symlink(
            outside.path().join("outside.md"),
            root.path().join("CONTEXT.md"),
        )
        .unwrap();
        assert!(load(&snapshot).is_err());
        symlink(outside.path(), root.path().join("docs")).unwrap();
        assert!(open_file(root.path(), "docs/outside.md").is_err());
        std::fs::remove_file(root.path().join("CONTEXT.md")).unwrap();
        nix::unistd::mkfifo(
            &root.path().join("CONTEXT.md"),
            nix::sys::stat::Mode::S_IRUSR,
        )
        .unwrap();
        assert!(load(&snapshot).is_err());
        std::fs::remove_file(root.path().join("CONTEXT.md")).unwrap();
        std::fs::hard_link(
            outside.path().join("outside.md"),
            root.path().join("CONTEXT.md"),
        )
        .unwrap();
        assert!(load(&snapshot).is_err());
    }
}
