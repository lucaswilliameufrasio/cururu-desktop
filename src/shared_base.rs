//! Memory-only, explicitly approved shared repository configuration.

use anyhow::{Context as _, ensure};
use cururu_engine::config::{ConfigToml, CururuToml};
use sha2::{Digest as _, Sha256};

pub const MAX_BASE_BYTES: usize = 65_536;

#[derive(Clone, PartialEq, Eq)]
pub struct ConfigDocument(String);

impl std::fmt::Debug for ConfigDocument {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ConfigDocument([REDACTED])")
    }
}

impl ConfigDocument {
    pub fn new(raw: &str) -> Self {
        Self(raw.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedBaseSource {
    pub owner: String,
    pub repository: String,
    pub commit: String,
    pub path: String,
}

impl SharedBaseSource {
    pub fn parse(config: ConfigToml) -> anyhow::Result<Self> {
        let (Some(repository), Some(commit), Some(path)) =
            (config.base, config.base_ref, config.base_path)
        else {
            anyhow::bail!("shared base requires repository, full commit SHA and path together");
        };
        let (owner, repository) = repository
            .split_once('/')
            .context("shared base requires an owner/repository")?;
        let source = Self {
            owner: owner.to_string(),
            repository: repository.to_string(),
            commit,
            path,
        };
        source.validate()?;
        Ok(source)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        for part in [&self.owner, &self.repository] {
            ensure!(
                !part.is_empty()
                    && part.len() <= 100
                    && part != "."
                    && part != ".."
                    && part
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte)),
                "shared base repository identity is invalid"
            );
        }
        ensure!(
            self.commit.len() == 40 && self.commit.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "shared base must use a full 40-character commit SHA"
        );
        cururu_engine::review_context::ReviewContext {
            focus: Vec::new(),
            files: vec![cururu_core::ContextFile {
                label: "Shared config".to_string(),
                path: self.path.clone(),
                content: String::new(),
            }],
        }
        .validate()?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovedBase {
    pub source: SharedBaseSource,
    pub fingerprint: String,
    repository_identity: String,
    local_fingerprint: String,
    base: ConfigDocument,
}

impl ApprovedBase {
    /// Prepares only an in-memory preview. The caller must explicitly approve
    /// before attaching the resulting policy to the active snapshot.
    pub fn prepare(
        snapshot: &crate::repository::RepositorySnapshot,
        raw: &str,
    ) -> anyhow::Result<Self> {
        ensure!(raw.len() <= MAX_BASE_BYTES, "shared base exceeds 64 KiB");
        let parsed = CururuToml::parse(raw)
            .map_err(|_| anyhow::anyhow!("shared base schema/version is invalid"))?;
        ensure!(
            parsed.config.is_none(),
            "nested shared bases are unsupported and will not be fetched"
        );
        ensure!(
            parsed.evaluator.is_none(),
            "shared base cannot configure local evaluator permissions"
        );
        let local = snapshot
            .review_policy
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let source = local
            .shared_base
            .clone()
            .context("local config does not declare a shared base")?;
        source.validate()?;
        let repository_identity = crate::local_review::repository_identity(&snapshot.root)?;
        let mut digest = Sha256::new();
        for value in [
            &repository_identity,
            &local.fingerprint,
            &source.owner,
            &source.repository,
            &source.commit,
            &source.path,
            raw,
        ] {
            digest.update(value.as_bytes());
            digest.update([0]);
        }
        let approved = Self {
            source,
            repository_identity,
            local_fingerprint: local.fingerprint.clone(),
            fingerprint: format!("{:x}", digest.finalize()),
            base: ConfigDocument::new(raw),
        };
        approved.resolve(snapshot)?;
        Ok(approved)
    }

    pub fn resolve(
        &self,
        snapshot: &crate::repository::RepositorySnapshot,
    ) -> anyhow::Result<crate::repository_policy::RepositoryPolicy> {
        self.source.validate()?;
        let local = snapshot
            .review_policy
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        ensure!(
            crate::local_review::repository_identity(&snapshot.root)? == self.repository_identity
                && local.fingerprint == self.local_fingerprint
                && local.shared_base.as_ref() == Some(&self.source),
            "shared base approval does not match the current checkout/local configuration"
        );
        let document = local
            .local_document
            .as_ref()
            .context("local config document is unavailable")?;
        let merged = cururu_engine::config::compose_toml(&self.base.0, &document.0)
            .map_err(|_| anyhow::anyhow!("could not compose shared and local configuration"))?;
        ensure!(
            merged.len() <= MAX_BASE_BYTES * 2,
            "composed repository configuration exceeds 128 KiB"
        );
        let mut policy = crate::repository_policy::RepositoryPolicy::parse(&merged)?;
        // Keep raw local identity stable for manual diff/command guards; the
        // independently recorded base fingerprint binds analysis and approval.
        policy.fingerprint.clone_from(&local.fingerprint);
        policy.local_document = Some(document.clone());
        policy.base_approval = Some(self.clone());
        Ok(policy)
    }
}

pub fn fingerprint(snapshot: &crate::repository::RepositorySnapshot) -> Option<String> {
    snapshot
        .review_policy
        .as_ref()
        .ok()?
        .base_approval
        .as_ref()
        .map(|base| base.fingerprint.clone())
}

pub fn approval(snapshot: &crate::repository::RepositorySnapshot) -> Option<ApprovedBase> {
    snapshot.review_policy.as_ref().ok()?.base_approval.clone()
}

pub fn unresolved_policy(
    snapshot: &crate::repository::RepositorySnapshot,
) -> anyhow::Result<crate::repository_policy::RepositoryPolicy> {
    let local = snapshot
        .review_policy
        .as_ref()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let document = local
        .local_document
        .as_ref()
        .context("local configuration is unavailable")?;
    crate::repository_policy::RepositoryPolicy::parse(&document.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_document() -> String {
        format!(
            "version = 1\nsynthetic_secret = 'do-not-echo-local'\n[config]\nbase = 'org/policy'\nbase_ref = '{}'\nbase_path = '.cururu.toml'\n[review]\nignore = ['local/**']\n[policy]\nmax_findings = 3\nfocus = ['correctness']",
            "a".repeat(40)
        )
    }

    fn fixture() -> (tempfile::TempDir, crate::repository::RepositorySnapshot) {
        let root = tempfile::tempdir().unwrap();
        gix::init(root.path()).unwrap();
        std::fs::write(root.path().join(".cururu.toml"), local_document()).unwrap();
        std::fs::write(root.path().join("source.rs"), "changed\n").unwrap();
        let snapshot = crate::repository::RepositorySnapshot::open(root.path()).unwrap();
        (root, snapshot)
    }

    #[test]
    fn shared_configuration_is_unresolved_until_explicit_approval_and_uses_canonical_merge() {
        let (root, snapshot) = fixture();
        assert!(crate::local_review::validate_snapshot(&snapshot).is_err());
        assert!(crate::local_context::load(&snapshot).is_err());
        let base = "version = 1\nsynthetic_secret = 'do-not-echo-base'\n[provider]\nbase_url = 'https://untrusted.test'\n[review]\nignore = ['base/**', 'local/**']\nchunk_bytes = 999999\n[policy]\nmax_findings = 10\nminimum_confidence = 0.8\nfocus = ['security']\n[context]\nconventions = ['MISSING.md']";
        let prepared = ApprovedBase::prepare(&snapshot, base).unwrap();
        assert!(crate::local_review::validate_snapshot(&snapshot).is_err());
        let resolved = snapshot.clone().with_shared_base(Some(&prepared)).unwrap();
        let policy = resolved.review_policy.as_ref().unwrap();
        assert_eq!(policy.max_findings, 3);
        assert_eq!(policy.minimum_confidence, 0.8);
        assert_eq!(policy.ignore, ["base/**", "local/**"]);
        assert_eq!(policy.context.focus, ["security", "correctness"]);
        assert_eq!(
            policy.chunk_bytes,
            crate::local_review::MAX_LOCAL_FILE_DIFF_BYTES
        );
        assert!(policy.provider_ignored);
        assert!(policy.require_ready().is_ok());
        assert!(!format!("{policy:?}").contains("do-not-echo"));
        assert!(!format!("{prepared:?}").contains("do-not-echo"));
        // Resolving config must never read its declared context file.
        assert!(!root.path().join("MISSING.md").exists());
        assert!(
            crate::local_context_ui::ContextState::Undecided
                .resolve(&resolved)
                .is_err()
        );
        assert!(crate::local_context::load(&resolved).is_err());
        let unresolved = unresolved_policy(&resolved).unwrap();
        assert!(unresolved.require_ready().is_err());
        assert_eq!(
            crate::local_command::CommandContext::from_snapshot(&snapshot).unwrap(),
            crate::local_command::CommandContext::from_snapshot(&resolved).unwrap()
        );
    }

    #[test]
    fn approval_is_scoped_to_checkout_and_exact_local_document_and_does_not_fetch_again() {
        let (root, snapshot) = fixture();
        let approved =
            ApprovedBase::prepare(&snapshot, "version = 1\n[policy]\nmax_findings = 8").unwrap();
        let (_other_root, other) = fixture();
        assert!(approved.resolve(&other).is_err());
        let resolved = snapshot.with_shared_base(Some(&approved)).unwrap();
        let fresh = crate::repository::RepositorySnapshot::open(root.path())
            .unwrap()
            .with_shared_base(Some(&approved))
            .unwrap();
        assert_eq!(fingerprint(&resolved), fingerprint(&fresh));
        let alternative =
            ApprovedBase::prepare(&fresh, "version = 1\n[policy]\nmax_findings = 9").unwrap();
        assert_ne!(approved.fingerprint, alternative.fingerprint);
        std::fs::write(
            root.path().join(".cururu.toml"),
            format!("{}\n# edited\n", local_document()),
        )
        .unwrap();
        let changed = crate::repository::RepositorySnapshot::open(root.path()).unwrap();
        assert!(changed.with_shared_base(Some(&approved)).is_err());
    }

    #[test]
    fn reopening_for_context_preview_keeps_approved_base_without_fetching_again() {
        let (root, _) = fixture();
        std::fs::write(root.path().join("CONTEXT.md"), "Check errors\n").unwrap();
        let snapshot = crate::repository::RepositorySnapshot::open(root.path()).unwrap();
        let approved = ApprovedBase::prepare(
            &snapshot,
            "version = 1\n[context]\nconventions = ['CONTEXT.md']",
        )
        .unwrap();
        let resolved = snapshot.with_shared_base(Some(&approved)).unwrap();
        // Prepare the same UI preview path against a fresh local snapshot.
        let fresh = resolved.reopen_with_approved_base().unwrap();
        assert_eq!(fingerprint(&fresh), fingerprint(&resolved));
        assert_eq!(
            crate::local_context::scope(&fresh).unwrap(),
            crate::local_context::scope(&resolved).unwrap()
        );
        let context = crate::local_context::load(&fresh).unwrap();
        assert_eq!(context.evidence.files[0].content, "Check errors\n");
        assert_eq!(context.evidence.focus, ["correctness"]);
        assert!(crate::local_context::validate(&fresh, Some(&context)).is_ok());
    }

    #[test]
    fn approved_base_is_reused_locally_and_config_edits_block_suggestion_application() {
        let (root, snapshot) = fixture();
        let approved = ApprovedBase::prepare(&snapshot, "version = 1").unwrap();
        let resolved = snapshot.with_shared_base(Some(&approved)).unwrap();
        let finding = cururu_core::ReviewFinding {
            path: "source.rs".to_string(),
            line: Some(1),
            severity: "medium".to_string(),
            title: "Check".to_string(),
            message: "Check this call".to_string(),
            suggestion: String::new(),
            confidence: 0.9,
            suggested_change: Some(cururu_core::SuggestedChange {
                replacement: "checked".to_string(),
            }),
            source: None,
            rule: None,
        };
        let preview = crate::suggested_change::prepare_with_context(
            root.path(),
            &crate::local_review::repository_identity(root.path()).unwrap(),
            &crate::local_review::snapshot_fingerprint(&resolved),
            &finding,
            None,
            Some(&approved),
            None,
        )
        .unwrap();
        std::fs::write(
            root.path().join(".cururu.toml"),
            format!("{}\n# changed\n", local_document()),
        )
        .unwrap();
        assert!(crate::suggested_change::apply(&preview).is_err());
        assert_eq!(
            std::fs::read_to_string(root.path().join("source.rs")).unwrap(),
            "changed\n"
        );
        std::fs::write(root.path().join(".cururu.toml"), local_document()).unwrap();
        crate::suggested_change::apply(&preview).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("source.rs")).unwrap(),
            "checked\n"
        );
    }

    #[test]
    fn nested_unsafe_oversized_and_unsupported_shared_configuration_fails_closed() {
        let (_root, snapshot) = fixture();
        for raw in [
            "version = 2",
            "version = 1\n[config]\nbase = 'nested/source'",
            "version = 1\n[evaluator]\nmode = 'filter'",
            "version = 1\n[analysis]\nenabled = true",
            "version = 1\n[context]\nadditional = ['.env']",
        ] {
            assert!(ApprovedBase::prepare(&snapshot, raw).is_err());
        }
        assert!(ApprovedBase::prepare(&snapshot, &"x".repeat(MAX_BASE_BYTES + 1)).is_err());
        for (repository, commit, path) in [
            ("../repo", "a".repeat(40), ".cururu.toml"),
            ("org/repo", "main".to_string(), ".cururu.toml"),
            ("org/repo", "a".repeat(40), "../secret"),
            ("org/repo?token=x", "a".repeat(40), ".cururu.toml"),
        ] {
            assert!(
                SharedBaseSource::parse(ConfigToml {
                    base: Some(repository.to_string()),
                    base_ref: Some(commit),
                    base_path: Some(path.to_string())
                })
                .is_err()
            );
        }
    }
}
