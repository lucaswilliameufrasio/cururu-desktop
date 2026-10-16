//! Host-owned, restriction-only interpretation of the shared repository schema.

use std::{fs, io::Read as _, path::Path};

use anyhow::{Context as _, ensure};
use cururu_core::Severity;
use cururu_engine::config::CururuToml;
use sha2::{Digest as _, Sha256};

const MAX_CONFIG_BYTES: usize = 65_536;

#[derive(Debug, Clone, PartialEq)]
pub struct RepositoryPolicy {
    pub fingerprint: String,
    pub max_diff_bytes: usize,
    pub chunk_bytes: usize,
    pub minimum_confidence: f32,
    pub max_findings: usize,
    pub allowed_severities: Vec<Severity>,
    pub suggested_changes: bool,
    pub ignore: Vec<String>,
    pub provider_ignored: bool,
}

impl Default for RepositoryPolicy {
    fn default() -> Self {
        Self {
            fingerprint: "absent".to_string(),
            max_diff_bytes: crate::local_review::MAX_LOCAL_DIFF_BYTES,
            chunk_bytes: crate::local_review::MAX_LOCAL_FILE_DIFF_BYTES,
            minimum_confidence: 0.0,
            max_findings: 100,
            allowed_severities: Severity::all(),
            suggested_changes: true,
            ignore: vec![],
            provider_ignored: false,
        }
    }
}

impl RepositoryPolicy {
    pub fn load(root: &Path) -> anyhow::Result<Self> {
        let path = root.join(".cururu.toml");
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Ok(metadata) => ensure!(
                metadata.is_file(),
                ".cururu.toml must be a regular, non-symlink file"
            ),
            Err(_) => anyhow::bail!("could not inspect .cururu.toml"),
        }
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
        }
        let file = options
            .open(path)
            .context("could not open .cururu.toml safely")?;
        ensure!(
            file.metadata()?.is_file(),
            ".cururu.toml must be a regular file"
        );
        let mut bytes = Vec::new();
        file.take(u64::try_from(MAX_CONFIG_BYTES + 1)?)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= MAX_CONFIG_BYTES,
            ".cururu.toml exceeds 64 KiB"
        );
        let raw = std::str::from_utf8(&bytes).context(".cururu.toml must be UTF-8")?;
        Self::parse(raw)
    }

    pub fn parse(raw: &str) -> anyhow::Result<Self> {
        // Do not expose parser snippets: repository config may contain secrets.
        let parsed = CururuToml::parse(raw).map_err(|_| {
            anyhow::anyhow!(".cururu.toml has an invalid schema or unsupported version")
        })?;
        ensure!(
            parsed.evaluator.is_none(),
            "[evaluator] is local-only and cannot be enabled by .cururu.toml"
        );
        ensure!(
            parsed.config.is_none() && parsed.context.is_none() && parsed.analysis.is_none(),
            "desktop review does not yet support [config], [context], or [analysis]; no extra context will be loaded"
        );
        let mut policy = Self {
            fingerprint: format!("{:x}", Sha256::digest(raw.as_bytes())),
            provider_ignored: parsed.provider.is_some(),
            ..Self::default()
        };
        if let Some(review) = parsed.review {
            ensure!(
                review.profile.is_none()
                    && review.language.is_none()
                    && review.tone.is_none()
                    && review.technical_level.is_none()
                    && review.suggestion_detail.is_none()
                    && review.comment_mode.is_none()
                    && review.recommendations.is_none(),
                "desktop review currently supports only review.max_diff_bytes, chunk_bytes, and ignore"
            );
            policy.max_diff_bytes = review
                .max_diff_bytes
                .unwrap_or(policy.max_diff_bytes)
                .min(policy.max_diff_bytes);
            policy.chunk_bytes = review
                .chunk_bytes
                .unwrap_or(policy.chunk_bytes)
                .min(policy.chunk_bytes);
            policy.ignore = review.ignore.unwrap_or_default();
            ensure!(
                policy.ignore.len() <= 256,
                "review.ignore exceeds 256 patterns"
            );
            policy.ignore_set()?;
        }
        if let Some(repository) = parsed.policy {
            ensure!(
                repository.fail_on.is_none()
                    && repository.focus.as_ref().is_none_or(Vec::is_empty)
                    && !repository.incremental.unwrap_or(false)
                    && !repository.synthesis.unwrap_or(false),
                "desktop review does not yet support policy.fail_on, focus, incremental, or synthesis"
            );
            if let Some(confidence) = repository.minimum_confidence {
                cururu_engine::config::validate_confidence(confidence)?;
                policy.minimum_confidence = confidence;
            }
            policy.max_findings = repository
                .max_findings
                .unwrap_or(policy.max_findings)
                .min(policy.max_findings);
            policy.suggested_changes = repository.suggested_changes.unwrap_or(true);
            if let Some(severities) = repository.allowed_severities {
                policy.allowed_severities = severities
                    .iter()
                    .map(|name| {
                        Severity::from_name(name)
                            .context("policy.allowed_severities contains an unsupported severity")
                    })
                    .collect::<anyhow::Result<_>>()?;
            }
        }
        ensure!(
            policy.max_diff_bytes > 0 && policy.chunk_bytes > 0,
            "repository review limits must be positive"
        );
        Ok(policy)
    }

    fn ignore_set(&self) -> anyhow::Result<globset::GlobSet> {
        let mut builder = globset::GlobSetBuilder::new();
        for pattern in &self.ignore {
            builder.add(
                globset::Glob::new(pattern)
                    .map_err(|_| anyhow::anyhow!("review.ignore contains an invalid glob"))?,
            );
        }
        builder.build().context("could not compile review.ignore")
    }

    pub fn review_files(
        &self,
        files: &[cururu_core::ChangedFile],
    ) -> anyhow::Result<Vec<cururu_core::ChangedFile>> {
        let ignored = self.ignore_set()?;
        Ok(files
            .iter()
            .filter(|file| file.path != ".cururu.toml" && !ignored.is_match(&file.path))
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_limits_cannot_expand_local_budgets_or_change_provider() {
        let policy = RepositoryPolicy::parse("version = 1\n[provider]\nbase_url = 'https://untrusted.test'\n[review]\nmax_diff_bytes = 999999\nchunk_bytes = 999999\n[policy]\nmax_findings = 999999").unwrap();
        assert_eq!(
            policy.max_diff_bytes,
            crate::local_review::MAX_LOCAL_DIFF_BYTES
        );
        assert_eq!(
            policy.chunk_bytes,
            crate::local_review::MAX_LOCAL_FILE_DIFF_BYTES
        );
        assert_eq!(policy.max_findings, 100);
        assert!(policy.provider_ignored);
    }

    #[test]
    fn restriction_fields_are_parsed_and_ignored_files_are_not_reviewed() {
        let policy = RepositoryPolicy::parse("version = 1\n[review]\nignore = ['tests/**']\nmax_diff_bytes = 100\nchunk_bytes = 90\n[policy]\nminimum_confidence = 0.95\nmax_findings = 2\nallowed_severities = ['high']\nsuggested_changes = false").unwrap();
        assert_eq!(policy.max_diff_bytes, 100);
        assert_eq!(policy.chunk_bytes, 90);
        assert_eq!(policy.minimum_confidence, 0.95);
        assert_eq!(policy.allowed_severities, vec![Severity::High]);
        assert!(!policy.suggested_changes);
        let files = cururu_core::parse_unified_diff(
            "diff --git a/tests/a.rs b/tests/a.rs\n--- a/tests/a.rs\n+++ b/tests/a.rs\n@@ -1 +1 @@\n-old\n+new\n",
        );
        assert_eq!(policy.review_files(&files).unwrap(), vec![]);
    }

    #[test]
    fn malformed_unsupported_and_permission_expanding_config_fails_closed() {
        for raw in [
            "version = 1\n[evaluator]\nmode = 'filter'",
            "version = 1\n[context]\nadditional = ['../secret']",
            "version = 1\n[review]\nprofile = 'strict'",
            "version = 1\n[review]\nignore = ['[']",
            "version = 1\n[policy]\nminimum_confidence = nan",
            "version = 1\n[review]\nchunk_bytes = 0",
        ] {
            assert!(RepositoryPolicy::parse(raw).is_err());
        }
        let error = RepositoryPolicy::parse("version = 'synthetic-secret-marker'").unwrap_err();
        assert!(!format!("{error:#}").contains("synthetic-secret-marker"));
    }

    #[test]
    fn config_reader_is_bounded_and_does_not_follow_symlinks() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            RepositoryPolicy::load(root.path()).unwrap(),
            RepositoryPolicy::default()
        );
        fs::write(root.path().join(".cururu.toml"), "x".repeat(65_537)).unwrap();
        assert!(RepositoryPolicy::load(root.path()).is_err());
        #[cfg(unix)]
        {
            let other = tempfile::tempdir().unwrap();
            fs::write(other.path().join("policy"), "version = 1").unwrap();
            fs::remove_file(root.path().join(".cururu.toml")).unwrap();
            std::os::unix::fs::symlink(
                other.path().join("policy"),
                root.path().join(".cururu.toml"),
            )
            .unwrap();
            assert!(RepositoryPolicy::load(root.path()).is_err());
        }
    }
}
