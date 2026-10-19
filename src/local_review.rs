use std::{fmt, path::Path};

use anyhow::Context as _;
use sha2::{Digest as _, Sha256};

use crate::{repository::RepositorySnapshot, session_store::LocalReviewSettings};

const KEYRING_SERVICE: &str = "com.cururu.desktop.local-review";
const OPENAI_KEY_ACCOUNT: &str = "openai-compatible";
pub const DEFAULT_LOCAL_REVIEW_ENDPOINT: &str = "https://api.openai.com/v1";
pub const MAX_LOCAL_FILE_DIFF_BYTES: usize = 45_000;
pub const MAX_LOCAL_DIFF_BYTES: usize = 180_000;
pub const MAX_LOCAL_REVIEW_FILES: usize = 100;
pub const MAX_LOCAL_REVIEW_CHUNKS: usize = 4;
const MAX_REVIEW_FINDINGS: usize = 100;
const REVIEW_MAX_OUTPUT_TOKENS: u32 = 4_000;
const REVIEW_TEMPERATURE: f32 = 0.1;
const LOCAL_REVIEW_NOTE: &str = "Desktop-local review note: proposed changes are informational suggestions only. Never claim that a file was modified or that a check was run.";

#[derive(Clone, PartialEq, Eq)]
pub struct LocalReviewSecrets {
    api_key: String,
}

impl fmt::Debug for LocalReviewSecrets {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalReviewSecrets")
            .field("api_key", &"[REDACTED]")
            .finish()
    }
}

impl LocalReviewSecrets {
    pub fn load() -> anyhow::Result<Option<Self>> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, OPENAI_KEY_ACCOUNT)
            .map_err(|_| anyhow::anyhow!("operating-system credential store is unavailable"))?;
        match entry.get_password() {
            Ok(api_key) => {
                validate_api_key(&api_key)?;
                Ok(Some(Self { api_key }))
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => anyhow::bail!("could not read local review credentials from the OS keyring"),
        }
    }

    pub fn save(api_key: &str) -> anyhow::Result<()> {
        validate_api_key(api_key)?;
        let entry = keyring::Entry::new(KEYRING_SERVICE, OPENAI_KEY_ACCOUNT)
            .map_err(|_| anyhow::anyhow!("operating-system credential store is unavailable"))?;
        entry.set_password(api_key).map_err(|_| {
            anyhow::anyhow!("could not save local review credentials to the OS keyring")
        })
    }

    fn as_str(&self) -> &str {
        &self.api_key
    }
}

#[derive(Debug, Clone)]
pub struct PolicySuppressedFinding {
    pub finding: cururu_core::ReviewFinding,
    pub reason: &'static str,
}

#[derive(Debug, Clone)]
pub struct LocalReviewResult {
    pub base: Option<crate::shared_base::ApprovedBase>,
    pub context: Option<crate::local_context::PreparedContext>,
    pub review: cururu_core::ReviewResult,
    pub usage: Option<cururu_engine::ReviewUsage>,
    pub diff_fingerprint: String,
    pub head_oid: Option<String>,
    pub diff_bytes: usize,
    pub chunk_count: usize,
    pub discarded_findings: usize,
    pub policy_suppressed: Vec<PolicySuppressedFinding>,
}

pub fn repository_identity(root: &Path) -> anyhow::Result<String> {
    let canonical_root = root
        .canonicalize()
        .context("could not resolve the local repository path")?;
    let identity = canonical_root
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("local repository path is not UTF-8"))?;
    anyhow::ensure!(
        canonical_root.is_absolute() && !identity.chars().any(char::is_control),
        "local repository path is not a safe absolute path"
    );
    Ok(identity.to_string())
}

pub fn config_fingerprint(settings: &LocalReviewSettings) -> anyhow::Result<String> {
    settings.validate()?;
    let mut digest = Sha256::new();
    digest.update(b"cururu-desktop-local-review-v3-approved-base\0");
    digest.update(settings.openai_endpoint.as_bytes());
    digest.update([0]);
    digest.update(settings.openai_model.as_bytes());
    digest.update([0]);
    digest.update(system_prompt().as_bytes());
    digest.update(MAX_LOCAL_FILE_DIFF_BYTES.to_be_bytes());
    digest.update(MAX_LOCAL_DIFF_BYTES.to_be_bytes());
    digest.update(MAX_LOCAL_REVIEW_FILES.to_be_bytes());
    digest.update(MAX_LOCAL_REVIEW_CHUNKS.to_be_bytes());
    digest.update(MAX_REVIEW_FINDINGS.to_be_bytes());
    digest.update(REVIEW_MAX_OUTPUT_TOKENS.to_be_bytes());
    digest.update(REVIEW_TEMPERATURE.to_bits().to_be_bytes());
    Ok(format!("{:x}", digest.finalize()))
}

fn system_prompt() -> String {
    format!(
        "{}\n\n{}",
        cururu_engine::DEFAULT_REVIEW_PROMPT.trim(),
        LOCAL_REVIEW_NOTE
    )
}

#[must_use]
pub fn snapshot_fingerprint(snapshot: &RepositorySnapshot) -> String {
    let mut digest = Sha256::new();
    digest.update(b"cururu-desktop-local-review-diff-v1\0");
    match &snapshot.review_policy {
        Ok(policy) => digest.update(policy.fingerprint.as_bytes()),
        Err(error) => digest.update(error.as_bytes()),
    }
    digest.update([0]);
    digest.update(
        snapshot
            .head_oid
            .as_deref()
            .unwrap_or("<unborn>")
            .as_bytes(),
    );
    digest.update([0]);
    for file in &snapshot.changed_files {
        digest.update(file.path.as_bytes());
        digest.update([0]);
        digest.update(file.patch.as_bytes());
        digest.update([0]);
    }
    format!("{:x}", digest.finalize())
}

#[must_use]
pub fn diff_byte_count(snapshot: &RepositorySnapshot) -> usize {
    snapshot
        .changed_files
        .iter()
        .map(|file| file.patch.len())
        .sum()
}

pub fn validate_snapshot(snapshot: &RepositorySnapshot) -> anyhow::Result<usize> {
    bounded_chunks(snapshot).map(|chunks| chunks.len())
}

pub fn outbound_stats(snapshot: &RepositorySnapshot) -> anyhow::Result<(usize, usize)> {
    let policy = snapshot
        .review_policy
        .as_ref()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let files = policy.review_files(&snapshot.changed_files)?;
    Ok((files.len(), files.iter().map(|file| file.patch.len()).sum()))
}

pub async fn review_snapshot(
    settings: &LocalReviewSettings,
    secrets: &LocalReviewSecrets,
    snapshot: &RepositorySnapshot,
    context: Option<&crate::local_context::PreparedContext>,
) -> anyhow::Result<LocalReviewResult> {
    settings.validate()?;
    let chunks = bounded_chunks(snapshot)?;
    let agent = cururu_engine::OpenAiCompatibleAgent::new(
        cururu_engine::OpenAiCompatibleSettings {
            base_url: settings.openai_endpoint.clone(),
            api_key: secrets.as_str().to_string(),
            model: settings.openai_model.clone(),
            temperature: REVIEW_TEMPERATURE,
            max_output_tokens: REVIEW_MAX_OUTPUT_TOKENS,
        },
        system_prompt(),
    )?
    .with_review_context(&context.map_or_else(
        cururu_engine::review_context::ReviewContext::default,
        |context| context.evidence.clone(),
    ))?;
    let chunk_results = cururu_engine::review_chunks(&agent, &chunks).await?;
    let mut result = assemble_review_result(settings, snapshot, chunk_results)?;
    result.context = context.cloned();
    Ok(result)
}

fn bounded_chunks(snapshot: &RepositorySnapshot) -> anyhow::Result<Vec<cururu_core::DiffChunk>> {
    let policy = snapshot.review_policy.as_ref().map_err(|error| {
        anyhow::anyhow!("Repository review configuration blocks sending: {error}")
    })?;
    let files = policy.review_files(&snapshot.changed_files)?;
    anyhow::ensure!(
        !files.is_empty(),
        "there are no changed-file diffs to review"
    );
    anyhow::ensure!(
        files.len() <= MAX_LOCAL_REVIEW_FILES,
        "local review is limited to 100 changed files per request"
    );
    cururu_core::ensure_review_diff_limits(
        &files,
        MAX_LOCAL_FILE_DIFF_BYTES,
        policy.max_diff_bytes,
    )
    .map_err(|error| anyhow::anyhow!(error).context("local review diff exceeds its size limit"))?;
    let chunks = cururu_core::chunk_files(&files, policy.chunk_bytes, policy.max_diff_bytes);
    anyhow::ensure!(
        !chunks.is_empty() && chunks.len() <= MAX_LOCAL_REVIEW_CHUNKS,
        "local review exceeds the bounded chunk limit"
    );
    Ok(chunks)
}

fn assemble_review_result(
    settings: &LocalReviewSettings,
    snapshot: &RepositorySnapshot,
    chunk_results: Vec<cururu_engine::ChunkResult>,
) -> anyhow::Result<LocalReviewResult> {
    let chunk_count = chunk_results.len();
    anyhow::ensure!(
        chunk_count > 0 && chunk_count <= MAX_LOCAL_REVIEW_CHUNKS,
        "local review did not return a complete bounded chunk set"
    );
    let usage = cururu_engine::merge_usage(&chunk_results);
    let policy = snapshot
        .review_policy
        .as_ref()
        .map_err(|error| anyhow::anyhow!("Repository review configuration is invalid: {error}"))?;
    let reviewed_files = policy.review_files(&snapshot.changed_files)?;
    let reviews = chunk_results
        .into_iter()
        .map(|chunk_result| chunk_result.review)
        .collect();
    let mut review = cururu_core::collect_review_candidates(
        settings.openai_model.clone(),
        reviewed_files.len(),
        reviews,
        Vec::new(),
        cururu_core::CandidateOptions {
            include_suggested_changes: true,
            synthesize: false,
        },
    );

    let candidate_count = review.findings.len();
    review.findings.retain(|finding| {
        let has_valid_severity = cururu_core::Severity::from_name(&finding.severity).is_some();
        let has_valid_confidence =
            finding.confidence.is_finite() && (0.0..=1.0).contains(&finding.confidence);
        let has_valid_anchor = finding
            .line
            .is_some_and(|line| cururu_core::is_valid_anchor(&reviewed_files, &finding.path, line));
        has_valid_severity && has_valid_confidence && has_valid_anchor
    });
    let mut policy_suppressed = Vec::new();
    for mut finding in std::mem::take(&mut review.findings) {
        if !policy.suggested_changes {
            finding.suggested_change = None;
        }
        if finding.confidence >= policy.minimum_confidence
            && cururu_core::Severity::from_name(&finding.severity)
                .is_some_and(|severity| policy.allowed_severities.contains(&severity))
            && review.findings.len() < policy.max_findings.min(MAX_REVIEW_FINDINGS)
        {
            review.findings.push(finding);
        } else {
            let reason = if finding.confidence < policy.minimum_confidence {
                "below repository minimum confidence"
            } else if !cururu_core::Severity::from_name(&finding.severity)
                .is_some_and(|severity| policy.allowed_severities.contains(&severity))
            {
                "severity excluded by repository policy"
            } else {
                "local/repository finding-count limit"
            };
            policy_suppressed.push(PolicySuppressedFinding { finding, reason });
        }
    }
    let discarded_findings =
        candidate_count.saturating_sub(review.findings.len() + policy_suppressed.len());
    review.summary = format!(
        "Found {} anchored review finding(s); discarded {} invalid candidate(s).",
        review.findings.len(),
        discarded_findings
    );

    Ok(LocalReviewResult {
        base: snapshot
            .review_policy
            .as_ref()
            .ok()
            .and_then(|policy| policy.base_approval.clone()),
        context: None,
        review,
        usage,
        diff_fingerprint: snapshot_fingerprint(snapshot),
        head_oid: snapshot.head_oid.clone(),
        diff_bytes: reviewed_files.iter().map(|file| file.patch.len()).sum(),
        chunk_count,
        discarded_findings,
        policy_suppressed,
    })
}

fn validate_api_key(value: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!value.is_empty(), "local review API key must not be empty");
    anyhow::ensure!(
        value.len() <= 16_384,
        "local review API key exceeds the size limit"
    );
    anyhow::ensure!(
        value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b'"' && byte != b'\\'),
        "local review API key contains invalid header characters"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        LocalReviewSecrets, MAX_LOCAL_DIFF_BYTES, MAX_LOCAL_FILE_DIFF_BYTES,
        assemble_review_result, bounded_chunks, config_fingerprint, diff_byte_count,
        repository_identity, snapshot_fingerprint, validate_api_key,
    };
    use crate::{repository::RepositorySnapshot, session_store::LocalReviewSettings};
    use cururu_core::{ChangedFile, ReviewFinding, ReviewResult};

    fn settings(endpoint: &str, model: &str) -> LocalReviewSettings {
        LocalReviewSettings {
            openai_endpoint: endpoint.to_string(),
            openai_model: model.to_string(),
        }
    }

    fn changed_file(path: &str, diff: &str, right_lines: &[u32]) -> ChangedFile {
        ChangedFile {
            path: path.to_string(),
            patch: diff.to_string(),
            right_lines: right_lines.to_vec(),
        }
    }

    fn finding(path: &str, line: Option<u32>) -> ReviewFinding {
        ReviewFinding {
            severity: "high".to_string(),
            path: path.to_string(),
            line,
            title: "Possible bug".to_string(),
            message: "Concrete issue".to_string(),
            suggestion: "Use a safe fallback".to_string(),
            confidence: 0.9,
            suggested_change: None,
            source: None,
            rule: None,
        }
    }

    fn snapshot(files: Vec<ChangedFile>) -> RepositorySnapshot {
        RepositorySnapshot {
            root: std::path::PathBuf::from("/repo"),
            branch: Some("main".to_string()),
            head_oid: Some("abc123".to_string()),
            changed_paths: files.iter().map(|file| file.path.clone()).collect(),
            changed_files: files,
            review_policy: Ok(crate::repository_policy::RepositoryPolicy::default()),
        }
    }

    #[test]
    fn local_review_fingerprint_tracks_provider_and_canonical_prompt() {
        let base = config_fingerprint(&settings("https://api.example.test/v1", "model-a")).unwrap();
        let other_endpoint =
            config_fingerprint(&settings("https://other.example.test/v1", "model-a")).unwrap();
        let other_model =
            config_fingerprint(&settings("https://api.example.test/v1", "model-b")).unwrap();

        assert_ne!(base, other_endpoint);
        assert_ne!(base, other_model);
    }

    #[test]
    #[cfg(unix)]
    fn local_repository_identity_uses_the_canonical_checkout_path() {
        let directory = tempfile::tempdir().unwrap();
        let alias = directory.path().join("repo-link");
        std::os::unix::fs::symlink(directory.path(), &alias).unwrap();

        assert_eq!(
            repository_identity(&alias).unwrap(),
            repository_identity(directory.path()).unwrap()
        );
    }

    #[test]
    fn local_diff_fingerprint_and_size_change_with_reviewed_evidence() {
        let first = snapshot(vec![changed_file("src/lib.rs", "diff one", &[3])]);
        let second = snapshot(vec![changed_file("src/lib.rs", "diff two", &[3])]);

        assert_ne!(snapshot_fingerprint(&first), snapshot_fingerprint(&second));
        assert_eq!(diff_byte_count(&first), "diff one".len());
    }

    #[test]
    fn local_diff_chunks_preserve_limits_and_reject_oversized_files() {
        let valid = snapshot(vec![changed_file("src/lib.rs", &"x".repeat(1_000), &[1])]);
        let chunks = bounded_chunks(&valid).unwrap();
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].text.len() <= MAX_LOCAL_FILE_DIFF_BYTES + 1);

        let oversized = snapshot(vec![changed_file(
            "src/lib.rs",
            &"x".repeat(MAX_LOCAL_FILE_DIFF_BYTES + 1),
            &[1],
        )]);
        assert!(bounded_chunks(&oversized).is_err());

        let too_many_bytes = snapshot(vec![changed_file(
            "src/lib.rs",
            &"x".repeat(MAX_LOCAL_DIFF_BYTES + 1),
            &[1],
        )]);
        assert!(bounded_chunks(&too_many_bytes).is_err());
    }

    #[test]
    fn local_review_discards_untrusted_findings_without_valid_changed_line_anchors() {
        let snapshot = snapshot(vec![changed_file("src/lib.rs", "+changed", &[3])]);
        let mut valid = finding("src/lib.rs", Some(3));
        valid.suggested_change = Some(cururu_core::SuggestedChange {
            replacement: "safe_call()".to_string(),
        });
        let mut invalid_confidence = finding("src/lib.rs", Some(3));
        invalid_confidence.confidence = f32::NAN;
        let invalid_severity = ReviewFinding {
            severity: "<LEVEL>".to_string(),
            ..finding("src/lib.rs", Some(3))
        };
        let chunk = cururu_engine::ChunkResult {
            review: ReviewResult {
                model: "untrusted-model-name".to_string(),
                files_reviewed: 99,
                summary: "untrusted summary".to_string(),
                findings: vec![
                    valid,
                    finding("outside.rs", Some(3)),
                    finding("src/lib.rs", Some(4)),
                    finding("src/lib.rs", None),
                    invalid_confidence,
                    invalid_severity,
                ],
            },
            usage: None,
        };

        let result = assemble_review_result(
            &settings("https://api.example.test/v1", "configured-model"),
            &snapshot,
            vec![chunk],
        )
        .unwrap();

        assert_eq!(result.review.model, "configured-model");
        assert_eq!(result.review.files_reviewed, 1);
        assert_eq!(result.review.findings.len(), 1);
        assert_eq!(result.discarded_findings, 5);
        assert!(result.review.findings[0].suggested_change.is_some());
    }

    #[test]
    fn repository_restrictions_limit_outbound_evidence_and_keep_policy_audit() {
        let mut snapshot = snapshot(vec![
            changed_file("src/lib.rs", "+allowed", &[1, 2]),
            changed_file("ignored.rs", "+hidden", &[1]),
            changed_file(".cururu.toml", "+private configuration", &[1]),
        ]);
        snapshot.review_policy = Ok(crate::repository_policy::RepositoryPolicy::parse(
            "version = 1\n[review]\nignore = ['ignored.rs']\n[policy]\nminimum_confidence = 0.95\nsuggested_changes = false").unwrap());
        let chunks = bounded_chunks(&snapshot).unwrap();
        let outbound = chunks
            .iter()
            .map(|chunk| chunk.text.as_str())
            .collect::<String>();
        assert!(outbound.contains("+allowed"));
        assert!(!outbound.contains("+hidden"));
        assert!(!outbound.contains("private configuration"));
        let mut low_confidence = finding("src/lib.rs", Some(1));
        low_confidence.confidence = 0.9;
        let mut retained = finding("src/lib.rs", Some(2));
        retained.confidence = 0.99;
        retained.suggested_change = Some(cururu_core::SuggestedChange {
            replacement: "safe()".to_string(),
        });
        let chunk = cururu_engine::ChunkResult {
            review: ReviewResult {
                model: "ignored".to_string(),
                files_reviewed: 9,
                summary: String::new(),
                findings: vec![low_confidence, retained, finding("ignored.rs", Some(1))],
            },
            usage: None,
        };
        let result = assemble_review_result(
            &settings("https://api.example.test/v1", "model"),
            &snapshot,
            vec![chunk],
        )
        .unwrap();
        assert_eq!(result.review.files_reviewed, 1);
        assert_eq!(result.review.findings.len(), 1);
        assert!(result.review.findings[0].suggested_change.is_none());
        assert_eq!(result.policy_suppressed.len(), 1);
        assert_eq!(
            result.policy_suppressed[0].reason,
            "below repository minimum confidence"
        );
        assert_eq!(result.discarded_findings, 1);
        let mut changed = snapshot.clone();
        changed.review_policy = Ok(crate::repository_policy::RepositoryPolicy::default());
        assert_ne!(
            snapshot_fingerprint(&snapshot),
            snapshot_fingerprint(&changed)
        );
    }

    #[test]
    fn local_review_rejects_partial_or_empty_chunk_results() {
        let snapshot = snapshot(vec![changed_file("src/lib.rs", "+changed", &[3])]);

        assert!(
            assemble_review_result(
                &settings("https://api.example.test/v1", "model"),
                &snapshot,
                Vec::new()
            )
            .is_err()
        );
    }

    #[test]
    fn local_review_secrets_redact_debug_and_validate_header_values() {
        let secrets = LocalReviewSecrets {
            api_key: "secret-value".to_string(),
        };

        assert!(format!("{secrets:?}").contains("[REDACTED]"));
        assert!(!format!("{secrets:?}").contains("secret-value"));
        assert!(validate_api_key("valid-key").is_ok());
        assert!(validate_api_key("invalid\nkey").is_err());
    }
}
