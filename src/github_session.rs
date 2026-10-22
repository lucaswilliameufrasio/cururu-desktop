//! Session-only `gh` fallback UI. Device Flow remains the primary path.

use gpui::{App, Context, Entity};
use gpui_component::{Disableable as _, button::Button};

use crate::{GitHubAuthState, ReviewWorkspace, gh_cli, github_auth::SessionCredentials};

impl ReviewWorkspace {
    pub fn clear_gh_session(&mut self) -> bool {
        if !self
            .github_credentials
            .as_ref()
            .is_some_and(SessionCredentials::is_gh_cli)
        {
            return false;
        }
        self.github_credentials = None;
        self.github_auth_state = GitHubAuthState::SignedOut;
        self.notice = Some(
            "Cururu disconnected from gh. The gh login and OS credential store were not changed."
                .into(),
        );
        true
    }

    pub fn render_gh_fallback_button(&self, cx: &Context<Self>) -> Button {
        let entity = cx.entity();
        Button::new("use-existing-gh-session")
            .label("Use existing gh login (memory only)")
            .disabled(!can_use_gh(self))
            .on_click(move |_event, _window, cx| start_gh_session(&entity, cx))
    }
}

const fn can_use_gh(workspace: &ReviewWorkspace) -> bool {
    matches!(
        workspace.github_auth_state,
        GitHubAuthState::SignedOut
            | GitHubAuthState::Failed(_)
            | GitHubAuthState::ReauthorizationRequired
    ) && !workspace.review_publication.is_publishing()
        && !workspace.remote_commit_state.is_busy()
        && !matches!(
            workspace.thread_reply_state,
            crate::ThreadReplyState::Publishing(_)
        )
}

fn finish_gh_session(workspace: &mut ReviewWorkspace, result: anyhow::Result<gh_cli::GhToken>) {
    if !matches!(
        workspace.github_auth_state,
        GitHubAuthState::ReadingGhSession
    ) {
        return;
    }
    match result {
        Ok(token) => {
            workspace.github_credentials = Some(SessionCredentials::GhCli(token));
            workspace.github_auth_state = GitHubAuthState::SignedIn;
            workspace.notice = Some("Using gh's existing github.com token in memory only. It was not saved in Cururu; publication still needs confirmation.".into());
        }
        Err(error) => {
            workspace.github_auth_state =
                GitHubAuthState::Failed(format!("gh session unavailable: {error}").into());
        }
    }
}

fn start_gh_session(entity: &Entity<ReviewWorkspace>, cx: &mut App) {
    if !entity.update(cx, |this, cx| {
        if !can_use_gh(this) {
            return false;
        }
        this.github_auth_state = GitHubAuthState::ReadingGhSession;
        this.github_credentials = None;
        cx.notify();
        true
    }) {
        return;
    }
    let entity = entity.clone();
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async { gh_cli::read_existing_token() })
            .await;
        let _ = entity.update(cx, |this, cx| {
            finish_gh_session(this, result);
            cx.notify();
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_is_not_automatic_and_rejects_stale_completion() {
        let mut workspace = ReviewWorkspace::sample();
        workspace.github_auth_state = GitHubAuthState::SignedOut;
        assert!(can_use_gh(&workspace));
        finish_gh_session(
            &mut workspace,
            Ok(gh_cli::validate_token(b"synthetic-token".to_vec()).unwrap()),
        );
        assert!(workspace.github_credentials.is_none());
        workspace.github_auth_state = GitHubAuthState::ReadingGhSession;
        assert!(!can_use_gh(&workspace));
        finish_gh_session(
            &mut workspace,
            Ok(gh_cli::validate_token(b"synthetic-token".to_vec()).unwrap()),
        );
        let session = workspace.github_credentials.as_ref().unwrap();
        assert!(session.is_gh_cli());
        assert!(session.device_flow_credentials().is_none());
        assert!(!format!("{session:?}").contains("synthetic-token"));
        assert!(!can_use_gh(&workspace));
        assert!(workspace.clear_gh_session());
        assert!(workspace.github_credentials.is_none());
        assert!(matches!(
            workspace.github_auth_state,
            GitHubAuthState::SignedOut
        ));
        assert!(!workspace.clear_gh_session());
    }
}
