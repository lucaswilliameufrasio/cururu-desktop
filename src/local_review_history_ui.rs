//! Explicit immutable local review history. Archived findings have no actions.

use crate::{
    LocalReviewState, ReviewWorkspace,
    evaluation_history::HumanAssessment,
    local_review_history::{self, LocalReviewRecord, StoredLocalReview},
};
use anyhow::{Context as _, ensure};
use gpui::{App, Context, Entity, ParentElement as _, Styled as _, div};
use gpui_component::{Disableable as _, button::Button};

#[derive(Default)]
pub enum HistoryState {
    #[default]
    Idle,
    Busy,
    Loaded {
        repository_key: String,
        page: usize,
        rows: Vec<StoredLocalReview>,
        selected: Inspection,
    },
}

#[derive(Default, Clone)]
pub enum Inspection {
    #[default]
    None,
    Record(String),
    Finding {
        record_id: String,
        index: usize,
    },
}

#[derive(Clone)]
enum Action {
    Load,
    Save(Box<LocalReviewRecord>),
    SaveStatic(Box<LocalReviewRecord>),
    Delete(String),
    Annotate {
        record_id: String,
        index: usize,
        assessment: HumanAssessment,
    },
}

enum PendingSave {
    None,
    Provider(
        crate::repository::RepositorySnapshot,
        Box<crate::local_review::LocalReviewResult>,
    ),
    Static(
        crate::repository::RepositorySnapshot,
        Box<crate::local_analysis::PreparedAnalysis>,
    ),
}

impl ReviewWorkspace {
    fn current_static_history_record(
        &self,
        assessment: HumanAssessment,
    ) -> anyhow::Result<LocalReviewRecord> {
        let snapshot = self
            .local_repository_snapshot
            .as_ref()
            .context("local checkout is unavailable")?;
        let (crate::local_analysis_ui::AnalysisState::Complete(report)
        | crate::local_analysis_ui::AnalysisState::Included(report)) = &self.local_analysis_state
        else {
            anyhow::bail!("no loaded static artifact report is available");
        };
        LocalReviewRecord::from_static(snapshot, report, assessment)
    }

    fn current_history_record(
        &self,
        assessment: HumanAssessment,
    ) -> anyhow::Result<LocalReviewRecord> {
        let snapshot = self
            .local_repository_snapshot
            .as_ref()
            .context("local checkout is unavailable")?;
        let LocalReviewState::Complete(result) = &self.local_review_state else {
            anyhow::bail!("no completed local review is available");
        };
        ensure!(
            result.diff_fingerprint == crate::local_review::snapshot_fingerprint(snapshot)
                && result
                    .analysis
                    .as_ref()
                    .map(|analysis| &analysis.fingerprint)
                    == self
                        .approved_analysis()
                        .map(|analysis| &analysis.fingerprint)
                && result.base.as_ref().map(|base| &base.fingerprint)
                    == crate::shared_base::fingerprint(snapshot).as_ref(),
            "local review identity changed"
        );
        let context = self.local_context_state.resolve(snapshot)?;
        ensure!(
            context.as_ref().map(|context| &context.fingerprint)
                == result.context.as_ref().map(|context| &context.fingerprint),
            "local review context choice changed"
        );
        let config = crate::local_review::config_fingerprint(
            self.local_review_settings
                .as_ref()
                .context("local review settings are unavailable")?,
        )?;
        ensure!(
            config == result.config_fingerprint,
            "local review provider configuration changed"
        );
        LocalReviewRecord::from_result(&snapshot.root, result, assessment)
    }

    pub fn render_local_history(&self, cx: &Context<Self>) -> gpui::Div {
        let Some(snapshot) = &self.local_repository_snapshot else {
            return div();
        };
        let Ok(key) = local_review_history::repository_key(&snapshot.root) else {
            return div();
        };
        let busy =
            matches!(self.local_history_state, HistoryState::Busy) || self.session_store.is_none();
        let mut panel = div().flex().flex_col().gap_2()
            .child("LOCAL REVIEW HISTORY · explicit saves; records never restore apply/publish permissions")
            .child("Save results, suppressions or normalized static evidence with optional human decisions. Findings and tool messages may quote private content: inspect them before saving. Credentials/raw inputs/context/shared-config documents are excluded. Assessments are not calibrated labels. Limit: 200 records, 64,000 bytes each; delete explicitly.")
            .child(button(cx.entity(), key.clone(), 0, Action::Load, "load-local-history".into(), "Load saved local reviews", busy));
        if matches!(self.local_history_state, HistoryState::Busy) {
            panel = panel.child("Loading or updating local history…");
        }
        panel = panel.child(self.render_history_save_options(cx, &key, busy));
        if let HistoryState::Loaded {
            repository_key,
            page,
            rows,
            selected,
        } = &self.local_history_state
            && repository_key == &key
        {
            panel = panel.child(format!("Page {} · {} saved records", page + 1, rows.len()));
            if *page > 0 {
                panel = panel.child(button(
                    cx.entity(),
                    key.clone(),
                    page - 1,
                    Action::Load,
                    "local-history-previous".into(),
                    "Previous page",
                    busy,
                ));
            }
            if rows.len() == 20 && *page < 9 {
                panel = panel.child(button(
                    cx.entity(),
                    key.clone(),
                    page + 1,
                    Action::Load,
                    "local-history-next".into(),
                    "Next page",
                    busy,
                ));
            }
            for row in rows {
                let entity = cx.entity();
                let id = row.id.clone();
                panel = panel
                    .child(format!(
                        "{} · {} finding(s), {} suppressed · {} · saved {} ms since epoch",
                        crate::short_sha(&row.id),
                        row.record.review.findings.len(),
                        row.record.suppressed.len(),
                        row.record.assessment.label(),
                        row.created_at
                    ))
                    .child(
                        Button::new(gpui::SharedString::from(format!(
                            "inspect-local-review-{}",
                            row.id
                        )))
                        .label("Inspect saved result (no execution)")
                        .on_click(move |_, _, cx| {
                            entity.update(cx, |this, cx| {
                                if let HistoryState::Loaded { selected, .. } =
                                    &mut this.local_history_state
                                {
                                    *selected = Inspection::Record(id.clone());
                                }
                                cx.notify();
                            });
                        }),
                    )
                    .child(button(
                        cx.entity(),
                        key.clone(),
                        *page,
                        Action::Delete(row.id.clone()),
                        format!("delete-local-review-{}", row.id),
                        "Delete this local history record",
                        busy,
                    ));
                if matches!(selected, Inspection::Record(id) | Inspection::Finding { record_id: id, .. } if id == &row.id)
                {
                    panel = panel.child(render_record(
                        &row.record,
                        self.loaded_identity_matches(&row.record),
                    ));
                    panel = panel.child(render_finding_decisions(cx, &key, row, selected));
                }
            }
        }
        panel
    }

    fn render_history_save_options(&self, cx: &Context<Self>, key: &str, busy: bool) -> gpui::Div {
        let mut panel = div().flex().flex_col().gap_2();
        for assessment in [
            HumanAssessment::Unreviewed,
            HumanAssessment::Agree,
            HumanAssessment::Disagree,
            HumanAssessment::NeedsMoreEvidence,
        ] {
            if let Ok(record) = self.current_history_record(assessment) {
                panel = panel.child(button(
                    cx.entity(),
                    key.to_string(),
                    0,
                    Action::Save(Box::new(record)),
                    format!("save-local-review-{}", assessment.label()),
                    &format!("Save current review · {}", assessment.label()),
                    busy,
                ));
            }
            if let Ok(record) = self.current_static_history_record(assessment) {
                panel = panel.child(button(
                    cx.entity(),
                    key.to_string(),
                    0,
                    Action::SaveStatic(Box::new(record)),
                    format!("save-static-report-{}", assessment.label()),
                    &format!("Save loaded static evidence · {}", assessment.label()),
                    busy,
                ));
            }
        }
        panel
    }

    fn loaded_identity_matches(&self, record: &LocalReviewRecord) -> bool {
        let Some(snapshot) = &self.local_repository_snapshot else {
            return false;
        };
        if let Some(artifacts) = &record.artifacts {
            return record.diff_fingerprint == crate::local_review::snapshot_fingerprint(snapshot)
                && record.head_oid == snapshot.head_oid
                && record.base_fingerprint == crate::shared_base::fingerprint(snapshot)
                && matches!(&self.local_analysis_state, crate::local_analysis_ui::AnalysisState::Complete(report) | crate::local_analysis_ui::AnalysisState::Included(report) if report.fingerprint == artifacts.fingerprint);
        }
        let config = self
            .local_review_settings
            .as_ref()
            .and_then(|settings| crate::local_review::config_fingerprint(settings).ok());
        let context = self.local_context_state.resolve(snapshot);
        record.diff_fingerprint == crate::local_review::snapshot_fingerprint(snapshot)
            && record.analysis_fingerprint.as_ref()
                == self
                    .approved_analysis()
                    .map(|analysis| &analysis.fingerprint)
            && record.head_oid == snapshot.head_oid
            && config.as_ref() == Some(&record.config_fingerprint)
            && record.base_fingerprint == crate::shared_base::fingerprint(snapshot)
            && context.is_ok_and(|context| {
                context.as_ref().map(|context| &context.fingerprint)
                    == record.context_fingerprint.as_ref()
            })
    }
}

fn render_record(record: &LocalReviewRecord, matches: bool) -> gpui::Div {
    let mut panel = div().flex().flex_col().gap_2()
        .child(if matches { "Matches loaded identity only; the current filesystem was not revalidated by opening history." } else { "STALE / context or settings not verified. This is an archived result only." })
        .child(format!("head {} · model {} · diff {} · context {:?} · base {:?}\n{}", record.head_oid.as_deref().unwrap_or("unborn"), record.review.model, record.diff_fingerprint, record.context_fingerprint, record.base_fingerprint, record.review.summary));
    for finding in &record.review.findings {
        panel = panel.child(finding_text(finding));
    }
    if let Some(threshold) = record.fail_on {
        panel = panel.child(format!(
            "Archived quality status (informational): {:?}",
            cururu_core::evaluate_quality(&record.review, threshold)
        ));
    } else if record.artifacts.is_some() {
        panel = panel.child("Static evidence is separate from final AI quality policy; no provider tokens or chunks were used.");
    } else {
        panel = panel.child("Legacy record: quality threshold was not recorded.");
    }
    if let Some(artifacts) = &record.artifacts {
        panel = panel.child(format!("STATIC ARTIFACT PROVENANCE · {} · status {} · tool claims {:?}. No source artifact bytes were saved; claims are not proof of execution.", artifacts.fingerprint, artifacts.status, artifacts.tools));
    }
    panel = panel.child(format!("Annotation parent: {:?} · {} individual decision(s). Historical annotations do not change live findings or evaluator thresholds.", record.parent, record.finding_decisions.len()));
    for decision in &record.finding_decisions {
        panel = panel.child(format!(
            "Finding #{} · {} · {}",
            decision.index + 1,
            decision.fingerprint,
            decision.assessment.label()
        ));
    }
    for suppressed in &record.suppressed {
        panel = panel.child(format!(
            "SUPPRESSED: {}\n{}",
            suppressed.reason,
            finding_text(&suppressed.finding)
        ));
    }
    panel = panel.child(format!(
        "{} invalid candidates discarded · {} diff bytes · {} chunks",
        record.discarded_findings, record.diff_bytes, record.chunk_count
    ));
    if let Some(usage) = &record.usage {
        panel = panel.child(format!(
            "Usage: {} input · {} output · cost {:?}",
            usage.prompt_tokens, usage.completion_tokens, usage.cost
        ));
    }
    panel
}

fn render_finding_decisions(
    cx: &Context<ReviewWorkspace>,
    key: &str,
    row: &StoredLocalReview,
    inspection: &Inspection,
) -> gpui::Div {
    let mut panel = div().flex().flex_col().gap_2().child(
        "Select an archived finding to save an individual human decision (creates a new record).",
    );
    for index in 0..row.record.review.findings.len() + row.record.suppressed.len() {
        let Some(finding) = row.record.finding(index) else {
            continue;
        };
        let entity = cx.entity();
        let id = row.id.clone();
        panel = panel.child(
            Button::new(gpui::SharedString::from(format!(
                "select-archive-finding-{}-{index}",
                row.id
            )))
            .label(format!(
                "Finding #{} · {}:{}{}",
                index + 1,
                finding.path,
                finding.line.unwrap_or_default(),
                if index >= row.record.review.findings.len() {
                    " (policy-suppressed)"
                } else {
                    ""
                }
            ))
            .on_click(move |_, _, cx| {
                entity.update(cx, |this, cx| {
                    if let HistoryState::Loaded { selected, rows, .. } =
                        &mut this.local_history_state
                        && rows
                            .iter()
                            .any(|row| row.id == id && row.record.finding(index).is_some())
                    {
                        *selected = Inspection::Finding {
                            record_id: id.clone(),
                            index,
                        };
                    }
                    cx.notify();
                });
            }),
        );
        if matches!(inspection, Inspection::Finding { record_id, index: selected } if record_id == &row.id && *selected == index)
        {
            for assessment in [
                HumanAssessment::Unreviewed,
                HumanAssessment::Agree,
                HumanAssessment::Disagree,
                HumanAssessment::NeedsMoreEvidence,
            ] {
                panel = panel.child(button(
                    cx.entity(),
                    key.to_string(),
                    0,
                    Action::Annotate {
                        record_id: row.id.clone(),
                        index,
                        assessment,
                    },
                    format!("annotate-{}-{index}-{}", row.id, assessment.label()),
                    &format!(
                        "Save decision for finding #{} · {}",
                        index + 1,
                        assessment.label()
                    ),
                    false,
                ));
            }
        }
    }
    panel
}

fn finding_text(finding: &cururu_core::ReviewFinding) -> String {
    format!(
        "{} · {}:{} · confidence {}\n{}\n{}\n{}\nSuggested change: {:?}",
        finding.severity,
        finding.path,
        finding.line.unwrap_or_default(),
        finding.confidence,
        finding.title,
        finding.message,
        finding.suggestion,
        finding
            .suggested_change
            .as_ref()
            .map(|change| &change.replacement)
    )
}

fn button(
    entity: Entity<ReviewWorkspace>,
    key: String,
    page: usize,
    action: Action,
    id: String,
    label: &str,
    disabled: bool,
) -> Button {
    Button::new(gpui::SharedString::from(id))
        .label(label.to_owned())
        .disabled(disabled)
        .on_click(move |_, _, cx| {
            run(&entity, key.clone(), page, action.clone(), cx);
        })
}

fn pending_save(workspace: &ReviewWorkspace, action: &Action) -> anyhow::Result<PendingSave> {
    let (record, current) = match action {
        Action::Save(record) => (record, workspace.current_history_record(record.assessment)?),
        Action::SaveStatic(record) => (
            record,
            workspace.current_static_history_record(record.assessment)?,
        ),
        _ => return Ok(PendingSave::None),
    };
    ensure!(
        current.id()? == record.id()?,
        "selected result identity changed"
    );
    let snapshot = workspace
        .local_repository_snapshot
        .clone()
        .context("local checkout is unavailable")?;
    match action {
        Action::Save(_) => {
            let LocalReviewState::Complete(result) = &workspace.local_review_state else {
                anyhow::bail!("completed provider review is unavailable");
            };
            Ok(PendingSave::Provider(snapshot, result.clone()))
        }
        Action::SaveStatic(_) => {
            let (crate::local_analysis_ui::AnalysisState::Complete(report)
            | crate::local_analysis_ui::AnalysisState::Included(report)) =
                &workspace.local_analysis_state
            else {
                anyhow::bail!("loaded static evidence is unavailable");
            };
            Ok(PendingSave::Static(snapshot, report.clone()))
        }
        _ => anyhow::bail!("operation does not save a result snapshot"),
    }
}

fn validate_pending_save(evidence: &PendingSave, record: &LocalReviewRecord) -> anyhow::Result<()> {
    match evidence {
        PendingSave::Provider(snapshot, result) => {
            ensure!(
                record.artifacts.is_none(),
                "provider result cannot claim artifact provenance"
            );
            validate_save(snapshot, result)
        }
        PendingSave::Static(snapshot, approved) => {
            ensure!(
                crate::local_analysis::snapshot_for_save(snapshot, approved, record.assessment)?
                    .id()?
                    == record.id()?,
                "static snapshot data changed before saving"
            );
            Ok(())
        }
        PendingSave::None => anyhow::bail!("review evidence is unavailable"),
    }
}

fn validate_save(
    snapshot: &crate::repository::RepositorySnapshot,
    result: &crate::local_review::LocalReviewResult,
) -> anyhow::Result<()> {
    let fresh = snapshot.reopen_with_approved_base()?;
    ensure!(
        crate::local_review::snapshot_fingerprint(&fresh) == result.diff_fingerprint
            && crate::shared_base::fingerprint(&fresh).as_ref()
                == result.base.as_ref().map(|base| &base.fingerprint),
        "checkout or base approval changed before saving local review"
    );
    crate::local_context::validate(&fresh, result.context.as_ref())?;
    if let Some(analysis) = &result.analysis {
        crate::local_analysis::validate(&fresh, analysis)?;
    }
    Ok(())
}

fn run(entity: &Entity<ReviewWorkspace>, key: String, page: usize, action: Action, cx: &mut App) {
    let request = entity.update(cx, |this, cx| {
        if matches!(this.local_history_state, HistoryState::Busy) {
            return None;
        }
        let current_key = this
            .local_repository_snapshot
            .as_ref()
            .and_then(|snapshot| local_review_history::repository_key(&snapshot.root).ok())?;
        if current_key != key {
            return None;
        }
        if let Action::Annotate {
            record_id, index, ..
        } = &action
            && !annotation_is_selected(&this.local_history_state, &key, record_id, *index)
        {
            this.notice =
                Some("Select the archived finding again before saving a decision.".into());
            cx.notify();
            return None;
        }
        let store = this.session_store.clone()?;
        let evidence = match pending_save(this, &action) {
            Ok(evidence) => evidence,
            Err(error) => {
                this.notice =
                    Some(format!("Local evidence changed; save cancelled: {error}").into());
                cx.notify();
                return None;
            }
        };
        this.local_history_state = HistoryState::Busy;
        cx.notify();
        Some((store, evidence))
    });
    let Some((store, evidence)) = request else {
        return;
    };
    let entity = entity.clone();
    cx.spawn(async move |cx| {
        let query_key = key.clone();
        let result = cx
            .background_executor()
            .spawn(async move {
                match action {
                    Action::Load => {}
                    Action::Save(record) | Action::SaveStatic(record) => {
                        validate_pending_save(&evidence, &record)?;
                        store.save_local_review(&record)?;
                    }
                    Action::Delete(id) => {
                        store.delete_local_review(&query_key, &id)?;
                    }
                    Action::Annotate {
                        record_id,
                        index,
                        assessment,
                    } => {
                        store.annotate_local_finding(&query_key, &record_id, index, assessment)?;
                    }
                }
                store.local_review_history(&query_key, page)
            })
            .await;
        let _ = entity.update(cx, |this, cx| {
            match result {
                Ok(rows) => {
                    this.local_history_state = HistoryState::Loaded {
                        repository_key: key,
                        page,
                        rows,
                        selected: Inspection::None,
                    };
                    this.notice = Some(
                        "Local review history updated. No remote action was performed.".into(),
                    );
                }
                Err(error) => {
                    this.local_history_state = HistoryState::Idle;
                    this.notice = Some(format!("Local review history unavailable: {error}").into());
                }
            }
            cx.notify();
        });
    })
    .detach();
}

fn annotation_is_selected(state: &HistoryState, key: &str, id: &str, index: usize) -> bool {
    matches!(state,
        HistoryState::Loaded { repository_key, selected: Inspection::Finding { record_id, index: selected_index }, rows, .. }
        if repository_key == key && record_id == id && *selected_index == index
            && rows.iter().any(|row| row.id == id && row.record.finding(index).is_some()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn individual_decisions_require_the_selected_archived_finding_in_the_current_checkout() {
        let record = local_review_history::sample_record();
        let key = record.repository_key.clone();
        let id = record.id().unwrap();
        let mut state = HistoryState::Loaded {
            repository_key: key.clone(),
            page: 0,
            rows: vec![StoredLocalReview {
                id: id.clone(),
                created_at: 1,
                record,
            }],
            selected: Inspection::Record(id.clone()),
        };
        assert!(!annotation_is_selected(&state, &key, &id, 0));
        if let HistoryState::Loaded { selected, .. } = &mut state {
            *selected = Inspection::Finding {
                record_id: id.clone(),
                index: 1,
            };
        }
        assert!(annotation_is_selected(&state, &key, &id, 1));
        assert!(!annotation_is_selected(&state, &key, &id, 0));
        assert!(!annotation_is_selected(&state, &"d".repeat(64), &id, 1));
        assert!(!annotation_is_selected(&state, &key, &"e".repeat(64), 1));
        assert!(!annotation_is_selected(&HistoryState::Busy, &key, &id, 1));
        if let HistoryState::Loaded { rows, .. } = &mut state {
            rows.clear();
        }
        assert!(!annotation_is_selected(&state, &key, &id, 1));
    }

    #[test]
    fn saving_requires_current_provider_diff_and_evidence_and_revalidates_the_filesystem() {
        let root = tempfile::tempdir().unwrap();
        gix::init(root.path()).unwrap();
        std::fs::write(root.path().join("source.rs"), "code\n").unwrap();
        let snapshot = crate::repository::RepositorySnapshot::open(root.path()).unwrap();
        let settings = crate::session_store::LocalReviewSettings {
            openai_endpoint: "https://api.openai.com/v1".to_string(),
            openai_model: "test-model".to_string(),
        };
        let saved = local_review_history::sample_record();
        let mut review = saved.review;
        review.findings[0].path = "source.rs".to_string();
        let result = crate::local_review::LocalReviewResult {
            analysis: None,
            fail_on: cururu_core::FailOn::Off,
            config_fingerprint: crate::local_review::config_fingerprint(&settings).unwrap(),
            base: None,
            context: None,
            review,
            usage: None,
            diff_fingerprint: crate::local_review::snapshot_fingerprint(&snapshot),
            head_oid: snapshot.head_oid.clone(),
            diff_bytes: 100,
            chunk_count: 1,
            discarded_findings: 0,
            policy_suppressed: Vec::new(),
        };
        assert!(validate_save(&snapshot, &result).is_ok());
        let mut workspace = ReviewWorkspace::sample();
        workspace.local_repository_snapshot = Some(snapshot.clone());
        workspace.local_review_settings = Some(settings.clone());
        workspace.local_review_state = LocalReviewState::Complete(Box::new(result.clone()));
        assert!(
            workspace
                .current_history_record(HumanAssessment::Unreviewed)
                .is_ok()
        );
        workspace
            .local_review_settings
            .as_mut()
            .unwrap()
            .openai_model = "changed-model".to_string();
        assert!(
            workspace
                .current_history_record(HumanAssessment::Agree)
                .is_err()
        );
        workspace.local_review_settings = Some(settings);
        std::fs::write(root.path().join("source.rs"), "user edit\n").unwrap();
        assert!(validate_save(&snapshot, &result).is_err());
        workspace.local_repository_snapshot =
            Some(crate::repository::RepositorySnapshot::open(root.path()).unwrap());
        assert!(
            workspace
                .current_history_record(HumanAssessment::Unreviewed)
                .is_err()
        );
    }
}
