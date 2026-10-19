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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_on: Option<cururu_core::FailOn>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub finding_decisions: Vec<FindingDecision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<ArtifactEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactEvidence {
    pub fingerprint: String,
    pub tools: Vec<cururu_core::AnalysisTool>,
    pub status: String,
}

pub fn static_config_fingerprint() -> String {
    format!(
        "{:x}",
        Sha256::digest(b"cururu-desktop-local-static-archive-v1")
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindingDecision {
    pub index: usize,
    pub fingerprint: String,
    pub assessment: HumanAssessment,
}

pub fn finding_fingerprint(finding: &cururu_core::ReviewFinding) -> anyhow::Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(finding)?)
    ))
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
            fail_on: Some(result.fail_on),
            parent: None,
            finding_decisions: Vec::new(),
            artifacts: None,
        };
        record.validate()?;
        Ok(record)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        self.validate_decisions()?;
        self.validate_artifacts()?;
        ensure!(
            matches!(self.version, 1..=3),
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
                && (self.chunk_count > 0 || self.artifacts.is_some())
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

    pub fn finding(&self, index: usize) -> Option<&cururu_core::ReviewFinding> {
        if index < self.review.findings.len() {
            self.review.findings.get(index)
        } else {
            self.suppressed
                .get(index.checked_sub(self.review.findings.len())?)
                .map(|entry| &entry.finding)
        }
    }

    pub fn from_static(
        snapshot: &crate::repository::RepositorySnapshot,
        report: &crate::local_analysis::PreparedAnalysis,
        assessment: HumanAssessment,
    ) -> anyhow::Result<Self> {
        ensure!(
            crate::local_context::scope(snapshot)? == report.preview.scope,
            "static evidence belongs to a different checkout/snapshot"
        );
        let policy = snapshot
            .review_policy
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let files = policy.review_files(&snapshot.changed_files)?;
        let record = Self {
            version: 3,
            repository_key: repository_key(&snapshot.root)?,
            head_oid: snapshot.head_oid.clone(),
            diff_fingerprint: crate::local_review::snapshot_fingerprint(snapshot),
            config_fingerprint: static_config_fingerprint(),
            context_fingerprint: None,
            base_fingerprint: crate::shared_base::fingerprint(snapshot),
            review: cururu_core::ReviewResult {
                model: "static-analysis (local artifacts)".to_string(),
                files_reviewed: files.len(),
                summary: format!(
                    "Local artifact evidence: {}. Tool status is an untrusted claim, not proof of execution; not merged into provider findings.",
                    report.report.status
                ),
                findings: report.report.findings.clone(),
            },
            suppressed: Vec::new(),
            discarded_findings: report.discarded,
            diff_bytes: files.iter().map(|file| file.patch.len()).sum(),
            chunk_count: 0,
            usage: None,
            assessment,
            fail_on: None,
            parent: None,
            finding_decisions: Vec::new(),
            artifacts: Some(ArtifactEvidence {
                fingerprint: report.fingerprint.clone(),
                tools: report.report.tools.clone(),
                status: report.report.status.clone(),
            }),
        };
        record.validate()?;
        Ok(record)
    }

    fn validate_artifacts(&self) -> anyhow::Result<()> {
        if let Some(artifacts) = &self.artifacts {
            validate_hash(&artifacts.fingerprint)?;
            ensure!(
                self.version == 3
                    && self.config_fingerprint == static_config_fingerprint()
                    && self.review.model == "static-analysis (local artifacts)"
                    && self.chunk_count == 0
                    && self.head_oid.is_some()
                    && self.context_fingerprint.is_none()
                    && self.usage.is_none()
                    && self.fail_on.is_none()
                    && self.suppressed.is_empty()
                    && artifacts.tools.len() <= 8,
                "static archive provenance is inconsistent"
            );
            ensure!(
                cururu_core::AnalysisReport::from_parts(
                    artifacts.tools.clone(),
                    self.review.findings.clone()
                )
                .status
                    == artifacts.status,
                "static archive status does not match its normalized evidence"
            );
        } else {
            ensure!(
                self.version != 3,
                "static archive requires artifact provenance"
            );
        }
        Ok(())
    }

    fn validate_decisions(&self) -> anyhow::Result<()> {
        if self.version == 1 || (self.version == 3 && self.finding_decisions.is_empty()) {
            ensure!(
                self.parent.is_none() && self.finding_decisions.is_empty(),
                "legacy local history cannot contain finding decisions"
            );
        } else {
            validate_hash(
                self.parent
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("annotated local history requires a parent"))?,
            )?;
            ensure!(
                !self.finding_decisions.is_empty() && self.finding_decisions.len() <= 100,
                "local finding decision count is invalid"
            );
        }
        let mut prior = None;
        for decision in &self.finding_decisions {
            let finding = self
                .finding(decision.index)
                .ok_or_else(|| anyhow::anyhow!("local finding decision target is absent"))?;
            ensure!(
                prior.is_none_or(|index| index < decision.index),
                "local finding decisions must be unique and sorted"
            );
            ensure!(
                finding_fingerprint(finding)? == decision.fingerprint,
                "local finding decision fingerprint does not match its target"
            );
            prior = Some(decision.index);
        }
        Ok(())
    }

    pub fn with_decision(&self, index: usize, assessment: HumanAssessment) -> anyhow::Result<Self> {
        let parent = self.id()?;
        let finding = self
            .finding(index)
            .ok_or_else(|| anyhow::anyhow!("local finding decision target is absent"))?;
        let decision = FindingDecision {
            index,
            fingerprint: finding_fingerprint(finding)?,
            assessment,
        };
        let mut record = self.clone();
        record.version = if self.artifacts.is_some() { 3 } else { 2 };
        record.parent = Some(parent);
        if let Some(existing) = record
            .finding_decisions
            .iter_mut()
            .find(|existing| existing.index == index)
        {
            *existing = decision;
        } else {
            record.finding_decisions.push(decision);
            record
                .finding_decisions
                .sort_by_key(|decision| decision.index);
        }
        record.validate()?;
        Ok(record)
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
        fail_on: None,
        parent: None,
        finding_decisions: Vec::new(),
        artifacts: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finding_decisions_bind_visible_and_suppressed_targets_without_mutating_the_parent() {
        let parent = sample_record();
        let legacy = serde_json::to_string(&parent).unwrap();
        assert!(!legacy.contains("finding_decisions"));
        assert!(!legacy.contains("parent"));
        let id = parent.id().unwrap();
        assert_eq!(format!("{:x}", Sha256::digest(legacy.as_bytes())), id);
        let visible = parent.with_decision(0, HumanAssessment::Agree).unwrap();
        let suppressed = visible
            .with_decision(1, HumanAssessment::NeedsMoreEvidence)
            .unwrap();
        assert_eq!(visible.parent.as_ref(), Some(&id));
        assert_eq!(suppressed.parent.as_ref(), Some(&visible.id().unwrap()));
        assert_eq!(suppressed.finding_decisions.len(), 2);
        assert_eq!(parent.id().unwrap(), id);
        assert!(parent.finding_decisions.is_empty());
        assert!(parent.with_decision(2, HumanAssessment::Disagree).is_err());
        let changed = suppressed
            .with_decision(0, HumanAssessment::Disagree)
            .unwrap();
        assert_eq!(changed.finding_decisions.len(), 2);
        let mut corrupt = changed;
        corrupt.finding_decisions[0].fingerprint = "f".repeat(64);
        assert!(corrupt.validate().is_err());
        let mut corrupt = suppressed.clone();
        corrupt.finding_decisions.reverse();
        assert!(corrupt.validate().is_err());
        let mut corrupt = suppressed;
        corrupt.version = 1;
        assert!(corrupt.validate().is_err());
    }

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
            fail_on: cururu_core::FailOn::Off,
            config_fingerprint: saved.config_fingerprint.clone(),
            base: Some(base),
            context: Some(crate::local_context::PreparedContext {
                preferences: cururu_engine::review_preferences::ReviewPreferences::default(),
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
