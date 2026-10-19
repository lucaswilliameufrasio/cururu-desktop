//! Host-owned restrictions and explicitly previewed context declarations.

use std::{fs, io::Read as _, path::Path};

use anyhow::{Context as _, ensure};
use cururu_core::Severity;
use cururu_engine::config::CururuToml;
use sha2::{Digest as _, Sha256};

const MAX_CONFIG_BYTES: usize = 65_536;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewReuse {
    Fresh,
    ExactArchive,
}

impl ReviewReuse {
    const fn from_enabled(enabled: bool) -> Self {
        if enabled {
            Self::ExactArchive
        } else {
            Self::Fresh
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RepositoryPolicy {
    pub profile: Option<String>,
    pub publication_hint: Option<String>,
    pub fingerprint: String,
    pub max_diff_bytes: usize,
    pub chunk_bytes: usize,
    pub minimum_confidence: f32,
    pub max_findings: usize,
    pub fail_on: cururu_core::FailOn,
    pub synthesis: bool,
    pub review_reuse: ReviewReuse,
    pub allowed_severities: Vec<Severity>,
    pub suggested_changes: bool,
    pub ignore: Vec<String>,
    pub provider_ignored: bool,
    pub context: crate::local_context::ContextSpec,
    pub analysis: crate::local_analysis::AnalysisSpec,
    pub shared_base: Option<crate::shared_base::SharedBaseSource>,
    pub base_approval: Option<crate::shared_base::ApprovedBase>,
    pub local_document: Option<crate::shared_base::ConfigDocument>,
}

impl Default for RepositoryPolicy {
    fn default() -> Self {
        Self {
            profile: None,
            publication_hint: None,
            fingerprint: "absent".to_string(),
            max_diff_bytes: crate::local_review::MAX_LOCAL_DIFF_BYTES,
            chunk_bytes: crate::local_review::MAX_LOCAL_FILE_DIFF_BYTES,
            minimum_confidence: 0.0,
            max_findings: crate::local_review::MAX_REVIEW_FINDINGS,
            fail_on: cururu_core::FailOn::Off,
            synthesis: false,
            review_reuse: ReviewReuse::Fresh,
            allowed_severities: Severity::all(),
            suggested_changes: true,
            ignore: vec![],
            provider_ignored: false,
            context: crate::local_context::ContextSpec::default(),
            analysis: crate::local_analysis::AnalysisSpec::default(),
            shared_base: None,
            base_approval: None,
            local_document: None,
        }
    }
}

impl RepositoryPolicy {
    fn presentation(parsed: &CururuToml) -> cururu_engine::review_preferences::ReviewPreferences {
        let Some(review) = &parsed.review else {
            return cururu_engine::review_preferences::ReviewPreferences::default();
        };
        cururu_engine::review_preferences::ReviewPreferences {
            language: review.language.clone(),
            tone: review.tone.clone(),
            technical_level: review.technical_level.clone(),
            suggestion_detail: review.suggestion_detail.clone(),
        }
    }

    fn selected_profile(
        parsed: &CururuToml,
    ) -> anyhow::Result<Option<cururu_engine::review_policy::ReviewPolicy>> {
        parsed
            .review
            .as_ref()
            .and_then(|review| review.profile.as_deref())
            .map(cururu_engine::review_policy::profile_defaults)
            .transpose()
            .map_err(|_| anyhow::anyhow!("review.profile is unsupported"))
    }

    fn from_profile(profile: Option<&cururu_engine::review_policy::ReviewPolicy>) -> Self {
        let mut policy = Self::default();
        if let Some(profile) = profile {
            policy.review_reuse = ReviewReuse::from_enabled(profile.incremental);
            policy.profile = Some(profile.profile.trim().to_ascii_lowercase());
            policy.minimum_confidence = profile.minimum_confidence;
            policy.max_findings = profile
                .max_findings
                .min(crate::local_review::MAX_REVIEW_FINDINGS);
            policy.fail_on = profile.fail_on;
            policy
                .allowed_severities
                .clone_from(&profile.allowed_severities);
            policy.suggested_changes = profile.suggested_changes;
            policy.synthesis = profile.synthesis;
        }
        policy
    }

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
        let profile = Self::selected_profile(&parsed)?;
        let preferences = Self::presentation(&parsed);
        let mut policy = Self {
            analysis: crate::local_analysis::AnalysisSpec::parse(parsed.analysis)?,
            shared_base: parsed
                .config
                .map(crate::shared_base::SharedBaseSource::parse)
                .transpose()?,
            fingerprint: format!("{:x}", Sha256::digest(raw.as_bytes())),
            provider_ignored: parsed.provider.is_some(),
            context: crate::local_context::ContextSpec::parse(
                parsed.context,
                parsed
                    .policy
                    .as_ref()
                    .and_then(|policy| policy.focus.clone())
                    .unwrap_or_else(|| {
                        profile
                            .as_ref()
                            .map_or_else(Vec::new, |profile| profile.focus.clone())
                    }),
            )?
            .with_preferences(preferences)?,
            ..Self::from_profile(profile.as_ref())
        };
        if policy.shared_base.is_some() {
            policy.local_document = Some(crate::shared_base::ConfigDocument::new(raw));
        }
        if let Some(review) = parsed.review {
            ensure!(
                !review.recommendations.unwrap_or(false),
                "repository recommendations cannot enable extra provider requests; only recommendations = false is supported"
            );
            if let Some(mode) = review.comment_mode {
                cururu_engine::config::validate_choice(
                    "review.comment_mode",
                    &mode,
                    &["inline", "summary"],
                )
                .map_err(|_| anyhow::anyhow!("review.comment_mode is invalid"))?;
                policy.publication_hint = Some(mode.trim().to_ascii_lowercase());
            }
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
            if let Some(incremental) = repository.incremental {
                policy.review_reuse = ReviewReuse::from_enabled(incremental);
            }
            if let Some(threshold) = repository.fail_on {
                policy.fail_on = cururu_core::FailOn::from_name(&threshold)
                    .context("policy.fail_on contains an unsupported severity")?;
            }
            policy.synthesis = repository.synthesis.unwrap_or(policy.synthesis);
            if let Some(confidence) = repository.minimum_confidence {
                cururu_engine::config::validate_confidence(confidence)?;
                policy.minimum_confidence = confidence;
            }
            policy.max_findings = repository
                .max_findings
                .unwrap_or(policy.max_findings)
                .min(crate::local_review::MAX_REVIEW_FINDINGS);
            policy.suggested_changes = repository
                .suggested_changes
                .unwrap_or(policy.suggested_changes);
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
        self.require_ready()?;
        let ignored = self.ignore_set()?;
        Ok(files
            .iter()
            .filter(|file| file.path != ".cururu.toml" && !ignored.is_match(&file.path))
            .cloned()
            .collect())
    }

    pub fn require_ready(&self) -> anyhow::Result<()> {
        ensure!(
            self.shared_base.is_none() || self.base_approval.is_some(),
            "shared base requires explicit fetch, preview and approval before AI review"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presentation_is_bounded_and_repository_publication_hints_do_not_choose_personal_mode() {
        let policy = RepositoryPolicy::parse(
            "version = 1\n[review]\ncomment_mode = 'summary'\nrecommendations = false",
        )
        .unwrap();
        assert_eq!(policy.publication_hint.as_deref(), Some("summary"));
        assert!(!policy.context.requires_choice());
        for raw in [
            "language = ''",
            "tone = 'unsupported'",
            "technical_level = 'unsupported'",
            "suggestion_detail = 'unsupported'",
            "comment_mode = 'unsupported'",
            "recommendations = true",
        ] {
            assert!(RepositoryPolicy::parse(&format!("version = 1\n[review]\n{raw}")).is_err());
        }
    }

    #[test]
    fn explicit_profiles_match_the_shared_catalog_but_absence_preserves_desktop_defaults() {
        let absent = RepositoryPolicy::parse("version = 1").unwrap();
        assert_eq!(
            absent.minimum_confidence,
            RepositoryPolicy::default().minimum_confidence
        );
        assert_eq!(
            absent.max_findings,
            crate::local_review::MAX_REVIEW_FINDINGS
        );
        assert!(absent.profile.is_none());
        for name in ["balanced", "strict", "security", "minimal"] {
            let expected = cururu_engine::review_policy::profile_defaults(name).unwrap();
            let policy =
                RepositoryPolicy::parse(&format!("version = 1\n[review]\nprofile = '{name}'"))
                    .unwrap();
            assert_eq!(policy.minimum_confidence, expected.minimum_confidence);
            assert_eq!(policy.max_findings, expected.max_findings);
            assert_eq!(policy.fail_on, expected.fail_on);
            assert_eq!(policy.allowed_severities, expected.allowed_severities);
            assert_eq!(policy.suggested_changes, expected.suggested_changes);
            assert_eq!(policy.context.focus, expected.focus);
            assert_eq!(policy.profile.as_deref(), Some(name));
        }
    }

    #[test]
    fn explicit_overrides_follow_profile_and_are_bounded_by_host_not_profile_limits() {
        let raw = "version = 1\n[review]\nprofile = 'minimal'\n[policy]\nmax_findings = 80\nminimum_confidence = 0.9\nallowed_severities = ['medium']\nsuggested_changes = true\nsynthesis = true\nfail_on = 'critical'\nfocus = ['correctness']";
        let policy = RepositoryPolicy::parse(raw).unwrap();
        assert_eq!(policy.max_findings, 80);
        assert_eq!(policy.minimum_confidence, 0.9);
        assert_eq!(policy.allowed_severities, [Severity::Medium]);
        assert!(policy.suggested_changes && policy.synthesis);
        assert_eq!(policy.fail_on, cururu_core::FailOn::Critical);
        assert_eq!(policy.context.focus, ["correctness"]);
        let clamped =
            RepositoryPolicy::parse(&raw.replace("max_findings = 80", "max_findings = 999999"))
                .unwrap();
        assert_eq!(
            clamped.max_findings,
            crate::local_review::MAX_REVIEW_FINDINGS
        );
        let security =
            RepositoryPolicy::parse("version = 1\n[review]\nprofile = 'security'").unwrap();
        assert!(security.context.requires_choice());
        let override_focus = RepositoryPolicy::parse(
            "version = 1\n[review]\nprofile = 'security'\n[policy]\nfocus = []",
        )
        .unwrap();
        assert!(!override_focus.context.requires_choice());
    }

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
            "version = 1\n[review]\nprofile = 'unsupported'",
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
