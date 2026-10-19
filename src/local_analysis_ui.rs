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
    Failed(String),
}

enum Prepared {
    Manifest(AnalysisPreview),
    Report(Box<PreparedAnalysis>),
}

impl ReviewWorkspace {
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
        let busy = matches!(self.local_analysis_state, AnalysisState::Loading(_))
            || self.is_opening_repository;
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
            AnalysisState::Complete(report) => scope.as_ref() != Some(&report.preview.scope),
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
            AnalysisState::Complete(report) => {
                panel = panel.child(format!("Local artifact evidence (not merged into final AI findings) · {} · status {} · {} anchored finding(s); {} invalid anchors discarded. Navigation revalidates artifacts and checkout first.", report.fingerprint, report.report.status, report.report.findings.len(), report.discarded));
                for (index, finding) in report.report.findings.iter().enumerate() {
                    let entity = cx.entity();
                    let report = report.clone();
                    panel = panel
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
                                .on_click(move |_, _, cx| {
                                    navigate(&entity, (*report).clone(), index, cx);
                                }),
                        );
                }
            }
            AnalysisState::Idle => {}
        }
        panel
    }
}

fn prepare(entity: &Entity<ReviewWorkspace>, preview: Option<AnalysisPreview>, cx: &mut App) {
    let request = entity.update(cx, |this, cx| {
        if matches!(this.local_analysis_state, AnalysisState::Loading(_)) || this.is_opening_repository {
            return None;
        }
        let snapshot = this.local_repository_snapshot.clone()?;
        let scope = crate::local_context::scope(&snapshot).ok()?;
        if let Some(preview) = &preview
            && !matches!(&this.local_analysis_state, AnalysisState::Manifest(current) if current == preview) {
            return None;
        }
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

fn navigate(
    entity: &Entity<ReviewWorkspace>,
    report: PreparedAnalysis,
    index: usize,
    cx: &mut App,
) {
    let request = entity.update(cx, |this, cx| {
        let AnalysisState::Complete(current) = &this.local_analysis_state else {
            return None;
        };
        if current.fingerprint != report.fingerprint || this.is_opening_repository {
            return None;
        }
        let snapshot = this.local_repository_snapshot.clone()?;
        let finding = report.report.findings.get(index)?.clone();
        this.local_analysis_state = AnalysisState::Loading(report.preview.scope.clone());
        cx.notify();
        Some((snapshot, finding))
    });
    let Some((snapshot, finding)) = request else {
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
                        this.local_analysis_state = AnalysisState::Complete(Box::new(original));
                    }
                    Err(error) => {
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
