//! Explicitly confirmed manual GitHub reviews; no background/autonomous writes.

use anyhow::{Context as _, ensure};
use gpui::{App, Context, Entity, prelude::*};
use gpui_component::{
    Disableable as _,
    button::Button,
    input::{InputState, TextInput},
};

use crate::{GitHubAuthState, RemoteReviewState, ReviewWorkspace, github_pr};

use crate::github_pr::PendingReview;
use crate::publication_preference::PublicationPreference;

impl PendingReview {
    fn matches(&self, snapshot: &github_pr::PullRequestSnapshot) -> bool {
        self.reference == snapshot.reference
            && self.base_sha == snapshot.base_sha
            && self.head_sha == snapshot.head_sha
    }
}

#[derive(Default)]
pub enum PublicationState {
    #[default]
    Drafting,
    Confirming(PendingReview),
    Publishing(PendingReview),
    Published(String),
    Failed(String),
}

#[derive(Default)]
pub struct ReviewPublication {
    pub drafts: Vec<PendingReview>,
    pub state: PublicationState,
    storage: DraftStorage,
    storage_revision: i64,
    restored_summary: String,
    preference: PreferenceState,
}

#[derive(Default)]
enum PreferenceState {
    #[default]
    Default,
    Ready(PublicationPreference),
    Saving(PublicationPreference),
}

#[derive(Default)]
enum DraftStorage {
    #[default]
    Ready,
    Saving,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommentSelection {
    Standalone,
    All,
    One(usize),
}

impl ReviewPublication {
    pub const fn restore_preference(&mut self, preference: PublicationPreference) {
        self.preference = PreferenceState::Ready(preference);
    }

    fn preference(&self) -> PublicationPreference {
        match self.preference {
            PreferenceState::Default => PublicationPreference::default(),
            PreferenceState::Ready(value) | PreferenceState::Saving(value) => value,
        }
    }

    fn preferred_preview(&self, snapshot: &github_pr::PullRequestSnapshot) -> Option<usize> {
        match self.preference() {
            PublicationPreference::Batch => None,
            PublicationPreference::Individual => {
                self.drafts.iter().position(|draft| draft.matches(snapshot))
            }
        }
    }

    fn change_preference(&mut self, preference: PublicationPreference) {
        self.preference = PreferenceState::Ready(preference);
        if matches!(self.state, PublicationState::Confirming(_)) {
            self.state = PublicationState::Drafting;
        }
    }

    pub fn restore(
        &mut self,
        result: anyhow::Result<(i64, crate::session_store::GitHubDraftQueue)>,
    ) -> anyhow::Result<()> {
        match result {
            Ok((revision, queue)) => {
                self.drafts = queue.drafts;
                self.storage_revision = revision;
                self.restored_summary = queue.summary;
                Ok(())
            }
            Err(error) => {
                self.storage = DraftStorage::Unavailable;
                Err(error)
            }
        }
    }

    pub const fn is_publishing(&self) -> bool {
        matches!(self.state, PublicationState::Publishing(_))
    }

    pub fn refreshed(&mut self) {
        if !self.is_publishing() {
            self.state = PublicationState::Drafting;
        }
    }

    fn queue(
        &mut self,
        snapshot: &github_pr::PullRequestSnapshot,
        comment: github_pr::NewReviewComment,
    ) -> anyhow::Result<()> {
        ensure!(!self.is_publishing(), "wait for publication to finish");
        ensure!(
            snapshot.state == "open",
            "comments require an open pull request"
        );
        ensure!(
            self.drafts.len() < 50,
            "the in-memory draft queue is full (50 comments)"
        );
        github_pr::validate_review_comments(&snapshot.files, std::slice::from_ref(&comment))?;
        ensure!(
            !self
                .drafts
                .iter()
                .any(|draft| draft.matches(snapshot) && draft.comments.contains(&comment)),
            "this exact comment is already queued"
        );
        self.drafts.push(PendingReview {
            reference: snapshot.reference.clone(),
            base_sha: snapshot.base_sha.clone(),
            head_sha: snapshot.head_sha.clone(),
            event: github_pr::ReviewEvent::Comment,
            body: String::new(),
            comments: vec![comment],
        });
        if !matches!(self.state, PublicationState::Failed(_)) {
            self.state = PublicationState::Drafting;
        }
        Ok(())
    }

    fn preview(
        &mut self,
        snapshot: &github_pr::PullRequestSnapshot,
        selection: CommentSelection,
        event: github_pr::ReviewEvent,
        body: String,
    ) -> anyhow::Result<()> {
        ensure!(
            !self.is_publishing() && !matches!(self.state, PublicationState::Failed(_)),
            "refresh the pull request before another publication attempt"
        );
        ensure!(snapshot.state == "open", "the pull request is closed");
        let mut comments = Vec::new();
        match selection {
            CommentSelection::Standalone => {}
            CommentSelection::All => {
                for draft in &self.drafts {
                    if draft.matches(snapshot) {
                        comments.extend(draft.comments.clone());
                    }
                }
            }
            CommentSelection::One(index) => {
                let draft = self
                    .drafts
                    .get(index)
                    .context("the selected comment draft no longer exists")?;
                ensure!(
                    draft.matches(snapshot),
                    "the selected comment belongs to a different PR or head"
                );
                comments.extend(draft.comments.clone());
            }
        }
        github_pr::validate_review_submission(event, &snapshot.files, &body, &comments)?;
        self.state = PublicationState::Confirming(PendingReview {
            reference: snapshot.reference.clone(),
            base_sha: snapshot.base_sha.clone(),
            head_sha: snapshot.head_sha.clone(),
            event,
            body,
            comments,
        });
        Ok(())
    }

    fn complete(
        &mut self,
        pending: &PendingReview,
        result: anyhow::Result<github_pr::PullRequestReview>,
    ) {
        if !matches!(&self.state, PublicationState::Publishing(active) if active == pending) {
            return;
        }
        match result {
            Ok(review) => {
                self.drafts.retain(|draft| {
                    !(!pending.comments.is_empty()
                        && draft.reference == pending.reference
                        && draft.base_sha == pending.base_sha
                        && draft.head_sha == pending.head_sha
                        && draft
                            .comments
                            .iter()
                            .all(|comment| pending.comments.contains(comment)))
                });
                self.state = PublicationState::Published(review.html_url);
            }
            Err(error) => {
                self.state = PublicationState::Failed(format!(
                    "Publication was not confirmed: {error}. Drafts kept. Refresh the PR before deciding whether to retry; GitHub may have accepted it."
                ));
            }
        }
    }
}

impl ReviewWorkspace {
    fn render_publication_preference(&self, cx: &Context<Self>) -> gpui::Div {
        let mut panel = gpui::div().flex().flex_col().gap_2().child(format!(
            "Preferred preview: {} · local only, always requires confirmation",
            self.review_publication.preference().label()
        ));
        for preference in [
            PublicationPreference::Individual,
            PublicationPreference::Batch,
        ] {
            let entity = cx.entity();
            panel = panel.child(
                Button::new(gpui::SharedString::from(format!(
                    "prefer-{}",
                    preference.as_str()
                )))
                .label(format!("Remember preference: {}", preference.label()))
                .disabled(
                    self.session_store.is_none()
                        || self.review_publication.is_publishing()
                        || matches!(
                            self.review_publication.preference,
                            PreferenceState::Saving(_)
                        ),
                )
                .on_click(move |_, _, cx| save_preference(&entity, preference, cx)),
            );
        }
        if self.review_publication.preference() == PublicationPreference::Individual {
            let entity = cx.entity();
            panel = panel.child(
                Button::new("preview-all-despite-preference")
                    .label("Preview all queued comments instead")
                    .disabled(self.review_publication.is_publishing())
                    .on_click(move |_, _, cx| {
                        preview(
                            &entity,
                            CommentSelection::All,
                            github_pr::ReviewEvent::Comment,
                            cx,
                        );
                    }),
            );
        }
        panel
    }

    pub fn ensure_review_comment_input(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        if self.remote_review_summary_input.is_none() {
            self.remote_review_summary_input = Some(cx.new(|input_cx| {
                InputState::new(window, input_cx)
                    .multi_line()
                    .rows(3)
                    .placeholder("Write the summary sent with your COMMENT review…")
                    .default_value(self.review_publication.restored_summary.clone())
            }));
        }
        if self.remote_comment_input.is_none() {
            self.remote_comment_input = Some(cx.new(|input_cx| {
                InputState::new(window, input_cx)
                    .multi_line()
                    .rows(4)
                    .placeholder("Write a new inline comment (not a thread reply)…")
            }));
        }
    }

    // One panel presents the related queue, event choices, and exact preview actions.
    #[allow(clippy::too_many_lines)]
    pub fn render_comment_review(
        &self,
        cx: &Context<Self>,
        snapshot: &github_pr::PullRequestSnapshot,
        path: &str,
        line: Option<usize>,
    ) -> gpui::Div {
        let mut content = gpui::div()
            .flex()
            .flex_col()
            .gap_2()
            .child("New inline comments · local queue, not yet published")
            .child("Drafts stay in memory until explicitly saved locally. Saving never publishes.");
        let Some(input) = &self.remote_comment_input else {
            return content;
        };
        content = content.child(TextInput::new(input));
        let entity = cx.entity();
        let input = input.clone();
        let path = path.to_string();
        let line = line.and_then(|line| u32::try_from(line).ok());
        content = content.child(
            Button::new("queue-review-comment")
                .label("Queue comment at selected new-side line")
                .disabled(
                    line.is_none()
                        || snapshot.state != "open"
                        || self.remote_commit_state.is_busy()
                        || self.review_publication.is_publishing(),
                )
                .on_click(move |_event, window, cx| {
                    let body = input.read(cx).value().to_string();
                    entity.update(cx, |this, cx| {
                        let RemoteReviewState::Loaded(snapshot) = &this.remote_review_state else {
                            return;
                        };
                        let Some(line) = line else {
                            return;
                        };
                        let comment = github_pr::NewReviewComment {
                            path: path.clone(),
                            line,
                            body,
                        };
                        match this.review_publication.queue(snapshot, comment) {
                            Ok(()) => {
                                input.update(cx, |input, cx| input.set_value("", window, cx));
                            }
                            Err(error) => this.notice = Some(error.to_string().into()),
                        }
                        cx.notify();
                    });
                }),
        );
        for (index, draft) in self.review_publication.drafts.iter().enumerate() {
            content = content.child(self.render_queued_comment(cx, snapshot, index, draft));
        }
        let entity = cx.entity();
        let preferred = self.review_publication.preferred_preview(snapshot);
        let preference = self.review_publication.preference();
        content = content.child(self.render_publication_preference(cx));
        content = content.child(
            Button::new("preview-comment-review")
                .label(match preference {
                    PublicationPreference::Individual => "Preview next comment for this PR/head",
                    PublicationPreference::Batch => "Preview queued review for this PR/head",
                })
                .disabled(
                    self.review_publication.is_publishing()
                        || self.remote_commit_state.is_busy()
                        || (preference == PublicationPreference::Individual && preferred.is_none()),
                )
                .on_click(move |_event, _window, cx| {
                    if preference == PublicationPreference::Individual && preferred.is_none() {
                        return;
                    }
                    let selection = preferred.map_or(CommentSelection::All, CommentSelection::One);
                    preview(&entity, selection, github_pr::ReviewEvent::Comment, cx);
                }),
        );
        let entity = cx.entity();
        content = content.child(
            Button::new("preview-request-changes-standalone")
                .label("Preview standalone REQUEST_CHANGES")
                .disabled(
                    self.review_publication.is_publishing()
                        || self.remote_commit_state.is_busy()
                        || snapshot.state != "open",
                )
                .on_click(move |_event, _window, cx| {
                    preview(
                        &entity,
                        CommentSelection::Standalone,
                        github_pr::ReviewEvent::RequestChanges,
                        cx,
                    );
                }),
        );
        let entity = cx.entity();
        let has_current_comments = self
            .review_publication
            .drafts
            .iter()
            .any(|draft| draft.matches(snapshot));
        content = content.child(
            Button::new("preview-request-changes-with-comments")
                .label("Preview REQUEST_CHANGES with queued inline comments")
                .disabled(
                    self.review_publication.is_publishing()
                        || self.remote_commit_state.is_busy()
                        || snapshot.state != "open"
                        || !has_current_comments,
                )
                .on_click(move |_event, _window, cx| {
                    preview(
                        &entity,
                        CommentSelection::All,
                        github_pr::ReviewEvent::RequestChanges,
                        cx,
                    );
                }),
        );
        content
            .child("Review summary (required for COMMENT and REQUEST_CHANGES)")
            .children(
                self.remote_review_summary_input
                    .as_ref()
                    .map(TextInput::new),
            )
            .child(self.render_save_github_drafts(cx))
            .child(self.render_review_confirmation(cx))
    }

    fn render_save_github_drafts(&self, cx: &Context<Self>) -> gpui::Div {
        let entity = cx.entity();
        let saving = matches!(self.review_publication.storage, DraftStorage::Saving);
        gpui::div().child(
            Button::new("save-github-drafts")
                .label(if saving {
                    "Saving GitHub drafts…"
                } else {
                    "Save queued comments and summary locally"
                })
                .disabled(
                    self.session_store.is_none()
                        || saving
                        || self.review_publication.is_publishing()
                        || matches!(self.review_publication.storage, DraftStorage::Unavailable),
                )
                .on_click(move |_event, _window, cx| save_drafts(&entity, cx)),
        )
    }

    fn render_queued_comment(
        &self,
        cx: &Context<Self>,
        snapshot: &github_pr::PullRequestSnapshot,
        index: usize,
        draft: &PendingReview,
    ) -> gpui::Div {
        let comment = &draft.comments[0];
        let entity = cx.entity();
        let preview_entity = entity.clone();
        let changes_preview_entity = entity.clone();
        gpui::div()
            .flex()
            .flex_col()
            .gap_2()
            .child(format!(
                "{}/{}#{} · {} · {}:{}{}\n{}",
                draft.reference.owner,
                draft.reference.repository,
                draft.reference.number,
                crate::short_sha(&draft.head_sha),
                comment.path,
                comment.line,
                if draft.matches(snapshot) {
                    ""
                } else {
                    " · stale/other PR (not publishable here)"
                },
                comment.body
            ))
            .child(
                Button::new(("preview-one-comment", index))
                    .label("Preview this comment")
                    .disabled(!draft.matches(snapshot) || self.review_publication.is_publishing())
                    .on_click(move |_event, _window, cx| {
                        preview(
                            &preview_entity,
                            CommentSelection::One(index),
                            github_pr::ReviewEvent::Comment,
                            cx,
                        );
                    }),
            )
            .child(
                Button::new(("preview-one-request-changes", index))
                    .label("Preview this comment as REQUEST_CHANGES")
                    .disabled(!draft.matches(snapshot) || self.review_publication.is_publishing())
                    .on_click(move |_event, _window, cx| {
                        preview(
                            &changes_preview_entity,
                            CommentSelection::One(index),
                            github_pr::ReviewEvent::RequestChanges,
                            cx,
                        );
                    }),
            )
            .child(
                Button::new(("remove-queued-comment", index))
                    .label("Remove draft")
                    .disabled(self.review_publication.is_publishing())
                    .on_click(move |_event, _window, cx| {
                        entity.update(cx, |this, cx| {
                            if !this.review_publication.is_publishing()
                                && index < this.review_publication.drafts.len()
                            {
                                this.review_publication.drafts.remove(index);
                                if !matches!(
                                    this.review_publication.state,
                                    PublicationState::Failed(_)
                                ) {
                                    this.review_publication.state = PublicationState::Drafting;
                                }
                                cx.notify();
                            }
                        });
                    }),
            )
    }

    fn render_review_confirmation(&self, cx: &Context<Self>) -> gpui::Div {
        let mut content = gpui::div().flex().flex_col().gap_2();
        match &self.review_publication.state {
            PublicationState::Drafting => {}
            PublicationState::Publishing(_) => {
                content = content.child("Publishing one GitHub review…");
            }
            PublicationState::Published(url) => {
                content = content.child(format!("Review published: {url}"));
            }
            PublicationState::Failed(message) => content = content.child(message.clone()),
            PublicationState::Confirming(pending) => {
                content = content.child(format!(
                    "Confirm {} review on https://github.com/{}/{}/pull/{}\nBase: {}\nHead: {}",
                    pending.event.label(),
                    pending.reference.owner,
                    pending.reference.repository,
                    pending.reference.number,
                    pending.base_sha,
                    pending.head_sha
                ));
                for comment in &pending.comments {
                    content = content.child(format!(
                        "{}:{} RIGHT\n{}",
                        comment.path, comment.line, comment.body
                    ));
                }
                let entity = cx.entity();
                let cancel_entity = entity.clone();
                content = content.child(format!("Review summary:\n{}", pending.body));
                content = content
                    .child(
                        Button::new("confirm-comment-review")
                            .label(format!("Confirm and publish {}", pending.event.label()))
                            .disabled(self.remote_commit_state.is_busy())
                            .on_click(move |_event, _window, cx| publish(&entity, cx)),
                    )
                    .child(
                        Button::new("cancel-comment-review")
                            .label("Cancel preview (keep drafts)")
                            .on_click(move |_event, _window, cx| {
                                cancel_entity.update(cx, |this, cx| {
                                    this.review_publication.state = PublicationState::Drafting;
                                    cx.notify();
                                });
                            }),
                    );
            }
        }
        content
    }
}

fn save_preference(
    entity: &Entity<ReviewWorkspace>,
    preference: PublicationPreference,
    cx: &mut App,
) {
    let store = entity.update(cx, |this, cx| {
        if this.review_publication.is_publishing()
            || this.remote_commit_state.is_busy()
            || matches!(
                this.review_publication.preference,
                PreferenceState::Saving(_)
            )
        {
            return None;
        }
        let store = this.session_store.clone()?;
        let previous = this.review_publication.preference();
        this.review_publication.change_preference(previous);
        this.review_publication.preference = PreferenceState::Saving(previous);
        cx.notify();
        Some(store)
    });
    let Some(store) = store else {
        return;
    };
    let entity = entity.clone();
    cx.spawn(async move |cx| {
        let result = cx.background_executor().spawn(async move { store.save_publication_preference(preference) }).await;
        let _ = entity.update(cx, |this, cx| {
            let previous = this.review_publication.preference();
            match result {
                Ok(()) => {
                    this.review_publication.change_preference(preference);
                    this.notice = Some("Publication preference saved locally. Drafts were preserved; prepare a new preview before publishing.".into());
                },
                Err(error) => {
                    this.review_publication.restore_preference(previous);
                    this.notice = Some(format!("Could not save publication preference: {error}").into());
                },
            }
            cx.notify();
        });
    }).detach();
}

fn preview(
    entity: &Entity<ReviewWorkspace>,
    selection: CommentSelection,
    event: github_pr::ReviewEvent,
    cx: &mut App,
) {
    entity.update(cx, |this, cx| {
        if this.remote_commit_state.is_busy() {
            this.notice = Some(
                "Wait for the confirmed remote commit operation before preparing another GitHub write."
                    .into(),
            );
            cx.notify();
            return;
        }
        let RemoteReviewState::Loaded(snapshot) = &this.remote_review_state else {
            return;
        };
        let body = this
            .remote_review_summary_input
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default();
        if let Err(error) = this
            .review_publication
            .preview(snapshot, selection, event, body)
        {
            this.notice = Some(error.to_string().into());
        }
        cx.notify();
    });
}

fn save_drafts(entity: &Entity<ReviewWorkspace>, cx: &mut App) {
    let request = entity.update(cx, |this, cx| {
        let store = this.session_store.clone()?;
        if !matches!(this.review_publication.storage, DraftStorage::Ready)
            || this.review_publication.is_publishing()
        {
            return None;
        }
        let queue = crate::session_store::GitHubDraftQueue {
            drafts: this.review_publication.drafts.clone(),
            summary: this
                .remote_review_summary_input
                .as_ref()
                .map(|input| input.read(cx).value().to_string())
                .unwrap_or_default(),
        };
        let revision = this.review_publication.storage_revision;
        this.review_publication.storage = DraftStorage::Saving;
        cx.notify();
        Some((store, revision, queue))
    });
    let Some((store, revision, queue)) = request else {
        return;
    };
    let entity = entity.clone();
    cx.spawn(async move |cx| {
        let result = cx.background_executor().spawn(async move { store.save_github_drafts(revision, &queue) }).await;
        let _ = entity.update(cx, |this, cx| {
            this.review_publication.storage = DraftStorage::Ready;
            match result {
                Ok(revision) => {
                    this.review_publication.storage_revision = revision;
                    this.notice = Some("GitHub drafts and summary saved locally. Later edits/removals require another save. No comment was published.".into());
                }
                Err(error) => this.notice = Some(format!("Drafts were NOT saved; in-memory edits kept: {error}").into()),
            }
            cx.notify();
        });
    }).detach();
}

fn publish(entity: &Entity<ReviewWorkspace>, cx: &mut App) {
    let Some((pending, token)) = entity.update(cx, |this, cx| {
        let PublicationState::Confirming(pending) = &this.review_publication.state else {
            return None;
        };
        let RemoteReviewState::Loaded(snapshot) = &this.remote_review_state else {
            return None;
        };
        if !pending.matches(snapshot)
            || snapshot.state != "open"
            || !matches!(this.github_auth_state, GitHubAuthState::SignedIn)
            || this.remote_commit_state.is_busy()
            || matches!(
                this.thread_reply_state,
                crate::ThreadReplyState::Publishing(_)
            )
        {
            this.notice = Some(
                "The preview is stale or another publication is active; refresh first.".into(),
            );
            cx.notify();
            return None;
        }
        let credentials = this.github_credentials.as_ref()?;
        if credentials.access_token_is_expired() {
            return None;
        }
        let pending = pending.clone();
        let token = credentials.access_token().to_string();
        this.review_publication.state = PublicationState::Publishing(pending.clone());
        cx.notify();
        Some((pending, token))
    }) else {
        return;
    };
    let http = cx.http_client();
    let entity = entity.clone();
    cx.spawn(async move |cx| {
        let result = async {
            let client = github_pr::GitHubPullRequestClient::new(http, token)?;
            crate::with_network_timeout(
                client.publish_review(
                    &pending.reference,
                    &pending.base_sha,
                    &pending.head_sha,
                    pending.event,
                    &pending.body,
                    &pending.comments,
                ),
                cx.background_executor(),
            )
            .await
            .context("review write outcome is uncertain")
        }
        .await;
        let _ = entity.update(cx, |this, cx| {
            this.capture_action(crate::action_history::review(&pending, &result));
            this.review_publication.complete(&pending, result);
            cx.notify();
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> github_pr::PullRequestSnapshot {
        github_pr::PullRequestSnapshot {
            reference: github_pr::PullRequestRef::parse("octo/repo#1").unwrap(),
            title: "Review".to_string(),
            state: "open".to_string(),
            author: "author".to_string(),
            html_url: "https://github.com/octo/repo/pull/1".to_string(),
            base_sha: "b".repeat(40),
            head_sha: "a".repeat(40),
            head_repository: None,
            head_branch: None,
            files: cururu_core::parse_unified_diff(
                "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1,2 @@\n-old\n+new\n+extra\n",
            ),
            reviews: vec![],
            review_threads: vec![],
            conversation_comments: vec![],
        }
    }

    fn comment(line: u32) -> github_pr::NewReviewComment {
        github_pr::NewReviewComment {
            path: "a.rs".to_string(),
            line,
            body: format!("Check line {line}"),
        }
    }

    #[test]
    fn changing_preference_preserves_drafts_and_requires_another_confirmation() {
        let snapshot = snapshot();
        let mut publication = ReviewPublication::default();
        publication.queue(&snapshot, comment(1)).unwrap();
        publication.queue(&snapshot, comment(2)).unwrap();
        let drafts = publication.drafts.clone();
        assert_eq!(publication.preferred_preview(&snapshot), None);
        publication
            .preview(
                &snapshot,
                CommentSelection::All,
                github_pr::ReviewEvent::Comment,
                "Review notes".to_string(),
            )
            .unwrap();
        publication.change_preference(PublicationPreference::Individual);
        assert_eq!(publication.preferred_preview(&snapshot), Some(0));
        assert!(matches!(publication.state, PublicationState::Drafting));
        assert_eq!(publication.drafts, drafts);
        publication
            .preview(
                &snapshot,
                publication
                    .preferred_preview(&snapshot)
                    .map_or(CommentSelection::All, CommentSelection::One),
                github_pr::ReviewEvent::Comment,
                "Review notes".to_string(),
            )
            .unwrap();
        let PublicationState::Confirming(pending) = &publication.state else {
            panic!("preview");
        };
        assert_eq!(pending.comments.len(), 1);
        publication.change_preference(PublicationPreference::Batch);
        assert!(matches!(publication.state, PublicationState::Drafting));
        assert_eq!(publication.drafts, drafts);
        publication.state = PublicationState::Failed("refresh required".to_string());
        publication.change_preference(PublicationPreference::Individual);
        assert!(matches!(publication.state, PublicationState::Failed(_)));
        let mut other_head = snapshot;
        other_head.head_sha = "c".repeat(40);
        assert_eq!(publication.preferred_preview(&other_head), None);
    }

    #[test]
    fn restoring_saved_drafts_does_not_create_a_publication_confirmation() {
        let snapshot = snapshot();
        let mut publication = ReviewPublication::default();
        publication.queue(&snapshot, comment(1)).unwrap();
        let queue = crate::session_store::GitHubDraftQueue {
            drafts: publication.drafts.clone(),
            summary: "Saved summary".to_string(),
        };
        let mut restored = ReviewPublication::default();
        restored.restore(Ok((7, queue))).unwrap();
        assert_eq!(restored.drafts, publication.drafts);
        assert_eq!(restored.restored_summary, "Saved summary");
        assert_eq!(restored.storage_revision, 7);
        assert!(matches!(restored.state, PublicationState::Drafting));
        assert!(!restored.is_publishing());
        assert!(
            restored
                .restore(Err(anyhow::anyhow!("invalid saved data")))
                .is_err()
        );
        assert!(matches!(restored.storage, DraftStorage::Unavailable));
        assert_eq!(restored.drafts.len(), 1);
    }

    #[test]
    fn individual_and_batch_previews_preserve_drafts_and_exact_bodies() {
        let snapshot = snapshot();
        let mut publication = ReviewPublication::default();
        publication.queue(&snapshot, comment(1)).unwrap();
        publication.queue(&snapshot, comment(2)).unwrap();
        assert!(publication.queue(&snapshot, comment(1)).is_err());
        publication
            .preview(
                &snapshot,
                CommentSelection::One(1),
                github_pr::ReviewEvent::Comment,
                "Review notes".to_string(),
            )
            .unwrap();
        let PublicationState::Confirming(pending) = &publication.state else {
            panic!("preview missing")
        };
        assert_eq!(pending.comments, vec![comment(2)]);
        publication
            .preview(
                &snapshot,
                CommentSelection::All,
                github_pr::ReviewEvent::Comment,
                "Review notes".to_string(),
            )
            .unwrap();
        let PublicationState::Confirming(pending) = &publication.state else {
            panic!("preview missing")
        };
        assert_eq!(pending.comments, vec![comment(1), comment(2)]);
        assert_eq!(publication.drafts.len(), 2);
        assert!(!publication.is_publishing());
    }

    #[test]
    fn request_changes_preview_supports_standalone_and_queued_comments() {
        let snapshot = snapshot();
        let mut publication = ReviewPublication::default();
        publication
            .preview(
                &snapshot,
                CommentSelection::Standalone,
                github_pr::ReviewEvent::RequestChanges,
                "Please address the blocking issue.".to_string(),
            )
            .unwrap();
        let PublicationState::Confirming(pending) = &publication.state else {
            panic!("standalone request-changes preview missing")
        };
        assert_eq!(pending.event, github_pr::ReviewEvent::RequestChanges);
        assert_eq!(pending.comments.len(), 0);
        assert_eq!(pending.body, "Please address the blocking issue.");

        publication.queue(&snapshot, comment(1)).unwrap();
        publication.queue(&snapshot, comment(2)).unwrap();
        publication
            .preview(
                &snapshot,
                CommentSelection::One(1),
                github_pr::ReviewEvent::RequestChanges,
                "Please address this line before merge.".to_string(),
            )
            .unwrap();
        let PublicationState::Confirming(pending) = &publication.state else {
            panic!("request-changes preview with comment missing")
        };
        assert_eq!(pending.event, github_pr::ReviewEvent::RequestChanges);
        assert_eq!(pending.comments, vec![comment(2)]);
        assert_eq!(publication.drafts.len(), 2);
    }

    #[test]
    fn standalone_request_changes_requires_a_summary_and_keeps_queued_comments() {
        let snapshot = snapshot();
        let mut publication = ReviewPublication::default();
        publication.queue(&snapshot, comment(1)).unwrap();
        assert!(
            publication
                .preview(
                    &snapshot,
                    CommentSelection::Standalone,
                    github_pr::ReviewEvent::RequestChanges,
                    "  ".to_string(),
                )
                .is_err()
        );
        assert_eq!(publication.drafts.len(), 1);
    }

    #[test]
    fn stale_or_different_pr_drafts_are_retained_but_cannot_be_published() {
        let mut snapshot = snapshot();
        let mut publication = ReviewPublication::default();
        publication.queue(&snapshot, comment(1)).unwrap();
        snapshot.head_sha = "c".repeat(40);
        assert!(
            publication
                .preview(
                    &snapshot,
                    CommentSelection::All,
                    github_pr::ReviewEvent::Comment,
                    "Review notes".to_string(),
                )
                .is_err()
        );
        snapshot.head_sha = "a".repeat(40);
        snapshot.reference.number = 2;
        assert!(
            publication
                .preview(
                    &snapshot,
                    CommentSelection::One(0),
                    github_pr::ReviewEvent::Comment,
                    "Review notes".to_string(),
                )
                .is_err()
        );
        assert_eq!(publication.drafts.len(), 1);
    }

    #[test]
    fn uncertain_publication_preserves_queue_and_requires_refresh_before_retry() {
        let snapshot = snapshot();
        let mut publication = ReviewPublication::default();
        publication.queue(&snapshot, comment(1)).unwrap();
        publication
            .preview(
                &snapshot,
                CommentSelection::All,
                github_pr::ReviewEvent::Comment,
                "Review notes".to_string(),
            )
            .unwrap();
        let PublicationState::Confirming(pending) = &publication.state else {
            panic!("preview missing")
        };
        let pending = pending.clone();
        publication.state = PublicationState::Publishing(pending.clone());
        publication.complete(&pending, Err(anyhow::anyhow!("connection lost")));
        assert_eq!(publication.drafts.len(), 1);
        assert!(
            publication
                .preview(
                    &snapshot,
                    CommentSelection::All,
                    github_pr::ReviewEvent::Comment,
                    "Review notes".to_string(),
                )
                .is_err()
        );
        publication.refreshed();
        assert!(
            publication
                .preview(
                    &snapshot,
                    CommentSelection::All,
                    github_pr::ReviewEvent::Comment,
                    "Review notes".to_string(),
                )
                .is_ok()
        );
    }

    #[test]
    fn successful_individual_review_removes_only_the_submitted_draft() {
        let snapshot = snapshot();
        let mut publication = ReviewPublication::default();
        publication.queue(&snapshot, comment(1)).unwrap();
        publication.queue(&snapshot, comment(2)).unwrap();
        publication
            .preview(
                &snapshot,
                CommentSelection::One(0),
                github_pr::ReviewEvent::Comment,
                "Review notes".to_string(),
            )
            .unwrap();
        let PublicationState::Confirming(pending) = &publication.state else {
            panic!("preview missing")
        };
        let pending = pending.clone();
        publication.state = PublicationState::Publishing(pending.clone());
        assert!(publication.queue(&snapshot, comment(2)).is_err());
        publication.complete(
            &pending,
            Ok(github_pr::PullRequestReview {
                id: 1,
                state: "COMMENTED".to_string(),
                body: String::new(),
                author: "reviewer".to_string(),
                commit_id: Some(snapshot.head_sha),
                submitted_at: None,
                html_url: "https://github.com/octo/repo/pull/1#pullrequestreview-1".to_string(),
            }),
        );
        assert_eq!(publication.drafts.len(), 1);
        assert_eq!(publication.drafts[0].comments, vec![comment(2)]);
        assert!(matches!(publication.state, PublicationState::Published(_)));
    }
}
