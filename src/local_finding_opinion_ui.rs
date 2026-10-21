//! Per-preview, one-shot consent for local findings. No automatic filtering.

use crate::{
    ReviewWorkspace, evaluator,
    local_finding_opinion::{self, Evidence, Pending, Target},
};
use anyhow::{Context as _, ensure};
use gpui::{App, Context, Entity, ParentElement as _, Styled as _, div};
use gpui_component::{Disableable as _, button::Button};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
pub enum OpinionState {
    #[default]
    Idle,
    Preparing(u64),
    Preview(Box<Pending>),
    Running {
        request_id: u64,
        pending: Box<Pending>,
    },
    Complete {
        pending: Box<Pending>,
        result: Box<evaluator::CombinedReviewAnalysis>,
    },
    Saving {
        request_id: u64,
        pending: Box<Pending>,
    },
    Failed(String),
}

impl OpinionState {
    pub const fn busy(&self) -> bool {
        matches!(
            self,
            Self::Preparing(_) | Self::Running { .. } | Self::Saving { .. }
        )
    }

    const fn running_request_matches(&self, id: u64) -> bool {
        matches!(self, Self::Running { request_id, .. } if *request_id == id)
    }
}

impl ReviewWorkspace {
    pub fn render_local_understanding(&self, cx: &Context<Self>) -> gpui::Div {
        let mut panel = div().flex().flex_col().gap_2()
            .child("Select a local provider/static finding for a second opinion, or open a GitHub PR to inspect a thread. No evaluator is contacted automatically.");
        if let Some(snapshot) = &self.local_repository_snapshot
            && let Ok(identity) = crate::local_review::repository_identity(&snapshot.root)
        {
            panel = panel.child(self.render_local_review_results(
                snapshot,
                &identity,
                &crate::local_review::snapshot_fingerprint(snapshot),
                cx,
            ));
        }
        panel
            .child(self.render_local_analysis(cx))
            .child(self.render_local_finding_opinion(cx))
            .child(self.render_local_history(cx))
    }

    fn confirm_local_opinion(
        &mut self,
        fingerprint: &str,
    ) -> Option<(u64, crate::session_store::SessionStore, Box<Pending>)> {
        let OpinionState::Preview(pending) = &self.local_opinion_state else {
            return None;
        };
        if pending.fingerprint != fingerprint
            || !self.opinion_is_current(pending)
            || self.evidence_edits_busy()
        {
            return None;
        }
        let store = self.session_store.clone()?;
        let pending = pending.clone();
        let id = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
        self.local_opinion_state = OpinionState::Running {
            request_id: id,
            pending: pending.clone(),
        };
        Some((id, store, pending))
    }

    fn opinion_evidence(
        &self,
        target: Target,
    ) -> anyhow::Result<(crate::repository::RepositorySnapshot, Evidence)> {
        ensure!(
            !self.is_opening_repository
                && !matches!(
                    self.remote_review_state,
                    crate::RemoteReviewState::Loaded(_)
                ),
            "select a current local checkout first"
        );
        let snapshot = self
            .local_repository_snapshot
            .clone()
            .context("local checkout is unavailable")?;
        let evidence = match target {
            Target::Provider(_) => {
                let crate::LocalReviewState::Complete(result) = &self.local_review_state else {
                    anyhow::bail!("a live provider review is required");
                };
                let settings = self
                    .local_review_settings
                    .as_ref()
                    .context("review settings are unavailable")?;
                ensure!(
                    result.diff_fingerprint == crate::local_review::snapshot_fingerprint(&snapshot)
                        && result.config_fingerprint
                            == crate::local_review::config_fingerprint(settings)?
                        && result
                            .analysis
                            .as_ref()
                            .map(|analysis| &analysis.fingerprint)
                            == self
                                .approved_analysis()
                                .map(|analysis| &analysis.fingerprint)
                        && self
                            .local_context_state
                            .resolve(&snapshot)?
                            .as_ref()
                            .map(|context| context.fingerprint.clone())
                            == result
                                .context
                                .as_ref()
                                .map(|context| context.fingerprint.clone()),
                    "live provider review is stale"
                );
                Evidence::Provider(result.clone())
            }
            Target::Static(_) => {
                let (crate::local_analysis_ui::AnalysisState::Complete(report)
                | crate::local_analysis_ui::AnalysisState::Included(report)) =
                    &self.local_analysis_state
                else {
                    anyhow::bail!("a live local static report is required");
                };
                Evidence::Static(report.clone())
            }
        };
        ensure!(
            evidence
                .snapshot_record(&snapshot)?
                .finding(target.index())
                .is_some(),
            "selected finding no longer exists"
        );
        Ok((snapshot, evidence))
    }

    fn opinion_is_current(&self, pending: &Pending) -> bool {
        let current = || -> anyhow::Result<bool> {
            let (snapshot, evidence) = self.opinion_evidence(pending.target)?;
            Ok(self.evaluator_settings.as_ref() == Some(&pending.settings)
                && crate::local_context::scope(&snapshot)? == pending.scope
                && evidence.snapshot_record(&snapshot)?.id()? == pending.source.id()?)
        };
        current().unwrap_or(false)
    }

    pub fn render_local_finding_opinion(&self, cx: &Context<Self>) -> gpui::Div {
        if matches!(
            self.remote_review_state,
            crate::RemoteReviewState::Loaded(_)
        ) {
            return div();
        }
        let entity = cx.entity();
        let mut panel = div().flex().flex_col().gap_2()
            .child("LOCAL FINDING SECOND OPINION · observe-only; never changes findings, files or publication")
            .child("Configure both providers in Understand a review. A local finding needs its own one-shot confirmation below; existing review/GitHub authorizations do not apply.");
        match &self.local_opinion_state {
            OpinionState::Idle => {
                panel =
                    panel.child("Select a live finding to preview its exact outbound evidence.");
            }
            OpinionState::Preparing(_) => {
                panel = panel.child("Revalidating the finding and preparing a local-only preview…");
            }
            OpinionState::Running { pending, .. } => {
                panel = panel.child(
                    "Both evaluators are running; no partial result is shown. No automatic retry.",
                ).child(format!("Input {}", pending.input_fingerprint));
            }
            OpinionState::Saving { pending, .. } => {
                panel = panel
                    .child("Revalidating and saving an immutable opinion snapshot locally…")
                    .child(format!("Input {}", pending.input_fingerprint));
            }
            OpinionState::Failed(error) => {
                panel = panel.child(format!("Second opinion unavailable: {error}"));
            }
            OpinionState::Preview(pending) => {
                let fingerprint = pending.fingerprint.clone();
                panel = panel.child(format!("Checkout {} · HEAD {:?}\nOpenAI destination {} / {} · Jev https://api.typesafe.ai/v1/systemone / {}\nInput {} · rubric {}\nBoth providers will receive this exact JSON (no extra files/context are read or sent):\n{}", pending.snapshot.root.display(), pending.snapshot.head_oid, pending.settings.openai_endpoint, pending.settings.openai_model, pending.settings.jev_model, pending.input_fingerprint, pending.rubric_fingerprint, serde_json::to_string_pretty(&pending.input).unwrap_or_default()))
                    .child("Confirming grants one-shot permission to send this exact input to both evaluators. It does not authorize filtering, commands, apply or publication.")
                    .child(Button::new("confirm-local-opinion").label("Confirm send to both").disabled(!self.opinion_is_current(pending) || self.evidence_edits_busy())
                        .on_click(move |_, _, cx| send(&entity, &fingerprint, cx)));
            }
            OpinionState::Complete { pending, result } => {
                panel = panel.child(if self.opinion_is_current(pending) { "Input matches loaded UI; saving revalidates files/settings. Opinions are not calibrated labels." } else { "STALE / historical opinion: live finding/settings changed. No action or save is authorized." })
                    .child(crate::render_evaluator_result(result));
                let fingerprint = pending.fingerprint.clone();
                panel = panel.child("Provider prose may quote code. Save creates a source snapshot plus an immutable linked opinion record; raw evaluator inputs/context are excluded. Existing archive limits apply.")
                    .child(Button::new("save-local-opinion").label("Save opinion snapshot").disabled(!self.opinion_is_current(pending) || self.session_store.is_none() || self.evidence_edits_busy())
                        .on_click(move |_, _, cx| save(&entity, &fingerprint, cx)));
            }
        }
        if !self.local_opinion_state.busy()
            && !matches!(self.local_opinion_state, OpinionState::Idle)
        {
            let entity = cx.entity();
            panel = panel.child(
                Button::new("discard-local-opinion")
                    .label("Discard preview / result")
                    .on_click(move |_, _, cx| {
                        entity.update(cx, |this, cx| {
                            if !this.local_opinion_state.busy() {
                                this.local_opinion_state = OpinionState::Idle;
                                cx.notify();
                            }
                        });
                    }),
            );
        }
        panel
    }
}

pub fn preview_button(
    entity: Entity<ReviewWorkspace>,
    target: Target,
    id: String,
    disabled: bool,
) -> Button {
    Button::new(gpui::SharedString::from(id))
        .label("Preview second opinion")
        .disabled(disabled)
        .on_click(move |_, _, cx| preview(&entity, target, cx))
}

fn preview(entity: &Entity<ReviewWorkspace>, target: Target, cx: &mut App) {
    let request = entity.update(cx, |this, cx| {
        if this.local_opinion_state.busy() || this.evidence_edits_busy() {
            return None;
        }
        let prepared = (|| {
            let (snapshot, evidence) = this.opinion_evidence(target)?;
            let settings = this
                .evaluator_settings
                .clone()
                .context("configure both evaluator providers first in Understand a review")?;
            Ok::<_, anyhow::Error>((snapshot, evidence, settings))
        })();
        match prepared {
            Ok((snapshot, evidence, settings)) => {
                let id = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
                this.local_opinion_state = OpinionState::Preparing(id);
                cx.notify();
                Some((id, snapshot, evidence, settings))
            }
            Err(error) => {
                this.local_opinion_state = OpinionState::Failed(error.to_string());
                cx.notify();
                None
            }
        }
    });
    let Some((id, snapshot, evidence, settings)) = request else {
        return;
    };
    let entity = entity.clone();
    cx.spawn(async move |cx| {
        let result =
            cx.background_executor()
                .spawn(async move {
                    local_finding_opinion::prepare(&snapshot, target, evidence, settings)
                })
                .await;
        let _ = entity.update(cx, |this, cx| {
            if !matches!(this.local_opinion_state, OpinionState::Preparing(active) if active == id)
            {
                return;
            }
            this.local_opinion_state = match result {
                Ok(pending) if this.opinion_is_current(&pending) => {
                    OpinionState::Preview(Box::new(pending))
                }
                Ok(_) => OpinionState::Failed(
                    "Finding or settings changed during preview; select it again.".to_string(),
                ),
                Err(error) => OpinionState::Failed(error.to_string()),
            };
            cx.notify();
        });
    })
    .detach();
}

fn send(entity: &Entity<ReviewWorkspace>, fingerprint: &str, cx: &mut App) {
    let request = entity.update(cx, |this, cx| {
        let request = this.confirm_local_opinion(fingerprint)?;
        cx.notify();
        Some(request)
    });
    let Some((id, store, pending)) = request else {
        return;
    };
    let http = cx.http_client();
    let entity = entity.clone();
    cx.spawn(async move |cx| {
        let result = evaluate(http, store, &pending, cx.background_executor()).await;
        let _ = entity.update(cx, |this, cx| {
            if !this.local_opinion_state.running_request_matches(id) {
                return;
            }
            this.local_opinion_state = match result {
                Ok(result) if this.opinion_is_current(&pending) => OpinionState::Complete {
                    pending,
                    result: Box::new(result),
                },
                Ok(_) => OpinionState::Failed(
                    "Finding or provider configuration changed; opinion discarded.".to_string(),
                ),
                Err(error) => OpinionState::Failed(error.to_string()),
            };
            cx.notify();
        });
    })
    .detach();
}

async fn evaluate(
    http: std::sync::Arc<dyn gpui::http_client::HttpClient>,
    store: crate::session_store::SessionStore,
    pending: &Pending,
    executor: &gpui::BackgroundExecutor,
) -> anyhow::Result<evaluator::CombinedReviewAnalysis> {
    let preflight_store = store.clone();
    let request = pending.clone();
    let secrets = executor
        .spawn(async move {
            local_finding_opinion::validate(&preflight_store, &request)?;
            evaluator::EvaluatorSecrets::load()?
                .context("both evaluator keys must be configured in the OS keyring")
        })
        .await?;
    let evaluator = evaluator::DualReviewEvaluator::new(http, pending.settings.clone(), secrets)?;
    let result = crate::with_network_timeout(evaluator.analyze(&pending.input), executor).await?;
    crate::evaluation_history::validate_analysis(&result)?;
    let request = pending.clone();
    executor
        .spawn(async move { local_finding_opinion::validate(&store, &request) })
        .await?;
    Ok(result)
}

fn save(entity: &Entity<ReviewWorkspace>, fingerprint: &str, cx: &mut App) {
    let request = entity.update(cx, |this, cx| {
        let OpinionState::Complete { pending, result } = &this.local_opinion_state else {
            return None;
        };
        if pending.fingerprint != fingerprint
            || !this.opinion_is_current(pending)
            || this.evidence_edits_busy()
        {
            return None;
        }
        let id = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
        let request = (
            id,
            this.session_store.clone()?,
            pending.clone(),
            result.clone(),
        );
        this.local_opinion_state = OpinionState::Saving {
            request_id: id,
            pending: pending.clone(),
        };
        cx.notify();
        Some(request)
    });
    let Some((id, store, pending, analysis)) = request else {
        return;
    };
    let entity = entity.clone();
    cx.spawn(async move |cx| {
        let original = pending.clone();
        let original_analysis = analysis.clone();
        let result = cx.background_executor().spawn(async move {
            local_finding_opinion::validate(&store, &pending)?;
            store.save_local_finding_opinion(&pending.source, local_finding_opinion::opinion(&pending, *analysis)?)
        }).await;
        let _ = entity.update(cx, |this, cx| {
            if !matches!(&this.local_opinion_state, OpinionState::Saving { request_id, .. } if *request_id == id) {
                return;
            }
            this.notice = Some(match result {
                Ok(id) => format!("Saved local finding opinion {id}. No action/filter/permission was restored."),
                Err(error) => format!("Opinion save failed: {error}. In-memory result retained; no evaluator was retried."),
            }.into());
            this.local_opinion_state = OpinionState::Complete { pending: original, result: original_analysis };
            cx.notify();
        });
    }).detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_shot_send_requires_exact_live_preview_and_does_not_restore_consent() {
        let (_root, snapshot, report) = crate::local_analysis::sample_loaded_report();
        let db = tempfile::tempdir().unwrap();
        let store =
            crate::session_store::SessionStore::open(db.path().join("consent.sqlite3")).unwrap();
        let settings = crate::session_store::EvaluatorSettings {
            openai_endpoint: "https://judge.example.test/chat/completions".to_string(),
            openai_model: "judge".to_string(),
            jev_model: "jev".to_string(),
        };
        let pending = local_finding_opinion::prepare(
            &snapshot,
            Target::Static(0),
            Evidence::Static(Box::new(report.clone())),
            settings.clone(),
        )
        .unwrap();
        let mut workspace = ReviewWorkspace::sample();
        workspace.session_store = Some(store);
        workspace.local_repository_snapshot = Some(snapshot);
        workspace.evaluator_settings = Some(settings);
        workspace.local_analysis_state =
            crate::local_analysis_ui::AnalysisState::Complete(Box::new(report));
        assert!(
            workspace
                .confirm_local_opinion(&pending.fingerprint)
                .is_none()
        );
        workspace.local_opinion_state = OpinionState::Preview(Box::new(pending.clone()));
        assert!(
            workspace
                .confirm_local_opinion("stale fingerprint")
                .is_none()
        );
        workspace.is_opening_repository = true;
        assert!(
            workspace
                .confirm_local_opinion(&pending.fingerprint)
                .is_none()
        );
        workspace.is_opening_repository = false;
        let (first_id, _, _) = workspace
            .confirm_local_opinion(&pending.fingerprint)
            .unwrap();
        assert!(
            workspace
                .local_opinion_state
                .running_request_matches(first_id)
        );
        assert!(
            workspace
                .confirm_local_opinion(&pending.fingerprint)
                .is_none()
        );
        assert!(workspace.local_opinion_state.busy());
        workspace.local_opinion_state = OpinionState::Preview(Box::new(pending.clone()));
        let (second_id, _, _) = workspace
            .confirm_local_opinion(&pending.fingerprint)
            .unwrap();
        assert_ne!(first_id, second_id);
        assert!(
            !workspace
                .local_opinion_state
                .running_request_matches(first_id)
        );
        assert!(
            workspace
                .local_opinion_state
                .running_request_matches(second_id)
        );
        workspace.local_opinion_state = OpinionState::Complete {
            pending: Box::new(pending.clone()),
            result: Box::new(crate::evaluation_history::sample_record().analysis),
        };
        assert!(
            workspace
                .confirm_local_opinion(&pending.fingerprint)
                .is_none()
        );
        workspace.local_analysis_state = crate::local_analysis_ui::AnalysisState::Idle;
        assert!(!workspace.opinion_is_current(&pending));
    }
}
