//! Durable result DTOs intentionally exclude credentials and approved documents.

use crate::{evaluation_history::HumanAssessment, local_review::LocalReviewResult};
use anyhow::ensure;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedSuppression {
    pub finding: cururu_core::ReviewFinding,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    pub cached_tokens: u32,
    pub reasoning_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalReviewRecord {
    pub version: u32,
    pub repository_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_oid: Option<String>,
    pub diff_fingerprint: String,
    pub config_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_fingerprint: Option<String>,
    pub review: cururu_core::ReviewResult,
    pub suppressed: Vec<SavedSuppression>,
    pub discarded_findings: usize,
    pub diff_bytes: usize,
    pub chunk_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<SavedUsage>,
    pub assessment: HumanAssessment,
}

pub fn repository_key(root: &std::path::Path) -> anyhow::Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(crate::local_review::repository_identity(root)?.as_bytes())
    ))
}

pub fn validate_hash(value: &str) -> anyhow::Result<()> {
    ensure!(
        value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "local history fingerprint is invalid"
    );
    Ok(())
}

impl LocalReviewRecord {
    pub fn from_result(
        root: &std::path::Path,
        result: &LocalReviewResult,
        assessment: HumanAssessment,
    ) -> anyhow::Result<Self> {
        let record = Self {
            version: 1,
            repository_key: repository_key(root)?,
            head_oid: result.head_oid.clone(),
            diff_fingerprint: result.diff_fingerprint.clone(),
            config_fingerprint: result.config_fingerprint.clone(),
            context_fingerprint: result
                .context
                .as_ref()
                .map(|context| context.fingerprint.clone()),
            base_fingerprint: result.base.as_ref().map(|base| base.fingerprint.clone()),
            review: result.review.clone(),
            suppressed: result
                .policy_suppressed
                .iter()
                .map(|suppressed| SavedSuppression {
                    finding: suppressed.finding.clone(),
                    reason: suppressed.reason.to_string(),
                })
                .collect(),
            discarded_findings: result.discarded_findings,
            diff_bytes: result.diff_bytes,
            chunk_count: result.chunk_count,
            usage: result.usage.as_ref().map(|usage| SavedUsage {
                prompt_tokens: usage.prompt_tokens,
                completion_tokens: usage.completion_tokens,
                total_tokens: usage.total_tokens,
                cached_tokens: usage.cached_tokens,
                reasoning_tokens: usage.reasoning_tokens,
                cost: usage.cost,
            }),
            assessment,
        };
        record.validate()?;
        Ok(record)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.version == 1,
            "local history record version is unsupported"
        );
        for hash in [
            &self.repository_key,
            &self.diff_fingerprint,
            &self.config_fingerprint,
        ] {
            validate_hash(hash)?;
        }
        for hash in [&self.context_fingerprint, &self.base_fingerprint]
            .into_iter()
            .flatten()
        {
            validate_hash(hash)?;
        }
        if let Some(head) = &self.head_oid {
            ensure!(
                head.len() == 40 && head.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "local history head must be a full commit SHA"
            );
        }
        ensure!(
            !self.review.model.is_empty()
                && self.review.model.len() <= 2_048
                && !self.review.model.chars().any(char::is_control),
            "local history model identifier is invalid"
        );
        ensure!(
            self.review.files_reviewed <= 100
                && self.review.findings.len() + self.suppressed.len() <= 100
                && self.chunk_count > 0
                && self.chunk_count <= 4
                && self.diff_bytes <= crate::local_review::MAX_LOCAL_DIFF_BYTES,
            "local history review exceeds bounded limits"
        );
        for finding in self
            .review
            .findings
            .iter()
            .chain(self.suppressed.iter().map(|entry| &entry.finding))
        {
            ensure!(
                cururu_core::Severity::from_name(&finding.severity).is_some()
                    && finding.confidence.is_finite()
                    && (0.0..=1.0).contains(&finding.confidence)
                    && finding.line.is_some_and(|line| line > 0),
                "local history contains an invalid finding"
            );
            cururu_engine::review_context::ReviewContext {
                focus: Vec::new(),
                files: vec![cururu_core::ContextFile {
                    label: "Finding".to_string(),
                    path: finding.path.clone(),
                    content: String::new(),
                }],
            }
            .validate()?;
        }
        for suppression in &self.suppressed {
            ensure!(
                !suppression.reason.is_empty() && suppression.reason.len() <= 1_024,
                "local history suppression reason is invalid"
            );
        }
        if let Some(cost) = self.usage.as_ref().and_then(|usage| usage.cost) {
            ensure!(
                cost.is_finite() && cost >= 0.0,
                "local history cost is invalid"
            );
        }
        ensure!(
            serde_json::to_vec(self)?.len() <= 64_000,
            "local history record exceeds 64,000 bytes; nothing was truncated"
        );
        Ok(())
    }

    pub fn id(&self) -> anyhow::Result<String> {
        self.validate()?;
        Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(self)?)))
    }
}

#[derive(Debug, Clone)]
pub struct StoredLocalReview {
    pub id: String,
    pub created_at: i64,
    pub record: LocalReviewRecord,
}

#[cfg(test)]
pub fn sample_record() -> LocalReviewRecord {
    let finding = cururu_core::ReviewFinding {
        path: "src/lib.rs".to_string(),
        line: Some(1),
        severity: "medium".to_string(),
        title: "Check the boundary".to_string(),
        message: "A bounded finding".to_string(),
        suggestion: "Add a test".to_string(),
        confidence: 0.9,
        suggested_change: None,
        source: None,
        rule: None,
    };
    LocalReviewRecord {
        version: 1,
        repository_key: "a".repeat(64),
        head_oid: None,
        diff_fingerprint: "b".repeat(64),
        config_fingerprint: "c".repeat(64),
        context_fingerprint: None,
        base_fingerprint: None,
        review: cururu_core::ReviewResult {
            model: "test-model".to_string(),
            files_reviewed: 1,
            summary: "One finding".to_string(),
            findings: vec![finding.clone()],
        },
        suppressed: vec![SavedSuppression {
            finding,
            reason: "below repository minimum confidence".to_string(),
        }],
        discarded_findings: 1,
        diff_bytes: 100,
        chunk_count: 1,
        usage: None,
        assessment: HumanAssessment::Unreviewed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_result_excludes_context_and_shared_documents_and_preserves_suppression_audit() {
        let root = tempfile::tempdir().unwrap();
        gix::init(root.path()).unwrap();
        std::fs::write(root.path().join(".cururu.toml"), format!("version = 1\n[config]\nbase = 'org/policy'\nbase_ref = '{}'\nbase_path = '.cururu.toml'", "a".repeat(40))).unwrap();
        let snapshot = crate::repository::RepositorySnapshot::open(root.path()).unwrap();
        let base = crate::shared_base::ApprovedBase::prepare(
            &snapshot,
            "version = 1\nunknown = 'synthetic-base-secret-marker'",
        )
        .unwrap();
        let saved = sample_record();
        let result = LocalReviewResult {
            config_fingerprint: saved.config_fingerprint.clone(),
            base: Some(base),
            context: Some(crate::local_context::PreparedContext {
                manifest: crate::context_discovery::ContextManifest {
                    snapshot_fingerprint: "d".repeat(64),
                    paths: Vec::new(),
                },
                snapshot_fingerprint: "d".repeat(64),
                fingerprint: "e".repeat(64),
                evidence: cururu_engine::review_context::ReviewContext {
                    focus: Vec::new(),
                    files: vec![cururu_core::ContextFile {
                        label: "Conventions".to_string(),
                        path: "CONTEXT.md".to_string(),
                        content: "synthetic-context-secret-marker".to_string(),
                    }],
                },
            }),
            review: saved.review.clone(),
            usage: Some(cururu_engine::ReviewUsage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                cached_tokens: 0,
                reasoning_tokens: 0,
                cost: Some(0.01),
            }),
            diff_fingerprint: saved.diff_fingerprint,
            head_oid: None,
            diff_bytes: 100,
            chunk_count: 1,
            discarded_findings: 1,
            policy_suppressed: vec![crate::local_review::PolicySuppressedFinding {
                finding: saved.suppressed[0].finding.clone(),
                reason: "below repository minimum confidence",
            }],
        };
        let record =
            LocalReviewRecord::from_result(root.path(), &result, HumanAssessment::Disagree)
                .unwrap();
        let encoded = serde_json::to_string(&record).unwrap();
        assert!(!encoded.contains("synthetic-base-secret-marker"));
        assert!(!encoded.contains("synthetic-context-secret-marker"));
        assert!(!encoded.contains(root.path().to_str().unwrap()));
        assert!(record.base_fingerprint.is_some());
        assert!(record.context_fingerprint.is_some());
        assert_eq!(
            record.suppressed[0].reason,
            "below repository minimum confidence"
        );
        assert_eq!(record.usage.as_ref().unwrap().total_tokens, 15);
        let decoded: LocalReviewRecord = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.id().unwrap(), record.id().unwrap());
        let mut changed = decoded;
        changed.assessment = HumanAssessment::Agree;
        assert_ne!(changed.id().unwrap(), record.id().unwrap());
    }

    #[test]
    fn invalid_local_review_archives_fail_without_truncating_or_normalizing_data() {
        let mut record = sample_record();
        record.review.findings[0].confidence = f32::NAN;
        assert!(record.validate().is_err());
        record = sample_record();
        record.suppressed[0].finding.path = "../secret".to_string();
        assert!(record.validate().is_err());
        record = sample_record();
        record.review.summary = "x".repeat(64_001);
        assert!(record.validate().is_err());
        record = sample_record();
        record.version = 2;
        assert!(record.validate().is_err());
    }
}
