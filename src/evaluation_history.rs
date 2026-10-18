//! Local archive contracts. Identity hashes evidence, but never stores its input.

use anyhow::{Context as _, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{evaluator, github_pr};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvaluationAnchor {
    pub reference: github_pr::PullRequestRef,
    pub base_sha: String,
    pub head_sha: String,
    pub source_sha: String,
    pub comment_id: u64,
    pub evidence_fingerprint: String,
    pub config_fingerprint: String,
    pub rubric_fingerprint: String,
}

impl EvaluationAnchor {
    pub fn new(
        snapshot: &github_pr::PullRequestSnapshot,
        comment: &github_pr::PullRequestComment,
        input: &evaluator::ReviewAnalysisInput,
        settings: &crate::session_store::EvaluatorSettings,
    ) -> anyhow::Result<Self> {
        let mut reference = snapshot.reference.clone();
        reference.owner.make_ascii_lowercase();
        reference.repository.make_ascii_lowercase();
        let anchor = Self {
            reference,
            base_sha: snapshot.base_sha.clone(),
            head_sha: snapshot.head_sha.clone(),
            source_sha: comment.original_commit_id.clone(),
            comment_id: comment.id,
            evidence_fingerprint: evaluator::input_fingerprint(input)?,
            config_fingerprint: evaluator::DualReviewEvaluator::config_fingerprint(settings)?,
            rubric_fingerprint: evaluator::rubric_fingerprint(),
        };
        anchor.validate()?;
        Ok(anchor)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        github_pr::PullRequestRef::parse(&format!(
            "{}/{}#{}",
            self.reference.owner, self.reference.repository, self.reference.number
        ))?;
        i64::try_from(self.reference.number)
            .context("pull request number exceeds local storage range")?;
        ensure!(
            self.comment_id > 0
                && self.reference.owner.len() <= 256
                && self.reference.repository.len() <= 256,
            "archive identity exceeds its limits"
        );
        for sha in [&self.base_sha, &self.head_sha, &self.source_sha] {
            ensure!(
                sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "archive requires full commit SHAs"
            );
        }
        for hash in [
            &self.evidence_fingerprint,
            &self.config_fingerprint,
            &self.rubric_fingerprint,
        ] {
            ensure!(
                hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "archive fingerprint is invalid"
            );
        }
        Ok(())
    }

    pub fn repository_key(&self) -> String {
        format!("{}/{}", self.reference.owner, self.reference.repository).to_ascii_lowercase()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum HumanAssessment {
    #[default]
    Unreviewed,
    Agree,
    Disagree,
    NeedsMoreEvidence,
}

impl HumanAssessment {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Unreviewed => "Not assessed",
            Self::Agree => "Agree",
            Self::Disagree => "Disagree",
            Self::NeedsMoreEvidence => "Needs more evidence",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvaluationRecord {
    pub anchor: EvaluationAnchor,
    pub analysis: evaluator::CombinedReviewAnalysis,
    pub assessment: HumanAssessment,
}

impl EvaluationRecord {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.anchor.validate()?;
        for model in [&self.analysis.openai_model, &self.analysis.jev.model] {
            ensure!(
                !model.is_empty() && model.len() <= 2_048 && !model.chars().any(char::is_control),
                "archive model identifier is invalid"
            );
        }
        ensure!(
            self.analysis.jev.defect_probability.is_finite()
                && (0.0..=1.0).contains(&self.analysis.jev.defect_probability),
            "archive probability is invalid"
        );
        if let Some(value) = self.analysis.jev.severity_confidence {
            ensure!(
                value.is_finite() && (0.0..=1.0).contains(&value),
                "archive severity confidence is invalid"
            );
        }
        ensure!(
            self.analysis.jev.severity == "ignore"
                || cururu_core::Severity::from_name(&self.analysis.jev.severity).is_some(),
            "archive severity is invalid"
        );
        ensure!(
            serde_json::to_vec(self)?.len() <= 64_000,
            "archive record exceeds 64,000 bytes; no text was truncated"
        );
        Ok(())
    }

    pub fn id(&self) -> anyhow::Result<String> {
        self.validate()?;
        Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(self)?)))
    }
}

#[derive(Debug, Clone)]
pub struct StoredEvaluation {
    pub id: String,
    pub created_at: i64,
    pub record: EvaluationRecord,
}

#[cfg(test)]
pub fn sample_record() -> EvaluationRecord {
    EvaluationRecord {
        anchor: EvaluationAnchor {
            reference: github_pr::PullRequestRef::parse("octo/repo#1").unwrap(),
            base_sha: "b".repeat(40),
            head_sha: "a".repeat(40),
            source_sha: "c".repeat(40),
            comment_id: 1,
            evidence_fingerprint: "d".repeat(64),
            config_fingerprint: "e".repeat(64),
            rubric_fingerprint: "f".repeat(64),
        },
        analysis: evaluator::CombinedReviewAnalysis {
            openai_model: "judge-model".to_string(),
            jev: evaluator::JevOpinion {
                model: "jev-model".to_string(),
                defect_probability: 0.7,
                severity: "high".to_string(),
                severity_confidence: Some(0.8),
            },
            explanation: evaluator::Explanation {
                claims: vec!["A claim".to_string()],
                classification: evaluator::ClaimClassification::InsufficientContext,
                explanation: "Needs context".to_string(),
                evidence: vec![],
                conditions: vec![],
                verification_steps: vec!["Inspect the boundary".to_string()],
                uncertainties: vec![],
                draft_response: "Please clarify".to_string(),
            },
            openai_input_tokens: Some(100),
            openai_output_tokens: Some(50),
        },
        assessment: HumanAssessment::Unreviewed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_identity_is_exact_for_revision_evidence_rubric_and_human_decision() {
        let original = sample_record();
        let id = original.id().unwrap();
        let mut changed = original.clone();
        for field in ["base", "head", "source", "evidence", "config", "rubric"] {
            let value = match field {
                "base" => &mut changed.anchor.base_sha,
                "head" => &mut changed.anchor.head_sha,
                "source" => &mut changed.anchor.source_sha,
                "evidence" => &mut changed.anchor.evidence_fingerprint,
                "config" => &mut changed.anchor.config_fingerprint,
                _ => &mut changed.anchor.rubric_fingerprint,
            };
            *value = "1".repeat(value.len());
            assert_ne!(changed.anchor, original.anchor);
            assert_ne!(changed.id().unwrap(), id);
            changed = original.clone();
        }
        changed.anchor.reference.number += 1;
        assert_ne!(changed.id().unwrap(), id);
        changed = original.clone();
        changed.anchor.comment_id += 1;
        assert_ne!(changed.id().unwrap(), id);
        changed = original.clone();
        changed.assessment = HumanAssessment::Disagree;
        assert_ne!(changed.id().unwrap(), id);
        assert_eq!(changed.anchor, original.anchor);
    }

    #[test]
    fn malformed_archive_anchor_is_rejected_before_persistence() {
        let mut record = sample_record();
        record.anchor.head_sha = "short".to_string();
        assert!(record.id().is_err());
        record = sample_record();
        record.anchor.reference.number = u64::MAX;
        assert!(record.id().is_err());
        record = sample_record();
        record.anchor.evidence_fingerprint = "z".repeat(64);
        assert!(record.id().is_err());
    }
}
