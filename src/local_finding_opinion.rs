//! Host-owned selection and freshness for one observe-only local finding.

use crate::{
    evaluation_history::HumanAssessment,
    evaluator::{self, ReviewAnalysisInput},
    local_review_history::{FindingOpinion, LocalReviewRecord},
    repository::RepositorySnapshot,
    session_store::{EvaluatorSettings, SessionStore},
};
use anyhow::{Context as _, ensure};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Provider(usize),
    Static(usize),
}

impl Target {
    pub const fn index(self) -> usize {
        match self {
            Self::Provider(index) | Self::Static(index) => index,
        }
    }
}

#[derive(Clone)]
pub enum Evidence {
    Provider(Box<crate::local_review::LocalReviewResult>),
    Static(Box<crate::local_analysis::PreparedAnalysis>),
}

impl Evidence {
    pub fn snapshot_record(
        &self,
        snapshot: &RepositorySnapshot,
    ) -> anyhow::Result<LocalReviewRecord> {
        match self {
            Self::Provider(result) => {
                LocalReviewRecord::from_result(&snapshot.root, result, HumanAssessment::Unreviewed)
            }
            Self::Static(report) => {
                LocalReviewRecord::from_static(snapshot, report, HumanAssessment::Unreviewed)
            }
        }
    }

    pub fn record(&self, snapshot: &RepositorySnapshot) -> anyhow::Result<LocalReviewRecord> {
        match self {
            Self::Provider(result) => {
                ensure!(
                    crate::local_review::snapshot_fingerprint(snapshot) == result.diff_fingerprint
                        && snapshot.head_oid == result.head_oid
                        && crate::shared_base::fingerprint(snapshot).as_ref()
                            == result.base.as_ref().map(|base| &base.fingerprint),
                    "provider finding snapshot is stale"
                );
                crate::local_context::validate(snapshot, result.context.as_ref())?;
                if let Some(analysis) = &result.analysis {
                    crate::local_analysis::validate(snapshot, analysis)?;
                }
            }
            Self::Static(report) => {
                crate::local_analysis::validate(snapshot, report)?;
            }
        }
        self.snapshot_record(snapshot)
    }
}

#[derive(Clone)]
pub struct Pending {
    pub snapshot: RepositorySnapshot,
    pub target: Target,
    pub evidence: Evidence,
    pub source: LocalReviewRecord,
    pub settings: EvaluatorSettings,
    pub input: ReviewAnalysisInput,
    pub input_fingerprint: String,
    pub config_fingerprint: String,
    pub rubric_fingerprint: String,
    pub scope: String,
    pub fingerprint: String,
}

fn build_input(
    snapshot: &RepositorySnapshot,
    source: &LocalReviewRecord,
    index: usize,
) -> anyhow::Result<ReviewAnalysisInput> {
    let finding = source
        .finding(index)
        .context("selected finding is absent")?;
    let policy = snapshot
        .review_policy
        .as_ref()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let files = policy.review_files(&snapshot.changed_files)?;
    let file = files
        .iter()
        .find(|file| file.path == finding.path)
        .context("finding path is outside the current eligible diff")?;
    ensure!(
        finding
            .line
            .is_some_and(|line| cururu_core::is_valid_anchor(&files, &finding.path, line)),
        "finding is not anchored to a current changed line"
    );
    let input = ReviewAnalysisInput {
        origin: evaluator::AnalysisOrigin::LocalCheckout,
        repository_identity: format!("local/{}", source.repository_key),
        path: finding.path.clone(),
        line: finding.line,
        comment_body: serde_json::to_string(finding)?,
        thread_context: Vec::new(),
        source_at_comment: String::new(),
        current_diff: file.patch.clone(),
    };
    evaluator::input_fingerprint(&input)?;
    Ok(input)
}

pub fn prepare(
    snapshot: &RepositorySnapshot,
    target: Target,
    evidence: Evidence,
    settings: EvaluatorSettings,
) -> anyhow::Result<Pending> {
    ensure!(
        matches!(
            (&target, &evidence),
            (Target::Provider(_), Evidence::Provider(_)) | (Target::Static(_), Evidence::Static(_))
        ),
        "finding provenance does not match its selection"
    );
    let snapshot = snapshot.reopen_with_approved_base()?;
    let source = evidence.record(&snapshot)?;
    let input = build_input(&snapshot, &source, target.index())?;
    let input_fingerprint = evaluator::input_fingerprint(&input)?;
    let config_fingerprint = evaluator::DualReviewEvaluator::config_fingerprint(&settings)?;
    let rubric_fingerprint = evaluator::rubric_fingerprint();
    let scope = crate::local_context::scope(&snapshot)?;
    let fingerprint = crate::action_history::hash(&(
        &scope,
        source.id()?,
        target.index(),
        &input_fingerprint,
        &config_fingerprint,
        &rubric_fingerprint,
    ))?;
    Ok(Pending {
        snapshot,
        target,
        evidence,
        source,
        settings,
        input,
        input_fingerprint,
        config_fingerprint,
        rubric_fingerprint,
        scope,
        fingerprint,
    })
}

pub fn validate(store: &SessionStore, pending: &Pending) -> anyhow::Result<()> {
    if let Evidence::Provider(result) = &pending.evidence {
        let settings = store
            .local_review_settings()?
            .context("source review settings are unavailable")?;
        ensure!(
            crate::local_review::config_fingerprint(&settings)? == result.config_fingerprint,
            "source review provider settings changed"
        );
    }
    ensure!(
        store.evaluator_settings()?.as_ref() == Some(&pending.settings),
        "evaluator settings changed; preview again"
    );
    let current = prepare(
        &pending.snapshot,
        pending.target,
        pending.evidence.clone(),
        pending.settings.clone(),
    )?;
    ensure!(
        current.fingerprint == pending.fingerprint
            && current.source.id()? == pending.source.id()?
            && evaluator::input_fingerprint(&pending.input)? == current.input_fingerprint
            && current.input_fingerprint == pending.input_fingerprint
            && current.config_fingerprint == pending.config_fingerprint
            && current.rubric_fingerprint == pending.rubric_fingerprint,
        "local finding, evidence or evaluator identity changed since preview"
    );
    Ok(())
}

pub fn opinion(
    pending: &Pending,
    analysis: evaluator::CombinedReviewAnalysis,
) -> anyhow::Result<FindingOpinion> {
    crate::evaluation_history::validate_analysis(&analysis)?;
    Ok(FindingOpinion {
        index: pending.target.index(),
        finding_fingerprint: crate::local_review_history::finding_fingerprint(
            pending
                .source
                .finding(pending.target.index())
                .context("opinion target is absent")?,
        )?,
        input_fingerprint: pending.input_fingerprint.clone(),
        config_fingerprint: pending.config_fingerprint.clone(),
        rubric_fingerprint: pending.rubric_fingerprint.clone(),
        analysis,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn settings() -> EvaluatorSettings {
        EvaluatorSettings {
            openai_endpoint: "https://judge.example.test/chat/completions".to_string(),
            openai_model: "judge".to_string(),
            jev_model: "jev".to_string(),
        }
    }

    #[test]
    fn preview_binds_local_provenance_and_rejects_artifact_settings_and_input_edits() {
        let (root, snapshot, report) = crate::local_analysis::sample_loaded_report();
        let db = tempfile::tempdir().unwrap();
        let store = SessionStore::open(db.path().join("opinion.sqlite3")).unwrap();
        store.save_evaluator_settings(&settings()).unwrap();
        let pending = prepare(
            &snapshot,
            Target::Static(0),
            Evidence::Static(Box::new(report)),
            settings(),
        )
        .unwrap();
        assert_eq!(
            pending.input.origin,
            evaluator::AnalysisOrigin::LocalCheckout
        );
        assert_eq!(
            pending.input.repository_identity,
            format!("local/{}", pending.source.repository_key)
        );
        assert!(
            pending.input.thread_context.is_empty() && pending.input.source_at_comment.is_empty()
        );
        assert!(
            serde_json::to_string(&pending.input)
                .unwrap()
                .contains("R1")
        );
        validate(&store, &pending).unwrap();
        let mut corrupt = pending.clone();
        corrupt.input.current_diff.push_str("unapproved");
        assert!(validate(&store, &corrupt).is_err());
        let mut changed = settings();
        changed.jev_model = "changed".to_string();
        store.save_evaluator_settings(&changed).unwrap();
        assert!(validate(&store, &pending).is_err());
        store.save_evaluator_settings(&settings()).unwrap();
        std::fs::write(root.path().join("artifacts/check.sarif"), "{}").unwrap();
        assert!(validate(&store, &pending).is_err());
    }

    #[test]
    fn suppressed_provider_findings_can_be_evaluated_but_changed_provider_settings_block_send() {
        let (_root, snapshot, report) = crate::local_analysis::sample_loaded_report();
        let db = tempfile::tempdir().unwrap();
        let store = SessionStore::open(db.path().join("provider.sqlite3")).unwrap();
        let review_settings = crate::session_store::LocalReviewSettings {
            openai_endpoint: "https://review.example.test/v1".to_string(),
            openai_model: "reviewer".to_string(),
        };
        store.save_local_review_settings(&review_settings).unwrap();
        store.save_evaluator_settings(&settings()).unwrap();
        let finding = report.report.findings[0].clone();
        let result = crate::local_review::LocalReviewResult {
            analysis: Some(report),
            fail_on: cururu_core::FailOn::Off,
            config_fingerprint: crate::local_review::config_fingerprint(&review_settings).unwrap(),
            base: None,
            context: None,
            review: cururu_core::ReviewResult {
                model: "reviewer".to_string(),
                files_reviewed: 1,
                summary: String::new(),
                findings: Vec::new(),
            },
            usage: None,
            diff_fingerprint: crate::local_review::snapshot_fingerprint(&snapshot),
            head_oid: snapshot.head_oid.clone(),
            diff_bytes: 1,
            chunk_count: 1,
            discarded_findings: 0,
            policy_suppressed: vec![crate::local_review::PolicySuppressedFinding {
                finding,
                reason: "severity excluded by repository policy",
            }],
        };
        let pending = prepare(
            &snapshot,
            Target::Provider(0),
            Evidence::Provider(Box::new(result)),
            settings(),
        )
        .unwrap();
        validate(&store, &pending).unwrap();
        assert!(pending.source.review.findings.is_empty());
        assert_eq!(pending.source.suppressed.len(), 1);
        let mut changed = review_settings;
        changed.openai_model = "changed".to_string();
        store.save_local_review_settings(&changed).unwrap();
        assert!(validate(&store, &pending).is_err());
    }

    #[test]
    fn excessive_diff_is_rejected_without_sending_a_truncated_claim() {
        let (_root, snapshot, report) = crate::local_analysis::sample_loaded_report();
        let record =
            LocalReviewRecord::from_static(&snapshot, &report, HumanAssessment::Unreviewed)
                .unwrap();
        let mut oversized = snapshot;
        oversized
            .changed_files
            .iter_mut()
            .find(|file| file.path == "file.rs")
            .unwrap()
            .patch = "x".repeat(8_001);
        assert!(build_input(&oversized, &record, 0).is_err());
    }
}
