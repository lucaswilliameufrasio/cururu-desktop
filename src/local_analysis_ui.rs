//! Local artifact preview and guarded navigation, independent of AI credentials.

use crate::{
    ReviewWorkspace,
    local_analysis::{self, AnalysisPreview, PreparedAnalysis},
    repository::RepositorySnapshot,
};
use gpui::{App, Context, Entity, ParentElement as _, Styled as _, div};
use gpui_component::{Disableable as _, button::Button};

#[derive(Default)]
pub enum AnalysisState {
    #[default]
    Idle,
    Loading(String),
    Manifest(AnalysisPreview),
    Complete(Box<PreparedAnalysis>),
    Included(Box<PreparedAnalysis>),
    Failed(String),
}

enum Prepared {
    Manifest(AnalysisPreview),
    Report(Box<PreparedAnalysis>),
}

impl ReviewWorkspace {
    #[must_use]
    pub fn approved_analysis(&self) -> Option<&PreparedAnalysis> {
        match &self.local_analysis_state {
            AnalysisState::Included(report) => Some(report),
            _ => None,
        }
    }

    pub const fn evidence_edits_busy(&self) -> bool {
        self.is_opening_repository
            || self.local_opinion_state.busy()
            || self.remote_commit_state.is_busy()
            || matches!(self.local_analysis_state, AnalysisState::Loading(_))
            || matches!(
                self.local_review_state,
                crate::LocalReviewState::Running(_)
                    | crate::LocalReviewState::RecommendationRunning(_)
                    | crate::LocalReviewState::SavingSettings
                    | crate::LocalReviewState::Authorizing { .. }
                    | crate::LocalReviewState::Revoking { .. }
            )
            || matches!(
                self.suggested_change_state,
                crate::SuggestedChangeState::Preparing(_)
                    | crate::SuggestedChangeState::Applying(_)
            )
            || matches!(
                self.local_command_state,
                crate::LocalCommandState::Preparing(_) | crate::LocalCommandState::Running { .. }
            )
    }

    pub fn revoke_analysis_inclusion(&mut self) {
        self.local_analysis_state = match std::mem::take(&mut self.local_analysis_state) {
            AnalysisState::Included(report) => {
                self.local_review_state = crate::LocalReviewState::Idle;
                self.suggested_change_state = crate::SuggestedChangeState::Idle;
                AnalysisState::Complete(report)
            }
            state => state,
        };
    }

    fn update_analysis_inclusion(&mut self, fingerprint: &str, include: bool) -> bool {
        if self.evidence_edits_busy() {
            return false;
        }
        let (AnalysisState::Complete(report) | AnalysisState::Included(report)) =
            &self.local_analysis_state
        else {
            return false;
        };
        if report.fingerprint != fingerprint
            || self
                .local_repository_snapshot
                .as_ref()
                .and_then(|snapshot| crate::local_context::scope(snapshot).ok())
                .as_ref()
                != Some(&report.preview.scope)
        {
            return false;
        }
        let report = report.clone();
        self.local_analysis_state = if include {
            AnalysisState::Included(report)
        } else {
            AnalysisState::Complete(report)
        };
        self.local_review_state = crate::LocalReviewState::Idle;
        self.suggested_change_state = crate::SuggestedChangeState::Idle;
        true
    }

    pub fn render_local_analysis(&self, cx: &Context<Self>) -> gpui::Div {
        if matches!(
            self.remote_review_state,
            crate::RemoteReviewState::Loaded(_)
        ) {
            return div();
        }
        let Some(snapshot) = &self.local_repository_snapshot else {
            return div();
        };
        let Ok(policy) = &snapshot.review_policy else {
            return div();
        };
        if !policy.analysis.enabled {
            return div();
        }
        let busy = self.evidence_edits_busy();
        let entity = cx.entity();
        let mut panel = div().flex().flex_col().gap_2()
            .child("LOCAL STATIC ANALYSIS · artifact inspection only; no analyzer or provider is started")
            .child(format!("Manifest {:?}. Required HEAD {:?} and desktop_diff_fingerprint {}. Keep artifacts in a Git-ignored directory to avoid self-referential diffs.", policy.analysis.manifest, snapshot.head_oid, crate::local_review::snapshot_fingerprint(snapshot)))
            .child("Tool status is an untrusted manifest claim, not proof of execution. Loading this evidence never sends it to an AI provider or authorizes apply/publication.")
            .child(Button::new("preview-analysis-manifest").label("Read manifest and preview referenced paths only").disabled(busy)
                .on_click(move |_, _, cx| prepare(&entity, None, cx)));
        if let Some(head) = &snapshot.head_oid {
            let fields = serde_json::json!({"commit_sha": head, "desktop_diff_fingerprint": crate::local_review::snapshot_fingerprint(snapshot)}).to_string();
            panel = panel.child(
                Button::new("copy-analysis-revision-fields")
                    .label("Copy HEAD / diff manifest fields")
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(fields.clone()));
                    }),
            );
        }
        let scope = crate::local_context::scope(snapshot).ok();
        let stale = match &self.local_analysis_state {
            AnalysisState::Manifest(preview) => scope.as_ref() != Some(&preview.scope),
            AnalysisState::Complete(report) | AnalysisState::Included(report) => {
                scope.as_ref() != Some(&report.preview.scope)
            }
            _ => false,
        };
        if stale {
            return panel.child("Previous static evidence belongs to another checkout/snapshot. Read the manifest again before inspection.");
        }
        match &self.local_analysis_state {
            AnalysisState::Loading(_) => {
                panel = panel.child("Loading or revalidating local analysis artifacts…");
            }
            AnalysisState::Failed(error) => {
                panel = panel.child(format!("Static analysis unavailable: {error}"));
            }
            AnalysisState::Manifest(preview) => {
                let entity = cx.entity();
                let preview = preview.clone();
                panel = panel
                    .child(format!(
                        "Manifest {} · tools {:?} · exact SARIF paths {:?}",
                        preview.manifest_hash, preview.tools, preview.paths
                    ))
                    .child(
                        Button::new("load-analysis-artifacts")
                            .label("Load these SARIF artifacts locally for inspection")
                            .disabled(busy)
                            .on_click(move |_, _, cx| prepare(&entity, Some(preview.clone()), cx)),
                    );
            }
            AnalysisState::Complete(report) | AnalysisState::Included(report) => {
                panel = panel.child(self.render_static_report(cx, report, busy));
            }
            AnalysisState::Idle => {}
        }
        panel
    }

    fn render_static_report(
        &self,
        cx: &Context<Self>,
        report: &PreparedAnalysis,
        busy: bool,
    ) -> gpui::Div {
        let entity = cx.entity();
        let fingerprint = report.fingerprint.clone();
        let included = matches!(self.local_analysis_state, AnalysisState::Included(_));
        let mut panel = div().flex().flex_col().gap_2()
            .child(format!("Local static evidence · {} · status {} · {} anchored finding(s); {} invalid anchors discarded. Included for AI: {included}. Navigation revalidates artifacts and checkout first.", report.fingerprint, report.report.status, report.report.findings.len(), report.discarded))
            .child("Diagnostics/tool messages may contain private content and repeat per chunk (up to 64,000 bytes × 4 chunks). Inclusion clears active findings; save first if needed. No raw SARIF is sent.")
            .child(Button::new("toggle-analysis-inclusion").label(if included { "Exclude static evidence from AI (keep local preview)" } else { "Explicitly include this normalized evidence with the next AI review" }).disabled(busy)
                .on_click(move |_, _, cx| set_inclusion(&entity, &fingerprint, !included, cx)));
        for (index, finding) in report.report.findings.iter().enumerate() {
            let entity = cx.entity();
            let fingerprint = report.fingerprint.clone();
            panel = panel
                .child(crate::local_finding_opinion_ui::preview_button(
                    entity.clone(),
                    crate::local_finding_opinion::Target::Static(index),
                    format!("opinion-static-{index}"),
                    busy,
                ))
                .child(format!(
                    "{:?} / {:?} · {} · {}:{}\n{}\n{}",
                    finding.source,
                    finding.rule,
                    finding.severity,
                    finding.path,
                    finding.line.unwrap_or_default(),
                    finding.title,
                    finding.message
                ))
                .child(
                    Button::new(("navigate-static-finding", index))
                        .label("Revalidate and show this finding in the diff")
                        .disabled(busy)
                        .on_click(move |_, _, cx| navigate(&entity, &fingerprint, index, cx)),
                );
        }
        panel
    }
}

fn prepare(entity: &Entity<ReviewWorkspace>, preview: Option<AnalysisPreview>, cx: &mut App) {
    let request = entity.update(cx, |this, cx| {
        if this.evidence_edits_busy() {
            return None;
        }
        let snapshot = this.local_repository_snapshot.clone()?;
        let scope = crate::local_context::scope(&snapshot).ok()?;
        if let Some(preview) = &preview
            && !matches!(&this.local_analysis_state, AnalysisState::Manifest(current) if current == preview) {
            return None;
        }
        this.revoke_analysis_inclusion();
        this.local_analysis_state = AnalysisState::Loading(scope.clone());
        cx.notify();
        Some((snapshot, scope))
    });
    let Some((snapshot, scope)) = request else {
        return;
    };
    let entity = entity.clone();
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move {
                let current = snapshot.reopen_with_approved_base()?;
                anyhow::ensure!(
                    crate::local_context::scope(&current)? == scope,
                    "checkout changed before analysis preview"
                );
                preview.map_or_else(
                    || local_analysis::discover(&current).map(Prepared::Manifest),
                    |preview| {
                        local_analysis::load(&current, &preview)
                            .map(|report| Prepared::Report(Box::new(report)))
                    },
                )
            })
            .await;
        let _ = entity.update(cx, |this, cx| {
            if loading_is_current(this) {
                this.local_analysis_state = match result {
                    Ok(Prepared::Manifest(preview)) => AnalysisState::Manifest(preview),
                    Ok(Prepared::Report(report)) => AnalysisState::Complete(report),
                    Err(error) => AnalysisState::Failed(error.to_string()),
                };
            } else {
                this.local_analysis_state = AnalysisState::Idle;
            }
            cx.notify();
        });
    })
    .detach();
}

fn loading_is_current(workspace: &ReviewWorkspace) -> bool {
    matches!(&workspace.local_analysis_state, AnalysisState::Loading(scope)
        if workspace.local_repository_snapshot.as_ref().and_then(|snapshot| crate::local_context::scope(snapshot).ok()).as_ref() == Some(scope))
}

fn navigate(entity: &Entity<ReviewWorkspace>, fingerprint: &str, index: usize, cx: &mut App) {
    let request = entity.update(cx, |this, cx| {
        let (AnalysisState::Complete(current) | AnalysisState::Included(current)) =
            &this.local_analysis_state
        else {
            return None;
        };
        if current.fingerprint != fingerprint || this.evidence_edits_busy() {
            return None;
        }
        let snapshot = this.local_repository_snapshot.clone()?;
        let was_included = matches!(this.local_analysis_state, AnalysisState::Included(_));
        let report = (**current).clone();
        let finding = report.report.findings.get(index)?.clone();
        this.local_analysis_state = AnalysisState::Loading(report.preview.scope.clone());
        cx.notify();
        Some((snapshot, finding, report, was_included))
    });
    let Some((snapshot, finding, report, was_included)) = request else {
        return;
    };
    let entity = entity.clone();
    cx.spawn(async move |cx| {
        let original = report.clone();
        let result = cx
            .background_executor()
            .spawn(async move {
                let fresh: RepositorySnapshot = snapshot.reopen_with_approved_base()?;
                local_analysis::validate(&fresh, &report)
            })
            .await;
        let _ = entity.update(cx, |this, cx| {
            if loading_is_current(this)
                && !matches!(
                    this.remote_review_state,
                    crate::RemoteReviewState::Loaded(_)
                )
            {
                match result {
                    Ok(()) => {
                        if let Some(file) = this
                            .files
                            .iter()
                            .position(|file| file.path.as_ref() == finding.path)
                            && let Some(line) = this.files[file].lines.iter().position(|line| {
                                line.new_number
                                    == finding.line.and_then(|line| usize::try_from(line).ok())
                            })
                        {
                            this.selected_file = Some(file);
                            this.selected_line = line;
                            this.diff_scroll_handle.scroll_to_item(
                                this.diff_display_index(line),
                                gpui::ScrollStrategy::Center,
                            );
                        }
                        this.local_analysis_state = if was_included {
                            AnalysisState::Included(Box::new(original))
                        } else {
                            AnalysisState::Complete(Box::new(original))
                        };
                    }
                    Err(error) => {
                        if was_included {
                            this.local_review_state = crate::LocalReviewState::Idle;
                            this.suggested_change_state = crate::SuggestedChangeState::Idle;
                        }
                        this.local_analysis_state = AnalysisState::Failed(error.to_string());
                    }
                }
            } else {
                this.local_analysis_state = AnalysisState::Idle;
            }
            cx.notify();
        });
    })
    .detach();
}

fn set_inclusion(entity: &Entity<ReviewWorkspace>, fingerprint: &str, include: bool, cx: &mut App) {
    entity.update(cx, |this, cx| {
        if !this.update_analysis_inclusion(fingerprint, include) {
            return;
        }
        this.notice = Some("Static evidence choice updated in memory. No provider request was made; active findings were cleared.".into());
        cx.notify();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_preview_is_not_consent_and_inclusion_is_scoped_busy_guarded_and_revocable() {
        let root = tempfile::tempdir().unwrap();
        gix::init(root.path()).unwrap();
        let snapshot = RepositorySnapshot::open(root.path()).unwrap();
        let report = PreparedAnalysis {
            preview: AnalysisPreview {
                scope: crate::local_context::scope(&snapshot).unwrap(),
                manifest_hash: "a".repeat(64),
                paths: Vec::new(),
                tools: Vec::new(),
            },
            fingerprint: "b".repeat(64),
            report: cururu_core::AnalysisReport::from_parts(Vec::new(), Vec::new()),
            discarded: 0,
        };
        let mut workspace = ReviewWorkspace::sample();
        workspace.local_repository_snapshot = Some(snapshot);
        workspace.local_analysis_state = AnalysisState::Complete(Box::new(report.clone()));
        assert!(workspace.approved_analysis().is_none());
        assert!(!workspace.update_analysis_inclusion("wrong", true));
        workspace.is_opening_repository = true;
        assert!(!workspace.update_analysis_inclusion(&report.fingerprint, true));
        workspace.is_opening_repository = false;
        workspace.local_review_state = crate::LocalReviewState::Skipped("prior".to_string());
        assert!(workspace.update_analysis_inclusion(&report.fingerprint, true));
        assert!(matches!(
            workspace.local_review_state,
            crate::LocalReviewState::Idle
        ));
        assert_eq!(
            workspace.approved_analysis().unwrap().fingerprint,
            report.fingerprint
        );
        workspace.revoke_analysis_inclusion();
        assert!(workspace.approved_analysis().is_none());
        assert!(matches!(
            workspace.local_analysis_state,
            AnalysisState::Complete(_)
        ));
        workspace.local_repository_snapshot = None;
        assert!(!workspace.update_analysis_inclusion(&report.fingerprint, true));
    }
}
