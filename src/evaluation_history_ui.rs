//! Explicit local archive actions; never restore an archived result as live state.

use gpui::{App, Context, Entity, IntoElement, ParentElement as _, Styled as _, div};
use gpui_component::{Disableable as _, button::Button};

use crate::{
    RemoteReviewState, RemoteSourceContextState, ReviewWorkspace,
    evaluation_history::{EvaluationAnchor, EvaluationRecord, HumanAssessment, StoredEvaluation},
    github_pr::PullRequestRef,
};

#[derive(Default)]
pub enum HistoryState {
    #[default]
    Idle,
    Busy,
    Loaded {
        reference: PullRequestRef,
        page: usize,
        rows: Vec<StoredEvaluation>,
    },
}

enum Action {
    Load,
    Save(Box<EvaluationRecord>),
    Delete(String),
}

impl ReviewWorkspace {
    pub fn current_evaluation_anchor(&self) -> Option<EvaluationAnchor> {
        let RemoteReviewState::Loaded(snapshot) = &self.remote_review_state else {
            return None;
        };
        let comment = self.selected_remote_comment.as_ref()?;
        let canonical = snapshot
            .review_threads
            .iter()
            .flat_map(|thread| std::iter::once(&thread.root).chain(&thread.replies))
            .find(|entry| entry.id == comment.id)?;
        if canonical != comment {
            return None;
        }
        let RemoteSourceContextState::Loaded {
            comment_id,
            source,
            anchor_line,
            ..
        } = &self.remote_source_context
        else {
            return None;
        };
        if *comment_id != comment.id {
            return None;
        }
        let input =
            crate::build_review_analysis_input(snapshot, comment, source, *anchor_line).ok()?;
        EvaluationAnchor::new(snapshot, comment, &input, self.evaluator_settings.as_ref()?).ok()
    }

    pub fn render_evaluation_history(
        &self,
        cx: &Context<Self>,
        snapshot: &crate::github_pr::PullRequestSnapshot,
    ) -> impl IntoElement {
        let busy = matches!(self.evaluation_history, HistoryState::Busy);
        let current = self.current_evaluation_anchor();
        let mut panel = div().flex().flex_col().gap_2()
            .child("LOCAL EVALUATION ARCHIVE · explicit saves, no automatic filtering")
            .child("Saved provider prose may contain quoted code/comments. Inputs, keys and full source snapshots are not archived. Up to 200 records; delete explicitly. Human assessments are not calibrated labels.")
            .child(action_button(cx.entity(), snapshot.reference.clone(), 0, Action::Load, "load-evaluations".into(), "Load saved evaluations", busy));
        if let (Some(anchor), Some(analysis)) = (
            self.evaluator_result_anchor.as_ref(),
            self.evaluator_result.as_ref(),
        ) && current.as_ref() == Some(anchor)
        {
            for assessment in [
                HumanAssessment::Unreviewed,
                HumanAssessment::Agree,
                HumanAssessment::Disagree,
                HumanAssessment::NeedsMoreEvidence,
            ] {
                panel = panel.child(action_button(
                    cx.entity(),
                    snapshot.reference.clone(),
                    0,
                    Action::Save(Box::new(EvaluationRecord {
                        anchor: anchor.clone(),
                        analysis: analysis.clone(),
                        assessment,
                    })),
                    format!("save-evaluation-{}", assessment.label()),
                    &format!("Save locally · {}", assessment.label()),
                    busy,
                ));
            }
        }
        if let HistoryState::Loaded {
            reference,
            page,
            rows,
        } = &self.evaluation_history
            && reference == &snapshot.reference
        {
            panel = panel.child(format!("Page {} · {} saved records", page + 1, rows.len()));
            if *page > 0 {
                panel = panel.child(action_button(
                    cx.entity(),
                    reference.clone(),
                    page - 1,
                    Action::Load,
                    "history-previous".into(),
                    "Previous page",
                    busy,
                ));
            }
            if rows.len() == 20 && *page < 9 {
                panel = panel.child(action_button(
                    cx.entity(),
                    reference.clone(),
                    page + 1,
                    Action::Load,
                    "history-next".into(),
                    "Next page",
                    busy,
                ));
            }
            for row in rows {
                let status = if current.as_ref() == Some(&row.record.anchor) {
                    "Matches current evidence"
                } else {
                    "STALE / context not verified"
                };
                panel = panel.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(format!(
                            "{} · comment {} · {} · saved {} ms since epoch",
                            status,
                            row.record.anchor.comment_id,
                            row.record.assessment.label(),
                            row.created_at
                        ))
                        .child(format!(
                            "base {} · head {} · source {}",
                            row.record.anchor.base_sha,
                            row.record.anchor.head_sha,
                            row.record.anchor.source_sha
                        ))
                        .child(crate::render_evaluator_result(&row.record.analysis))
                        .child(action_button(
                            cx.entity(),
                            reference.clone(),
                            *page,
                            Action::Delete(row.id.clone()),
                            format!("delete-evaluation-{}", row.id),
                            "Delete this local record",
                            busy,
                        )),
                );
            }
        }
        panel
    }
}

fn action_button(
    entity: Entity<ReviewWorkspace>,
    reference: PullRequestRef,
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
            let action = match &action {
                Action::Load => Action::Load,
                Action::Save(record) => Action::Save(record.clone()),
                Action::Delete(id) => Action::Delete(id.clone()),
            };
            run_action(&entity, reference.clone(), page, action, cx);
        })
}

fn run_action(
    entity: &Entity<ReviewWorkspace>,
    reference: PullRequestRef,
    page: usize,
    action: Action,
    cx: &mut App,
) {
    let store = entity.update(cx, |this, cx| {
        if matches!(this.evaluation_history, HistoryState::Busy) {
            return None;
        }
        if let Action::Save(record) = &action
            && (this.current_evaluation_anchor().as_ref() != Some(&record.anchor)
                || this.evaluator_result.as_ref() != Some(&record.analysis))
        {
            this.evaluator_notice = Some("Evaluation changed; local save cancelled.".into());
            cx.notify();
            return None;
        }
        let store = this.session_store.clone()?;
        this.evaluation_history = HistoryState::Busy;
        cx.notify();
        Some(store)
    });
    let Some(store) = store else {
        return;
    };
    let entity = entity.clone();
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move {
                match action {
                    Action::Load => {}
                    Action::Save(record) => {
                        store.save_evaluation(&record)?;
                    }
                    Action::Delete(id) => {
                        store.delete_evaluation(&reference, &id)?;
                    }
                }
                let rows = store.evaluations(&reference, page)?;
                Ok::<_, anyhow::Error>((reference, rows))
            })
            .await;
        let _ = entity.update(cx, |this, cx| {
            match result {
                Ok((reference, rows)) => {
                    this.evaluation_history = HistoryState::Loaded {
                        reference,
                        page,
                        rows,
                    };
                    this.evaluator_notice =
                        Some("Local archive updated. No network action was performed.".into());
                }
                Err(error) => {
                    this.evaluation_history = HistoryState::Idle;
                    this.evaluator_notice =
                        Some(format!("Local archive unavailable: {error}").into());
                }
            }
            cx.notify();
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github_pr::{
        PullRequestComment, PullRequestReviewThread, PullRequestSnapshot, PullRequestSourceFile,
    };

    fn workspace() -> ReviewWorkspace {
        let mut workspace = ReviewWorkspace::sample();
        let comment = PullRequestComment {
            id: 1,
            path: "src/lib.rs".to_string(),
            line: Some(1),
            original_line: Some(1),
            body: "Can this fail?".to_string(),
            author: "reviewer".to_string(),
            html_url: String::new(),
            commit_id: "a".repeat(40),
            original_commit_id: "c".repeat(40),
            in_reply_to_id: None,
            is_outdated: false,
            subject_type: Some("line".to_string()),
            side: Some("RIGHT".to_string()),
        };
        workspace.remote_review_state = RemoteReviewState::Loaded(Box::new(PullRequestSnapshot {
            reference: PullRequestRef::parse("octo/repo#1").unwrap(),
            title: "Test".to_string(),
            state: "open".to_string(),
            author: "author".to_string(),
            html_url: String::new(),
            base_sha: "b".repeat(40),
            head_sha: "a".repeat(40),
            files: vec![cururu_core::ChangedFile {
                path: "src/lib.rs".to_string(),
                patch: "@@ -1 +1 @@\n-before\n+after".to_string(),
                right_lines: vec![1],
            }],
            reviews: Vec::new(),
            review_threads: vec![PullRequestReviewThread {
                root: comment.clone(),
                replies: Vec::new(),
            }],
            conversation_comments: Vec::new(),
        }));
        workspace.selected_remote_comment = Some(comment);
        workspace.remote_source_context = RemoteSourceContextState::Loaded {
            comment_id: 1,
            source: PullRequestSourceFile {
                path: "src/lib.rs".to_string(),
                commit_sha: "c".repeat(40),
                content: "after".to_string(),
            },
            anchor_line: Some(1),
        };
        workspace.evaluator_settings = Some(crate::session_store::EvaluatorSettings {
            openai_endpoint: "https://api.openai.com/v1/chat/completions".to_string(),
            openai_model: "judge".to_string(),
            jev_model: "jev".to_string(),
        });
        workspace
    }

    #[test]
    fn live_evidence_identity_detects_sha_thread_source_and_provider_changes() {
        let original = workspace().current_evaluation_anchor().unwrap();
        for field in [
            "head", "base", "thread", "patch", "source", "provider", "comment",
        ] {
            let mut workspace = workspace();
            match field {
                "head" | "base" | "thread" | "patch" => {
                    let RemoteReviewState::Loaded(snapshot) = &mut workspace.remote_review_state
                    else {
                        panic!("fixture");
                    };
                    match field {
                        "head" => snapshot.head_sha = "1".repeat(40),
                        "base" => snapshot.base_sha = "2".repeat(40),
                        "thread" => snapshot.review_threads[0].root.body.push_str(" Edited"),
                        _ => snapshot.files[0].patch.push_str("\n+more evidence"),
                    }
                }
                "source" => {
                    let RemoteSourceContextState::Loaded { source, .. } =
                        &mut workspace.remote_source_context
                    else {
                        panic!("fixture");
                    };
                    source.content.push_str("\nmore code");
                }
                "provider" => workspace
                    .evaluator_settings
                    .as_mut()
                    .unwrap()
                    .openai_model
                    .push_str("-changed"),
                _ => workspace
                    .selected_remote_comment
                    .as_mut()
                    .unwrap()
                    .body
                    .push_str(" Edited"),
            }
            assert_ne!(
                workspace.current_evaluation_anchor().as_ref(),
                Some(&original),
                "{field}"
            );
        }
        let mut workspace = workspace();
        workspace.remote_source_context = RemoteSourceContextState::Idle;
        assert!(workspace.current_evaluation_anchor().is_none());
        assert!(workspace.evaluator_result.is_none());
    }
}
