//! Explicit shared-base fetch, safe effective-policy preview and memory approval.

use crate::{
    LocalReviewState, ReviewWorkspace, SuggestedChangeState,
    shared_base::{self, ApprovedBase},
};
use gpui::{App, Context, Entity, ParentElement as _, Styled as _, div};
use gpui_component::{Disableable as _, button::Button};

#[derive(Default)]
pub enum SharedBaseState {
    #[default]
    Idle,
    Loading(String),
    Preview(Box<ApprovedBase>),
    Failed(String),
}

const fn busy(workspace: &ReviewWorkspace) -> bool {
    matches!(workspace.shared_base_state, SharedBaseState::Loading(_))
        || matches!(workspace.local_review_state, LocalReviewState::Running(_))
        || matches!(
            workspace.suggested_change_state,
            SuggestedChangeState::Preparing(_) | SuggestedChangeState::Applying(_)
        )
        || workspace.is_opening_repository
}

impl ReviewWorkspace {
    pub fn render_shared_base(
        &self,
        cx: &Context<Self>,
        snapshot: &crate::repository::RepositorySnapshot,
    ) -> gpui::Div {
        let Some(policy) = snapshot.review_policy.as_ref().ok() else {
            return div();
        };
        let Some(source) = &policy.shared_base else {
            return div();
        };
        let entity = cx.entity();
        let clear_entity = entity.clone();
        let authenticated = self
            .github_credentials
            .as_ref()
            .is_some_and(|credentials| !credentials.access_token_is_expired());
        let mut panel = div().flex().flex_col().gap_2()
            .child("SHARED CONFIGURATION · explicit github.com read; no automatic fetch")
            .child(format!("{}/{} · commit {} · {}", source.owner, source.repository, source.commit, source.path))
            .child("Fetch uses your current GitHub credential only at api.github.com. It reads this pinned configuration, not referenced context files. Provider destinations, credentials and consent remain local. Nothing is sent to an AI provider by fetching or approving.")
            .child(Button::new("fetch-shared-base").label("Fetch pinned shared configuration for preview")
                .disabled(busy(self) || !authenticated).on_click(move |_, _, cx| fetch(&entity, cx)))
            .child(Button::new("clear-shared-base-approval").label("Clear shared-base approval (blocks AI review)")
                .disabled(busy(self)).on_click(move |_, _, cx| clear(&clear_entity, cx)));
        if !authenticated {
            panel = panel.child("Connect GitHub explicitly to read this configuration.");
        }
        if let Some(approved) = &policy.base_approval {
            panel = panel.child(format!("Approved in memory · {}", approved.fingerprint));
        } else {
            panel = panel.child("AI review is blocked until the effective shared/local policy is previewed and approved.");
        }
        match &self.shared_base_state {
            SharedBaseState::Loading(_) => {
                panel = panel.child("Fetching pinned configuration only…");
            }
            SharedBaseState::Failed(error) => {
                panel = panel.child(format!("Shared configuration blocked: {error}"));
            }
            SharedBaseState::Preview(base) => {
                if let Ok(effective) = base.resolve(snapshot) {
                    let entity = cx.entity();
                    panel = panel.child(format!(
                        "Local analysis declarations (no execution): {:?}",
                        effective.analysis
                    ));
                    panel = panel.child(format!(
                        "Effective profile: {:?} · synthesis: {} · quality threshold: {:?} (informational)",
                        effective.profile, effective.synthesis, effective.fail_on
                    ));
                    panel = panel.child(format!("EFFECTIVE PREVIEW · {}\nDiff budget {} bytes · chunk {} · minimum confidence {} · max findings {} · severities {:?} · suggestions {}\nIgnore {:?}\nFocus {:?}\nContext declarations {:?}\nProvider settings ignored: {}. Summary/unknown settings are not applied. Raw TOML is not displayed because it may contain credentials.",
                        base.fingerprint, effective.max_diff_bytes, effective.chunk_bytes, effective.minimum_confidence,
                        effective.max_findings, effective.allowed_severities, effective.suggested_changes, effective.ignore,
                        effective.context.focus, effective.context, effective.provider_ignored))
                        .child(Button::new("approve-shared-base").label("Approve this effective policy for this checkout/configuration")
                            .disabled(busy(self)).on_click(move |_, _, cx| approve(&entity, cx)));
                } else {
                    panel = panel.child(
                        "Preview no longer matches this checkout/configuration; fetch again.",
                    );
                }
            }
            SharedBaseState::Idle => {}
        }
        panel
    }
}

fn clear(entity: &Entity<ReviewWorkspace>, cx: &mut App) {
    entity.update(cx, |this, cx| {
        if busy(this) {
            return;
        }
        let Some(snapshot) = &mut this.local_repository_snapshot else {
            return;
        };
        match shared_base::unresolved_policy(snapshot) {
            Ok(policy) => {
                snapshot.review_policy = Ok(policy);
                this.shared_base_state = SharedBaseState::Idle;
                this.local_context_state = crate::local_context_ui::ContextState::Undecided;
                this.local_review_state = LocalReviewState::Idle;
                this.suggested_change_state = SuggestedChangeState::Idle;
            }
            Err(error) => this.shared_base_state = SharedBaseState::Failed(error.to_string()),
        }
        cx.notify();
    });
}

fn approve(entity: &Entity<ReviewWorkspace>, cx: &mut App) {
    entity.update(cx, |this, cx| {
        if busy(this) {
            return;
        }
        let SharedBaseState::Preview(base) = &this.shared_base_state else {
            return;
        };
        let Some(snapshot) = &mut this.local_repository_snapshot else {
            return;
        };
        match base.resolve(snapshot) {
            Ok(policy) => {
                snapshot.review_policy = Ok(policy);
                this.shared_base_state = SharedBaseState::Idle;
                this.local_context_state = crate::local_context_ui::ContextState::Undecided;
                this.local_review_state = LocalReviewState::Idle;
                this.suggested_change_state = SuggestedChangeState::Idle;
            }
            Err(error) => this.shared_base_state = SharedBaseState::Failed(error.to_string()),
        }
        cx.notify();
    });
}

fn fetch(entity: &Entity<ReviewWorkspace>, cx: &mut App) {
    let request = entity.update(cx, |this, cx| {
        if busy(this) {
            return None;
        }
        let credentials = this.github_credentials.as_ref()?;
        if credentials.access_token_is_expired() {
            return None;
        }
        let token = credentials.access_token().to_string();
        let snapshot = this.local_repository_snapshot.clone()?;
        let source = snapshot.review_policy.as_ref().ok()?.shared_base.clone()?;
        let scope = crate::local_context::scope(&snapshot).ok()?;
        this.shared_base_state = SharedBaseState::Loading(scope);
        cx.notify();
        Some((snapshot, source, token))
    });
    let Some((snapshot, source, token)) = request else {
        return;
    };
    let http = cx.http_client();
    let entity = entity.clone();
    cx.spawn(async move |cx| {
        let result = async {
            let client = crate::github_pr::GitHubPullRequestClient::new(http, token)?;
            let raw = crate::with_network_timeout(
                client.load_shared_config(&source),
                cx.background_executor(),
            )
            .await?;
            cx.background_executor()
                .spawn(async move { ApprovedBase::prepare(&snapshot, &raw) })
                .await
        }
        .await;
        let _ = entity.update(cx, |this, cx| {
            let SharedBaseState::Loading(scope) = &this.shared_base_state else {
                return;
            };
            if this
                .local_repository_snapshot
                .as_ref()
                .and_then(|snapshot| crate::local_context::scope(snapshot).ok())
                .as_ref()
                != Some(scope)
            {
                this.shared_base_state = SharedBaseState::Idle;
                cx.notify();
                return;
            }
            this.shared_base_state = match result {
                Ok(base) => SharedBaseState::Preview(Box::new(base)),
                Err(error) => SharedBaseState::Failed(error.to_string()),
            };
            cx.notify();
        });
    })
    .detach();
}
