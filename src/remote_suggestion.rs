//! GitHub suggestion import into an explicitly selected local checkout.
//! No stage, commit, push or remote mutation is performed.

use crate::{
    RemoteReviewState, RemoteSourceContextState, ReviewWorkspace,
    github_pr::{self, PullRequestComment, PullRequestSnapshot},
    suggested_change,
};
use anyhow::{Context as _, ensure};
use gpui::{App, Context, Entity, ParentElement as _, PathPromptOptions, Styled as _, div};
use gpui_component::{Disableable as _, button::Button};
use std::sync::Arc;

#[derive(Clone)]
pub struct PendingSuggestion {
    reference: github_pr::PullRequestRef,
    base_sha: String,
    head_sha: String,
    comment: PullRequestComment,
    preview: suggested_change::Preview,
}

#[derive(Default)]
pub enum RemoteSuggestionState {
    #[default]
    Idle,
    Preparing,
    Confirming(Box<PendingSuggestion>),
    Applying,
    Applied(String),
    Failed(String),
}

impl RemoteSuggestionState {
    pub const fn is_busy(&self) -> bool {
        matches!(self, Self::Preparing | Self::Applying)
    }
}

pub fn parse_suggestion(body: &str) -> anyhow::Result<String> {
    ensure!(
        body.len() <= 64_000,
        "suggestion comment exceeds the import limit"
    );
    let lines: Vec<&str> = body.lines().collect();
    let starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| (*line == "```suggestion").then_some(index))
        .collect();
    ensure!(
        starts.len() == 1,
        "import requires exactly one plain suggestion block"
    );
    let start = starts[0];
    ensure!(
        lines.get(start + 2) == Some(&"```"),
        "only a single-line suggestion without offsets can be imported"
    );
    for (index, line) in lines.iter().enumerate() {
        ensure!(
            index == start
                || index == start + 2
                || (!line.trim_start().starts_with("```") && !line.trim_start().starts_with("~~~")),
            "additional code fences make the suggestion ambiguous"
        );
    }
    let replacement = lines
        .get(start + 1)
        .context("suggestion replacement is missing")?;
    ensure!(
        !replacement.trim().is_empty()
            && replacement.len() <= 10_000
            && !replacement
                .chars()
                .any(|character| character.is_control() && character != '\t'),
        "suggestion replacement is empty, invalid or oversized"
    );
    Ok((*replacement).to_string())
}

pub fn validate_comment(
    snapshot: &PullRequestSnapshot,
    comment: &PullRequestComment,
) -> anyhow::Result<u32> {
    ensure!(snapshot.state == "open", "pull request must be open");
    ensure!(
        !comment.is_outdated
            && comment.side.as_deref() == Some("RIGHT")
            && comment.original_commit_id == snapshot.head_sha
            && comment.commit_id == snapshot.head_sha,
        "only current-head, right-side suggestions can be imported; refresh stale comments first"
    );
    let line = comment
        .line
        .context("suggestion has no current line anchor")?;
    ensure!(
        cururu_core::is_valid_anchor(&snapshot.files, &comment.path, line),
        "suggestion anchor is outside the current PR diff"
    );
    let canonical = snapshot
        .review_threads
        .iter()
        .flat_map(|thread| std::iter::once(&thread.root).chain(&thread.replies))
        .find(|current| current.id == comment.id)
        .context("suggestion comment is missing from the current thread")?;
    ensure!(
        canonical == comment,
        "suggestion comment changed; refresh and prepare it again"
    );
    parse_suggestion(&comment.body)?;
    Ok(line)
}

impl PendingSuggestion {
    fn revalidate(&self, snapshot: &PullRequestSnapshot) -> anyhow::Result<()> {
        ensure!(
            snapshot.reference == self.reference
                && snapshot.base_sha == self.base_sha
                && snapshot.head_sha == self.head_sha,
            "PR revision changed after suggestion preview"
        );
        validate_comment(snapshot, &self.comment)?;
        Ok(())
    }
}

impl ReviewWorkspace {
    pub fn render_remote_suggestion(
        &self,
        cx: &Context<Self>,
        snapshot: &PullRequestSnapshot,
    ) -> gpui::Div {
        let mut panel = div().flex().flex_col().gap_2();
        let busy = self.remote_suggestion_state.is_busy() || self.remote_commit_state.is_busy();
        if let Some(comment) = &self.selected_remote_comment
            && parse_suggestion(&comment.body).is_ok()
        {
            let entity = cx.entity();
            let enabled = validate_comment(snapshot, comment).is_ok()
                && matches!(&self.remote_source_context, RemoteSourceContextState::Loaded { comment_id, source, .. }
                    if *comment_id == comment.id && source.commit_sha == snapshot.head_sha);
            panel = panel.child("GITHUB SUGGESTION · local file change only; no stage, commit or push")
                .child("Choose a checkout already at this PR head. Its target file must exactly match the loaded GitHub source. Applying revalidates the PR/comment and the local preview.")
                .child(Button::new("choose-suggestion-checkout").label("Choose checkout and preview suggestion")
                    .disabled(busy || !enabled).on_click(move |_, _, cx| prepare(&entity, cx)));
            if !enabled {
                panel = panel.child(
                    "Import blocked: load a current-head right-side comment and its source first.",
                );
            }
            let reference = snapshot.reference.clone();
            let comment_id = comment.id;
            panel = panel.child(
                Button::new("view-github-suggestion")
                    .label("Open suggestion on GitHub (external application/commit flow)")
                    .on_click(move |_, _, cx| {
                        cx.open_url(&format!(
                            "https://github.com/{}/{}/pull/{}#discussion_r{}",
                            reference.owner, reference.repository, reference.number, comment_id
                        ));
                    }),
            );
        }
        match &self.remote_suggestion_state {
            RemoteSuggestionState::Confirming(pending) => {
                let entity = cx.entity();
                let cancel = entity.clone();
                let matches = pending.revalidate(snapshot).is_ok()
                    && self.selected_remote_comment.as_ref() == Some(&pending.comment);
                panel = panel
                    .child(format!(
                        "LOCAL PREVIEW · {} · {}:{}\n{}",
                        pending.preview.repository_root().display(),
                        pending.preview.path,
                        pending.preview.line,
                        pending.preview.diff
                    ))
                    .child(
                        Button::new("confirm-imported-suggestion")
                            .label("Apply this exact preview locally (no commit/push)")
                            .disabled(!matches)
                            .on_click(move |_, _, cx| apply(&entity, cx)),
                    )
                    .child(
                        Button::new("cancel-imported-suggestion")
                            .label("Cancel local suggestion preview")
                            .on_click(move |_, _, cx| {
                                cancel.update(cx, |this, cx| {
                                    this.remote_suggestion_state = RemoteSuggestionState::Idle;
                                    cx.notify();
                                });
                            }),
                    );
            }
            RemoteSuggestionState::Preparing => {
                panel = panel.child("Choosing/preparing local checkout; no write yet.");
            }
            RemoteSuggestionState::Applying => {
                panel =
                    panel.child("Revalidating GitHub and applying the confirmed local preview…");
            }
            RemoteSuggestionState::Applied(message) | RemoteSuggestionState::Failed(message) => {
                panel = panel.child(message.clone());
            }
            RemoteSuggestionState::Idle => {}
        }
        panel
    }
}

fn prepare(entity: &Entity<ReviewWorkspace>, cx: &mut App) {
    let request = entity.update(cx, |this, cx| {
        if this.remote_suggestion_state.is_busy()
            || this.remote_commit_state.is_busy()
            || this.is_opening_repository
        {
            return None;
        }
        let RemoteReviewState::Loaded(snapshot) = &this.remote_review_state else {
            return None;
        };
        let comment = this.selected_remote_comment.clone()?;
        let line = validate_comment(snapshot, &comment).ok()?;
        let RemoteSourceContextState::Loaded {
            comment_id, source, ..
        } = &this.remote_source_context
        else {
            return None;
        };
        if *comment_id != comment.id
            || source.commit_sha != snapshot.head_sha
            || source.path != comment.path
        {
            return None;
        }
        let request = (snapshot.clone(), comment, source.clone(), line);
        this.remote_suggestion_state = RemoteSuggestionState::Preparing;
        cx.notify();
        Some(request)
    });
    let Some((snapshot, comment, source, line)) = request else {
        return;
    };
    let picker = cx.prompt_for_paths(PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some("Choose checkout at the PR head for this local suggestion".into()),
    });
    let entity = entity.clone();
    cx.spawn(async move |cx| {
        let selected = match picker.await {
            Ok(Ok(Some(paths))) => paths.into_iter().next(),
            Ok(Ok(None)) => None,
            _ => {
                let _ = entity.update(cx, |this, cx| {
                    this.remote_suggestion_state = RemoteSuggestionState::Failed(
                        "Checkout picker failed; no file changed.".to_string(),
                    );
                    cx.notify();
                });
                return;
            }
        };
        let Some(root) = selected else {
            let _ = entity.update(cx, |this, cx| {
                this.remote_suggestion_state = RemoteSuggestionState::Idle;
                cx.notify();
            });
            return;
        };
        let result = cx
            .background_executor()
            .spawn(async move {
                let replacement = parse_suggestion(&comment.body)?;
                let preview = suggested_change::prepare_remote(&root, &source, line, &replacement)?;
                Ok::<_, anyhow::Error>(PendingSuggestion {
                    reference: snapshot.reference.clone(),
                    base_sha: snapshot.base_sha.clone(),
                    head_sha: snapshot.head_sha.clone(),
                    comment,
                    preview,
                })
            })
            .await;
        let _ = entity.update(cx, |this, cx| {
            this.remote_suggestion_state = match result {
                Ok(pending) => match &this.remote_review_state {
                    RemoteReviewState::Loaded(current)
                        if pending.revalidate(current).is_ok()
                            && this.selected_remote_comment.as_ref() == Some(&pending.comment) =>
                    {
                        RemoteSuggestionState::Confirming(Box::new(pending))
                    }
                    _ => RemoteSuggestionState::Failed(
                        "Selected PR/comment changed; prepare the suggestion again.".to_string(),
                    ),
                },
                Err(error) => {
                    RemoteSuggestionState::Failed(format!("Suggestion preview blocked: {error}"))
                }
            };
            cx.notify();
        });
    })
    .detach();
}

fn apply(entity: &Entity<ReviewWorkspace>, cx: &mut App) {
    let request = entity.update(cx, |this, cx| {
        let RemoteSuggestionState::Confirming(pending) = &this.remote_suggestion_state else {
            return None;
        };
        let RemoteReviewState::Loaded(snapshot) = &this.remote_review_state else {
            return None;
        };
        if pending.revalidate(snapshot).is_err()
            || this.selected_remote_comment.as_ref() != Some(&pending.comment)
            || this.remote_commit_state.is_busy()
        {
            return None;
        }
        let credentials = this.github_credentials.as_ref()?;
        if credentials.access_token_is_expired() {
            return None;
        }
        let request = (pending.clone(), credentials.access_token().to_string());
        this.remote_suggestion_state = RemoteSuggestionState::Applying;
        cx.notify();
        Some(request)
    });
    let Some((pending, token)) = request else {
        return;
    };
    let http = cx.http_client();
    let entity = entity.clone();
    cx.spawn(async move |cx| {
        let receipt = crate::action_history::suggestion(&pending.preview, true);
        let result = crate::with_network_timeout(revalidate_remote(http, token, &pending), cx.background_executor()).await;
        let result = match result {
            Ok(()) => cx.background_executor().spawn(async move { suggested_change::apply(&pending.preview) }).await,
            Err(error) => Err(error),
        };
        let _ = entity.update(cx, |this, cx| {
            let confirmed = result.is_ok();
            this.remote_suggestion_state = match result {
                Ok(()) => RemoteSuggestionState::Applied("Suggestion applied locally. No stage, commit, push or GitHub write was performed. Inspect your checkout before any separate commit/push.".to_string()),
                Err(error) => RemoteSuggestionState::Failed(format!("Suggestion application blocked: {error}")),
            };
            this.capture_action(receipt.map(|mut record| {
                if confirmed {
                    record.outcome = crate::action_history::ActionOutcome::Confirmed;
                }
                record
            }));
            cx.notify();
        });
    }).detach();
}

async fn revalidate_remote(
    http: Arc<dyn gpui::http_client::HttpClient>,
    token: String,
    pending: &PendingSuggestion,
) -> anyhow::Result<()> {
    let client = github_pr::GitHubPullRequestClient::new(http, token)?;
    let fresh = client.load_pull_request(pending.reference.clone()).await?;
    pending.revalidate(&fresh)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github_pr::PullRequestSourceFile;
    use gix::bstr::ByteSlice as _;

    fn fixture() -> (tempfile::TempDir, PullRequestSnapshot, PendingSuggestion) {
        let root = tempfile::tempdir().unwrap();
        let repository = gix::init(root.path()).unwrap();
        let content = "unsafe_call()\n";
        std::fs::write(root.path().join("file.rs"), content).unwrap();
        let blob = repository.write_blob(content.as_bytes()).unwrap().detach();
        let tree = gix::objs::Tree {
            entries: vec![gix::objs::tree::Entry {
                mode: (0o100_644).try_into().unwrap(),
                filename: "file.rs".into(),
                oid: blob,
            }],
        };
        let tree_id = repository.write_object(&tree).unwrap().detach();
        let actor = gix::actor::SignatureRef {
            name: b"Test".as_bstr(),
            email: b"test@example.test".as_bstr(),
            time: "1 +0000",
        };
        let head = repository
            .commit_as(
                actor,
                actor,
                "HEAD",
                "fixture",
                tree_id,
                std::iter::empty::<gix::ObjectId>(),
            )
            .unwrap()
            .to_string();
        let comment = PullRequestComment {
            id: 1,
            path: "file.rs".to_string(),
            line: Some(1),
            original_line: Some(1),
            body: "Use the checked helper.\n```suggestion\nchecked_call()\n```".to_string(),
            author: "reviewer".to_string(),
            html_url: "https://github.com/octo/repo/pull/1#discussion_r1".to_string(),
            commit_id: head.clone(),
            original_commit_id: head.clone(),
            in_reply_to_id: None,
            is_outdated: false,
            subject_type: Some("line".to_string()),
            side: Some("RIGHT".to_string()),
        };
        let snapshot = PullRequestSnapshot {
            reference: github_pr::PullRequestRef::parse("octo/repo#1").unwrap(),
            title: "fixture".to_string(),
            state: "open".to_string(),
            author: "author".to_string(),
            html_url: "https://github.com/octo/repo/pull/1".to_string(),
            base_sha: "b".repeat(40),
            head_sha: head.clone(),
            head_repository: None,
            head_branch: None,
            files: vec![cururu_core::ChangedFile {
                path: "file.rs".to_string(),
                patch: "@@ -1 +1 @@\n-old\n+unsafe_call()".to_string(),
                right_lines: vec![1],
            }],
            reviews: Vec::new(),
            review_threads: vec![github_pr::PullRequestReviewThread {
                root: comment.clone(),
                replies: Vec::new(),
            }],
            conversation_comments: Vec::new(),
        };
        let source = PullRequestSourceFile {
            path: comment.path.clone(),
            commit_sha: head.clone(),
            content: content.to_string(),
        };
        let preview = suggested_change::prepare_remote(
            root.path(),
            &source,
            1,
            &parse_suggestion(&comment.body).unwrap(),
        )
        .unwrap();
        let pending = PendingSuggestion {
            reference: snapshot.reference.clone(),
            base_sha: snapshot.base_sha.clone(),
            head_sha: head,
            comment,
            preview,
        };
        (root, snapshot, pending)
    }

    #[test]
    fn suggestion_parser_rejects_ambiguous_offset_multiline_and_empty_blocks() {
        assert_eq!(
            parse_suggestion("Reason\n```suggestion\nchecked()\n```\nMore prose").unwrap(),
            "checked()"
        );
        for body in [
            "```suggestion:-1+1\na\n```",
            "```suggestion\na\nb\n```",
            "```suggestion\n\n```",
            "```suggestion\na\n```\n```suggestion\nb\n```",
            "~~~md\n```suggestion\na\n```\n~~~",
            "```md\n```suggestion\na\n```\n```",
            "```suggestion\na\n",
        ] {
            assert!(parse_suggestion(body).is_err());
        }
    }

    #[test]
    fn local_import_previews_without_writing_then_applies_without_committing_or_staging() {
        let (root, snapshot, pending) = fixture();
        assert_eq!(
            std::fs::read_to_string(root.path().join("file.rs")).unwrap(),
            "unsafe_call()\n"
        );
        let index = root.path().join(".git/index");
        let index_before = std::fs::read(&index).ok();
        pending.revalidate(&snapshot).unwrap();
        suggested_change::apply(&pending.preview).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("file.rs")).unwrap(),
            "checked_call()\n"
        );
        assert_eq!(
            gix::open(root.path())
                .unwrap()
                .head_id()
                .unwrap()
                .to_string(),
            snapshot.head_sha
        );
        assert_eq!(std::fs::read(index).ok(), index_before);
        assert!(suggested_change::apply(&pending.preview).is_err());
    }

    #[test]
    fn imported_suggestion_rejects_local_edits_wrong_head_and_remote_staleness() {
        let (root, snapshot, pending) = fixture();
        for field in [
            "head",
            "base",
            "closed",
            "comment",
            "missing",
            "anchor",
            "left",
            "outdated",
            "oldcomment",
            "missingline",
        ] {
            let mut changed = snapshot.clone();
            match field {
                "head" => changed.head_sha = "c".repeat(40),
                "base" => changed.base_sha = "c".repeat(40),
                "closed" => changed.state = "closed".to_string(),
                "comment" => changed.review_threads[0].root.body.push_str(" edited"),
                "missing" => changed.review_threads.clear(),
                "left" => changed.review_threads[0].root.side = Some("LEFT".to_string()),
                "outdated" => changed.review_threads[0].root.is_outdated = true,
                "oldcomment" => changed.review_threads[0].root.original_commit_id = "c".repeat(40),
                "missingline" => changed.review_threads[0].root.line = None,
                _ => changed.files[0].right_lines.clear(),
            }
            assert!(pending.revalidate(&changed).is_err(), "{field}");
        }
        let mut source = PullRequestSourceFile {
            path: "file.rs".to_string(),
            commit_sha: "c".repeat(40),
            content: "unsafe_call()\n".to_string(),
        };
        assert!(suggested_change::prepare_remote(root.path(), &source, 1, "checked()").is_err());
        source.commit_sha = snapshot.head_sha;
        source.content = "different source\n".to_string();
        assert!(suggested_change::prepare_remote(root.path(), &source, 1, "checked()").is_err());
        std::fs::write(root.path().join("file.rs"), "user_edit()\n").unwrap();
        assert!(suggested_change::apply(&pending.preview).is_err());
        assert_eq!(
            std::fs::read_to_string(root.path().join("file.rs")).unwrap(),
            "user_edit()\n"
        );
    }

    struct FakeHttp {
        responses: std::sync::Mutex<std::collections::VecDeque<String>>,
        methods: std::sync::Mutex<Vec<String>>,
    }

    impl gpui::http_client::HttpClient for FakeHttp {
        fn type_name(&self) -> &'static str {
            "fake-suggestion-github"
        }
        fn user_agent(&self) -> Option<&gpui::http_client::http::HeaderValue> {
            None
        }
        fn proxy(&self) -> Option<&gpui::http_client::Url> {
            None
        }
        fn send(
            &self,
            request: gpui::http_client::Request<gpui::http_client::AsyncBody>,
        ) -> futures::future::BoxFuture<
            'static,
            anyhow::Result<gpui::http_client::Response<gpui::http_client::AsyncBody>>,
        > {
            self.methods
                .lock()
                .unwrap()
                .push(request.method().to_string());
            let body = self.responses.lock().unwrap().pop_front();
            Box::pin(async move {
                Ok(gpui::http_client::Response::builder().status(200).body(
                    gpui::http_client::AsyncBody::from(body.context("missing fake response")?),
                )?)
            })
        }
    }

    #[test]
    fn remote_revalidation_uses_only_gets_and_performs_no_remote_write() {
        let (root, snapshot, pending) = fixture();
        let comment = &pending.comment;
        for changed_head in [false, true] {
            let responses = [
                serde_json::json!({"number":1,"title":"fixture","state":"open","html_url":snapshot.html_url,"user":{"login":"author"},"base":{"sha":snapshot.base_sha},"head":{"sha":if changed_head { "c".repeat(40) } else { snapshot.head_sha.clone() }}}),
                serde_json::json!([{"filename":"file.rs","status":"modified","additions":1,"deletions":1,"patch":"@@ -1 +1 @@\n-old\n+unsafe_call()"}]),
                serde_json::json!([{"id":1,"path":"file.rs","line":1,"original_line":1,"body":comment.body,"html_url":comment.html_url,"commit_id":comment.commit_id,"original_commit_id":comment.original_commit_id,"side":"RIGHT","subject_type":"line","user":{"login":"reviewer"}}]),
                serde_json::json!([]),
                serde_json::json!([]),
            ];
            let http = Arc::new(FakeHttp {
                responses: std::sync::Mutex::new(
                    responses
                        .into_iter()
                        .map(|value| value.to_string())
                        .collect(),
                ),
                methods: std::sync::Mutex::new(Vec::new()),
            });
            let result = futures::executor::block_on(revalidate_remote(
                http.clone(),
                "synthetic-token".to_string(),
                &pending,
            ));
            assert_eq!(result.is_ok(), !changed_head);
            assert_eq!(*http.methods.lock().unwrap(), vec!["GET"; 5]);
            assert_eq!(
                std::fs::read_to_string(root.path().join("file.rs")).unwrap(),
                "unsafe_call()\n"
            );
        }
    }
}
