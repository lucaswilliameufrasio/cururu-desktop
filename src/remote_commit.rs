//! Explicit GitHub Git-object creation followed by separately confirmed ref update.

use std::path::PathBuf;

use anyhow::{Context as _, ensure};
use gpui::{
    App, AppContext as _, BackgroundExecutor, Context, Entity, ParentElement as _, Styled as _, div,
};
use gpui_component::{Disableable as _, button::Button, input::TextInput};
use similar::TextDiff;

use crate::{
    GitHubAuthState, RemoteReviewState, RemoteSourceContextState, ReviewWorkspace,
    github_pr::{self, GitHubBranchUpdate, GitHubCommitFile, GitHubCommitObject},
    repository::{self, RepositorySnapshot},
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommitTarget {
    reference: github_pr::PullRequestRef,
    repository: github_pr::GitHubRepositoryRef,
    branch: String,
    pull_request_url: String,
    base_sha: String,
    head_sha: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalSourceProof {
    root: PathBuf,
    identity: String,
    branch: String,
    head_sha: String,
    all_files: Vec<GitHubCommitFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommitCandidate {
    origin: &'static str,
    file: GitHubCommitFile,
    diff: String,
}

#[derive(Debug, Clone)]
pub struct PendingSelection {
    target: CommitTarget,
    candidates: Vec<CommitCandidate>,
    selected: Vec<bool>,
    local_proof: Option<LocalSourceProof>,
    expected_suggestions: Vec<github_pr::PullRequestComment>,
}

#[derive(Debug, Clone)]
pub struct PendingCommit {
    target: CommitTarget,
    files: Vec<GitHubCommitFile>,
    previews: Vec<CommitCandidate>,
    message: String,
    local_proof: Option<LocalSourceProof>,
    expected_suggestions: Vec<github_pr::PullRequestComment>,
}

#[derive(Default)]
pub enum RemoteCommitState {
    #[default]
    Idle,
    Preparing,
    Selecting(PendingSelection),
    Preview(PendingCommit),
    Creating(PendingCommit),
    Created(GitHubCommitObject),
    UpdatePreview(GitHubCommitObject),
    Updating(GitHubCommitObject),
    Updated(GitHubBranchUpdate),
    Failed(String),
}

impl RemoteCommitState {
    pub const fn is_busy(&self) -> bool {
        matches!(
            self,
            Self::Preparing | Self::Creating(_) | Self::Updating(_)
        )
    }
}

fn target_from_snapshot(snapshot: &github_pr::PullRequestSnapshot) -> anyhow::Result<CommitTarget> {
    ensure!(snapshot.state == "open", "pull request must be open");
    ensure!(
        is_commit_sha(&snapshot.base_sha) && is_commit_sha(&snapshot.head_sha),
        "pull request base/head is not a valid commit identity"
    );
    let repository = snapshot
        .head_repository
        .clone()
        .context("GitHub did not provide the pull request source repository")?;
    let branch = snapshot
        .head_branch
        .clone()
        .context("GitHub did not provide the pull request source branch")?;
    let target = CommitTarget {
        reference: snapshot.reference.clone(),
        repository,
        branch,
        pull_request_url: snapshot.html_url.clone(),
        base_sha: snapshot.base_sha.clone(),
        head_sha: snapshot.head_sha.clone(),
    };
    github_pr::validate_remote_commit_target(
        &target.reference,
        &target.repository,
        &target.branch,
        &target.base_sha,
        &target.head_sha,
    )?;
    Ok(target)
}

fn is_commit_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn target_matches_snapshot(
    target: &CommitTarget,
    snapshot: &github_pr::PullRequestSnapshot,
) -> bool {
    target_from_snapshot(snapshot).is_ok_and(|current| current == *target)
}

fn local_candidates(
    snapshot: &RepositorySnapshot,
) -> anyhow::Result<(Vec<CommitCandidate>, LocalSourceProof)> {
    let files = repository::remote_commit_files(snapshot)?;
    ensure!(
        files.len() == snapshot.changed_files.len(),
        "local diff candidates do not match the current file list"
    );
    let mut candidates = Vec::with_capacity(files.len());
    for file in &files {
        let diff = snapshot
            .changed_files
            .iter()
            .find(|changed| changed.path == file.path)
            .context("local diff changed while preparing candidates")?
            .patch
            .clone();
        candidates.push(CommitCandidate {
            origin: "local checkout diff",
            file: file.clone(),
            diff,
        });
    }
    let proof = LocalSourceProof {
        root: snapshot.root.clone(),
        identity: crate::local_review::repository_identity(&snapshot.root)?,
        branch: snapshot
            .branch
            .clone()
            .context("local checkout has no named branch")?,
        head_sha: snapshot
            .head_oid
            .clone()
            .context("local checkout has no commit to match the pull request")?,
        all_files: files,
    };
    Ok((candidates, proof))
}

fn suggestion_candidate(
    snapshot: &github_pr::PullRequestSnapshot,
    comment: &github_pr::PullRequestComment,
    source: &github_pr::PullRequestSourceFile,
) -> anyhow::Result<CommitCandidate> {
    let line = crate::remote_suggestion::validate_comment(snapshot, comment)?;
    let source_revision_matches = source.commit_sha == snapshot.head_sha;
    let source_file_matches = source.path == comment.path;
    ensure!(
        source_revision_matches && source_file_matches,
        "loaded GitHub suggestion source is stale or belongs to another file"
    );
    let replacement = crate::remote_suggestion::parse_suggestion(&comment.body)?;
    let content =
        crate::suggested_change::remote_suggestion_content(&source.content, line, &replacement)?;
    let diff = TextDiff::from_lines(&source.content, &content)
        .unified_diff()
        .header(&format!("a/{}", source.path), &format!("b/{}", source.path))
        .to_string();
    Ok(CommitCandidate {
        origin: "GitHub suggestion",
        file: GitHubCommitFile {
            path: source.path.clone(),
            mode: "100644".to_string(),
            content: Some(content),
        },
        diff,
    })
}

impl ReviewWorkspace {
    pub fn ensure_remote_commit_input(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        if self.remote_commit_message_input.is_none() {
            self.remote_commit_message_input = Some(cx.new(|input_cx| {
                gpui_component::input::InputState::new(window, input_cx)
                    .multi_line()
                    .rows(2)
                    .placeholder("Write the remote commit message…")
            }));
        }
    }

    // GPUI's remote commit panel renders one explicit multi-stage consent state.
    #[allow(clippy::too_many_lines)]
    pub fn render_remote_commit(
        &self,
        cx: &Context<Self>,
        snapshot: &github_pr::PullRequestSnapshot,
    ) -> gpui::Div {
        let mut panel = div()
            .flex()
            .flex_col()
            .gap_2()
            .child("REMOTE COMMIT · first create an unreferenced commit object, then separately preview and confirm the branch update")
            .child("This flow never stages, modifies, or commits local files. Unsupported binary, symlink, mode-only, or ambiguous changes fail closed.")
            .child("Remote Git writes require a GitHub App with repository Contents: read/write, installed and authorized for this repository. App registration and permissions are configured on GitHub, not in Cururu Desktop; see the README setup guide.");
        let target = match target_from_snapshot(snapshot) {
            Ok(target) => target,
            Err(error) => return panel.child(format!("Remote commit unavailable: {error}")),
        };
        let busy = self.remote_commit_state.is_busy();
        let local_matches = self
            .local_repository_snapshot
            .as_ref()
            .is_some_and(|local| {
                local.head_oid.as_deref() == Some(target.head_sha.as_str())
                    && local.branch.as_deref() == Some(target.branch.as_str())
                    && !local.changed_files.is_empty()
            });
        let entity = cx.entity();
        panel = panel.child(
            Button::new("prepare-local-remote-commit")
                .label("Select files from matching local diff")
                .disabled(
                    busy || !local_matches
                        || self.is_opening_repository
                        || self.remote_suggestion_state.is_busy(),
                )
                .on_click(move |_, _, cx| prepare_local(&entity, cx)),
        );

        let suggestion = self.selected_remote_comment.as_ref().and_then(|comment| {
            let RemoteSourceContextState::Loaded {
                comment_id, source, ..
            } = &self.remote_source_context
            else {
                return None;
            };
            if *comment_id != comment.id {
                return None;
            }
            suggestion_candidate(snapshot, comment, source)
                .ok()
                .map(|candidate| (comment.clone(), candidate))
        });
        if let Some((comment, candidate)) = suggestion {
            let entity = cx.entity();
            panel = panel.child(
                Button::new("prepare-github-suggestion-remote-commit")
                    .label(format!(
                        "Select GitHub suggestion file: {}",
                        candidate.file.path
                    ))
                    .disabled(busy || self.remote_suggestion_state.is_busy())
                    .on_click(move |_, _, cx| {
                        prepare_suggestion(&entity, comment.clone(), candidate.clone(), cx);
                    }),
            );
        }

        match &self.remote_commit_state {
            RemoteCommitState::Idle => {}
            RemoteCommitState::Preparing => {
                panel = panel
                    .child("Preparing bounded file candidates; no remote write is being made.");
            }
            RemoteCommitState::Selecting(selection) => {
                let target_is_current = target_matches_snapshot(&selection.target, snapshot);
                panel = panel.child(format!(
                    "SELECT FILES · {}/{} · branch {} · parent {}",
                    selection.target.repository.owner,
                    selection.target.repository.repository,
                    selection.target.branch,
                    crate::short_sha(&selection.target.head_sha)
                ));
                for (index, candidate) in selection.candidates.iter().enumerate() {
                    let selected = selection.selected.get(index).copied().unwrap_or(false);
                    let entity = cx.entity();
                    panel = panel.child(
                        Button::new(("toggle-remote-commit-file", index))
                            .label(format!(
                                "{} [{}] {} · {} bytes",
                                if selected { "☑" } else { "☐" },
                                candidate.origin,
                                candidate.file.path,
                                candidate.file.content.as_ref().map_or(0, String::len)
                            ))
                            .disabled(!target_is_current || busy)
                            .on_click(move |_, _, cx| toggle_candidate(&entity, index, cx)),
                    );
                    panel = panel.child(candidate.diff.clone());
                }
                if let Some(input) = &self.remote_commit_message_input {
                    panel = panel.child("Commit message").child(TextInput::new(input));
                }
                let entity = cx.entity();
                panel = panel.child(
                    Button::new("preview-selected-remote-commit")
                        .label("Preview selected remote commit")
                        .disabled(
                            !target_is_current
                                || busy
                                || !selection.selected.iter().any(|value| *value),
                        )
                        .on_click(move |_, _, cx| preview_commit(&entity, cx)),
                );
                let entity = cx.entity();
                panel = panel.child(
                    Button::new("cancel-remote-commit-selection")
                        .label("Cancel remote commit selection")
                        .on_click(move |_, _, cx| {
                            entity.update(cx, |this, cx| {
                                this.remote_commit_state = RemoteCommitState::Idle;
                                cx.notify();
                            });
                        }),
                );
            }
            RemoteCommitState::Preview(pending) => {
                let current = target_matches_snapshot(&pending.target, snapshot);
                panel = panel
                    .child("COMMIT OBJECT PREVIEW · confirming creates Git blobs/tree/commit objects only; it does not update a branch")
                    .child(format!(
                "PR: {}\nRepository: {}/{}\nBranch: {}\nBase: {}\nParent/head: {}\nCommit message: {}\nSelected files: {}",
                pending.target.pull_request_url,
                pending.target.repository.owner,
                pending.target.repository.repository,
                pending.target.branch,
                pending.target.base_sha,
                        pending.target.head_sha,
                        pending.message,
                        pending.files.len()
                    ));
                for candidate in &pending.previews {
                    panel = panel.child(format!(
                        "{} · {} · mode {} · {} bytes\n{}",
                        candidate.origin,
                        candidate.file.path,
                        candidate.file.mode,
                        candidate.file.content.as_ref().map_or(0, String::len),
                        candidate.diff
                    ));
                }
                let entity = cx.entity();
                let pending = pending.clone();
                panel = panel.child(
                    Button::new("confirm-create-remote-commit-object")
                        .label("Confirm and create remote commit object (branch unchanged)")
                        .disabled(!current || busy || !github_authorized(self))
                        .on_click(move |_, _, cx| create_commit_object(&entity, &pending, cx)),
                );
            }
            RemoteCommitState::Creating(_) => {
                panel = panel.child("Creating the confirmed tree and commit objects. No branch reference is being updated.");
            }
            RemoteCommitState::Created(commit) => {
                let current = target_matches_commit_snapshot(commit, snapshot);
                panel = panel
                    .child(format!(
                        "REMOTE COMMIT OBJECT CREATED · {}\nParent: {}\nThe branch is unchanged. The object may remain unreferenced unless you separately confirm an update.",
                        commit.html_url,
                        commit.parent_sha
                    ));
                let entity = cx.entity();
                let commit = commit.clone();
                panel = panel.child(
                    Button::new("preview-update-remote-branch")
                        .label("Preview separate branch update")
                        .disabled(!current || busy || !github_authorized(self))
                        .on_click(move |_, _, cx| preview_branch_update(&entity, &commit, cx)),
                );
            }
            RemoteCommitState::UpdatePreview(commit) => {
                let current = target_matches_commit_snapshot(commit, snapshot);
                panel = panel
                    .child("BRANCH UPDATE PREVIEW · this is a separate GitHub write")
                    .child(format!(
                "PR: https://github.com/{}/{}/pull/{}\nRepository: {}/{}\nBranch: {}\nExpected current ref: {}\nNew commit: {}\nForce: false",
                commit.reference.owner,
                commit.reference.repository,
                commit.reference.number,
                commit.repository.owner,
                commit.repository.repository,
                commit.branch,
                        commit.parent_sha,
                        commit.commit_sha
                    ));
                let entity = cx.entity();
                let commit = commit.clone();
                panel = panel.child(
                    Button::new("confirm-update-remote-branch")
                        .label("Separately confirm updating the PR branch")
                        .disabled(!current || busy || !github_authorized(self))
                        .on_click(move |_, _, cx| update_branch(&entity, &commit, cx)),
                );
            }
            RemoteCommitState::Updating(_) => {
                panel = panel.child(
                    "Revalidating the PR and exact branch parent before the non-force update…",
                );
            }
            RemoteCommitState::Updated(update) => {
                panel = panel.child(format!(
                    "Branch updated: {}/{} · branch {} · {} → {}",
                    update.repository.owner,
                    update.repository.repository,
                    update.branch,
                    update.previous_sha,
                    update.commit_sha
                ));
            }
            RemoteCommitState::Failed(message) => {
                panel = panel
                    .child(message.clone())
                    .child("No write is retried automatically. Refresh the PR and inspect the branch before deciding what to do next.");
            }
        }
        panel
    }
}

fn github_authorized(workspace: &ReviewWorkspace) -> bool {
    matches!(workspace.github_auth_state, GitHubAuthState::SignedIn)
        && workspace
            .github_credentials
            .as_ref()
            .is_some_and(|credentials| !credentials.access_token_is_expired())
}

fn prepare_local(entity: &Entity<ReviewWorkspace>, cx: &mut App) {
    let request = entity.update(cx, |this, cx| {
        if this.remote_commit_state.is_busy()
            || this.remote_suggestion_state.is_busy()
            || this.is_opening_repository
        {
            return None;
        }
        let RemoteReviewState::Loaded(snapshot) = &this.remote_review_state else {
            return None;
        };
        let target = target_from_snapshot(snapshot).ok()?;
        let local = this.local_repository_snapshot.as_ref()?;
        if local.head_oid.as_deref() != Some(target.head_sha.as_str())
            || local.branch.as_deref() != Some(target.branch.as_str())
        {
            this.notice = Some(
                "Local diff must be on the exact PR head and source branch before preparing a remote commit.".into(),
            );
            cx.notify();
            return None;
        }
        let root = local.root.clone();
        this.remote_commit_state = RemoteCommitState::Preparing;
        cx.notify();
        Some((root, target))
    });
    let Some((root, target)) = request else {
        return;
    };
    let entity = entity.clone();
    let target_for_task = target.clone();
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move {
                let snapshot = RepositorySnapshot::open(root)?;
                ensure!(
                    snapshot.head_oid.as_deref() == Some(target_for_task.head_sha.as_str())
                        && snapshot.branch.as_deref() == Some(target_for_task.branch.as_str()),
                    "local checkout moved away from the pull request head/branch"
                );
                local_candidates(&snapshot)
            })
            .await;
        let _ = entity.update(cx, |this, cx| {
            if !matches!(this.remote_commit_state, RemoteCommitState::Preparing) {
                return;
            }
            match result {
                Ok((candidates, proof)) => {
                    let RemoteReviewState::Loaded(current) = &this.remote_review_state else {
                        this.remote_commit_state = RemoteCommitState::Failed(
                            "Pull request changed while preparing the local diff.".to_string(),
                        );
                        cx.notify();
                        return;
                    };
                    if target_matches_snapshot(&target, current) {
                        this.remote_commit_state = RemoteCommitState::Selecting(PendingSelection {
                            target,
                            selected: vec![true; candidates.len()],
                            candidates,
                            local_proof: Some(proof),
                            expected_suggestions: Vec::new(),
                        });
                    } else {
                        this.remote_commit_state = RemoteCommitState::Failed(
                            "Pull request changed while preparing the local diff.".to_string(),
                        );
                    }
                }
                Err(error) => {
                    this.remote_commit_state = RemoteCommitState::Failed(format!(
                        "Local commit candidate preparation failed: {error}"
                    ));
                }
            }
            cx.notify();
        });
    })
    .detach();
}

fn prepare_suggestion(
    entity: &Entity<ReviewWorkspace>,
    comment: github_pr::PullRequestComment,
    candidate: CommitCandidate,
    cx: &mut App,
) {
    entity.update(cx, |this, cx| {
        if this.remote_commit_state.is_busy() || this.remote_suggestion_state.is_busy() {
            return;
        }
        let RemoteReviewState::Loaded(snapshot) = &this.remote_review_state else {
            return;
        };
        let target = match target_from_snapshot(snapshot) {
            Ok(target) => target,
            Err(error) => {
                this.remote_commit_state = RemoteCommitState::Failed(error.to_string());
                cx.notify();
                return;
            }
        };
        if this.selected_remote_comment.as_ref() == Some(&comment) {
            this.remote_commit_state = RemoteCommitState::Selecting(PendingSelection {
                target,
                candidates: vec![candidate],
                selected: vec![true],
                local_proof: None,
                expected_suggestions: vec![comment],
            });
        } else {
            this.remote_commit_state = RemoteCommitState::Failed(
                "Selected GitHub suggestion changed; load it again before preparing a commit."
                    .to_string(),
            );
        }
        cx.notify();
    });
}

fn toggle_candidate(entity: &Entity<ReviewWorkspace>, index: usize, cx: &mut App) {
    entity.update(cx, |this, cx| {
        if let RemoteCommitState::Selecting(selection) = &mut this.remote_commit_state
            && let Some(selected) = selection.selected.get_mut(index)
        {
            *selected = !*selected;
        }
        cx.notify();
    });
}

fn preview_commit(entity: &Entity<ReviewWorkspace>, cx: &mut App) {
    entity.update(cx, |this, cx| {
        let RemoteCommitState::Selecting(selection) = &this.remote_commit_state else {
            return;
        };
        let RemoteReviewState::Loaded(snapshot) = &this.remote_review_state else {
            return;
        };
        if !target_matches_snapshot(&selection.target, snapshot) {
            this.notice =
                Some("Pull request changed; refresh before previewing a remote commit.".into());
            cx.notify();
            return;
        }
        let message = this
            .remote_commit_message_input
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default();
        let previews = selection
            .candidates
            .iter()
            .zip(&selection.selected)
            .filter_map(|(candidate, selected)| selected.then_some(candidate.clone()))
            .collect::<Vec<_>>();
        let files = previews
            .iter()
            .map(|candidate| candidate.file.clone())
            .collect::<Vec<_>>();
        if let Err(error) = github_pr::validate_remote_commit_preview(&message, &files) {
            this.notice = Some(error.to_string().into());
            cx.notify();
            return;
        }
        this.remote_commit_state = RemoteCommitState::Preview(PendingCommit {
            target: selection.target.clone(),
            files,
            previews,
            message,
            local_proof: selection.local_proof.clone(),
            expected_suggestions: selection.expected_suggestions.clone(),
        });
        cx.notify();
    });
}

fn revalidate_local_proof(proof: &LocalSourceProof) -> anyhow::Result<()> {
    ensure!(
        crate::local_review::repository_identity(&proof.root)? == proof.identity,
        "local repository identity changed after the remote commit preview"
    );
    let snapshot = RepositorySnapshot::open(&proof.root)?;
    ensure!(
        snapshot.head_oid.as_deref() == Some(proof.head_sha.as_str())
            && snapshot.branch.as_deref() == Some(proof.branch.as_str()),
        "local checkout HEAD/branch changed after the remote commit preview"
    );
    ensure!(
        repository::remote_commit_files(&snapshot)? == proof.all_files,
        "local diff contents or file modes changed after the remote commit preview"
    );
    Ok(())
}

fn create_commit_object(entity: &Entity<ReviewWorkspace>, expected: &PendingCommit, cx: &mut App) {
    let operation = entity.update(cx, |this, cx| {
        let RemoteCommitState::Preview(pending) = &this.remote_commit_state else {
            return None;
        };
        let RemoteReviewState::Loaded(snapshot) = &this.remote_review_state else {
            return None;
        };
        if pending.target != expected.target
            || pending.files != expected.files
            || pending.message != expected.message
            || !target_matches_snapshot(&pending.target, snapshot)
            || !github_authorized(this)
            || this.review_publication.is_publishing()
            || matches!(this.thread_reply_state, crate::ThreadReplyState::Publishing(_))
            || this.remote_suggestion_state.is_busy()
            || this.remote_commit_state.is_busy()
        {
            this.notice = Some(
                "Remote commit preview is stale, authorization expired, or another write is active.".into(),
            );
            cx.notify();
            return None;
        }
        let token = this
            .github_credentials
            .as_ref()?
            .access_token()
            .to_string();
        let pending = pending.clone();
        this.remote_commit_state = RemoteCommitState::Creating(pending.clone());
        cx.notify();
        Some((token, pending))
    });
    let Some((token, pending)) = operation else {
        return;
    };
    let http = cx.http_client();
    let entity = entity.clone();
    cx.spawn(async move |cx| {
        let executor = cx.background_executor();
        let result = async {
            if let Some(proof) = &pending.local_proof {
                let proof = proof.clone();
                executor
                    .spawn(async move { revalidate_local_proof(&proof) })
                    .await?;
            }
            let client = github_pr::GitHubPullRequestClient::new(http, token)?;
            crate::with_network_timeout(
                client.create_remote_commit_object(&github_pr::RemoteCommitRequest {
                    reference: pending.target.reference.clone(),
                    repository: pending.target.repository.clone(),
                    branch: pending.target.branch.clone(),
                    base_sha: pending.target.base_sha.clone(),
                    parent_sha: pending.target.head_sha.clone(),
                    message: pending.message.clone(),
                    files: pending.files.clone(),
                    expected_suggestions: pending.expected_suggestions.clone(),
                }),
                executor,
            )
            .await
            .context("remote commit object creation outcome may be uncertain")
        }
        .await;
        let _ = entity.update(cx, |this, cx| {
            if !matches!(&this.remote_commit_state, RemoteCommitState::Creating(active)
                if active.target == pending.target && active.files == pending.files
                    && active.message == pending.message)
            {
                return;
            }
            this.remote_commit_state = match result {
                Ok(commit) => RemoteCommitState::Created(commit),
                Err(error) => RemoteCommitState::Failed(format!(
                    "Remote commit object creation failed or is uncertain: {error}. The branch was not intentionally updated; refresh GitHub before deciding whether to retry."
                )),
            };
            cx.notify();
        });
    })
    .detach();
}

fn target_matches_commit_snapshot(
    commit: &GitHubCommitObject,
    snapshot: &github_pr::PullRequestSnapshot,
) -> bool {
    target_from_snapshot(snapshot).is_ok_and(|target| {
        let parent_matches = target.head_sha == commit.parent_sha;
        target.reference == commit.reference
            && target.repository == commit.repository
            && target.branch == commit.branch
            && target.base_sha == commit.base_sha
            && parent_matches
    })
}

fn preview_branch_update(
    entity: &Entity<ReviewWorkspace>,
    commit: &GitHubCommitObject,
    cx: &mut App,
) {
    entity.update(cx, |this, cx| {
        let RemoteCommitState::Created(active) = &this.remote_commit_state else {
            return;
        };
        let RemoteReviewState::Loaded(snapshot) = &this.remote_review_state else {
            return;
        };
        if active != commit || !target_matches_commit_snapshot(commit, snapshot) {
            this.notice = Some("Created commit no longer matches the loaded pull request.".into());
        } else {
            this.remote_commit_state = RemoteCommitState::UpdatePreview(commit.clone());
        }
        cx.notify();
    });
}

fn update_branch(entity: &Entity<ReviewWorkspace>, expected: &GitHubCommitObject, cx: &mut App) {
    let operation = entity.update(cx, |this, cx| {
        let RemoteCommitState::UpdatePreview(commit) = &this.remote_commit_state else {
            return None;
        };
        let RemoteReviewState::Loaded(snapshot) = &this.remote_review_state else {
            return None;
        };
        if commit != expected
            || !target_matches_commit_snapshot(commit, snapshot)
            || !github_authorized(this)
            || this.review_publication.is_publishing()
            || matches!(this.thread_reply_state, crate::ThreadReplyState::Publishing(_))
            || this.remote_suggestion_state.is_busy()
            || this.remote_commit_state.is_busy()
        {
            this.notice = Some(
                "Branch update preview is stale, authorization expired, or another write is active.".into(),
            );
            cx.notify();
            return None;
        }
        let token = this
            .github_credentials
            .as_ref()?
            .access_token()
            .to_string();
        let commit = commit.clone();
        this.remote_commit_state = RemoteCommitState::Updating(commit.clone());
        cx.notify();
        Some((token, commit))
    });
    let Some((token, commit)) = operation else {
        return;
    };
    let http = cx.http_client();
    let entity = entity.clone();
    cx.spawn(async move |cx| {
        let executor: BackgroundExecutor = cx.background_executor().clone();
        let result = async {
            let client = github_pr::GitHubPullRequestClient::new(http, token)?;
            crate::with_network_timeout(client.update_remote_branch(&commit), &executor)
                .await
                .context("remote branch update outcome may be uncertain")
        }
        .await;
        let _ = entity.update(cx, |this, cx| {
            if !matches!(&this.remote_commit_state, RemoteCommitState::Updating(active)
                if active == &commit)
            {
                return;
            }
            this.remote_commit_state = match result {
                Ok(update) => RemoteCommitState::Updated(update),
                Err(error) => RemoteCommitState::Failed(format!(
                    "Remote branch update failed or is uncertain: {error}. No automatic retry was attempted; refresh the PR and branch first."
                )),
            };
            cx.notify();
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_suggestion_candidate_is_a_single_file_diff_without_checkout_writes() {
        let head = "a".repeat(40);
        let comment = github_pr::PullRequestComment {
            id: 7,
            path: "src/lib.rs".to_string(),
            line: Some(2),
            original_line: Some(2),
            body: "Please use the checked call.\n```suggestion\nchecked_call()\n```".to_string(),
            author: "reviewer".to_string(),
            html_url: "https://github.com/octo/repo/pull/1#discussion_r7".to_string(),
            commit_id: head.clone(),
            original_commit_id: head.clone(),
            in_reply_to_id: None,
            is_outdated: false,
            subject_type: Some("line".to_string()),
            side: Some("RIGHT".to_string()),
        };
        let snapshot = github_pr::PullRequestSnapshot {
            reference: github_pr::PullRequestRef::parse("octo/repo#1").unwrap(),
            title: "Review".to_string(),
            state: "open".to_string(),
            author: "author".to_string(),
            html_url: "https://github.com/octo/repo/pull/1".to_string(),
            base_sha: "b".repeat(40),
            head_sha: head.clone(),
            head_repository: Some(github_pr::GitHubRepositoryRef {
                owner: "octo".to_string(),
                repository: "repo".to_string(),
            }),
            head_branch: Some("feature/change".to_string()),
            files: vec![cururu_core::ChangedFile {
                path: "src/lib.rs".to_string(),
                patch: "@@ -1,2 +1,2 @@\n safe_call()\n unsafe_call()\n".to_string(),
                right_lines: vec![1, 2],
            }],
            reviews: Vec::new(),
            review_threads: vec![github_pr::PullRequestReviewThread {
                root: comment.clone(),
                replies: Vec::new(),
            }],
            conversation_comments: Vec::new(),
        };
        let source = github_pr::PullRequestSourceFile {
            path: "src/lib.rs".to_string(),
            commit_sha: head,
            content: "safe_call()\nunsafe_call()\n".to_string(),
        };

        let candidate = suggestion_candidate(&snapshot, &comment, &source).unwrap();

        assert_eq!(candidate.origin, "GitHub suggestion");
        assert_eq!(candidate.file.mode, "100644");
        assert_eq!(
            candidate.file.content.as_deref(),
            Some("safe_call()\nchecked_call()\n")
        );
        assert!(candidate.diff.contains("-unsafe_call()"));
        assert!(candidate.diff.contains("+checked_call()"));
    }
}
