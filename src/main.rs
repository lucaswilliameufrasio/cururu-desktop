use anyhow::Context as _;
use gpui::{
    App, Application, BackgroundExecutor, Bounds, Context, Entity, PathPromptOptions, Render,
    ScrollStrategy, SharedString, UniformListScrollHandle, Window, WindowBounds, WindowOptions,
    div, prelude::*, px, rgb, size, uniform_list,
};
use gpui_component::input::{InputState, TextInput};
use gpui_component::{
    Disableable as _,
    button::{Button, ButtonVariants as _},
};
use std::{
    fmt::Write as _,
    future::Future,
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub mod evaluator;
pub mod github_auth;
pub mod github_pr;
pub mod local_command;
pub mod local_review;
pub mod repository;
pub mod session_store;
pub mod suggested_change;

const DIFF_LINE_COUNT: usize = 10_000;
const LINE_HEIGHT: f32 = 24.0;
const SAMPLE_FILE_PATHS: [&str; 4] = [
    "src/engine.rs",
    "src/parser.rs",
    "src/model.rs",
    "tests/engine.rs",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffKind {
    Context,
    Added,
    Removed,
}

struct DiffLine {
    old_number: Option<usize>,
    new_number: Option<usize>,
    kind: DiffKind,
    text: SharedString,
}

struct DiffFile {
    path: SharedString,
    lines: Vec<DiffLine>,
    added: usize,
    removed: usize,
}

struct ReviewWorkspace {
    files: Vec<DiffFile>,
    repository_root: Option<SharedString>,
    branch: Option<SharedString>,
    load_error: Option<SharedString>,
    notice: Option<SharedString>,
    is_opening_repository: bool,
    is_saving_draft: bool,
    deleting_draft_id: Option<i64>,
    session_store: Option<session_store::SessionStore>,
    last_session: Option<session_store::StoredSession>,
    session_store_error: Option<SharedString>,
    head_oid: Option<String>,
    local_repository_snapshot: Option<repository::RepositorySnapshot>,
    local_review_identity: Option<String>,
    local_review_authorized_for: Option<(String, String)>,
    local_review_authorization_error: Option<SharedString>,
    local_review_settings: Option<session_store::LocalReviewSettings>,
    local_review_endpoint_input: Option<Entity<InputState>>,
    local_review_model_input: Option<Entity<InputState>>,
    local_review_key_input: Option<Entity<InputState>>,
    local_review_state: LocalReviewState,
    next_local_review_request_id: u64,
    local_command_input: Option<Entity<InputState>>,
    local_command_state: LocalCommandState,
    next_local_command_request_id: u64,
    suggested_change_state: SuggestedChangeState,
    next_suggestion_request_id: u64,
    comment_drafts: Vec<session_store::StoredCommentDraft>,
    editing_draft: Option<session_store::StoredCommentDraft>,
    draft_input: Option<Entity<InputState>>,
    github_app_client_id: Option<String>,
    github_auth_state: GitHubAuthState,
    github_credentials: Option<github_auth::StoredCredentials>,
    pull_request_input: Option<Entity<InputState>>,
    remote_review_state: RemoteReviewState,
    remote_source_context: RemoteSourceContextState,
    remote_reply_input: Option<Entity<InputState>>,
    thread_reply_state: ThreadReplyState,
    review_mode: ReviewMode,
    selected_remote_comment: Option<github_pr::PullRequestComment>,
    evaluator_settings: Option<session_store::EvaluatorSettings>,
    evaluator_notice: Option<SharedString>,
    evaluator_endpoint_input: Option<Entity<InputState>>,
    evaluator_openai_model_input: Option<Entity<InputState>>,
    evaluator_jev_model_input: Option<Entity<InputState>>,
    evaluator_openai_key_input: Option<Entity<InputState>>,
    evaluator_jev_key_input: Option<Entity<InputState>>,
    evaluator_result: Option<evaluator::CombinedReviewAnalysis>,
    evaluator_result_comment_id: Option<u64>,
    is_evaluator_running: bool,
    evaluator_authorized_repository: Option<String>,
    active_evaluator_context: Option<EvaluatorRequestContext>,
    next_evaluator_request_id: u64,
    selected_file: Option<usize>,
    selected_line: usize,
    diff_scroll_handle: UniformListScrollHandle,
}

#[derive(Clone)]
struct EvaluatorSettingsInputs {
    endpoint: Option<Entity<InputState>>,
    openai_model: Option<Entity<InputState>>,
    jev_model: Option<Entity<InputState>>,
    openai_key: Option<Entity<InputState>>,
    jev_key: Option<Entity<InputState>>,
}

#[derive(Clone)]
struct LocalReviewSettingsInputs {
    endpoint: Option<Entity<InputState>>,
    model: Option<Entity<InputState>>,
    api_key: Option<Entity<InputState>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EvaluatorRequestContext {
    request_id: u64,
    repository_identity: String,
    comment_id: u64,
    config_fingerprint: String,
}

#[derive(Clone)]
struct EvaluatorAnalysisRequest {
    settings: session_store::EvaluatorSettings,
    repository_identity: String,
    config_fingerprint: String,
    input: evaluator::ReviewAnalysisInput,
    comment_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalReviewRequestContext {
    request_id: u64,
    repository_identity: String,
    config_fingerprint: String,
    snapshot_fingerprint: String,
}

#[derive(Clone)]
struct LocalReviewRequest {
    context: LocalReviewRequestContext,
    repository_root: PathBuf,
    settings: session_store::LocalReviewSettings,
}

#[derive(Clone)]
struct PendingLocalCommand {
    request_id: u64,
    plan: local_command::CommandPlan,
}

#[derive(Clone)]
struct SuggestedChangeRequest {
    request_id: u64,
    repository_root: PathBuf,
    repository_identity: String,
    reviewed_diff_fingerprint: String,
    finding: cururu_core::ReviewFinding,
}

#[derive(Clone)]
struct PendingSuggestedChange {
    request_id: u64,
    preview: suggested_change::Preview,
}

enum SuggestedChangeState {
    Idle,
    Preparing(u64),
    Preview(PendingSuggestedChange),
    Applying(PendingSuggestedChange),
    Applied { path: String, line: u32 },
    Failed(SharedString),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SuggestedChangePreviewDecision {
    IgnoreBusy,
    RejectStale,
    Start,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingThreadReply {
    reference: github_pr::PullRequestRef,
    pull_request_title: String,
    pull_request_url: String,
    expected_head_sha: String,
    target_comment: github_pr::PullRequestComment,
    body: String,
}

struct ThreadReplyComposer<'a> {
    snapshot: &'a github_pr::PullRequestSnapshot,
    comment: &'a github_pr::PullRequestComment,
    input: &'a Entity<InputState>,
    body: String,
    validation_message: Option<String>,
    access_token: Option<String>,
}

struct ThreadReplyComposerStatus {
    is_confirming: bool,
    failed_for_current_body: bool,
    already_published: bool,
    element: Option<gpui::Div>,
}

enum ThreadReplyState {
    Idle,
    Confirming(PendingThreadReply),
    Publishing(PendingThreadReply),
    Published {
        pending: PendingThreadReply,
        comment_url: String,
    },
    Failed {
        pending: PendingThreadReply,
        message: SharedString,
    },
}

enum LocalReviewState {
    Idle,
    SavingSettings,
    Authorizing {
        repository_identity: String,
        config_fingerprint: String,
    },
    Revoking {
        repository_identity: String,
    },
    Running(LocalReviewRequestContext),
    Complete(local_review::LocalReviewResult),
    Failed(SharedString),
}

enum LocalCommandState {
    Idle,
    Preparing(u64),
    Confirming(PendingLocalCommand),
    ConfirmingUnsandboxed(PendingLocalCommand),
    Running {
        pending: PendingLocalCommand,
        cancelled: Arc<AtomicBool>,
    },
    Complete {
        pending: PendingLocalCommand,
        output: local_command::CommandOutput,
    },
    Failed(SharedString),
}

enum GitHubAuthState {
    Uninitialized,
    CheckingCredentials,
    SignedOut,
    StartingDeviceFlow,
    AwaitingDeviceAuthorization {
        user_code: String,
        verification_url: String,
        expires_at: Instant,
        cancel: Arc<AtomicBool>,
    },
    RefreshingToken,
    SavingCredentials,
    SignedIn,
    ReauthorizationRequired,
    Disconnecting,
    Failed(SharedString),
}

enum RemoteReviewState {
    Idle,
    Loading(github_pr::PullRequestRef),
    Loaded(Box<github_pr::PullRequestSnapshot>),
    Failed {
        reference: github_pr::PullRequestRef,
        message: SharedString,
    },
}

enum RemoteSourceContextState {
    Idle,
    Loading {
        comment_id: u64,
        path: String,
        commit_sha: String,
    },
    Loaded {
        comment_id: u64,
        source: github_pr::PullRequestSourceFile,
        anchor_line: Option<u32>,
    },
    Failed {
        comment_id: u64,
        path: String,
        commit_sha: String,
        message: SharedString,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewMode {
    ReviewChanges,
    UnderstandReview,
}

impl RemoteReviewState {
    const fn blocks_local_drafts(&self) -> bool {
        matches!(self, Self::Loading(_) | Self::Loaded(_))
    }
}

fn remote_review_status(state: &RemoteReviewState) -> String {
    match state {
        RemoteReviewState::Loading(reference) => {
            format!("Loading {}#{}…", reference.owner, reference.number)
        }
        RemoteReviewState::Loaded(snapshot) => format!(
            "Loaded {}#{} · {} files · {} reviews · {} inline threads · {} conversation comments",
            snapshot.reference.owner,
            snapshot.reference.number,
            snapshot.files.len(),
            snapshot.reviews.len(),
            snapshot.review_threads.len(),
            snapshot.conversation_comments.len()
        ),
        RemoteReviewState::Failed { reference, message } => format!(
            "Could not load {}#{}: {message}",
            reference.owner, reference.number
        ),
        RemoteReviewState::Idle => {
            "Read-only GitHub PR review · content is not sent to an evaluator.".to_string()
        }
    }
}

impl ReviewWorkspace {
    fn sample() -> Self {
        let diff = sample_unified_diff();
        let files = cururu_core::parse_unified_diff(&diff)
            .into_iter()
            .map(parse_diff_file)
            .collect();

        Self {
            files,
            repository_root: None,
            branch: None,
            load_error: None,
            notice: None,
            is_opening_repository: false,
            is_saving_draft: false,
            deleting_draft_id: None,
            session_store: None,
            last_session: None,
            session_store_error: None,
            head_oid: None,
            local_repository_snapshot: None,
            local_review_identity: None,
            local_review_authorized_for: None,
            local_review_authorization_error: None,
            local_review_settings: None,
            local_review_endpoint_input: None,
            local_review_model_input: None,
            local_review_key_input: None,
            local_review_state: LocalReviewState::Idle,
            next_local_review_request_id: 0,
            local_command_input: None,
            local_command_state: LocalCommandState::Idle,
            next_local_command_request_id: 0,
            suggested_change_state: SuggestedChangeState::Idle,
            next_suggestion_request_id: 0,
            comment_drafts: Vec::new(),
            editing_draft: None,
            draft_input: None,
            github_app_client_id: std::env::var("CURURU_GITHUB_APP_CLIENT_ID").ok(),
            github_auth_state: GitHubAuthState::Uninitialized,
            github_credentials: None,
            pull_request_input: None,
            remote_review_state: RemoteReviewState::Idle,
            remote_source_context: RemoteSourceContextState::Idle,
            remote_reply_input: None,
            thread_reply_state: ThreadReplyState::Idle,
            review_mode: ReviewMode::ReviewChanges,
            selected_remote_comment: None,
            evaluator_settings: None,
            evaluator_notice: None,
            evaluator_endpoint_input: None,
            evaluator_openai_model_input: None,
            evaluator_jev_model_input: None,
            evaluator_openai_key_input: None,
            evaluator_jev_key_input: None,
            evaluator_result: None,
            evaluator_result_comment_id: None,
            is_evaluator_running: false,
            evaluator_authorized_repository: None,
            active_evaluator_context: None,
            next_evaluator_request_id: 0,
            selected_file: Some(0),
            selected_line: 42,
            diff_scroll_handle: UniformListScrollHandle::new(),
        }
    }

    fn from_cli() -> Self {
        let mut workspace = Self::sample();
        match session_store::SessionStore::open_default() {
            Ok(store) => {
                match store.evaluator_settings() {
                    Ok(settings) => workspace.evaluator_settings = settings,
                    Err(error) => {
                        workspace.evaluator_notice =
                            Some(format!("Could not read evaluator settings: {error}").into());
                    }
                }
                match store.local_review_settings() {
                    Ok(settings) => workspace.local_review_settings = settings,
                    Err(error) => {
                        workspace.local_review_authorization_error =
                            Some(format!("Could not read local review settings: {error}").into());
                    }
                }
                match store.most_recent() {
                    Ok(session) => workspace.last_session = session,
                    Err(error) => {
                        workspace.session_store_error =
                            Some(format!("Could not read saved sessions: {error}").into());
                    }
                }
                workspace.session_store = Some(store);
            }
            Err(error) => {
                workspace.session_store_error =
                    Some(format!("Local session storage is unavailable: {error}").into());
            }
        }
        if let Some(path) = std::env::args_os().nth(1) {
            let result = load_repository_session(path.into(), workspace.session_store.as_ref());
            workspace.apply_repository_result(result);
        }
        workspace
    }

    fn apply_repository_result(&mut self, result: anyhow::Result<OpenedRepository>) {
        self.is_opening_repository = false;
        match result {
            Ok(opened) => {
                self.cancel_local_command_run();
                self.local_command_state = LocalCommandState::Idle;
                let snapshot = opened.snapshot;
                self.remote_review_state = RemoteReviewState::Idle;
                self.remote_source_context = RemoteSourceContextState::Idle;
                self.head_oid.clone_from(&snapshot.head_oid);
                let files = snapshot
                    .changed_files
                    .iter()
                    .cloned()
                    .map(parse_diff_file)
                    .collect::<Vec<_>>();
                let selected_file = (!files.is_empty()).then_some(0);
                self.files = files;
                self.repository_root = Some(snapshot.root.display().to_string().into());
                self.branch = snapshot.branch.clone().map(Into::into);
                self.local_repository_snapshot = Some(snapshot);
                self.local_review_identity = opened.local_review_identity;
                self.local_review_authorized_for = opened.local_review_authorized_for;
                self.local_review_authorization_error =
                    opened.local_review_authorization_error.map(Into::into);
                self.local_review_state = LocalReviewState::Idle;
                self.suggested_change_state = SuggestedChangeState::Idle;
                self.load_error = None;
                self.notice = Some("Repository opened · local access is read-only".into());
                if let Some(session_result) = opened.session_result {
                    match session_result {
                        Ok(session) => {
                            self.last_session = Some(session);
                            self.session_store_error = None;
                        }
                        Err(error) => {
                            self.session_store_error = Some(
                                format!("Repository opened, but session was not saved: {error}")
                                    .into(),
                            );
                        }
                    }
                }
                if let Some(drafts_result) = opened.drafts_result {
                    match drafts_result {
                        Ok(drafts) => {
                            self.comment_drafts = drafts;
                            self.editing_draft = None;
                        }
                        Err(error) => {
                            self.session_store_error = Some(
                                format!(
                                    "Repository opened, but drafts could not be loaded: {error}"
                                )
                                .into(),
                            );
                        }
                    }
                }
                self.selected_file = selected_file;
                self.selected_line = 0;
            }
            Err(error) => {
                let message = format!("Could not open repository: {error}");
                self.load_error = Some(message.clone().into());
                self.notice = Some(message.into());
            }
        }
    }

    fn select_file(&mut self, index: usize) {
        if index < self.files.len() {
            self.selected_file = Some(index);
            self.selected_line = 0;
            self.editing_draft = None;
        }
    }

    fn focus_remote_comment(&mut self, comment: &github_pr::PullRequestComment) -> bool {
        let Some((file_index, line_index)) = locate_remote_comment(&self.files, comment) else {
            self.notice =
                Some("This comment's original line is not present in the loaded diff.".into());
            return false;
        };
        self.selected_file = Some(file_index);
        self.selected_line = line_index;
        self.editing_draft = None;
        self.diff_scroll_handle
            .scroll_to_item(line_index, ScrollStrategy::Center);
        self.notice = Some(
            format!(
                "Focused {}:{} from the selected review thread.",
                comment.path,
                comment.line.or(comment.original_line).unwrap_or_default()
            )
            .into(),
        );
        true
    }

    fn begin_source_context_load(&mut self, comment: &github_pr::PullRequestComment) {
        self.remote_source_context = RemoteSourceContextState::Loading {
            comment_id: comment.id,
            path: comment.path.clone(),
            commit_sha: comment.original_commit_id.clone(),
        };
    }

    fn select_review_mode(&mut self, mode: ReviewMode) {
        if self.review_mode != mode && mode == ReviewMode::UnderstandReview {
            self.cancel_local_command_run();
            if !matches!(self.local_command_state, LocalCommandState::Running { .. }) {
                self.local_command_state = LocalCommandState::Idle;
            }
        }
        self.review_mode = mode;
        self.notice = Some(match mode {
            ReviewMode::ReviewChanges => "Review changes mode selected.".into(),
            ReviewMode::UnderstandReview => "Understand a review mode selected.".into(),
        });
    }

    fn cancel_local_command_run(&self) {
        if let LocalCommandState::Running { cancelled, .. } = &self.local_command_state {
            cancelled.store(true, Ordering::Release);
        }
    }
}

impl Drop for ReviewWorkspace {
    fn drop(&mut self) {
        self.cancel_local_command_run();
    }
}

fn locate_remote_comment(
    files: &[DiffFile],
    comment: &github_pr::PullRequestComment,
) -> Option<(usize, usize)> {
    let file_index = files
        .iter()
        .position(|file| file.path.as_ref() == comment.path)?;
    let file = &files[file_index];
    let line_number = comment.line.or(comment.original_line)?;
    let line_number = usize::try_from(line_number).ok()?;
    let is_left_side = comment.side.as_deref() == Some("LEFT");
    let line_index = file.lines.iter().position(|line| {
        if is_left_side {
            line.old_number == Some(line_number)
        } else {
            line.new_number == Some(line_number)
        }
    })?;
    Some((file_index, line_index))
}

struct OpenedRepository {
    snapshot: repository::RepositorySnapshot,
    session_result: Option<anyhow::Result<session_store::StoredSession>>,
    drafts_result: Option<anyhow::Result<Vec<session_store::StoredCommentDraft>>>,
    local_review_identity: Option<String>,
    local_review_authorized_for: Option<(String, String)>,
    local_review_authorization_error: Option<String>,
}

struct DraftSave {
    store: session_store::SessionStore,
    repository_root: PathBuf,
    base_head_oid: Option<String>,
    current_head_oid: Option<String>,
    draft: cururu_core::ReviewCommentDraft,
    expected_revision: Option<i64>,
}

fn load_repository_session(
    path: PathBuf,
    store: Option<&session_store::SessionStore>,
) -> anyhow::Result<OpenedRepository> {
    let snapshot = repository::RepositorySnapshot::open(path)?;
    let (local_review_identity, identity_error) =
        match local_review::repository_identity(&snapshot.root) {
            Ok(identity) => (Some(identity), None),
            Err(error) => (None, Some(error.to_string())),
        };
    let (session_result, drafts_result) = match store.as_ref() {
        Some(store) => {
            let session_result = store.record_opened(
                &snapshot.root,
                snapshot.branch.as_deref(),
                snapshot.head_oid.as_deref(),
            );
            let drafts_result = store.comment_drafts(&snapshot.root, snapshot.head_oid.as_deref());
            (Some(session_result), Some(drafts_result))
        }
        None => (None, None),
    };
    let (local_review_authorized_for, local_review_authorization_error) =
        match (store.as_ref(), local_review_identity.as_deref()) {
            (Some(store), Some(identity)) => match store.local_review_settings() {
                Ok(Some(settings)) => match local_review::config_fingerprint(&settings) {
                    Ok(fingerprint) => {
                        match store.local_review_is_authorized(identity, &fingerprint) {
                            Ok(true) => (Some((identity.to_string(), fingerprint)), None),
                            Ok(false) => (None, None),
                            Err(error) => (None, Some(error.to_string())),
                        }
                    }
                    Err(error) => (None, Some(error.to_string())),
                },
                Ok(None) => (None, None),
                Err(error) => (None, Some(error.to_string())),
            },
            (_, None) => (None, identity_error),
            (None, Some(_)) => (None, None),
        };
    Ok(OpenedRepository {
        snapshot,
        session_result,
        drafts_result,
        local_review_identity,
        local_review_authorized_for,
        local_review_authorization_error,
    })
}

fn start_repository_picker(
    cx: &App,
    entity: Entity<ReviewWorkspace>,
    store: Option<session_store::SessionStore>,
) {
    let receiver = cx.prompt_for_paths(PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some("Open repository".into()),
    });
    cx.spawn(async move |cx| {
        let paths = match receiver.await {
            Ok(Ok(Some(paths))) => paths,
            Ok(Ok(None)) => {
                finish_repository_picker(&entity, cx, None);
                return;
            }
            Ok(Err(error)) => {
                finish_repository_picker(
                    &entity,
                    cx,
                    Some(format!("Folder picker failed: {error}")),
                );
                return;
            }
            Err(error) => {
                finish_repository_picker(
                    &entity,
                    cx,
                    Some(format!("Folder picker did not complete: {error}")),
                );
                return;
            }
        };
        let Some(path) = paths.into_iter().next() else {
            finish_repository_picker(&entity, cx, None);
            return;
        };

        let _ = entity.update(cx, |this, cx| {
            this.notice = Some("Scanning repository changes…".into());
            cx.notify();
        });
        let result = cx
            .background_executor()
            .spawn(async move { load_repository_session(path, store.as_ref()) })
            .await;
        let _ = entity.update(cx, |this, cx| {
            this.apply_repository_result(result);
            cx.notify();
        });
    })
    .detach();
}

fn start_reopen_last_session(
    cx: &App,
    entity: Entity<ReviewWorkspace>,
    path: PathBuf,
    store: Option<session_store::SessionStore>,
) {
    cx.spawn(async move |cx| {
        let _ = entity.update(cx, |this, cx| {
            this.notice = Some("Scanning repository changes…".into());
            cx.notify();
        });
        let result = cx
            .background_executor()
            .spawn(async move { load_repository_session(path, store.as_ref()) })
            .await;
        let _ = entity.update(cx, |this, cx| {
            this.apply_repository_result(result);
            cx.notify();
        });
    })
    .detach();
}

fn finish_repository_picker(
    entity: &Entity<ReviewWorkspace>,
    cx: &mut gpui::AsyncApp,
    notice: Option<String>,
) {
    let _ = entity.update(cx, |this, cx| {
        this.is_opening_repository = false;
        this.notice = notice.map(Into::into);
        cx.notify();
    });
}

fn parse_diff_file(file: cururu_core::ChangedFile) -> DiffFile {
    let mut lines = Vec::with_capacity(file.right_lines.len());
    let mut old_number = 1;
    let mut right_lines = file.right_lines.into_iter();
    let mut added = 0;
    let mut removed = 0;

    for raw_line in file.patch.lines() {
        if raw_line.starts_with("diff --git ")
            || raw_line.starts_with("--- ")
            || raw_line.starts_with("+++ ")
        {
            continue;
        }
        if let Some(start) = hunk_old_start(raw_line) {
            old_number = start;
            continue;
        }
        let Some(marker) = raw_line.chars().next() else {
            continue;
        };
        let kind = match marker {
            ' ' => DiffKind::Context,
            '+' => DiffKind::Added,
            '-' => DiffKind::Removed,
            _ => continue,
        };
        let old = match kind {
            DiffKind::Context | DiffKind::Removed => {
                let number = Some(old_number);
                old_number += 1;
                number
            }
            DiffKind::Added => None,
        };
        let new = match kind {
            DiffKind::Context | DiffKind::Added => right_lines.next().map(|line| line as usize),
            DiffKind::Removed => None,
        };
        if kind == DiffKind::Added {
            added += 1;
        } else if kind == DiffKind::Removed {
            removed += 1;
        }
        lines.push(DiffLine {
            old_number: old,
            new_number: new,
            kind,
            text: raw_line.to_string().into(),
        });
    }

    DiffFile {
        path: file.path.into(),
        lines,
        added,
        removed,
    }
}

fn hunk_old_start(header: &str) -> Option<usize> {
    let range = header
        .strip_prefix("@@ ")?
        .split_whitespace()
        .find(|range| range.starts_with('-'))?
        .strip_prefix('-')?;
    range.split(',').next()?.parse().ok()
}

fn sample_unified_diff() -> String {
    let mut diff = String::new();
    let rows_per_file = DIFF_LINE_COUNT / SAMPLE_FILE_PATHS.len();
    let extra_rows = DIFF_LINE_COUNT % SAMPLE_FILE_PATHS.len();

    for (file_index, path) in SAMPLE_FILE_PATHS.iter().enumerate() {
        let row_count = rows_per_file + usize::from(file_index < extra_rows);
        let mut body = String::new();
        let mut old_count = 0;
        let mut new_count = 0;

        for index in 0..row_count {
            let (marker, source) = match (index + file_index) % 11 {
                3 | 4 => {
                    old_count += 1;
                    ('-', "let result = calculate_legacy(input);")
                }
                5 | 6 => {
                    new_count += 1;
                    ('+', "let result = calculate_checked(input)?;")
                }
                _ => {
                    old_count += 1;
                    new_count += 1;
                    (' ', "let result = calculate(input);")
                }
            };
            writeln!(body, "{marker}{source} // {path} row {index}")
                .expect("writing a line into a String cannot fail");
        }

        writeln!(
            diff,
            "diff --git a/{path} b/{path}\nindex 0000000..1111111 100644\n--- a/{path}\n+++ b/{path}\n@@ -1,{old_count} +1,{new_count} @@"
        )
        .expect("writing file metadata into a String cannot fail");
        diff.push_str(&body);
    }

    diff
}

impl ReviewWorkspace {
    fn ensure_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.draft_input.is_none() {
            self.draft_input = Some(cx.new(|input_cx| {
                InputState::new(window, input_cx)
                    .multi_line()
                    .rows(5)
                    .placeholder("Write a local review comment…")
            }));
        }
        self.ensure_local_command_input(window, cx);
        if self.pull_request_input.is_none() {
            self.pull_request_input = Some(cx.new(|input_cx| {
                InputState::new(window, input_cx).placeholder("owner/repository#123")
            }));
        }
        if self.remote_reply_input.is_none() {
            self.remote_reply_input = Some(cx.new(|input_cx| {
                InputState::new(window, input_cx)
                    .multi_line()
                    .rows(4)
                    .placeholder("Write a reply to this GitHub review thread…")
            }));
        }
        if self.local_review_endpoint_input.is_none() {
            let value = self.local_review_settings.as_ref().map_or_else(
                || local_review::DEFAULT_LOCAL_REVIEW_ENDPOINT.to_string(),
                |settings| settings.openai_endpoint.clone(),
            );
            self.local_review_endpoint_input = Some(cx.new(|input_cx| {
                InputState::new(window, input_cx)
                    .placeholder("https://api.openai.com/v1")
                    .default_value(value)
            }));
        }
        if self.local_review_model_input.is_none() {
            let value = self
                .local_review_settings
                .as_ref()
                .map_or_else(String::new, |settings| settings.openai_model.clone());
            self.local_review_model_input = Some(cx.new(|input_cx| {
                InputState::new(window, input_cx)
                    .placeholder("OpenAI-compatible model")
                    .default_value(value)
            }));
        }
        if self.local_review_key_input.is_none() {
            self.local_review_key_input = Some(cx.new(|input_cx| {
                InputState::new(window, input_cx)
                    .placeholder("API key · stored only in OS keyring")
                    .masked(true)
            }));
        }
        if self.evaluator_endpoint_input.is_none() {
            let value = self
                .evaluator_settings
                .as_ref()
                .map_or_else(String::new, |settings| settings.openai_endpoint.clone());
            self.evaluator_endpoint_input = Some(cx.new(|input_cx| {
                InputState::new(window, input_cx)
                    .placeholder("https://provider.example/v1/chat/completions")
                    .default_value(value)
            }));
        }
        if self.evaluator_openai_model_input.is_none() {
            let value = self
                .evaluator_settings
                .as_ref()
                .map_or_else(String::new, |settings| settings.openai_model.clone());
            self.evaluator_openai_model_input = Some(cx.new(|input_cx| {
                InputState::new(window, input_cx)
                    .placeholder("OpenAI-compatible model (required)")
                    .default_value(value)
            }));
        }
        if self.evaluator_jev_model_input.is_none() {
            let value = self
                .evaluator_settings
                .as_ref()
                .map_or_else(String::new, |settings| settings.jev_model.clone());
            self.evaluator_jev_model_input = Some(cx.new(|input_cx| {
                InputState::new(window, input_cx)
                    .placeholder("Jev model (required)")
                    .default_value(value)
            }));
        }
        if self.evaluator_openai_key_input.is_none() {
            self.evaluator_openai_key_input = Some(cx.new(|input_cx| {
                InputState::new(window, input_cx)
                    .placeholder("OpenAI-compatible API key")
                    .masked(true)
            }));
        }
        if self.evaluator_jev_key_input.is_none() {
            self.evaluator_jev_key_input = Some(cx.new(|input_cx| {
                InputState::new(window, input_cx)
                    .placeholder("Jev API key")
                    .masked(true)
            }));
        }
    }

    fn ensure_local_command_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.local_command_input.is_none() {
            self.local_command_input = Some(cx.new(|input_cx| {
                InputState::new(window, input_cx).placeholder("cargo test --locked --workspace")
            }));
        }
    }
}

impl Render for ReviewWorkspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_github_credentials_loaded(cx);
        self.ensure_inputs(window, cx);
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x0011_1820))
            .text_color(rgb(0x00d5_dee8))
            .child(self.render_header(cx))
            .child(self.render_mode_switch(cx))
            .children(
                (self.review_mode == ReviewMode::ReviewChanges
                    && matches!(self.remote_review_state, RemoteReviewState::Idle))
                .then(|| self.render_local_review_settings(cx)),
            )
            .child(self.render_github_auth_status())
            .child(self.render_pull_request_loader(cx))
            .children(
                (self.review_mode == ReviewMode::UnderstandReview)
                    .then(|| self.render_evaluator_settings(cx)),
            )
            .children(self.notice.clone().map(|notice| {
                div()
                    .px_4()
                    .py_2()
                    .text_color(rgb(0x00b8_dfc4))
                    .child(notice)
            }))
            .children(self.session_store_error.clone().map(|error| {
                div()
                    .px_4()
                    .py_2()
                    .text_color(rgb(0x00f0_a090))
                    .child(error)
            }))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_file_rail(cx))
                    .child(self.render_diff(cx))
                    .child(self.render_finding_panel(cx)),
            )
    }
}

impl ReviewWorkspace {
    fn render_local_review_settings(&self, cx: &Context<Self>) -> impl IntoElement {
        let endpoint = self
            .local_review_endpoint_input
            .as_ref()
            .map(TextInput::new);
        let model = self.local_review_model_input.as_ref().map(TextInput::new);
        let api_key = self.local_review_key_input.as_ref().map(TextInput::new);
        let inputs = LocalReviewSettingsInputs {
            endpoint: self.local_review_endpoint_input.clone(),
            model: self.local_review_model_input.clone(),
            api_key: self.local_review_key_input.clone(),
        };
        let entity = cx.entity();
        let is_saving = matches!(self.local_review_state, LocalReviewState::SavingSettings);
        let save_label = if is_saving {
            "Saving local review settings…"
        } else {
            "Save provider settings"
        };
        let save_button = div()
            .id("save-local-review-settings")
            .px_3()
            .py_2()
            .bg(rgb(if is_saving { 0x001b_2732 } else { 0x0026_3442 }))
            .child(save_label);
        let save_button = if is_saving {
            save_button
        } else {
            save_button
                .cursor_pointer()
                .on_click(move |_event, window, cx| {
                    save_local_review_configuration(&entity, inputs.clone(), window, cx);
                })
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .px_4()
            .py_3()
            .border_b_1()
            .border_color(rgb(0x002b_3946))
            .child("Local review · only changed-file diffs are sent")
            .child("Provider base URL (HTTPS)")
            .children(endpoint)
            .child("Model")
            .children(model)
            .child("API key · stored only in the OS keyring")
            .children(api_key)
            .child(save_button)
            .child(
                div()
                    .text_color(rgb(0x009b_aaba))
                    .child("Saving settings sends no code. Authorization and each review are separate actions."),
            )
            .children(
                local_review_state_message(&self.local_review_state).map(|message| {
                    div()
                        .text_color(rgb(local_review_state_color(
                            &self.local_review_state,
                        )))
                        .child(message)
                }),
            )
            .children(
                self.local_review_authorization_error
                    .clone()
                    .map(|error| div().text_color(rgb(0x00e5_b6ba)).child(error)),
            )
    }

    fn render_evaluator_settings(&self, cx: &Context<Self>) -> impl IntoElement {
        let endpoint = self.evaluator_endpoint_input.as_ref().map(TextInput::new);
        let openai_model = self
            .evaluator_openai_model_input
            .as_ref()
            .map(TextInput::new);
        let jev_model = self.evaluator_jev_model_input.as_ref().map(TextInput::new);
        let openai_key = self.evaluator_openai_key_input.as_ref().map(TextInput::new);
        let jev_key = self.evaluator_jev_key_input.as_ref().map(TextInput::new);
        let entity = cx.entity();
        let inputs = EvaluatorSettingsInputs {
            endpoint: self.evaluator_endpoint_input.clone(),
            openai_model: self.evaluator_openai_model_input.clone(),
            jev_model: self.evaluator_jev_model_input.clone(),
            openai_key: self.evaluator_openai_key_input.clone(),
            jev_key: self.evaluator_jev_key_input.clone(),
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(rgb(0x002b_3946))
            .child("Second opinion · both providers receive the selected review context")
            .children(endpoint)
            .children(openai_model)
            .children(jev_model)
            .children(openai_key)
            .children(jev_key)
            .child(
                div()
                    .id("save-evaluator-configuration")
                    .px_3()
                    .py_2()
                    .bg(rgb(0x0026_3442))
                    .cursor_pointer()
                    .child("Save both providers (keys stored in OS keyring)")
                    .on_click(move |_event, window, cx| {
                        save_evaluator_configuration(&entity, inputs.clone(), window, cx);
                    }),
            )
            .children(
                self.evaluator_notice
                    .clone()
                    .map(|notice| div().text_color(rgb(0x00b8_dfc4)).child(notice)),
            )
    }

    fn render_mode_switch(&self, cx: &Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let modes = [
            (ReviewMode::ReviewChanges, "Review changes"),
            (ReviewMode::UnderstandReview, "Understand a review"),
        ];
        div()
            .flex()
            .items_center()
            .gap_1()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(rgb(0x002b_3946))
            .children(modes.into_iter().map(|(mode, label)| {
                let is_selected = self.review_mode == mode;
                let entity = entity.clone();
                div()
                    .id(match mode {
                        ReviewMode::ReviewChanges => "mode-review-changes",
                        ReviewMode::UnderstandReview => "mode-understand-review",
                    })
                    .px_3()
                    .py_2()
                    .bg(rgb(if is_selected {
                        0x0026_3442
                    } else {
                        0x0015_1e27
                    }))
                    .text_color(rgb(if is_selected {
                        0x00f0_f3f6
                    } else {
                        0x009b_aaba
                    }))
                    .cursor_pointer()
                    .child(label)
                    .on_click(move |_event, _window, cx| {
                        entity.update(cx, |this, cx| {
                            this.select_review_mode(mode);
                            cx.notify();
                        });
                    })
            }))
    }

    fn render_pull_request_loader(&self, cx: &Context<Self>) -> impl IntoElement {
        let input = self.pull_request_input.as_ref().map(TextInput::new);
        let input_entity = self.pull_request_input.clone();
        let is_publishing_reply =
            matches!(&self.thread_reply_state, ThreadReplyState::Publishing(_));
        let is_applying_suggestion = matches!(
            &self.suggested_change_state,
            SuggestedChangeState::Applying(_)
        );
        let is_running_local_command = matches!(
            self.local_command_state,
            LocalCommandState::Preparing(_) | LocalCommandState::Running { .. }
        );
        let is_loading = matches!(self.remote_review_state, RemoteReviewState::Loading(_))
            || is_publishing_reply
            || is_applying_suggestion
            || is_running_local_command;
        let label = remote_review_status(&self.remote_review_state);
        let entity = cx.entity();
        let http = cx.http_client();
        let load_button = Button::new("load-github-pull-request")
            .label(if is_loading {
                if is_publishing_reply {
                    "Publishing reply…"
                } else if is_applying_suggestion {
                    "Applying local suggestion…"
                } else if is_running_local_command {
                    "Finish or cancel the local command first…"
                } else {
                    "Loading pull request…"
                }
            } else {
                "Open GitHub PR"
            })
            .disabled(is_loading)
            .on_click(move |_event, _window, cx| {
                handle_open_pull_request(&entity, http.clone(), input_entity.clone(), cx);
            });
        div()
            .flex()
            .items_center()
            .gap_3()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(rgb(0x002b_3946))
            .children(input)
            .child(load_button)
            .child(div().text_color(rgb(0x009b_aaba)).child(label))
    }

    fn handle_github_auth_action(
        &mut self,
        http: Arc<dyn gpui::http_client::HttpClient>,
        cx: &mut Context<Self>,
    ) {
        let access_token_expired = self
            .github_credentials
            .as_ref()
            .is_some_and(github_auth::StoredCredentials::access_token_is_expired);
        match &self.github_auth_state {
            GitHubAuthState::AwaitingDeviceAuthorization { cancel, .. } => {
                cancel.store(true, Ordering::Release);
                self.github_auth_state = GitHubAuthState::SignedOut;
                self.notice = Some("GitHub sign-in cancelled.".into());
                cx.notify();
            }
            GitHubAuthState::SignedIn if access_token_expired => {
                let (Some(client_id), Some(credentials)) = (
                    self.github_app_client_id.clone(),
                    self.github_credentials.clone(),
                ) else {
                    self.github_auth_state = GitHubAuthState::Failed(
                        "Configure CURURU_GITHUB_APP_CLIENT_ID before refreshing GitHub.".into(),
                    );
                    cx.notify();
                    return;
                };
                self.github_auth_state = GitHubAuthState::RefreshingToken;
                start_github_refresh(cx.entity(), client_id, http, credentials, cx);
            }
            GitHubAuthState::SignedIn => {
                self.github_auth_state = GitHubAuthState::Disconnecting;
                let entity = cx.entity();
                start_github_disconnect(entity, cx);
            }
            GitHubAuthState::ReauthorizationRequired => {
                let (Some(client_id), Some(credentials)) = (
                    self.github_app_client_id.clone(),
                    self.github_credentials.clone(),
                ) else {
                    self.github_auth_state = GitHubAuthState::Failed(
                        "Configure CURURU_GITHUB_APP_CLIENT_ID before connecting GitHub.".into(),
                    );
                    cx.notify();
                    return;
                };
                self.github_auth_state = GitHubAuthState::RefreshingToken;
                start_github_refresh(cx.entity(), client_id, http, credentials, cx);
            }
            GitHubAuthState::SignedOut | GitHubAuthState::Failed(_) => {
                let Some(client_id) = self.github_app_client_id.clone() else {
                    self.github_auth_state = GitHubAuthState::Failed(
                        "Configure CURURU_GITHUB_APP_CLIENT_ID before connecting GitHub.".into(),
                    );
                    cx.notify();
                    return;
                };
                self.github_auth_state = GitHubAuthState::StartingDeviceFlow;
                start_github_device_login(cx.entity(), client_id, http, cx);
            }
            GitHubAuthState::Uninitialized
            | GitHubAuthState::CheckingCredentials
            | GitHubAuthState::StartingDeviceFlow
            | GitHubAuthState::RefreshingToken
            | GitHubAuthState::SavingCredentials
            | GitHubAuthState::Disconnecting => {}
        }
    }

    fn ensure_github_credentials_loaded(&mut self, cx: &Context<Self>) {
        if !matches!(self.github_auth_state, GitHubAuthState::Uninitialized) {
            return;
        }
        self.github_auth_state = GitHubAuthState::CheckingCredentials;
        let entity = cx.entity();
        cx.spawn(async move |_this, cx| {
            let result = cx
                .background_executor()
                .spawn(async { github_auth::CredentialStore::load() })
                .await;
            let _ = entity.update(cx, |this, cx| {
                match result {
                    Ok(Some(credentials)) => {
                        this.github_auth_state = if credentials.access_token_is_expired() {
                            GitHubAuthState::ReauthorizationRequired
                        } else {
                            GitHubAuthState::SignedIn
                        };
                        this.github_credentials = Some(credentials);
                    }
                    Ok(None) => {
                        this.github_auth_state = GitHubAuthState::SignedOut;
                    }
                    Err(error) => {
                        this.github_auth_state = GitHubAuthState::Failed(
                            format!("Could not access the OS credential store: {error}").into(),
                        );
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn render_github_auth_status(&self) -> impl IntoElement {
        let message = match &self.github_auth_state {
            GitHubAuthState::Uninitialized | GitHubAuthState::CheckingCredentials => {
                Some("Checking the operating-system credential store…".to_string())
            }
            GitHubAuthState::StartingDeviceFlow => {
                Some("Requesting a GitHub device code…".to_string())
            }
            GitHubAuthState::AwaitingDeviceAuthorization {
                user_code,
                verification_url,
                expires_at,
                ..
            } => Some(format!(
                "Enter {user_code} at {verification_url} to authorize Cururu · expires in {}s.",
                expires_at
                    .saturating_duration_since(Instant::now())
                    .as_secs()
            )),
            GitHubAuthState::RefreshingToken => Some("Refreshing the GitHub session…".to_string()),
            GitHubAuthState::SavingCredentials => {
                Some("Saving credentials to the OS keyring…".to_string())
            }
            GitHubAuthState::SignedIn => {
                Some("GitHub connected · publication still requires confirmation.".to_string())
            }
            GitHubAuthState::ReauthorizationRequired => {
                Some("GitHub session expired. Try refreshing or reconnecting.".to_string())
            }
            GitHubAuthState::Disconnecting => Some("Removing GitHub credentials…".to_string()),
            GitHubAuthState::Failed(message) => Some(message.to_string()),
            GitHubAuthState::SignedOut => Some(
                "GitHub is not connected · the app requests Contents read and Pull requests read/write."
                    .to_string(),
            ),
        };
        div()
            .px_4()
            .py_2()
            .text_color(rgb(0x009b_aaba))
            .children(message)
    }

    fn render_header(&self, cx: &Context<Self>) -> impl IntoElement {
        let subtitle = match (
            &self.remote_review_state,
            &self.repository_root,
            &self.branch,
        ) {
            (RemoteReviewState::Loaded(snapshot), _, _) => format!(
                "{}#{} · {} · {}",
                snapshot.reference.owner, snapshot.reference.number, snapshot.state, snapshot.title
            ),
            (RemoteReviewState::Loading(reference), _, _) => {
                format!("Loading {}#{}", reference.owner, reference.number)
            }
            (_, Some(root), Some(branch)) => format!("{root} · {branch} → working tree"),
            (_, Some(root), None) => format!("{root} · detached HEAD → working tree"),
            (_, None, None) => "synthetic review workspace".to_string(),
            (_, None, Some(branch)) => format!("{branch} → working tree"),
        };
        div()
            .flex()
            .items_center()
            .justify_between()
            .h(px(52.0))
            .px_4()
            .border_b_1()
            .border_color(rgb(0x002b_3946))
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(div().text_color(rgb(0x00e5_a84b)).child("CURURU"))
                    .child(div().text_color(rgb(0x009b_aaba)).child("review workspace")),
            )
            .child(div().text_color(rgb(0x009b_aaba)).child(subtitle))
            .child(self.render_header_actions(cx))
    }

    fn render_header_actions(&self, cx: &Context<Self>) -> gpui::Div {
        let entity = cx.entity();
        let store = self.session_store.clone();
        let is_applying_suggestion = matches!(
            &self.suggested_change_state,
            SuggestedChangeState::Applying(_)
        );
        let reopen_button = self.last_session.clone().map(|session| {
            let entity = entity.clone();
            let path = session.repository_root;
            let store = store.clone();
            div()
                .id("reopen-last-session")
                .px_3()
                .py_2()
                .bg(rgb(0x0020_2b36))
                .cursor_pointer()
                .child("Reopen last session")
                .on_click(move |_event, _window, cx| {
                    let entity = entity.clone();
                    let can_open = entity.update(cx, |this, cx| {
                        if this.is_opening_repository
                            || matches!(
                                &this.suggested_change_state,
                                SuggestedChangeState::Applying(_)
                            )
                        {
                            return false;
                        }
                        this.cancel_local_command_run();
                        this.is_opening_repository = true;
                        cx.notify();
                        true
                    });
                    if can_open {
                        start_reopen_last_session(cx, entity, path.clone(), store.clone());
                    }
                })
        });
        let open_button = div()
            .id("open-repository")
            .px_3()
            .py_2()
            .bg(rgb(
                if self.is_opening_repository || is_applying_suggestion {
                    0x001b_2732
                } else {
                    0x0026_3442
                },
            ))
            .cursor_pointer()
            .child(if is_applying_suggestion {
                "Applying local suggestion…"
            } else if self.is_opening_repository {
                "Opening repository…"
            } else {
                "Open repository…"
            })
            .on_click(move |_event, _window, cx| {
                let entity = entity.clone();
                let can_open = entity.update(cx, |this, cx| {
                    if this.is_opening_repository
                        || matches!(
                            &this.suggested_change_state,
                            SuggestedChangeState::Applying(_)
                        )
                    {
                        return false;
                    }
                    this.cancel_local_command_run();
                    this.is_opening_repository = true;
                    this.notice = Some("Choose a repository folder to open".into());
                    cx.notify();
                    true
                });
                if can_open {
                    start_repository_picker(cx, entity, store.clone());
                }
            });
        let auth_button = self.render_github_auth_button(cx);
        div()
            .flex()
            .gap_2()
            .children(reopen_button)
            .child(auth_button)
            .child(open_button)
    }

    fn render_github_auth_button(&self, cx: &Context<Self>) -> impl IntoElement {
        let label = match self.github_auth_state {
            GitHubAuthState::SignedIn
                if self
                    .github_credentials
                    .as_ref()
                    .is_some_and(github_auth::StoredCredentials::access_token_is_expired) =>
            {
                "Refresh GitHub session"
            }
            GitHubAuthState::SignedIn => "Disconnect GitHub",
            GitHubAuthState::AwaitingDeviceAuthorization { .. } => "Cancel sign-in",
            GitHubAuthState::StartingDeviceFlow => "Connecting…",
            GitHubAuthState::RefreshingToken => "Refreshing…",
            GitHubAuthState::SavingCredentials => "Saving credentials…",
            GitHubAuthState::Disconnecting => "Disconnecting…",
            GitHubAuthState::CheckingCredentials | GitHubAuthState::Uninitialized => {
                "Checking GitHub…"
            }
            GitHubAuthState::ReauthorizationRequired => "Refresh GitHub session",
            GitHubAuthState::SignedOut | GitHubAuthState::Failed(_) => "Connect GitHub",
        };
        let entity = cx.entity();
        let http = cx.http_client();
        div()
            .id("github-auth-action")
            .px_3()
            .py_2()
            .bg(rgb(0x0020_2b36))
            .cursor_pointer()
            .child(label)
            .on_click(move |_event, _window, cx| {
                entity.update(cx, |this, cx| {
                    this.handle_github_auth_action(http.clone(), cx);
                });
            })
    }

    fn render_file_rail(&self, cx: &Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let mut file_rows = Vec::with_capacity(self.files.len());
        for (index, file) in self.files.iter().enumerate() {
            let selected = Some(index) == self.selected_file;
            let path = file.path.clone();
            let summary = format!("+{}  −{}", file.added, file.removed);
            let entity = entity.clone();
            file_rows.push(
                div()
                    .id(("changed-file", index))
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .bg(rgb(if selected { 0x0026_3442 } else { 0x0015_1e27 }))
                    .cursor_pointer()
                    .on_click(move |_event, _window, cx| {
                        entity.update(cx, |this, cx| {
                            this.select_file(index);
                            cx.notify();
                        });
                    })
                    .child(div().text_color(rgb(0x00d5_dee8)).child(path))
                    .child(div().text_color(rgb(0x008c_a392)).child(summary)),
            );
        }
        div()
            .flex()
            .flex_col()
            .w(px(224.0))
            .flex_shrink_0()
            .border_r_1()
            .border_color(rgb(0x002b_3946))
            .bg(rgb(0x0015_1e27))
            .child(
                div()
                    .px_3()
                    .py_3()
                    .text_color(rgb(0x009b_aaba))
                    .child(format!("CHANGED FILES · {}", self.files.len())),
            )
            .children(file_rows)
    }

    fn render_diff(&self, cx: &Context<Self>) -> impl IntoElement {
        let Some(selected_file) = self.selected_file else {
            let message = self
                .load_error
                .clone()
                .unwrap_or_else(|| "No changed files in this working tree.".to_string().into());
            return div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .justify_center()
                .items_center()
                .text_color(rgb(0x009b_aaba))
                .child(message);
        };
        let entity = cx.entity();
        let file = &self.files[selected_file];
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .child(
                div()
                    .h(px(38.0))
                    .px_3()
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(rgb(0x002b_3946))
                    .child(file.path.clone()),
            )
            .child(
                uniform_list(
                    "review-diff-lines",
                    file.lines.len(),
                    cx.processor(move |this, range: Range<usize>, _window, _cx| {
                        let mut elements = Vec::with_capacity(range.len());
                        let lines = &this.files[selected_file].lines;
                        for index in range {
                            let line = &lines[index];
                            let is_selected = index == this.selected_line;
                            let (background, foreground) = match line.kind {
                                DiffKind::Context => (0x0011_1820, 0x00c5_ced8),
                                DiffKind::Added => (0x0018_3126, 0x00b8_dfc4),
                                DiffKind::Removed => (0x003a_2227, 0x00e5_b6ba),
                            };
                            let background = if is_selected { 0x0034_4558 } else { background };
                            let entity = entity.clone();
                            elements.push(
                                div()
                                    .id(index)
                                    .flex()
                                    .items_center()
                                    .h(px(LINE_HEIGHT))
                                    .bg(rgb(background))
                                    .cursor_pointer()
                                    .on_click(move |_event, _window, cx| {
                                        entity.update(cx, |this, cx| {
                                            this.selected_line = index;
                                            this.editing_draft = None;
                                            cx.notify();
                                        });
                                    })
                                    .child(div().w(px(52.0)).text_color(rgb(0x0071_8191)).child(
                                        line.old_number.map_or_else(String::new, |n| n.to_string()),
                                    ))
                                    .child(div().w(px(52.0)).text_color(rgb(0x0071_8191)).child(
                                        line.new_number.map_or_else(String::new, |n| n.to_string()),
                                    ))
                                    .child(
                                        div()
                                            .flex_1()
                                            .whitespace_nowrap()
                                            .text_color(rgb(foreground))
                                            .child(line.text.clone()),
                                    ),
                            );
                        }
                        elements
                    }),
                )
                .track_scroll(self.diff_scroll_handle.clone())
                .flex_1(),
            )
    }

    fn render_finding_panel(&self, cx: &Context<Self>) -> impl IntoElement {
        let Some(selected_file) = self.selected_file else {
            return div()
                .w(px(300.0))
                .flex_shrink_0()
                .border_l_1()
                .border_color(rgb(0x002b_3946))
                .bg(rgb(0x0015_1e27));
        };
        let file = &self.files[selected_file];
        let line = &file.lines[self.selected_line];
        let (label, title, description): (&str, SharedString, &str) = match &self
            .remote_review_state
        {
            RemoteReviewState::Loaded(snapshot) => (
                "GITHUB PULL REQUEST",
                snapshot.title.clone().into(),
                "Read-only review context · remote comments are not local drafts.",
            ),
            _ if self.repository_root.is_some() => (
                "LOCAL CHANGE",
                "Working-tree diff".into(),
                "No automated finding is attached to this line.",
            ),
            _ => (
                "SYNTHETIC FINDING · HIGH",
                "Unchecked result can escape".into(),
                "The new calculation may return an invalid value without preserving the parser error.",
            ),
        };
        let review_controls = self.render_review_controls(cx, file.path.as_ref(), line.new_number);
        div()
            .flex()
            .flex_col()
            .w(px(300.0))
            .flex_shrink_0()
            .border_l_1()
            .border_color(rgb(0x002b_3946))
            .bg(rgb(0x0015_1e27))
            .p_4()
            .gap_3()
            .child(div().text_color(rgb(0x00e5_a84b)).child(label))
            .child(div().text_lg().text_color(rgb(0x00f0_f3f6)).child(title))
            .child(div().text_color(rgb(0x00b5_c0cb)).child(description))
            .child(
                div()
                    .mt_2()
                    .border_t_1()
                    .border_color(rgb(0x002b_3946))
                    .pt_3()
                    .text_color(rgb(0x009b_aaba))
                    .child(format!("Selected diff row {}", self.selected_line + 1)),
            )
            .child(
                div()
                    .bg(rgb(0x0020_2b36))
                    .p_2()
                    .text_color(rgb(0x00b8_dfc4))
                    .child(line.text.clone()),
            )
            .child(review_controls)
            .children(
                (self.review_mode == ReviewMode::UnderstandReview)
                    .then(|| render_remote_source_context(&self.remote_source_context)),
            )
            .child(
                div()
                    .mt_2()
                    .text_color(rgb(0x0078_8a9b))
                    .child(format!("{} diff rows · virtualized list", file.lines.len())),
            )
    }

    fn render_review_controls(
        &self,
        cx: &Context<Self>,
        path: &str,
        selected_line: Option<usize>,
    ) -> impl IntoElement {
        match (&self.review_mode, &self.remote_review_state) {
            (ReviewMode::UnderstandReview, RemoteReviewState::Loaded(snapshot)) => {
                let entity = cx.entity();
                let http = cx.http_client();
                let access_token = self.github_credentials.as_ref().and_then(|credentials| {
                    (!credentials.access_token_is_expired())
                        .then(|| credentials.access_token.clone())
                });
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(snapshot.reviews.iter().map(render_remote_review_summary))
                    .children(render_remote_review_threads(
                        &snapshot.review_threads,
                        entity,
                        http,
                        access_token,
                        snapshot.reference.clone(),
                    ))
                    .children(
                        snapshot
                            .conversation_comments
                            .iter()
                            .map(render_remote_conversation_comment),
                    )
                    .child(self.render_evaluator_action(cx, snapshot))
                    .child(self.render_thread_reply_composer(cx))
            }
            (ReviewMode::ReviewChanges, RemoteReviewState::Loaded(snapshot)) => div()
                .flex()
                .flex_col()
                .gap_2()
                .child("PR diff loaded · switch to Understand a review to inspect discussions.")
                .child(format!(
                    "{} changed files at head {}",
                    snapshot.files.len(),
                    short_sha(&snapshot.head_sha)
                )),
            (ReviewMode::UnderstandReview, _) => div()
                .flex()
                .flex_col()
                .gap_2()
                .child("Load a GitHub pull request to inspect its reviews and discussion threads.")
                .child(self.render_draft_controls(cx, path, selected_line)),
            (ReviewMode::ReviewChanges, _) => div()
                .flex()
                .flex_col()
                .gap_2()
                .child(self.render_local_review_action(cx))
                .child(self.render_local_command_action(cx))
                .child(self.render_draft_controls(cx, path, selected_line)),
        }
    }

    fn render_local_command_action(&self, cx: &Context<Self>) -> gpui::Div {
        let Some(snapshot) = self.local_repository_snapshot.as_ref() else {
            return div();
        };
        let Some(input) = self.local_command_input.as_ref() else {
            return div();
        };
        let context = match local_command::CommandContext::from_snapshot(snapshot) {
            Ok(context) => context,
            Err(error) => {
                return div()
                    .p_2()
                    .text_color(rgb(0x00e5_b6ba))
                    .child(format!("Local command preview is unavailable: {error}"));
            }
        };
        let entity = cx.entity();
        let prepare_entity = entity.clone();
        let input_entity = input.clone();
        let is_busy = matches!(
            self.local_command_state,
            LocalCommandState::Preparing(_) | LocalCommandState::Running { .. }
        );
        let is_other_work_running = matches!(self.local_review_state, LocalReviewState::Running(_))
            || matches!(
                self.suggested_change_state,
                SuggestedChangeState::Preparing(_) | SuggestedChangeState::Applying(_)
            );
        let prepare_button = Button::new("prepare-local-command")
            .label(
                if matches!(self.local_command_state, LocalCommandState::Preparing(_)) {
                    "Checking command and sandbox…"
                } else {
                    "Review command"
                },
            )
            .disabled(is_busy || is_other_work_running)
            .on_click(move |_event, _window, cx| {
                let command_line = input_entity.read(cx).value().to_string();
                begin_local_command_preflight(&prepare_entity, context.clone(), command_line, cx);
            });

        div()
            .flex()
            .flex_col()
            .gap_2()
            .mt_2()
            .p_2()
            .border_t_1()
            .border_color(rgb(0x003b_4c5c))
            .child("Run a local test or analyzer")
            .child("Enter one command. It is parsed into an executable and literal arguments; no shell expansion is performed.")
            .children(Some(TextInput::new(input)))
            .child(prepare_button)
            .child(render_local_command_state(
                &self.local_command_state,
                snapshot,
                &entity,
            ))
    }

    fn render_local_review_action(&self, cx: &Context<Self>) -> impl IntoElement {
        let Some(snapshot) = self.local_repository_snapshot.as_ref() else {
            return div()
                .p_2()
                .text_color(rgb(0x009b_aaba))
                .child("Open a local repository to review its working-tree changes.");
        };
        let Some(identity) = self.local_review_identity.clone() else {
            let message = self
                .local_review_authorization_error
                .clone()
                .unwrap_or_else(|| "Could not identify this local checkout.".into());
            return div().p_2().text_color(rgb(0x00e5_b6ba)).child(message);
        };
        let Some(settings) = self.local_review_settings.clone() else {
            return div().p_2().text_color(rgb(0x009b_aaba)).child(
                "Save an HTTPS provider URL, model, and API key above to enable local review.",
            );
        };
        self.render_local_review_for_checkout(cx, snapshot, &identity, &settings)
    }

    fn render_local_review_for_checkout(
        &self,
        cx: &Context<Self>,
        snapshot: &repository::RepositorySnapshot,
        identity: &str,
        settings: &session_store::LocalReviewSettings,
    ) -> gpui::Div {
        let fingerprint = match local_review::config_fingerprint(settings) {
            Ok(fingerprint) => fingerprint,
            Err(error) => {
                return div()
                    .p_2()
                    .text_color(rgb(0x00e5_b6ba))
                    .child(format!("Local review settings are invalid: {error}"));
            }
        };
        let snapshot_fingerprint = local_review::snapshot_fingerprint(snapshot);
        let is_authorized = self.local_review_authorized_for.as_ref().is_some_and(
            |(authorized_identity, authorized_fingerprint)| {
                authorized_identity == identity && authorized_fingerprint == &fingerprint
            },
        );
        let is_authorization_busy = matches!(
            self.local_review_state,
            LocalReviewState::SavingSettings
                | LocalReviewState::Authorizing { .. }
                | LocalReviewState::Revoking { .. }
        );
        let is_review_running = matches!(self.local_review_state, LocalReviewState::Running(_));
        let is_local_command_running = matches!(
            self.local_command_state,
            LocalCommandState::Preparing(_) | LocalCommandState::Running { .. }
        );
        let is_suggestion_busy = is_local_command_running
            || matches!(
                &self.suggested_change_state,
                SuggestedChangeState::Preparing(_) | SuggestedChangeState::Applying(_)
            );
        let store = self.session_store.clone();
        let authorization_button = render_local_review_authorization_button(
            &cx.entity(),
            store.clone(),
            identity,
            &fingerprint,
            is_authorized,
            store.is_some() && !is_authorization_busy,
        );
        let validation = local_review::validate_snapshot(snapshot);
        let request = validation.as_ref().ok().map(|_| LocalReviewRequest {
            context: LocalReviewRequestContext {
                request_id: 0,
                repository_identity: identity.to_string(),
                config_fingerprint: fingerprint.clone(),
                snapshot_fingerprint: snapshot_fingerprint.clone(),
            },
            repository_root: snapshot.root.clone(),
            settings: (*settings).clone(),
        });
        let send_button = render_local_review_send_button(
            &cx.entity(),
            request,
            store.is_some()
                && is_authorized
                && !is_review_running
                && !is_local_command_running
                && !is_authorization_busy
                && !is_suggestion_busy,
        );
        let validation_message = local_review_validation_message(snapshot, validation);
        let authorization_message = local_review_authorization_message(is_authorized);
        div()
            .flex()
            .flex_col()
            .gap_2()
            .mt_2()
            .p_2()
            .border_t_1()
            .border_color(rgb(0x003b_4c5c))
            .child("Review local changes")
            .child(format!("Checkout: {}", snapshot.root.display()))
            .child(format!(
                "Destination: {} · model {}",
                settings.openai_endpoint, settings.openai_model
            ))
            .child(validation_message)
            .child(render_local_review_limits())
            .child(authorization_message)
            .child(authorization_button)
            .child(send_button)
            .children(
                local_review_state_message(&self.local_review_state).map(|message| {
                    div()
                        .text_color(rgb(local_review_state_color(&self.local_review_state)))
                        .child(message)
                }),
            )
            .child(self.render_local_review_results(snapshot, identity, &snapshot_fingerprint, cx))
    }

    fn render_local_review_results(
        &self,
        snapshot: &repository::RepositorySnapshot,
        identity: &str,
        snapshot_fingerprint: &str,
        cx: &Context<Self>,
    ) -> gpui::Div {
        let result = match &self.local_review_state {
            LocalReviewState::Complete(result)
                if result.diff_fingerprint == snapshot_fingerprint =>
            {
                Some(render_local_review_result(
                    result,
                    &cx.entity(),
                    &snapshot.root,
                    identity,
                    &self.suggested_change_state,
                ))
            }
            _ => None,
        };
        let suggestion_status = render_suggested_change_state(
            &cx.entity(),
            &self.suggested_change_state,
            Some(snapshot_fingerprint),
        );
        div().children(result).children(suggestion_status)
    }

    fn render_evaluator_action(
        &self,
        cx: &Context<Self>,
        snapshot: &github_pr::PullRequestSnapshot,
    ) -> impl IntoElement {
        let Some(settings) = self.evaluator_settings.clone() else {
            return div()
                .p_2()
                .text_color(rgb(0x00e5_b6ba))
                .child("Configure both evaluator providers above before analysis.");
        };
        let Some(comment) = self.selected_remote_comment.clone() else {
            return div()
                .p_2()
                .text_color(rgb(0x009b_aaba))
                .child("Select an inline review comment to prepare a second opinion.");
        };
        let source = match &self.remote_source_context {
            RemoteSourceContextState::Loaded {
                comment_id,
                source,
                anchor_line,
            } if *comment_id == comment.id => Some((source.clone(), *anchor_line)),
            _ => None,
        };
        let Some((source, anchor_line)) = source else {
            return div()
                .p_2()
                .text_color(rgb(0x009b_aaba))
                .child("Select a comment and wait for its source-at-commit context to load.");
        };
        let identity = format!(
            "{}/{}",
            snapshot.reference.owner, snapshot.reference.repository
        );
        let fingerprint = match evaluator::DualReviewEvaluator::config_fingerprint(&settings) {
            Ok(fingerprint) => fingerprint,
            Err(error) => {
                return div()
                    .p_2()
                    .text_color(rgb(0x00e5_b6ba))
                    .child(format!("Evaluator configuration is invalid: {error}"));
            }
        };
        let is_authorized = self.evaluator_authorized_repository.as_deref() == Some(&identity);
        let entity = cx.entity();
        let authorization_button = render_evaluator_authorization_button(
            entity.clone(),
            self.session_store.clone(),
            identity.clone(),
            fingerprint.clone(),
            is_authorized,
        );
        let authorization_status = if is_authorized {
            "Local consent is active for this repository and provider configuration."
        } else {
            "Analysis is blocked until you explicitly authorize this repository locally."
        };
        let analysis_input =
            build_review_analysis_input(snapshot, &comment, &source, anchor_line).ok();
        let analysis_request = analysis_input.map(|input| EvaluatorAnalysisRequest {
            settings: settings.clone(),
            repository_identity: identity.clone(),
            config_fingerprint: fingerprint.clone(),
            input,
            comment_id: comment.id,
        });
        let analysis_button = render_evaluator_send_button(
            entity,
            cx.http_client(),
            analysis_request,
            is_authorized && !self.is_evaluator_running,
        );
        div()
            .flex()
            .flex_col()
            .gap_2()
            .mt_2()
            .p_2()
            .border_t_1()
            .border_color(rgb(0x003b_4c5c))
            .child("AI SECOND OPINION · never filters or publishes")
            .child(format!(
                "OpenAI-compatible: {} · {}",
                settings.openai_model, settings.openai_endpoint
            ))
            .child(format!("Jev: {} · api.typesafe.ai", settings.jev_model))
            .child("If you continue, both providers receive the selected comment, its inline thread, an excerpt from the source at the comment commit, and a bounded current diff. Remote text is treated as untrusted data.")
            .child(authorization_status)
            .child(authorization_button)
            .child(analysis_button)
            .children(
                self.evaluator_result
                    .as_ref()
                    .filter(|_| self.evaluator_result_comment_id == Some(comment.id))
                    .map(render_evaluator_result),
            )
    }

    fn render_thread_reply_composer(&self, cx: &Context<Self>) -> gpui::Div {
        let RemoteReviewState::Loaded(snapshot) = &self.remote_review_state else {
            return div();
        };
        if let ThreadReplyState::Publishing(pending) = &self.thread_reply_state {
            return div().p_2().text_color(rgb(0x00b8_dfc4)).child(format!(
                "Publishing one reply to @{} in {}#{}…",
                pending.target_comment.author, pending.reference.owner, pending.reference.number
            ));
        }
        let Some(comment) = self.selected_remote_comment.as_ref() else {
            return div();
        };
        let Some(input) = self.remote_reply_input.as_ref() else {
            return div();
        };
        let comment_is_current = snapshot.review_threads.iter().any(|thread| {
            thread.root.id == comment.id
                || thread.replies.iter().any(|reply| reply.id == comment.id)
        });
        if !comment_is_current {
            return div()
                .p_2()
                .text_color(rgb(0x00e5_b6ba))
                .child("The selected review comment is no longer in the loaded pull request.");
        }
        let body = input.read(cx).value().to_string();
        let validation_message = github_pr::validate_thread_reply_body(&body)
            .err()
            .map(|error| error.to_string());
        let access_token = self.github_credentials.as_ref().and_then(|credentials| {
            (!credentials.access_token_is_expired()
                && matches!(&self.github_auth_state, GitHubAuthState::SignedIn))
            .then(|| credentials.access_token.clone())
        });
        render_thread_reply_editor(
            ThreadReplyComposer {
                snapshot,
                comment,
                input,
                body,
                validation_message,
                access_token,
            },
            &self.thread_reply_state,
            cx,
        )
    }

    fn render_draft_controls(
        &self,
        cx: &Context<Self>,
        path: &str,
        selected_line: Option<usize>,
    ) -> impl IntoElement {
        let can_comment = self.repository_root.is_some()
            && !self.remote_review_state.blocks_local_drafts()
            && (selected_line.is_some() || self.editing_draft.is_some());
        let draft_input = can_comment
            .then(|| self.draft_input.as_ref().map(TextInput::new))
            .flatten();
        let is_saving_draft = self.is_saving_draft;
        let save_draft_button = if can_comment {
            let entity = cx.entity();
            let input = self.draft_input.clone();
            let store = self.session_store.clone();
            let root = self
                .repository_root
                .as_ref()
                .map(|root| PathBuf::from(root.as_ref()));
            let selected_path = path.to_string();
            let selected_line_number = selected_line.and_then(|number| u32::try_from(number).ok());
            let head_oid = self.head_oid.clone();
            let editing_draft = self.editing_draft.clone();
            let expected_revision = editing_draft.as_ref().map(|draft| draft.revision);
            let (path, line_number, base_head_oid) = editing_draft.as_ref().map_or_else(
                || (selected_path, selected_line_number, head_oid.clone()),
                |draft| {
                    (
                        draft.draft.path.clone(),
                        draft.draft.line,
                        draft.base_head_oid.clone(),
                    )
                },
            );
            Some(
                div()
                    .id("save-comment-draft")
                    .px_3()
                    .py_2()
                    .bg(rgb(if is_saving_draft {
                        0x001b_2732
                    } else {
                        0x0026_3442
                    }))
                    .cursor_pointer()
                    .child(if is_saving_draft {
                        "Saving draft…"
                    } else {
                        "Save comment draft"
                    })
                    .on_click(move |_event, _window, cx| {
                        if is_saving_draft {
                            return;
                        }
                        let (Some(input), Some(store), Some(root), Some(line)) =
                            (input.clone(), store.clone(), root.clone(), line_number)
                        else {
                            return;
                        };
                        let body = input.read(cx).value().to_string();
                        if body.trim().is_empty() {
                            entity.update(cx, |this, cx| {
                                this.notice =
                                    Some("Write a comment before saving the draft.".into());
                                cx.notify();
                            });
                            return;
                        }
                        let draft = cururu_core::ReviewCommentDraft {
                            path: path.clone(),
                            line: Some(line),
                            body,
                        };
                        persist_comment_draft(
                            entity.clone(),
                            DraftSave {
                                store,
                                repository_root: root,
                                base_head_oid: base_head_oid.clone(),
                                current_head_oid: head_oid.clone(),
                                draft,
                                expected_revision,
                            },
                            cx,
                        );
                    }),
            )
        } else {
            None
        };
        let draft_rows = self.render_saved_drafts(cx);
        div()
            .flex()
            .flex_col()
            .gap_2()
            .children(draft_input)
            .children(save_draft_button)
            .children(draft_rows)
    }

    fn render_saved_drafts(
        &self,
        cx: &Context<Self>,
    ) -> impl IntoIterator<Item = impl IntoElement> {
        self.comment_drafts.iter().map(|draft| {
            let status = if draft.is_stale {
                "STALE · saved against another HEAD"
            } else {
                "Saved locally · not published"
            };
            let entity = cx.entity();
            let edit_entity = entity.clone();
            let input = self.draft_input.clone();
            let selected_draft = draft.clone();
            let draft_id = draft.id;
            let store = self.session_store.clone();
            let root = self
                .repository_root
                .as_ref()
                .map(|root| PathBuf::from(root.as_ref()));
            let is_deleting = self.deleting_draft_id == Some(draft.id);
            div()
                .id(("comment-draft", draft_id.unsigned_abs()))
                .p_2()
                .bg(rgb(0x0020_2b36))
                .child(
                    div()
                        .id(("comment-draft-body", draft_id.unsigned_abs()))
                        .cursor_pointer()
                        .on_click(move |_event, window, cx| {
                            if let Some(input) = input.clone() {
                                let body = selected_draft.draft.body.clone();
                                input.update(cx, |state, cx| state.set_value(body, window, cx));
                            }
                            edit_entity.update(cx, |this, cx| {
                                let draft_path = selected_draft.draft.path.as_str();
                                let draft_line = selected_draft
                                    .draft
                                    .line
                                    .and_then(|line| usize::try_from(line).ok());
                                if let Some(file_index) = this
                                    .files
                                    .iter()
                                    .position(|file| file.path.as_ref() == draft_path)
                                    && let Some(line_index) = this.files[file_index]
                                        .lines
                                        .iter()
                                        .position(|line| line.new_number == draft_line)
                                {
                                    this.selected_file = Some(file_index);
                                    this.selected_line = line_index;
                                }
                                this.editing_draft = Some(selected_draft.clone());
                                this.notice = Some("Draft loaded into the editor.".into());
                                cx.notify();
                            });
                        })
                        .child(format!(
                            "{}:{} · {status}",
                            draft.draft.path,
                            draft.draft.line.unwrap_or_default()
                        ))
                        .child(
                            div()
                                .text_color(rgb(0x00b5_c0cb))
                                .child(draft.draft.body.clone()),
                        ),
                )
                .child(
                    div()
                        .id(("delete-comment-draft", draft_id.unsigned_abs()))
                        .px_2()
                        .py_1()
                        .text_color(rgb(0x00e5_a84b))
                        .cursor_pointer()
                        .child(if is_deleting {
                            "Deleting…"
                        } else {
                            "Delete draft"
                        })
                        .on_click(move |_event, _window, cx| {
                            if is_deleting {
                                return;
                            }
                            let (Some(store), Some(root)) = (store.clone(), root.clone()) else {
                                return;
                            };
                            delete_comment_draft(entity.clone(), store, root, draft_id, cx);
                        }),
                )
        })
    }
}

fn render_thread_reply_editor(
    editor: ThreadReplyComposer<'_>,
    state: &ThreadReplyState,
    cx: &Context<ReviewWorkspace>,
) -> gpui::Div {
    let status = render_thread_reply_status(&editor, state, cx);
    let pending = editor
        .validation_message
        .is_none()
        .then(|| PendingThreadReply {
            reference: editor.snapshot.reference.clone(),
            pull_request_title: editor.snapshot.title.clone(),
            pull_request_url: editor.snapshot.html_url.clone(),
            expected_head_sha: editor.snapshot.head_sha.clone(),
            target_comment: editor.comment.clone(),
            body: editor.body.clone(),
        });
    let can_prepare = editor.snapshot.state == "open"
        && editor.access_token.is_some()
        && pending.is_some()
        && !status.is_confirming
        && !status.failed_for_current_body
        && !status.already_published;
    let prepare_label = if can_prepare {
        "Preview reply before publishing"
    } else if status.failed_for_current_body {
        "Refresh the pull request before retrying"
    } else if status.already_published {
        "This exact reply was already published"
    } else if editor.snapshot.state != "open" {
        "Replies are available only on open pull requests"
    } else if editor.access_token.is_none() {
        "Connect or refresh GitHub to publish"
    } else {
        "Enter a reply to preview"
    };
    let mut prepare_button = Button::new("preview-github-thread-reply")
        .label(prepare_label)
        .disabled(!can_prepare);
    if let Some(pending) = pending.filter(|_| can_prepare) {
        let entity = cx.entity();
        prepare_button = prepare_button.on_click(move |_event, _window, cx| {
            begin_thread_reply_confirmation(&entity, pending.clone(), cx);
        });
    }
    let show_input = !status.is_confirming;
    let validation_message = editor
        .validation_message
        .map(|message| div().text_color(rgb(0x00e5_b6ba)).child(message));
    let connection_message = if editor.access_token.is_some() {
        "The reply stays local until you confirm the exact preview below."
    } else {
        "Connect or refresh GitHub to publish; this text has not been sent."
    };
    let target_line = editor.comment.line.or(editor.comment.original_line);

    div()
        .flex()
        .flex_col()
        .gap_2()
        .mt_2()
        .p_2()
        .border_t_1()
        .border_color(rgb(0x003b_4c5c))
        .child("Reply to selected GitHub thread")
        .child(format!(
            "{}#{} · @{} · {}:{}",
            editor.snapshot.reference.owner,
            editor.snapshot.reference.number,
            editor.comment.author,
            editor.comment.path,
            target_line.unwrap_or_default()
        ))
        .child(connection_message)
        .children(show_input.then_some(div().child("Reply text")))
        .children(show_input.then(|| TextInput::new(editor.input)))
        .children(validation_message)
        .children((!status.is_confirming).then_some(prepare_button))
        .children(status.element)
}

fn render_thread_reply_status(
    editor: &ThreadReplyComposer<'_>,
    state: &ThreadReplyState,
    cx: &Context<ReviewWorkspace>,
) -> ThreadReplyComposerStatus {
    let matches_context = |pending: &PendingThreadReply| {
        pending_reply_matches_context(pending, editor.snapshot, editor.comment)
    };
    let is_confirming = matches!(
        state,
        ThreadReplyState::Confirming(pending) if matches_context(pending)
    );
    let failed_for_current_body = matches!(
        state,
        ThreadReplyState::Failed { pending, .. }
            if matches_context(pending) && pending.body == editor.body
    );
    let already_published = matches!(
        state,
        ThreadReplyState::Published { pending, .. }
            if matches_context(pending) && pending.body == editor.body
    );
    let element = match state {
        ThreadReplyState::Confirming(pending) if matches_context(pending) => Some(
            render_thread_reply_confirmation(
                &cx.entity(),
                cx.http_client(),
                editor.access_token.clone(),
                pending.clone(),
            ),
        ),
        ThreadReplyState::Published {
            pending,
            comment_url,
        } if matches_context(pending) && pending.body == editor.body => Some(
            div()
                .p_2()
                .text_color(rgb(0x00b8_dfc4))
                .child(format!("Reply published: {comment_url}")),
        ),
        ThreadReplyState::Failed { pending, message }
            if matches_context(pending) && pending.body == editor.body =>
        {
            Some(
                div()
                    .p_2()
                    .text_color(rgb(0x00e5_b6ba))
                    .child(format!(
                        "Publication was not confirmed: {message}. Refresh the pull request before retrying; the request may have reached GitHub."
                    )),
            )
        }
        _ => None,
    };
    ThreadReplyComposerStatus {
        is_confirming,
        failed_for_current_body,
        already_published,
        element,
    }
}

fn render_thread_reply_confirmation(
    entity: &Entity<ReviewWorkspace>,
    http: Arc<dyn gpui::http_client::HttpClient>,
    access_token: Option<String>,
    pending: PendingThreadReply,
) -> gpui::Div {
    let has_access_token = access_token.is_some();
    let target_line = pending
        .target_comment
        .line
        .or(pending.target_comment.original_line)
        .unwrap_or_default();
    let mut confirm_button = Button::new("confirm-github-thread-reply")
        .label("Confirm and publish this reply")
        .warning()
        .disabled(!has_access_token);
    if let Some(access_token) = access_token {
        let confirm_entity = entity.clone();
        let confirm_pending = pending.clone();
        confirm_button = confirm_button.on_click(move |_event, _window, cx| {
            start_thread_reply_publication(
                confirm_entity.clone(),
                http.clone(),
                access_token.clone(),
                confirm_pending.clone(),
                cx,
            );
        });
    }

    let cancel_entity = entity.clone();
    let cancel_pending = pending.clone();
    let cancel_button = Button::new("cancel-github-thread-reply")
        .label("Cancel")
        .ghost()
        .on_click(move |_event, _window, cx| {
            close_thread_reply_confirmation(&cancel_entity, &cancel_pending, cx);
        });
    let edit_entity = entity.clone();
    let edit_pending = pending.clone();
    let edit_button = Button::new("edit-github-thread-reply")
        .label("Edit reply")
        .ghost()
        .on_click(move |_event, _window, cx| {
            close_thread_reply_confirmation(&edit_entity, &edit_pending, cx);
        });

    div()
        .flex()
        .flex_col()
        .gap_2()
        .p_3()
        .bg(rgb(0x0020_2b36))
        .border_1()
        .border_color(rgb(0x00e5_a84b))
        .child("PUBLICATION PREVIEW · confirmation required")
        .child(format!(
            "{}#{} · {} · HEAD {}",
            pending.reference.owner,
            pending.reference.number,
            pending.pull_request_title,
            short_sha(&pending.expected_head_sha)
        ))
        .child(pending.pull_request_url)
        .child(format!(
            "Replying to @{} at {}:{}",
            pending.target_comment.author,
            pending.target_comment.path,
            target_line
        ))
        .child(format!("Selected comment: {}", pending.target_comment.body))
        .child("This will create a visible GitHub review-thread reply. It will not change local files, stage, commit, or push.")
        .child(div().p_2().bg(rgb(0x0011_1820)).child(pending.body))
        .children((!has_access_token).then(|| {
            div()
                .text_color(rgb(0x00e5_b6ba))
                .child("Reconnect or refresh GitHub before confirming this publication.")
        }))
        .child(div().flex().gap_2().child(cancel_button).child(edit_button).child(confirm_button))
}

fn close_thread_reply_confirmation(
    entity: &Entity<ReviewWorkspace>,
    pending: &PendingThreadReply,
    cx: &mut App,
) {
    entity.update(cx, |this, cx| {
        if matches!(
            &this.thread_reply_state,
            ThreadReplyState::Confirming(active) if active == pending
        ) {
            this.thread_reply_state = ThreadReplyState::Idle;
            this.notice = Some("Reply remains in the editor and was not sent.".into());
            cx.notify();
        }
    });
}

fn begin_thread_reply_confirmation(
    entity: &Entity<ReviewWorkspace>,
    pending: PendingThreadReply,
    cx: &mut App,
) {
    entity.update(cx, |this, cx| {
        let pull_request_matches = matches!(
            &this.remote_review_state,
            RemoteReviewState::Loaded(snapshot)
                if snapshot.state == "open"
                    && snapshot.reference.eq(&pending.reference)
                    && snapshot
                        .head_sha
                        .as_str()
                        .eq(pending.expected_head_sha.as_str())
                    && snapshot_has_review_comment(snapshot, pending.target_comment.id)
        );
        let selected_comment_matches = this
            .selected_remote_comment
            .as_ref()
            .is_some_and(|comment| comment.id == pending.target_comment.id);
        let credentials_are_current = matches!(
            &this.github_auth_state,
            GitHubAuthState::SignedIn
        ) && this.github_credentials.as_ref().is_some_and(|credentials| {
            !credentials.access_token_is_expired()
        });
        let body_is_valid = github_pr::validate_thread_reply_body(&pending.body).is_ok();
        let no_publication_is_running =
            !matches!(&this.thread_reply_state, ThreadReplyState::Publishing(_));
        if !pull_request_matches
            || !selected_comment_matches
            || !body_is_valid
            || !no_publication_is_running
        {
            this.notice = Some(
                "The pull request, selected thread, or reply changed. Review the current state before confirming."
                    .into(),
            );
        } else if !credentials_are_current {
            this.notice = Some("Reconnect or refresh GitHub before publishing.".into());
        } else {
            this.thread_reply_state = ThreadReplyState::Confirming(pending);
            this.notice = Some(
                "Review the exact public reply and target below. Nothing is sent until you confirm."
                    .into(),
            );
        }
        cx.notify();
    });
}

fn snapshot_has_review_comment(snapshot: &github_pr::PullRequestSnapshot, comment_id: u64) -> bool {
    snapshot.review_threads.iter().any(|thread| {
        thread.root.id == comment_id || thread.replies.iter().any(|reply| reply.id == comment_id)
    })
}

fn pending_reply_matches_context(
    pending: &PendingThreadReply,
    snapshot: &github_pr::PullRequestSnapshot,
    comment: &github_pr::PullRequestComment,
) -> bool {
    pending.reference.eq(&snapshot.reference)
        && pending
            .expected_head_sha
            .as_str()
            .eq(snapshot.head_sha.as_str())
        && pending.target_comment.id.eq(&comment.id)
}

fn local_review_state_message(state: &LocalReviewState) -> Option<SharedString> {
    match state {
        LocalReviewState::Idle => None,
        LocalReviewState::SavingSettings => Some("Saving settings and keyring credential…".into()),
        LocalReviewState::Authorizing { .. } => {
            Some("Recording local consent; no code is being sent.".into())
        }
        LocalReviewState::Revoking { .. } => {
            Some("Revoking local consent and discarding any in-flight result…".into())
        }
        LocalReviewState::Running(_) => {
            Some("Review is running. No partial findings will be shown.".into())
        }
        LocalReviewState::Complete(_) => Some(
            "Review completed. Findings are informational; nothing was changed or published."
                .into(),
        ),
        LocalReviewState::Failed(message) => Some(message.clone()),
    }
}

const fn local_review_state_color(state: &LocalReviewState) -> u32 {
    if matches!(state, LocalReviewState::Failed(_)) {
        0x00e5_b6ba
    } else {
        0x00b8_dfc4
    }
}

fn render_local_command_state(
    state: &LocalCommandState,
    snapshot: &repository::RepositorySnapshot,
    entity: &Entity<ReviewWorkspace>,
) -> gpui::Div {
    match state {
        LocalCommandState::Idle => div(),
        LocalCommandState::Preparing(_) => div().text_color(rgb(0x009b_aaba)).child(
            "Rechecking the reviewed diff and checking sandbox support. No command is running.",
        ),
        LocalCommandState::Confirming(pending) => {
            if !local_command_plan_matches_snapshot(&pending.plan, snapshot) {
                return div()
                    .text_color(rgb(0x00e5_b6ba))
                    .child("This command preview is stale. Prepare it again against the current HEAD and diff.");
            }
            render_local_command_confirmation(pending, entity, false)
        }
        LocalCommandState::ConfirmingUnsandboxed(pending) => {
            if !local_command_plan_matches_snapshot(&pending.plan, snapshot) {
                return div()
                    .text_color(rgb(0x00e5_b6ba))
                    .child("This command preview is stale. Prepare it again against the current HEAD and diff.");
            }
            render_local_command_confirmation(pending, entity, true)
        }
        LocalCommandState::Running { pending, cancelled } => {
            if !local_command_plan_matches_snapshot(&pending.plan, snapshot) {
                return div()
                    .text_color(rgb(0x00e5_b6ba))
                    .child("The reviewed checkout changed while the command was running; its output will be discarded.");
            }
            let entity = entity.clone();
            let cancel = cancelled.clone();
            let is_stopping = cancelled.load(Ordering::Acquire);
            let cancel_button = Button::new("cancel-local-command")
                .label(if is_stopping {
                    "Stopping process group…"
                } else {
                    "Cancel command"
                })
                .disabled(is_stopping)
                .on_click(move |_event, _window, cx| {
                    cancel.store(true, Ordering::Release);
                    entity.update(cx, |this, cx| {
                        this.notice = Some(
                            "Stopping the command and its process group; waiting for it to exit…"
                                .into(),
                        );
                        cx.notify();
                    });
                });
            div()
                .flex()
                .flex_col()
                .gap_2()
                .mt_2()
                .p_2()
                .bg(rgb(0x0020_2b36))
                .child(format!("Running · {}", pending.plan.command.display()))
                .child(format!("Isolation: {}", pending.plan.sandbox.description()))
                .child("Command output is captured in memory only and is not written to the session database.")
                .child(cancel_button)
        }
        LocalCommandState::Complete { pending, output } => {
            if !local_command_plan_matches_snapshot(&pending.plan, snapshot) {
                return div()
                    .text_color(rgb(0x00e5_b6ba))
                    .child("This result is stale because the checkout changed. Clear it before preparing another command.");
            }
            render_local_command_output(pending, output, entity)
        }
        LocalCommandState::Failed(message) => {
            let entity = entity.clone();
            let clear_button = Button::new("clear-local-command-state")
                .label("Clear")
                .ghost()
                .on_click(move |_event, _window, cx| {
                    entity.update(cx, |this, cx| {
                        this.local_command_state = LocalCommandState::Idle;
                        cx.notify();
                    });
                });
            div()
                .flex()
                .flex_col()
                .gap_2()
                .mt_2()
                .text_color(rgb(0x00e5_b6ba))
                .child(message.clone())
                .child(clear_button)
        }
    }
}

fn local_command_plan_matches_snapshot(
    plan: &local_command::CommandPlan,
    snapshot: &repository::RepositorySnapshot,
) -> bool {
    local_command::CommandContext::from_snapshot(snapshot)
        .is_ok_and(|context| context == plan.context)
}

fn render_local_command_confirmation(
    pending: &PendingLocalCommand,
    entity: &Entity<ReviewWorkspace>,
    unsandboxed: bool,
) -> gpui::Div {
    let pending_for_cancel = pending.clone();
    let cancel_entity = entity.clone();
    let cancel_button = Button::new("cancel-local-command-preview")
        .label("Cancel")
        .ghost()
        .on_click(move |_event, _window, cx| {
            close_local_command_confirmation(&cancel_entity, &pending_for_cancel, cx);
        });

    let panel = div()
        .flex()
        .flex_col()
        .gap_2()
        .mt_2()
        .p_2()
        .bg(rgb(0x0020_2b36))
        .border_1()
        .border_color(rgb(if unsandboxed {
            0x00e5_a84b
        } else {
            0x003b_4c5c
        }))
        .child(if unsandboxed {
            "UNSANDBOXED RUN · a separate confirmation is required"
        } else {
            "COMMAND PREVIEW · one run only"
        })
        .child(pending.plan.command.display())
        .child(format!(
            "Repository: {} · working directory: repository root",
            pending.plan.context.repository_root.display()
        ))
        .child(format!(
            "HEAD: {} · diff: {}",
            pending
                .plan
                .context
                .head_oid
                .as_deref()
                .map_or("unborn", short_sha),
            &pending.plan.context.diff_fingerprint[..12]
        ))
        .child(format!("Sandbox: {}", pending.plan.sandbox.description()))
        .child("Limits: 10 minutes · 32 KiB captured per stream · no retries · output is memory-only")
        .child("The executable and arguments are passed directly; shell expansion is not performed. Approval is never reused for another run.");

    let controls = if unsandboxed {
        render_unsandboxed_confirmation_controls(pending, entity, cancel_button)
    } else {
        render_sandboxed_confirmation_controls(pending, entity, cancel_button)
    };
    panel.child(controls)
}

fn render_unsandboxed_confirmation_controls(
    pending: &PendingLocalCommand,
    entity: &Entity<ReviewWorkspace>,
    cancel_button: Button,
) -> gpui::Div {
    let entity_for_run = entity.clone();
    let pending_for_run = pending.clone();
    let run_button = Button::new("confirm-unsandboxed-local-command")
        .label("I understand — run without sandbox")
        .on_click(move |_event, _window, cx| {
            start_local_command_run(
                &entity_for_run,
                pending_for_run.clone(),
                local_command::ExecutionMode::Unsandboxed,
                cx,
            );
        });
    let entity_for_back = entity.clone();
    let pending_for_back = pending.clone();
    let back_button = Button::new("back-to-local-command-preview")
        .label("Back")
        .ghost()
        .on_click(move |_event, _window, cx| {
            entity_for_back.update(cx, |this, cx| {
                if matches!(
                    &this.local_command_state,
                    LocalCommandState::ConfirmingUnsandboxed(active)
                        if active.request_id == pending_for_back.request_id
                ) {
                    this.local_command_state =
                        LocalCommandState::Confirming(pending_for_back.clone());
                    cx.notify();
                }
            });
        });

    div()
        .flex()
        .flex_col()
        .gap_2()
        .child("Without isolation, this command runs with your OS account permissions. It may read user files or credential stores, access the network, and modify the checkout. Cururu does not pass its provider keys or load repository environment files.")
        .child(div().flex().gap_2().child(run_button).child(back_button))
        .child(cancel_button)
}

fn render_sandboxed_confirmation_controls(
    pending: &PendingLocalCommand,
    entity: &Entity<ReviewWorkspace>,
    cancel_button: Button,
) -> gpui::Div {
    let sandbox_button = if pending.plan.sandbox.is_available() {
        let entity = entity.clone();
        let pending = pending.clone();
        Some(
            Button::new("confirm-sandboxed-local-command")
                .label("Confirm and run sandboxed")
                .on_click(move |_event, _window, cx| {
                    start_local_command_run(
                        &entity,
                        pending.clone(),
                        local_command::ExecutionMode::Sandboxed,
                        cx,
                    );
                }),
        )
    } else {
        None
    };
    let entity = entity.clone();
    let pending = pending.clone();
    let unsandboxed_button = Button::new("review-unsandboxed-local-command")
        .label("Review unsandboxed run…")
        .ghost()
        .on_click(move |_event, _window, cx| {
            enter_unsandboxed_local_command_confirmation(&entity, &pending, cx);
        });

    div()
        .flex()
        .flex_col()
        .gap_2()
        .children(sandbox_button)
        .child(
            div()
                .flex()
                .gap_2()
                .child(unsandboxed_button)
                .child(cancel_button),
        )
}

fn render_local_command_output(
    pending: &PendingLocalCommand,
    output: &local_command::CommandOutput,
    entity: &Entity<ReviewWorkspace>,
) -> gpui::Div {
    let status = if output.was_cancelled() {
        "Cancelled · process group stopped".to_string()
    } else if output.timed_out() {
        "Timed out after 10 minutes · process group stopped".to_string()
    } else if output.succeeded() {
        "Completed · exit code 0".to_string()
    } else {
        format!(
            "Finished · exit code {}",
            output
                .exit_code
                .map_or_else(|| "unavailable".to_string(), |code| code.to_string())
        )
    };
    let entity = entity.clone();
    let clear_button = Button::new("clear-local-command-output")
        .label("Clear output")
        .ghost()
        .on_click(move |_event, _window, cx| {
            entity.update(cx, |this, cx| {
                this.local_command_state = LocalCommandState::Idle;
                cx.notify();
            });
        });
    let mut panel = div()
        .flex()
        .flex_col()
        .gap_2()
        .mt_2()
        .p_2()
        .bg(rgb(0x0020_2b36))
        .child(format!("{} · {}", status, pending.plan.command.display()))
        .children(output.output_pipe_held_open().then(|| {
            div()
                .text_color(rgb(0x00e5_a84b))
                .child("A background process kept an output pipe open; the process group was stopped.")
        }))
        .child("Output remains in memory only; it is cleared on request, repository change, or app exit.");
    if !output.stdout.is_empty() {
        panel = panel
            .child(format!(
                "stdout{}",
                if output.stdout_truncated {
                    " · truncated at 32 KiB"
                } else {
                    ""
                }
            ))
            .child(
                div()
                    .p_2()
                    .bg(rgb(0x0011_1820))
                    .child(output.stdout.clone()),
            );
    }
    if !output.stderr.is_empty() {
        panel = panel
            .child(format!(
                "stderr{}",
                if output.stderr_truncated {
                    " · truncated at 32 KiB"
                } else {
                    ""
                }
            ))
            .child(
                div()
                    .p_2()
                    .bg(rgb(0x0011_1820))
                    .child(output.stderr.clone()),
            );
    }
    panel.child(clear_button)
}

fn close_local_command_confirmation(
    entity: &Entity<ReviewWorkspace>,
    pending: &PendingLocalCommand,
    cx: &mut App,
) {
    entity.update(cx, |this, cx| {
        let is_current = matches!(
            &this.local_command_state,
            LocalCommandState::Confirming(active)
                | LocalCommandState::ConfirmingUnsandboxed(active)
                if active.request_id == pending.request_id
        );
        if is_current {
            this.local_command_state = LocalCommandState::Idle;
            this.notice = Some("Command preview cancelled; nothing was run.".into());
            cx.notify();
        }
    });
}

fn enter_unsandboxed_local_command_confirmation(
    entity: &Entity<ReviewWorkspace>,
    pending: &PendingLocalCommand,
    cx: &mut App,
) {
    entity.update(cx, |this, cx| {
        if matches!(
            &this.local_command_state,
            LocalCommandState::Confirming(active) if active.request_id == pending.request_id
        ) {
            this.local_command_state = LocalCommandState::ConfirmingUnsandboxed(pending.clone());
            cx.notify();
        }
    });
}

fn render_local_review_limits() -> gpui::Div {
    div()
        .text_color(rgb(0x009b_aaba))
        .child(format!(
            "Outbound limit: {} files, {} bytes total, {} bytes per file. Only changed-file paths and unified diffs are sent; no repository snapshot.",
            local_review::MAX_LOCAL_REVIEW_FILES,
            local_review::MAX_LOCAL_DIFF_BYTES,
            local_review::MAX_LOCAL_FILE_DIFF_BYTES
        ))
}

fn local_review_validation_message(
    snapshot: &repository::RepositorySnapshot,
    validation: anyhow::Result<usize>,
) -> String {
    match validation {
        Ok(chunks) => format!(
            "{} changed files · {} bytes · {chunks} bounded chunk(s)",
            snapshot.changed_files.len(),
            local_review::diff_byte_count(snapshot)
        ),
        Err(error) => format!("Local review unavailable: {error}"),
    }
}

const fn local_review_authorization_message(is_authorized: bool) -> &'static str {
    if is_authorized {
        "Consent is active for this checkout and provider configuration."
    } else {
        "Authorize this checkout before sending any code. Authorization sends no code."
    }
}

fn save_local_review_configuration(
    entity: &Entity<ReviewWorkspace>,
    inputs: LocalReviewSettingsInputs,
    window: &mut Window,
    cx: &mut App,
) {
    let (Some(endpoint_input), Some(model_input), Some(api_key_input)) =
        (inputs.endpoint, inputs.model, inputs.api_key)
    else {
        return;
    };
    let settings = session_store::LocalReviewSettings {
        openai_endpoint: endpoint_input.read(cx).value().to_string(),
        openai_model: model_input.read(cx).value().to_string(),
    };
    let api_key = api_key_input.read(cx).value().to_string();
    api_key_input.update(cx, |state, input_cx| {
        state.set_value("", window, input_cx);
    });
    if let Err(error) = settings.validate() {
        entity.update(cx, |this, cx| {
            this.local_review_state = LocalReviewState::Failed(error.to_string().into());
            cx.notify();
        });
        return;
    }
    let Some(store) = entity.read(cx).session_store.clone() else {
        entity.update(cx, |this, cx| {
            this.local_review_state = LocalReviewState::Failed(
                "Local database is unavailable; settings were not saved.".into(),
            );
            cx.notify();
        });
        return;
    };
    entity.update(cx, |this, cx| {
        this.next_local_review_request_id = this.next_local_review_request_id.saturating_add(1);
        this.local_review_state = LocalReviewState::SavingSettings;
        cx.notify();
    });
    let entity_for_task = entity.clone();
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move {
                if api_key.is_empty() {
                    anyhow::ensure!(
                        local_review::LocalReviewSecrets::load()?.is_some(),
                        "enter an API key before saving; credentials are stored only in the OS keyring"
                    );
                } else {
                    local_review::LocalReviewSecrets::save(&api_key)?;
                }
                store.save_local_review_settings(&settings)?;
                Ok::<_, anyhow::Error>(settings)
            })
            .await;
        let _ = entity_for_task.update(cx, |this, cx| {
            match result {
                Ok(settings) => {
                    let fingerprint = local_review::config_fingerprint(&settings).ok();
                    let authorization_still_matches = this
                        .local_review_identity
                        .as_ref()
                        .zip(fingerprint.as_ref())
                        .is_some_and(|(identity, fingerprint)| {
                            this.local_review_authorized_for.as_ref()
                                == Some(&(identity.clone(), fingerprint.clone()))
                        });
                    if !authorization_still_matches {
                        this.local_review_authorized_for = None;
                    }
                    this.local_review_settings = Some(settings);
                    this.local_review_authorization_error = None;
                    this.local_review_state = LocalReviewState::Idle;
                    this.notice = Some(
                        "Local review settings saved. No code was sent; authorize the checkout separately."
                            .into(),
                    );
                }
                Err(error) => {
                    this.local_review_state = LocalReviewState::Failed(error.to_string().into());
                }
            }
            cx.notify();
        });
    })
    .detach();
}

fn render_local_review_authorization_button(
    entity: &Entity<ReviewWorkspace>,
    store: Option<session_store::SessionStore>,
    identity: &str,
    fingerprint: &str,
    is_authorized: bool,
    can_change: bool,
) -> impl IntoElement + use<> {
    let label = if is_authorized {
        "Revoke consent for this checkout"
    } else {
        "Authorize this checkout for local review"
    };
    let button = div()
        .id("authorize-local-review-checkout")
        .px_3()
        .py_2()
        .bg(rgb(if can_change { 0x0030_3b2e } else { 0x001b_2732 }))
        .child(label);
    if !can_change {
        return button;
    }
    let entity = entity.clone();
    let identity = identity.to_string();
    let fingerprint = fingerprint.to_string();
    button
        .cursor_pointer()
        .on_click(move |_event, _window, cx| {
            start_local_review_authorization(
                &entity,
                store.clone(),
                identity.clone(),
                fingerprint.clone(),
                is_authorized,
                cx,
            );
        })
}

fn start_local_review_authorization(
    entity: &Entity<ReviewWorkspace>,
    store: Option<session_store::SessionStore>,
    identity: String,
    fingerprint: String,
    is_authorized: bool,
    cx: &mut App,
) {
    let Some(store) = store else {
        entity.update(cx, |this, cx| {
            this.local_review_state = LocalReviewState::Failed(
                "Local database is unavailable; checkout consent was not changed.".into(),
            );
            cx.notify();
        });
        return;
    };
    entity.update(cx, |this, cx| {
        this.next_local_review_request_id = this.next_local_review_request_id.saturating_add(1);
        this.local_review_state = if is_authorized {
            this.local_review_authorized_for = None;
            LocalReviewState::Revoking {
                repository_identity: identity.clone(),
            }
        } else {
            LocalReviewState::Authorizing {
                repository_identity: identity.clone(),
                config_fingerprint: fingerprint.clone(),
            }
        };
        cx.notify();
    });
    let entity_for_task = entity.clone();
    let identity_for_store = identity.clone();
    let fingerprint_for_store = fingerprint.clone();
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move {
                if is_authorized {
                    store
                        .revoke_local_review_authorization(&identity_for_store)
                        .map(|_| false)
                } else {
                    store
                        .authorize_local_review(&identity_for_store, &fingerprint_for_store)
                        .map(|()| true)
                }
            })
            .await;
        let _ = entity_for_task.update(cx, |this, cx| {
            complete_local_review_authorization(
                this,
                result,
                is_authorized,
                &identity,
                &fingerprint,
            );
            cx.notify();
        });
    })
    .detach();
}

fn complete_local_review_authorization(
    workspace: &mut ReviewWorkspace,
    result: anyhow::Result<bool>,
    is_authorized: bool,
    identity: &str,
    fingerprint: &str,
) {
    let operation_is_current = match (&workspace.local_review_state, is_authorized) {
        (
            LocalReviewState::Revoking {
                repository_identity,
            },
            true,
        ) => repository_identity == identity,
        (
            LocalReviewState::Authorizing {
                repository_identity,
                config_fingerprint,
            },
            false,
        ) => repository_identity == identity && config_fingerprint == fingerprint,
        _ => false,
    };
    if !operation_is_current {
        return;
    }
    match result {
        Ok(true) => {
            workspace.local_review_authorized_for =
                Some((identity.to_string(), fingerprint.to_string()));
            workspace.local_review_state = LocalReviewState::Idle;
            workspace.notice = Some(
                "Checkout consent recorded. No code was sent; start a review explicitly when ready."
                    .into(),
            );
        }
        Ok(false) => {
            workspace.local_review_authorized_for = None;
            workspace.local_review_state = LocalReviewState::Idle;
            workspace.notice = Some(
                "Local review consent revoked. Any in-flight result will be discarded.".into(),
            );
        }
        Err(error) => {
            workspace.local_review_state = LocalReviewState::Failed(error.to_string().into());
        }
    }
}

fn render_local_review_send_button(
    entity: &Entity<ReviewWorkspace>,
    request: Option<LocalReviewRequest>,
    can_review: bool,
) -> impl IntoElement + use<> {
    let label = if can_review {
        "Send these bounded changes for local review"
    } else {
        "Local review requires settings, checkout consent, and valid diffs"
    };
    let button = div()
        .id("send-local-changes-to-reviewer")
        .px_3()
        .py_2()
        .bg(rgb(if can_review { 0x0030_3b2e } else { 0x001b_2732 }))
        .child(label);
    if !can_review {
        return button;
    }
    let entity = entity.clone();
    button
        .cursor_pointer()
        .on_click(move |_event, _window, cx| {
            let Some(request) = request.clone() else {
                return;
            };
            start_local_review(&entity, request, cx);
        })
}

fn begin_local_command_preflight(
    entity: &Entity<ReviewWorkspace>,
    context: local_command::CommandContext,
    command_line: String,
    cx: &mut App,
) {
    let request_id = entity.update(cx, |this, cx| {
        if this.review_mode != ReviewMode::ReviewChanges
            || !matches!(this.remote_review_state, RemoteReviewState::Idle)
        {
            this.notice = Some("Local commands are available only in the local review workspace.".into());
            cx.notify();
            return None;
        }
        if this.is_opening_repository {
            this.notice = Some("Wait for the repository to finish opening before preparing a command.".into());
            cx.notify();
            return None;
        }
        if matches!(
            this.local_command_state,
            LocalCommandState::Preparing(_) | LocalCommandState::Running { .. }
        ) || matches!(this.local_review_state, LocalReviewState::Running(_))
            || matches!(
                this.suggested_change_state,
                SuggestedChangeState::Preparing(_) | SuggestedChangeState::Applying(_)
            )
        {
            this.notice = Some("Wait for the current local operation to finish before preparing another command.".into());
            cx.notify();
            return None;
        }

        this.next_local_command_request_id =
            this.next_local_command_request_id.saturating_add(1);
        let request_id = this.next_local_command_request_id;
        this.local_command_state = LocalCommandState::Preparing(request_id);
        this.notice = Some(
            "Checking the exact command and revalidating the reviewed HEAD/diff. No repository command will run yet."
                .into(),
        );
        cx.notify();
        Some(request_id)
    });
    let Some(request_id) = request_id else {
        return;
    };

    let task_entity = entity.clone();
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move { local_command::prepare(context, &command_line) })
            .await;
        let _ = task_entity.update(cx, |this, cx| {
            if !matches!(
                this.local_command_state,
                LocalCommandState::Preparing(active) if active == request_id
            ) {
                return;
            }
            match result {
                Ok(plan) => {
                    this.local_command_state = LocalCommandState::Confirming(
                        PendingLocalCommand { request_id, plan },
                    );
                    this.notice = Some(
                        "Command preview ready. Review the executable, arguments, isolation, and checkout before confirming."
                            .into(),
                    );
                }
                Err(error) => {
                    this.local_command_state =
                        LocalCommandState::Failed(error.to_string().into());
                    this.notice = Some("Command preview failed; nothing was run.".into());
                }
            }
            cx.notify();
        });
    })
    .detach();
}

fn start_local_command_run(
    entity: &Entity<ReviewWorkspace>,
    pending: PendingLocalCommand,
    mode: local_command::ExecutionMode,
    cx: &mut App,
) {
    let cancelled = Arc::new(AtomicBool::new(false));
    if !accept_local_command_confirmation(entity, &pending, mode, &cancelled, cx) {
        return;
    }

    let receiver = match spawn_local_command_worker(pending.plan.clone(), mode, cancelled) {
        Ok(receiver) => receiver,
        Err(error) => {
            entity.update(cx, |this, cx| {
                if matches!(
                    &this.local_command_state,
                    LocalCommandState::Running { pending: active, .. }
                        if active.request_id == pending.request_id
                ) {
                    this.local_command_state = LocalCommandState::Failed(
                        format!("Could not start the command supervisor: {error}").into(),
                    );
                    cx.notify();
                }
            });
            return;
        }
    };

    let task_entity = entity.clone();
    cx.spawn(async move |cx| {
        let result = receiver
            .await
            .map_err(|_| anyhow::anyhow!("command supervisor stopped before returning a result"))
            .and_then(|result| result);
        let _ = task_entity.update(cx, |this, cx| {
            complete_local_command_run(this, &pending, result, cx);
        });
    })
    .detach();
}

fn accept_local_command_confirmation(
    entity: &Entity<ReviewWorkspace>,
    pending: &PendingLocalCommand,
    mode: local_command::ExecutionMode,
    cancelled: &Arc<AtomicBool>,
    cx: &mut App,
) -> bool {
    entity.update(cx, |this, cx| {
        let was_confirmed = match (&this.local_command_state, mode) {
            (LocalCommandState::Confirming(active), local_command::ExecutionMode::Sandboxed) => {
                active.request_id == pending.request_id && active.plan.sandbox.is_available()
            }
            (
                LocalCommandState::ConfirmingUnsandboxed(active),
                local_command::ExecutionMode::Unsandboxed,
            ) => active.request_id == pending.request_id,
            _ => false,
        };
        if !was_confirmed {
            return false;
        }
        let snapshot_matches = this
            .local_repository_snapshot
            .as_ref()
            .is_some_and(|snapshot| local_command_plan_matches_snapshot(&pending.plan, snapshot));
        if !snapshot_matches {
            this.local_command_state = LocalCommandState::Failed(
                "The reviewed HEAD or diff changed after preview; no command was run.".into(),
            );
            this.notice =
                Some("Command confirmation rejected because the checkout changed.".into());
            cx.notify();
            return false;
        }
        this.local_command_state = LocalCommandState::Running {
            pending: pending.clone(),
            cancelled: cancelled.clone(),
        };
        this.notice = Some(match mode {
            local_command::ExecutionMode::Sandboxed => {
                "Running the confirmed command in the isolated sandbox…".into()
            }
            local_command::ExecutionMode::Unsandboxed => {
                "Running the separately confirmed command without sandbox isolation…".into()
            }
        });
        cx.notify();
        true
    })
}

fn spawn_local_command_worker(
    plan: local_command::CommandPlan,
    mode: local_command::ExecutionMode,
    cancelled: Arc<AtomicBool>,
) -> anyhow::Result<
    futures::channel::oneshot::Receiver<anyhow::Result<local_command::ExecutionResult>>,
> {
    let (sender, receiver) = futures::channel::oneshot::channel();
    std::thread::Builder::new()
        .name("cururu-local-command-supervisor".to_string())
        .spawn(move || {
            let result = local_command::run(&plan, mode, cancelled.as_ref());
            let _ = sender.send(result);
        })
        .context("could not start the command supervisor")?;
    Ok(receiver)
}

fn complete_local_command_run(
    this: &mut ReviewWorkspace,
    pending: &PendingLocalCommand,
    result: anyhow::Result<local_command::ExecutionResult>,
    cx: &mut Context<ReviewWorkspace>,
) {
    if !matches!(
        &this.local_command_state,
        LocalCommandState::Running { pending: active, .. }
            if active.request_id == pending.request_id
    ) {
        return;
    }
    match result {
        Ok(local_command::ExecutionResult::Completed(output)) => {
            this.notice = Some(match (output.was_cancelled(), output.timed_out()) {
                (true, _) => "Command cancelled; its output is available in memory only.".into(),
                (_, true) => "Command reached the 10-minute limit and was stopped.".into(),
                _ => "Command finished; output is available in memory only.".into(),
            });
            this.local_command_state = LocalCommandState::Complete {
                pending: pending.clone(),
                output,
            };
        }
        Ok(local_command::ExecutionResult::Stale) => {
            this.local_command_state = LocalCommandState::Failed(
                "The command finished after the reviewed HEAD or diff changed. Its output was discarded; reopen the repository before another run.".into(),
            );
            this.notice = Some("Stale command output discarded.".into());
        }
        Err(error) => {
            this.local_command_state = LocalCommandState::Failed(error.to_string().into());
            this.notice = Some("Command could not complete safely; no retry was attempted.".into());
        }
    }
    cx.notify();
}

fn start_local_review(
    entity: &Entity<ReviewWorkspace>,
    mut request: LocalReviewRequest,
    cx: &mut App,
) {
    let Some(store) = entity.read(cx).session_store.clone() else {
        entity.update(cx, |this, cx| {
            this.local_review_state = LocalReviewState::Failed(
                "Local database is unavailable; review is blocked.".into(),
            );
            cx.notify();
        });
        return;
    };
    let request_context = entity.update(cx, |this, cx| {
        if matches!(
            &this.suggested_change_state,
            SuggestedChangeState::Applying(_)
        ) {
            this.notice = Some(
                "Wait for the confirmed local suggestion to finish before starting another review."
                    .into(),
            );
            cx.notify();
            return None;
        }
        this.next_local_review_request_id = this.next_local_review_request_id.saturating_add(1);
        request.context.request_id = this.next_local_review_request_id;
        this.local_review_state = LocalReviewState::Running(request.context.clone());
        this.suggested_change_state = SuggestedChangeState::Idle;
        this.notice =
            Some("Preparing a fresh snapshot, then sending only the confirmed local diffs…".into());
        cx.notify();
        Some(request.context.clone())
    });
    let Some(request_context) = request_context else {
        return;
    };
    let entity_for_task = entity.clone();
    cx.spawn(async move |cx| {
        let executor = cx.background_executor();
        let result = run_local_review(store, request.clone(), executor.clone()).await;
        let _ = entity_for_task.update(cx, |this, cx| {
            if !matches!(
                &this.local_review_state,
                LocalReviewState::Running(active) if active == &request_context
            ) {
                return;
            }
            let current_identity = this.local_review_identity.as_deref();
            let current_fingerprint = this
                .local_review_settings
                .as_ref()
                .and_then(|settings| local_review::config_fingerprint(settings).ok());
            let current_snapshot_fingerprint = this
                .local_repository_snapshot
                .as_ref()
                .map(local_review::snapshot_fingerprint);
            let still_matches = current_identity == Some(request_context.repository_identity.as_str())
                && current_fingerprint.as_deref()
                    == Some(request_context.config_fingerprint.as_str())
                && current_snapshot_fingerprint.as_deref()
                    == Some(request_context.snapshot_fingerprint.as_str());
            if !still_matches {
                this.local_review_state = LocalReviewState::Idle;
                this.notice = Some(
                    "Local review result discarded because the checkout, settings, or diff changed."
                        .into(),
                );
                cx.notify();
                return;
            }
            match result {
                Ok(review) => {
                    this.local_review_state = LocalReviewState::Complete(review);
                    this.notice = Some(
                        "Local review completed. Findings are informational; no files were changed or published."
                            .into(),
                    );
                }
                Err(error) => {
                    this.local_review_state = LocalReviewState::Failed(error.to_string().into());
                }
            }
            cx.notify();
        });
    })
    .detach();
}

async fn run_local_review(
    store: session_store::SessionStore,
    request: LocalReviewRequest,
    executor: BackgroundExecutor,
) -> anyhow::Result<local_review::LocalReviewResult> {
    let preflight_store = store.clone();
    let preflight_request = request.clone();
    let (snapshot, secrets) = executor
        .spawn(async move {
            validate_local_review_request(&preflight_store, &preflight_request)?;
            let snapshot =
                repository::RepositorySnapshot::open(&preflight_request.repository_root)?;
            validate_local_review_snapshot(&preflight_request, &snapshot)?;
            let secrets = local_review::LocalReviewSecrets::load()?
                .context("local review API key is unavailable in the OS keyring")?;
            Ok::<_, anyhow::Error>((snapshot, secrets))
        })
        .await?;

    let result = local_review::review_snapshot(&request.settings, &secrets, &snapshot).await?;

    let postflight_store = store;
    let postflight_request = request;
    executor
        .spawn(async move {
            validate_local_review_request(&postflight_store, &postflight_request)?;
            let current_snapshot =
                repository::RepositorySnapshot::open(&postflight_request.repository_root)?;
            validate_local_review_snapshot(&postflight_request, &current_snapshot)?;
            Ok::<_, anyhow::Error>(())
        })
        .await?;
    Ok(result)
}

fn validate_local_review_request(
    store: &session_store::SessionStore,
    request: &LocalReviewRequest,
) -> anyhow::Result<()> {
    let identity = local_review::repository_identity(&request.repository_root)?;
    anyhow::ensure!(
        identity == request.context.repository_identity,
        "local repository identity changed; reopen it before reviewing"
    );
    let settings = store
        .local_review_settings()?
        .context("local review settings are unavailable")?;
    anyhow::ensure!(
        settings == request.settings,
        "local review settings changed; save and authorize the current configuration"
    );
    let fingerprint = local_review::config_fingerprint(&settings)?;
    anyhow::ensure!(
        fingerprint == request.context.config_fingerprint,
        "local review configuration changed; authorize it again"
    );
    anyhow::ensure!(
        store.local_review_is_authorized(&identity, &fingerprint)?,
        "local consent is missing or no longer matches this checkout and provider configuration"
    );
    Ok(())
}

fn validate_local_review_snapshot(
    request: &LocalReviewRequest,
    snapshot: &repository::RepositorySnapshot,
) -> anyhow::Result<()> {
    let identity = local_review::repository_identity(&snapshot.root)?;
    anyhow::ensure!(
        identity == request.context.repository_identity,
        "repository changed during local review; the result was discarded"
    );
    anyhow::ensure!(
        local_review::snapshot_fingerprint(snapshot) == request.context.snapshot_fingerprint,
        "working-tree changes differ from the reviewed preview; reopen the repository and try again"
    );
    local_review::validate_snapshot(snapshot)?;
    Ok(())
}

fn render_local_review_result(
    result: &local_review::LocalReviewResult,
    entity: &Entity<ReviewWorkspace>,
    repository_root: &Path,
    repository_identity: &str,
    suggestion_state: &SuggestedChangeState,
) -> gpui::Div {
    let entity = entity.clone();
    let is_suggestion_busy = matches!(
        suggestion_state,
        SuggestedChangeState::Preparing(_) | SuggestedChangeState::Applying(_)
    );
    let rows = result
        .review
        .findings
        .iter()
        .enumerate()
        .map(|(index, finding)| {
            let path = finding.path.clone();
            let line = finding.line;
            let finding_for_display = finding.clone();
            let entity = entity.clone();
            let request = SuggestedChangeRequest {
                request_id: 0,
                repository_root: repository_root.to_path_buf(),
                repository_identity: repository_identity.to_string(),
                reviewed_diff_fingerprint: result.diff_fingerprint.clone(),
                finding: finding.clone(),
            };
            let apply_button = if finding.suggested_change.is_some() {
                let entity_for_action = entity.clone();
                Some(
                    Button::new(("preview-local-suggested-change", index))
                        .label("Preview suggested change")
                        .disabled(is_suggestion_busy)
                        .on_click(move |_event, _window, cx| {
                            begin_suggested_change_preview(&entity_for_action, request.clone(), cx);
                        }),
                )
            } else {
                None
            };
            div()
                .id(("local-review-finding", index))
                .flex()
                .flex_col()
                .gap_1()
                .p_2()
                .bg(rgb(0x0020_2b36))
                .cursor_pointer()
                .child(format!(
                    "{} · {}:{}",
                    finding_for_display.severity,
                    finding_for_display.path,
                    finding_for_display.line.unwrap_or_default()
                ))
                .child(finding_for_display.title)
                .child(finding_for_display.message)
                .children((!finding_for_display.suggestion.is_empty()).then(|| {
                    div()
                        .text_color(rgb(0x00b8_dfc4))
                        .child(finding_for_display.suggestion)
                }))
                .children(apply_button)
                .on_click(move |_event, _window, cx| {
                    entity.update(cx, |this, cx| {
                        let line_number = line.and_then(|line| usize::try_from(line).ok());
                        if let Some(file_index) = this
                            .files
                            .iter()
                            .position(|file| file.path.as_ref() == path)
                            && let Some(line_index) = this.files[file_index]
                                .lines
                                .iter()
                                .position(|diff_line| diff_line.new_number == line_number)
                        {
                            this.selected_file = Some(file_index);
                            this.selected_line = line_index;
                            this.diff_scroll_handle
                                .scroll_to_item(line_index, ScrollStrategy::Center);
                        }
                        cx.notify();
                    });
                })
        });
    div()
        .flex()
        .flex_col()
        .gap_2()
        .mt_2()
        .child(format!(
            "{} · {} finding(s) · {} chunk(s) · {} diff bytes",
            result.review.model,
            result.review.findings.len(),
            result.chunk_count,
            result.diff_bytes
        ))
        .child(result.review.summary.clone())
        .children(rows)
}

fn render_suggested_change_state(
    entity: &Entity<ReviewWorkspace>,
    state: &SuggestedChangeState,
    current_review_fingerprint: Option<&str>,
) -> Option<gpui::Div> {
    match state {
        SuggestedChangeState::Idle => None,
        SuggestedChangeState::Preparing(_) => Some(
            div()
                .p_2()
                .text_color(rgb(0x00b8_dfc4))
                .child("Checking the reviewed file and preparing a local-only preview…"),
        ),
        SuggestedChangeState::Preview(pending) => current_review_fingerprint
            .is_some_and(|fingerprint| {
                fingerprint == pending.preview.reviewed_diff_fingerprint()
            })
            .then(|| render_suggested_change_preview(entity, pending)),
        SuggestedChangeState::Applying(pending) => Some(
            div()
                .p_2()
                .text_color(rgb(0x00b8_dfc4))
                .child(format!(
                    "Rechecking the preview and applying {}:{} atomically…",
                    pending.preview.path, pending.preview.line
                )),
        ),
        SuggestedChangeState::Applied { path, line } => Some(
            div()
                .p_2()
                .text_color(rgb(0x00b8_dfc4))
                .child(format!(
                    "Applied the confirmed change to {path}:{line}. The previous findings were invalidated; run local review again. No stage, commit, or push was performed."
                )),
        ),
        SuggestedChangeState::Failed(message) => Some(
            div()
                .p_2()
                .text_color(rgb(0x00e5_b6ba))
                .child(format!("Suggested change was not applied: {message}")),
        ),
    }
}

fn render_suggested_change_preview(
    entity: &Entity<ReviewWorkspace>,
    pending: &PendingSuggestedChange,
) -> gpui::Div {
    let cancel_entity = entity.clone();
    let request_id = pending.request_id;
    let cancel_button = Button::new("discard-local-suggestion-preview")
        .label("Discard preview")
        .ghost()
        .on_click(move |_event, _window, cx| {
            cancel_suggested_change_preview(&cancel_entity, request_id, cx);
        });
    let apply_entity = entity.clone();
    let pending_for_apply = pending.clone();
    let apply_button = Button::new("confirm-apply-local-suggestion")
        .label("Apply this change to the worktree")
        .warning()
        .on_click(move |_event, _window, cx| {
            start_suggested_change_application(&apply_entity, pending_for_apply.clone(), cx);
        });

    div()
        .flex()
        .flex_col()
        .gap_2()
        .mt_2()
        .p_3()
        .bg(rgb(0x0020_2b36))
        .border_1()
        .border_color(rgb(0x00e5_a84b))
        .child("LOCAL CHANGE PREVIEW · no file has been modified")
        .child(format!(
            "{}:{} · {}",
            pending.preview.path, pending.preview.line, pending.preview.title
        ))
        .child("Only the selected worktree line will change. This will not stage, commit, or push.")
        .child(
            div()
                .p_2()
                .bg(rgb(0x0011_1820))
                .child(pending.preview.diff.clone()),
        )
        .child(
            div()
                .flex()
                .gap_2()
                .child(cancel_button)
                .child(apply_button),
        )
}

fn begin_suggested_change_preview(
    entity: &Entity<ReviewWorkspace>,
    mut request: SuggestedChangeRequest,
    cx: &mut App,
) {
    let request_id = entity.update(cx, |this, cx| {
        let already_busy = matches!(
            &this.suggested_change_state,
            SuggestedChangeState::Preparing(_) | SuggestedChangeState::Applying(_)
        ) || matches!(
            this.local_command_state,
            LocalCommandState::Preparing(_) | LocalCommandState::Running { .. }
        );
        let review_matches = matches!(
            &this.local_review_state,
            LocalReviewState::Complete(result)
                if result.diff_fingerprint == request.reviewed_diff_fingerprint
        );
        let snapshot_matches = this.local_repository_snapshot.as_ref().is_some_and(|snapshot| {
            local_review::snapshot_fingerprint(snapshot) == request.reviewed_diff_fingerprint
                && snapshot.root == request.repository_root
        });
        let identity_matches = this.local_review_identity.as_deref()
            == Some(request.repository_identity.as_str());
        match suggested_change_preview_decision(
            already_busy,
            review_matches && snapshot_matches && identity_matches,
        ) {
            SuggestedChangePreviewDecision::IgnoreBusy => return None,
            SuggestedChangePreviewDecision::RejectStale => {
                this.suggested_change_state = SuggestedChangeState::Failed(
                    "The local review or checkout is no longer current; reopen the repository and review it again.".into(),
                );
                this.notice = Some("Suggestion preview was blocked because its review context changed.".into());
                cx.notify();
                return None;
            }
            SuggestedChangePreviewDecision::Start => {}
        }
        this.next_suggestion_request_id = this.next_suggestion_request_id.saturating_add(1);
        request.request_id = this.next_suggestion_request_id;
        let request_id = request.request_id;
        this.suggested_change_state = SuggestedChangeState::Preparing(request_id);
        this.notice = Some("Checking the reviewed file; no local file will be changed during preview.".into());
        cx.notify();
        Some(request_id)
    });
    let Some(request_id) = request_id else {
        return;
    };
    let entity = entity.clone();
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move {
                suggested_change::prepare(
                    &request.repository_root,
                    &request.repository_identity,
                    &request.reviewed_diff_fingerprint,
                    &request.finding,
                )
            })
            .await;
        let _ = entity.update(cx, |this, cx| {
            if !matches!(
                &this.suggested_change_state,
                SuggestedChangeState::Preparing(active_id) if *active_id == request_id
            ) {
                return;
            }
            match result {
                Ok(preview) => {
                    this.suggested_change_state =
                        SuggestedChangeState::Preview(PendingSuggestedChange {
                            request_id,
                            preview,
                        });
                    this.notice =
                        Some("Review the complete local diff; nothing has been applied.".into());
                }
                Err(error) => {
                    this.suggested_change_state =
                        SuggestedChangeState::Failed(format!("{error:#}").into());
                    this.notice =
                        Some("Suggestion preview failed closed; no file was changed.".into());
                }
            }
            cx.notify();
        });
    })
    .detach();
}

fn cancel_suggested_change_preview(
    entity: &Entity<ReviewWorkspace>,
    request_id: u64,
    cx: &mut App,
) {
    entity.update(cx, |this, cx| {
        if matches!(
            &this.suggested_change_state,
            SuggestedChangeState::Preview(pending) if pending.request_id == request_id
        ) {
            this.suggested_change_state = SuggestedChangeState::Idle;
            this.notice = Some("Suggestion preview discarded; no file was changed.".into());
            cx.notify();
        }
    });
}

fn start_suggested_change_application(
    entity: &Entity<ReviewWorkspace>,
    pending: PendingSuggestedChange,
    cx: &mut App,
) {
    let store = entity.read(cx).session_store.clone();
    let can_apply = entity.update(cx, |this, cx| {
        if matches!(
            this.local_command_state,
            LocalCommandState::Preparing(_) | LocalCommandState::Running { .. }
        ) {
            this.notice =
                Some("Finish or cancel the local command before applying a suggestion.".into());
            cx.notify();
            return false;
        }
        let is_current_preview = matches!(
            &this.suggested_change_state,
            SuggestedChangeState::Preview(active)
                if active.request_id == pending.request_id
        );
        let current_review_matches = matches!(
            &this.local_review_state,
            LocalReviewState::Complete(result)
                if result.diff_fingerprint == pending.preview.reviewed_diff_fingerprint()
        );
        let current_snapshot_matches =
            this.local_repository_snapshot
                .as_ref()
                .is_some_and(|snapshot| {
                    snapshot.root == pending.preview.repository_root()
                        && local_review::snapshot_fingerprint(snapshot)
                            == pending.preview.reviewed_diff_fingerprint()
                });
        let identity_matches =
            this.local_review_identity.as_deref() == Some(pending.preview.repository_identity());
        if !suggested_change_application_is_current(
            this.is_opening_repository,
            is_current_preview
                && current_review_matches
                && current_snapshot_matches
                && identity_matches,
        ) {
            this.suggested_change_state = SuggestedChangeState::Failed(
                "The review or checkout changed after preview; no file was modified.".into(),
            );
            this.notice =
                Some("Suggestion application was blocked because the preview is stale.".into());
            cx.notify();
            return false;
        }
        this.suggested_change_state = SuggestedChangeState::Applying(pending.clone());
        this.notice =
            Some("Rechecking the file and applying only the confirmed local change…".into());
        cx.notify();
        true
    });
    if !can_apply {
        return;
    }
    let entity = entity.clone();
    let request_id = pending.request_id;
    cx.spawn(async move |cx| {
        let task_result = cx
            .background_executor()
            .spawn(async move {
                suggested_change::apply(&pending.preview)?;
                let refreshed = load_repository_session(
                    pending.preview.repository_root().to_path_buf(),
                    store.as_ref(),
                );
                Ok::<_, anyhow::Error>((pending, refreshed))
            })
            .await;
        let _ = entity.update(cx, |this, cx| match task_result {
            Ok((pending, refreshed)) => {
                finish_successful_suggested_change(this, &pending, refreshed, cx);
            }
            Err(error) => {
                finish_failed_suggested_change(this, request_id, &error, cx);
            }
        });
    })
    .detach();
}

const fn suggested_change_preview_decision(
    is_busy: bool,
    review_context_is_current: bool,
) -> SuggestedChangePreviewDecision {
    if is_busy {
        SuggestedChangePreviewDecision::IgnoreBusy
    } else if review_context_is_current {
        SuggestedChangePreviewDecision::Start
    } else {
        SuggestedChangePreviewDecision::RejectStale
    }
}

const fn suggested_change_application_is_current(
    is_opening_repository: bool,
    application_context_is_current: bool,
) -> bool {
    !is_opening_repository && application_context_is_current
}

fn finish_successful_suggested_change(
    this: &mut ReviewWorkspace,
    pending: &PendingSuggestedChange,
    refreshed: anyhow::Result<OpenedRepository>,
    cx: &mut Context<ReviewWorkspace>,
) {
    if !matches!(
        &this.suggested_change_state,
        SuggestedChangeState::Applying(active) if active.request_id == pending.request_id
    ) {
        return;
    }
    let path = pending.preview.path.clone();
    let line = pending.preview.line;
    match refreshed {
        Ok(opened) => this.apply_repository_result(Ok(opened)),
        Err(error) => {
            this.files.clear();
            this.selected_file = None;
            this.repository_root = None;
            this.branch = None;
            this.head_oid = None;
            this.local_repository_snapshot = None;
            this.local_review_identity = None;
            this.local_review_authorized_for = None;
            this.local_review_state = LocalReviewState::Idle;
            this.load_error = Some(format!(
                "The change was applied to {path}:{line}, but the repository could not be reloaded: {error}"
            ).into());
        }
    }
    this.suggested_change_state = SuggestedChangeState::Applied { path, line };
    this.notice = Some(
        "The confirmed suggestion was applied locally. Previous findings are stale; nothing was staged, committed, or pushed.".into(),
    );
    cx.notify();
}

fn finish_failed_suggested_change(
    this: &mut ReviewWorkspace,
    request_id: u64,
    error: &anyhow::Error,
    cx: &mut Context<ReviewWorkspace>,
) {
    if !matches!(
        &this.suggested_change_state,
        SuggestedChangeState::Applying(pending) if pending.request_id == request_id
    ) {
        return;
    }
    this.suggested_change_state = SuggestedChangeState::Failed(format!("{error:#}").into());
    this.notice =
        Some("Suggestion application failed closed; the reviewed file was preserved.".into());
    cx.notify();
}

fn render_evaluator_authorization_button(
    entity: Entity<ReviewWorkspace>,
    store: Option<session_store::SessionStore>,
    identity: String,
    fingerprint: String,
    is_authorized: bool,
) -> impl IntoElement {
    let label = if is_authorized {
        format!("Revoke local evaluator access for {identity}")
    } else {
        format!("Authorize local evaluator access for {identity}")
    };
    div()
        .id("authorize-evaluator-repository")
        .px_3()
        .py_2()
        .bg(rgb(0x0030_3b2e))
        .cursor_pointer()
        .child(label)
        .on_click(move |_event, _window, cx| {
            let Some(store) = store.clone() else {
                entity.update(cx, |this, cx| {
                    this.evaluator_notice = Some(
                        "Local database is unavailable; authorization was not changed.".into(),
                    );
                    cx.notify();
                });
                return;
            };
            let entity_for_task = entity.clone();
            let repository = identity.clone();
            let fingerprint = fingerprint.clone();
            let repository_for_store = repository.clone();
            let fingerprint_for_store = fingerprint;
            cx.spawn(async move |cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        if is_authorized {
                            store
                                .revoke_evaluator_authorization(&repository_for_store)
                                .map(|_| false)
                        } else {
                            store
                                .authorize_evaluator(&repository_for_store, &fingerprint_for_store)
                                .map(|()| true)
                        }
                    })
                    .await;
                let _ = entity_for_task.update(cx, |this, cx| {
                    match result {
                        Ok(true) => {
                            this.evaluator_authorized_repository = Some(repository);
                            this.evaluator_notice = Some(
                                "Local authorization recorded. No code was sent; use the explicit analysis button below when ready."
                                    .into(),
                            );
                        }
                        Ok(false) => {
                            this.evaluator_authorized_repository = None;
                            this.evaluator_notice = Some(
                                "Local evaluator authorization was revoked.".into(),
                            );
                        }
                        Err(error) => this.evaluator_notice = Some(error.to_string().into()),
                    }
                    cx.notify();
                });
            })
            .detach();
        })
}

fn render_evaluator_send_button(
    entity: Entity<ReviewWorkspace>,
    http: Arc<dyn gpui::http_client::HttpClient>,
    request: Option<EvaluatorAnalysisRequest>,
    can_analyze: bool,
) -> impl IntoElement {
    div()
        .id("send-thread-to-both-evaluators")
        .px_3()
        .py_2()
        .bg(rgb(if can_analyze {
            0x0030_3b2e
        } else {
            0x001b_2732
        }))
        .cursor_pointer()
        .child("Send this bounded thread to OpenAI-compatible + Jev")
        .on_click(move |_event, _window, cx| {
            if !can_analyze {
                return;
            }
            let Some(request) = request.clone() else {
                return;
            };
            start_review_analysis(entity.clone(), http.clone(), request, cx);
        })
}

fn build_review_analysis_input(
    snapshot: &github_pr::PullRequestSnapshot,
    comment: &github_pr::PullRequestComment,
    source: &github_pr::PullRequestSourceFile,
    anchor_line: Option<u32>,
) -> anyhow::Result<evaluator::ReviewAnalysisInput> {
    anyhow::ensure!(
        source.path == comment.path,
        "review source path does not match the selected comment"
    );
    anyhow::ensure!(
        source.commit_sha == comment.original_commit_id,
        "review source commit does not match the selected comment anchor"
    );
    let changed_file = snapshot
        .files
        .iter()
        .find(|file| file.path == comment.path)
        .context("selected comment file is not part of the loaded pull-request diff")?;
    let thread = snapshot
        .review_threads
        .iter()
        .find(|thread| {
            thread.root.id == comment.id
                || thread.replies.iter().any(|reply| reply.id == comment.id)
        })
        .context("selected inline comment no longer belongs to a loaded review thread")?;
    let mut thread_context = Vec::new();
    if thread.root.id != comment.id {
        thread_context.push(thread.root.body.clone());
    }
    for reply in &thread.replies {
        if reply.id != comment.id {
            thread_context.push(reply.body.clone());
        }
    }
    Ok(evaluator::ReviewAnalysisInput {
        repository_identity: format!(
            "{}/{}",
            snapshot.reference.owner, snapshot.reference.repository
        ),
        path: comment.path.clone(),
        line: comment.line.or(comment.original_line),
        comment_body: comment.body.clone(),
        thread_context,
        source_at_comment: source_excerpt(&source.content, anchor_line),
        current_diff: changed_file.patch.chars().take(2_500).collect(),
    })
}

fn save_evaluator_configuration(
    entity: &Entity<ReviewWorkspace>,
    inputs: EvaluatorSettingsInputs,
    window: &mut Window,
    cx: &mut App,
) {
    let (
        Some(endpoint_input),
        Some(openai_model_input),
        Some(jev_model_input),
        Some(openai_key_input),
        Some(jev_key_input),
    ) = (
        inputs.endpoint,
        inputs.openai_model,
        inputs.jev_model,
        inputs.openai_key,
        inputs.jev_key,
    )
    else {
        return;
    };
    let settings = session_store::EvaluatorSettings {
        openai_endpoint: endpoint_input.read(cx).value().to_string(),
        openai_model: openai_model_input.read(cx).value().to_string(),
        jev_model: jev_model_input.read(cx).value().to_string(),
    };
    let openai_secret = openai_key_input.read(cx).value().to_string();
    let jev_secret = jev_key_input.read(cx).value().to_string();
    for input in [&openai_key_input, &jev_key_input] {
        input.update(cx, |state, input_cx| {
            state.set_value("", window, input_cx);
        });
    }
    let store = entity.read(cx).session_store.clone();
    let Some(store) = store else {
        entity.update(cx, |this, cx| {
            this.evaluator_notice =
                Some("Local database is unavailable; evaluator settings were not saved.".into());
            cx.notify();
        });
        return;
    };
    entity.update(cx, |this, cx| {
        this.evaluator_notice = Some("Saving both evaluator configurations…".into());
        cx.notify();
    });
    let entity_for_task = entity.clone();
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move {
                evaluator::DualReviewEvaluator::config_fingerprint(&settings)?;
                anyhow::ensure!(
                    !openai_secret.is_empty() && !jev_secret.is_empty(),
                    "enter both provider API keys to save the configuration"
                );
                store.save_evaluator_settings(&settings)?;
                evaluator::EvaluatorSecrets::save(&openai_secret, &jev_secret)?;
                Ok::<_, anyhow::Error>(settings)
            })
            .await;
        let _ = entity_for_task.update(cx, |this, cx| {
            match result {
                Ok(settings) => {
                    this.evaluator_settings = Some(settings);
                    this.evaluator_authorized_repository = None;
                    this.evaluator_result = None;
                    this.evaluator_result_comment_id = None;
                    this.active_evaluator_context = None;
                    this.is_evaluator_running = false;
                    this.evaluator_notice = Some(
                        "Both providers are configured; keys were saved only in the OS keyring."
                            .into(),
                    );
                }
                Err(error) => this.evaluator_notice = Some(error.to_string().into()),
            }
            cx.notify();
        });
    })
    .detach();
}

fn render_evaluator_result(result: &evaluator::CombinedReviewAnalysis) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .mt_2()
        .p_2()
        .bg(rgb(0x0011_1820))
        .child(format!(
            "SECOND OPINION · {} + {}",
            result.openai_model, result.jev.model
        ))
        .child(format!(
            "{} · Jev defect probability {:.0}% · severity {}{}",
            match result.explanation.classification {
                evaluator::ClaimClassification::Sustained => "Sustained",
                evaluator::ClaimClassification::Contradicted => "Contradicted",
                evaluator::ClaimClassification::InsufficientContext => "Insufficient context",
                evaluator::ClaimClassification::PreferenceTradeoff => "Preference / trade-off",
                evaluator::ClaimClassification::AlreadyFixed => "Already fixed",
            },
            result.jev.defect_probability * 100.0,
            result.jev.severity,
            result
                .jev
                .severity_confidence
                .map_or_else(String::new, |confidence| {
                    format!(" · confidence {:.0}%", confidence * 100.0)
                })
        ))
        .child(result.explanation.explanation.clone())
        .child(format!("Claims: {}", result.explanation.claims.join(" · ")))
        .child(format!(
            "Evidence: {}",
            result.explanation.evidence.join(" · ")
        ))
        .child(format!(
            "Conditions: {}",
            result.explanation.conditions.join(" · ")
        ))
        .child(format!(
            "Verification: {}",
            result.explanation.verification_steps.join(" · ")
        ))
        .child(format!(
            "Uncertainties: {}",
            result.explanation.uncertainties.join(" · ")
        ))
        .child(format!(
            "Draft reply (not published): {}",
            result.explanation.draft_response
        ))
}

fn start_thread_reply_publication(
    entity: Entity<ReviewWorkspace>,
    http: Arc<dyn gpui::http_client::HttpClient>,
    access_token: String,
    pending: PendingThreadReply,
    cx: &mut App,
) {
    let can_publish = entity.update(cx, |this, cx| {
        let confirmation_is_current = matches!(
            &this.thread_reply_state,
            ThreadReplyState::Confirming(active) if active == &pending
        );
        let pull_request_is_current = matches!(
            &this.remote_review_state,
            RemoteReviewState::Loaded(snapshot)
                if snapshot.state == "open"
                    && snapshot.reference == pending.reference
                    && snapshot.head_sha == pending.expected_head_sha
                    && snapshot_has_review_comment(snapshot, pending.target_comment.id)
        );
        let selected_comment_is_current = this
            .selected_remote_comment
            .as_ref()
            .is_some_and(|comment| comment.id == pending.target_comment.id);
        let credentials_are_current = matches!(
            &this.github_auth_state,
            GitHubAuthState::SignedIn
        ) && this.github_credentials.as_ref().is_some_and(|credentials| {
            !credentials.access_token_is_expired()
                && credentials.access_token.eq(&access_token)
        });
        if !confirmation_is_current
            || !pull_request_is_current
            || !selected_comment_is_current
            || !credentials_are_current
            || github_pr::validate_thread_reply_body(&pending.body).is_err()
        {
            this.notice = Some(
                "Publication was blocked because the confirmation, pull request, thread, or GitHub session changed."
                    .into(),
            );
            cx.notify();
            return false;
        }
        this.thread_reply_state = ThreadReplyState::Publishing(pending.clone());
        this.notice = Some(
            "Publishing the confirmed reply once. No automatic retry will be made.".into(),
        );
        cx.notify();
        true
    });
    if !can_publish {
        return;
    }

    cx.spawn(async move |cx| {
        let result =
            publish_thread_reply_once(&http, access_token, &pending, cx.background_executor())
                .await;
        let _ = entity.update(cx, |this, cx| {
            finish_thread_reply_publication(this, pending, result, cx);
        });
    })
    .detach();
}

async fn publish_thread_reply_once(
    http: &Arc<dyn gpui::http_client::HttpClient>,
    access_token: String,
    pending: &PendingThreadReply,
    executor: &BackgroundExecutor,
) -> anyhow::Result<github_pr::PullRequestComment> {
    let client = github_pr::GitHubPullRequestClient::new(Arc::clone(http), access_token)?;
    with_network_timeout(
        client.publish_thread_reply(
            &pending.reference,
            &pending.expected_head_sha,
            &pending.target_comment,
            &pending.body,
        ),
        executor,
    )
    .await
}

fn finish_thread_reply_publication(
    this: &mut ReviewWorkspace,
    pending: PendingThreadReply,
    result: anyhow::Result<github_pr::PullRequestComment>,
    cx: &mut Context<ReviewWorkspace>,
) {
    if !matches!(
        &this.thread_reply_state,
        ThreadReplyState::Publishing(active) if active == &pending
    ) {
        return;
    }
    match result {
        Ok(comment) => {
            let updated_current_view = match &mut this.remote_review_state {
                RemoteReviewState::Loaded(snapshot)
                    if snapshot.reference.eq(&pending.reference)
                        && snapshot
                            .head_sha
                            .as_str()
                            .eq(pending.expected_head_sha.as_str()) =>
                {
                    append_reply_to_snapshot(snapshot, pending.target_comment.id, comment.clone())
                }
                _ => false,
            };
            this.thread_reply_state = ThreadReplyState::Published {
                pending,
                comment_url: comment.html_url,
            };
            this.notice = Some(if updated_current_view {
                "Reply published to GitHub and added to the visible thread.".into()
            } else {
                "Reply published to GitHub. The open pull-request view changed; reload it to refresh the thread."
                    .into()
            });
        }
        Err(error) => {
            this.thread_reply_state = ThreadReplyState::Failed {
                pending,
                message: format!("{error:#}").into(),
            };
            this.notice = Some(
                "GitHub did not confirm the publication outcome. Refresh the pull request before retrying; the request may have reached GitHub."
                    .into(),
            );
        }
    }
    cx.notify();
}

fn append_reply_to_snapshot(
    snapshot: &mut github_pr::PullRequestSnapshot,
    target_comment_id: u64,
    comment: github_pr::PullRequestComment,
) -> bool {
    for thread in &mut snapshot.review_threads {
        let target_is_in_thread = thread.root.id == target_comment_id
            || thread
                .replies
                .iter()
                .any(|reply| reply.id == target_comment_id);
        if !target_is_in_thread {
            continue;
        }
        if !thread.replies.iter().any(|reply| reply.id == comment.id) {
            thread.replies.push(comment);
        }
        return true;
    }
    false
}

fn start_review_analysis(
    entity: Entity<ReviewWorkspace>,
    http: Arc<dyn gpui::http_client::HttpClient>,
    request: EvaluatorAnalysisRequest,
    cx: &mut App,
) {
    let store = entity.read(cx).session_store.clone();
    let Some(store) = store else {
        entity.update(cx, |this, cx| {
            this.evaluator_notice =
                Some("Local database unavailable; evaluation is blocked.".into());
            cx.notify();
        });
        return;
    };
    let request_context = entity.update(cx, |this, cx| {
        this.next_evaluator_request_id = this.next_evaluator_request_id.saturating_add(1);
        let request_context = EvaluatorRequestContext {
            request_id: this.next_evaluator_request_id,
            ..evaluator_request_context(&request)
        };
        this.is_evaluator_running = true;
        this.evaluator_result = None;
        this.evaluator_result_comment_id = None;
        this.active_evaluator_context = Some(request_context.clone());
        this.evaluator_notice = Some(
            "Sending bounded review context to both configured providers; waiting for both results…"
                .into(),
        );
        cx.notify();
        request_context
    });
    cx.spawn(async move |cx| {
        let executor = cx.background_executor();
        let result = run_review_analysis(store, http, request.clone(), executor).await;
        let _ = entity.update(cx, |this, cx| {
            if this.active_evaluator_context.as_ref() != Some(&request_context) {
                return;
            }
            let current_repository = match &this.remote_review_state {
                RemoteReviewState::Loaded(snapshot) => format!(
                    "{}/{}",
                    snapshot.reference.owner, snapshot.reference.repository
                )
                .to_ascii_lowercase(),
                _ => String::new(),
            };
            let current_fingerprint = this.evaluator_settings.as_ref().and_then(|current| {
                evaluator::DualReviewEvaluator::config_fingerprint(current).ok()
            });
            let still_matches = current_repository == request_context.repository_identity
                && this
                    .selected_remote_comment
                    .as_ref()
                    .is_some_and(|comment| comment.id == request_context.comment_id)
                && current_fingerprint.as_deref()
                    == Some(request_context.config_fingerprint.as_str());
            this.is_evaluator_running = false;
            this.active_evaluator_context = None;
            if !still_matches {
                return;
            }
            match result {
                Ok(analysis) => {
                    this.evaluator_result = Some(analysis);
                    this.evaluator_result_comment_id = Some(request.comment_id);
                    this.evaluator_notice = Some(
                        "Both providers completed. This is informational only; no finding was filtered and nothing was published."
                            .into(),
                    );
                }
                Err(error) => {
                    this.evaluator_result = None;
                    this.evaluator_result_comment_id = None;
                    this.evaluator_notice = Some(
                        format!("No combined result is shown because both providers must succeed: {error}")
                            .into(),
                    );
                }
            }
            cx.notify();
        });
    })
    .detach();
}

fn evaluator_request_context(request: &EvaluatorAnalysisRequest) -> EvaluatorRequestContext {
    EvaluatorRequestContext {
        request_id: 0,
        repository_identity: request.repository_identity.to_ascii_lowercase(),
        comment_id: request.comment_id,
        config_fingerprint: request.config_fingerprint.clone(),
    }
}

async fn run_review_analysis(
    store: session_store::SessionStore,
    http: Arc<dyn gpui::http_client::HttpClient>,
    request: EvaluatorAnalysisRequest,
    executor: &BackgroundExecutor,
) -> anyhow::Result<evaluator::CombinedReviewAnalysis> {
    let repository_identity = request.repository_identity.clone();
    let config_fingerprint = request.config_fingerprint.clone();
    let authorized = executor
        .spawn(
            async move { store.evaluator_is_authorized(&repository_identity, &config_fingerprint) },
        )
        .await?;
    anyhow::ensure!(
        authorized,
        "local authorization is missing or no longer matches this repository/provider configuration"
    );
    let secrets = evaluator::EvaluatorSecrets::load()?
        .context("both provider keys must be available in the OS keyring")?;
    let client = evaluator::DualReviewEvaluator::new(http, request.settings, secrets)?;
    with_evaluator_timeout(client.analyze(&request.input), executor).await
}

fn handle_open_pull_request(
    entity: &Entity<ReviewWorkspace>,
    http: Arc<dyn gpui::http_client::HttpClient>,
    input_entity: Option<Entity<InputState>>,
    cx: &mut App,
) {
    let Some(input) = input_entity else {
        return;
    };
    let reference_text = input.read(cx).value().to_string();
    let reference = match github_pr::PullRequestRef::parse(&reference_text) {
        Ok(reference) => reference,
        Err(error) => {
            entity.update(cx, |this, cx| {
                this.notice = Some(error.to_string().into());
                cx.notify();
            });
            return;
        }
    };
    let credentials = entity.read(cx).github_credentials.clone();
    let Some(credentials) = credentials else {
        entity.update(cx, |this, cx| {
            this.notice = Some("Connect or refresh GitHub before opening a pull request.".into());
            cx.notify();
        });
        return;
    };
    if credentials.access_token_is_expired() {
        entity.update(cx, |this, cx| {
            this.github_auth_state = GitHubAuthState::ReauthorizationRequired;
            this.notice =
                Some("GitHub session expired; refresh it before opening a pull request.".into());
            cx.notify();
        });
        return;
    }
    let token = credentials.access_token;
    let can_load = entity.update(cx, |this, cx| {
        if matches!(
            this.local_command_state,
            LocalCommandState::Preparing(_) | LocalCommandState::Running { .. }
        ) {
            this.notice =
                Some("Finish or cancel the local command before opening a GitHub PR.".into());
            cx.notify();
            return false;
        }
        if matches!(this.remote_review_state, RemoteReviewState::Loading(_))
            || matches!(&this.thread_reply_state, ThreadReplyState::Publishing(_))
            || matches!(
                &this.suggested_change_state,
                SuggestedChangeState::Applying(_)
            )
        {
            return false;
        }
        this.notice = Some("Loading pull request from GitHub…".into());
        this.remote_source_context = RemoteSourceContextState::Idle;
        this.selected_remote_comment = None;
        if matches!(
            &this.thread_reply_state,
            ThreadReplyState::Confirming(_) | ThreadReplyState::Failed { .. }
        ) {
            this.thread_reply_state = ThreadReplyState::Idle;
        }
        this.evaluator_authorized_repository = None;
        this.evaluator_result = None;
        this.evaluator_result_comment_id = None;
        this.active_evaluator_context = None;
        this.is_evaluator_running = false;
        this.local_command_state = LocalCommandState::Idle;
        this.remote_review_state = RemoteReviewState::Loading(reference.clone());
        cx.notify();
        true
    });
    if can_load {
        start_pull_request_load(entity.clone(), http, token, reference, cx);
    }
}

fn start_pull_request_load(
    entity: Entity<ReviewWorkspace>,
    http: Arc<dyn gpui::http_client::HttpClient>,
    access_token: String,
    reference: github_pr::PullRequestRef,
    cx: &App,
) {
    cx.spawn(async move |cx| {
        let result = async {
            let client = github_pr::GitHubPullRequestClient::new(http, access_token)?;
            with_network_timeout(
                client.load_pull_request(reference.clone()),
                cx.background_executor(),
            )
            .await
        }
        .await;
        let _ = entity.update(cx, |this, cx| {
            match result {
                Ok(snapshot) => {
                    this.files = snapshot
                        .files
                        .iter()
                        .cloned()
                        .map(parse_diff_file)
                        .collect();
                    this.selected_file = (!this.files.is_empty()).then_some(0);
                    this.selected_line = 0;
                    this.remote_review_state = RemoteReviewState::Loaded(Box::new(snapshot));
                    this.notice = Some("Pull request loaded read-only from GitHub.".into());
                }
                Err(error) => {
                    this.remote_review_state = RemoteReviewState::Failed {
                        reference,
                        message: error.to_string().into(),
                    };
                    this.notice = Some(
                        "GitHub pull request could not be loaded; no partial diff was applied."
                            .into(),
                    );
                }
            }
            cx.notify();
        });
    })
    .detach();
}

fn start_source_context_load(
    entity: Entity<ReviewWorkspace>,
    http: Arc<dyn gpui::http_client::HttpClient>,
    access_token: String,
    reference: github_pr::PullRequestRef,
    comment: github_pr::PullRequestComment,
    cx: &App,
) {
    cx.spawn(async move |cx| {
        let comment_id = comment.id;
        let path = comment.path.clone();
        let commit_sha = comment.original_commit_id.clone();
        let anchor_line = comment.original_line.or(comment.line);
        let result = async {
            let client = github_pr::GitHubPullRequestClient::new(http, access_token)?;
            with_network_timeout(
                client.load_source_file(&reference, &path, &commit_sha),
                cx.background_executor(),
            )
            .await
        }
        .await;
        let _ = entity.update(cx, |this, cx| {
            let is_current_request = matches!(
                &this.remote_source_context,
                RemoteSourceContextState::Loading {
                    comment_id: active_id,
                    ..
                } if *active_id == comment_id
            );
            if !is_current_request {
                return;
            }
            match result {
                Ok(source) => {
                    this.remote_source_context = RemoteSourceContextState::Loaded {
                        comment_id,
                        source,
                        anchor_line,
                    };
                    this.notice = Some(
                        "Loaded the source file at the commit where this comment was written."
                            .into(),
                    );
                }
                Err(error) => {
                    this.remote_source_context = RemoteSourceContextState::Failed {
                        comment_id,
                        path,
                        commit_sha,
                        message: error.to_string().into(),
                    };
                    this.notice = Some(
                        "Reviewed source could not be loaded; the diff and thread remain available."
                            .into(),
                    );
                }
            }
            cx.notify();
        });
    })
    .detach();
}

async fn with_network_timeout<T>(
    operation: impl Future<Output = anyhow::Result<T>>,
    executor: &BackgroundExecutor,
) -> anyhow::Result<T> {
    let timeout = executor.timer(Duration::from_secs(30));
    futures::pin_mut!(operation);
    futures::pin_mut!(timeout);
    match futures::future::select(operation, timeout).await {
        futures::future::Either::Left((result, _)) => result,
        futures::future::Either::Right(((), _)) => {
            anyhow::bail!("GitHub request timed out after 30 seconds")
        }
    }
}

async fn with_evaluator_timeout<T>(
    operation: impl Future<Output = anyhow::Result<T>>,
    executor: &BackgroundExecutor,
) -> anyhow::Result<T> {
    let timeout = executor.timer(Duration::from_mins(2));
    futures::pin_mut!(operation);
    futures::pin_mut!(timeout);
    match futures::future::select(operation, timeout).await {
        futures::future::Either::Left((result, _)) => result,
        futures::future::Either::Right(((), _)) => {
            anyhow::bail!("dual evaluator analysis timed out after 2 minutes")
        }
    }
}

fn render_remote_review_summary(review: &github_pr::PullRequestReview) -> impl IntoElement {
    div()
        .p_2()
        .bg(rgb(0x0020_2b36))
        .child(format!("REVIEW · {} · @{}", review.state, review.author))
        .child(
            div()
                .mt_1()
                .text_color(rgb(0x00b5_c0cb))
                .child(review.body.clone()),
        )
}

fn render_remote_review_threads(
    threads: &[github_pr::PullRequestReviewThread],
    entity: Entity<ReviewWorkspace>,
    http: Arc<dyn gpui::http_client::HttpClient>,
    access_token: Option<String>,
    reference: github_pr::PullRequestRef,
) -> impl IntoIterator<Item = impl IntoElement> + '_ {
    threads.iter().map(move |thread| {
        let root = &thread.root;
        let root_status = if root.is_outdated {
            " · outdated location"
        } else {
            ""
        };
        div()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .bg(rgb(0x0020_2b36))
            .child(format!(
                "INLINE · {}:{} · @{}{}",
                root.path,
                root.line.or(root.original_line).unwrap_or_default(),
                root.author,
                root_status
            ))
            .child(render_remote_inline_comment(
                root,
                entity.clone(),
                http.clone(),
                access_token.clone(),
                reference.clone(),
                false,
            ))
            .children(thread.replies.iter().map(|reply| {
                div()
                    .ml_3()
                    .border_l_1()
                    .border_color(rgb(0x003b_4c5c))
                    .pl_2()
                    .child(format!("↳ @{}", reply.author))
                    .child(render_remote_inline_comment(
                        reply,
                        entity.clone(),
                        http.clone(),
                        access_token.clone(),
                        reference.clone(),
                        true,
                    ))
            }))
    })
}

fn render_remote_inline_comment(
    comment: &github_pr::PullRequestComment,
    entity: Entity<ReviewWorkspace>,
    http: Arc<dyn gpui::http_client::HttpClient>,
    access_token: Option<String>,
    reference: github_pr::PullRequestRef,
    nested: bool,
) -> impl IntoElement {
    let comment_for_click = comment.clone();
    let comment_id = comment.id;
    div()
        .id(("remote-inline-comment", comment_id))
        .px_1()
        .py_1()
        .bg(rgb(0x0020_2b36))
        .cursor_pointer()
        .child(div().text_color(rgb(0x00b5_c0cb)).child(comment.body.clone()))
        .on_click(move |_event, _window, cx| {
            let clicked_comment = comment_for_click.clone();
            entity.update(cx, |this, cx| {
                let found_in_diff = this.focus_remote_comment(&clicked_comment);
                this.selected_remote_comment = Some(clicked_comment.clone());
                if matches!(&this.thread_reply_state, ThreadReplyState::Confirming(_)) {
                    this.thread_reply_state = ThreadReplyState::Idle;
                }
                this.evaluator_result = None;
                this.evaluator_result_comment_id = None;
                this.begin_source_context_load(&clicked_comment);
                if !found_in_diff {
                    this.notice = Some(
                        "The referenced line is outside the current patch; the comment remains available above."
                            .into(),
                    );
                } else if nested {
                    this.notice = Some("Focused the line anchored by this reply's thread.".into());
                }
                cx.notify();
            });
            if let Some(access_token) = access_token.clone() {
                start_source_context_load(
                    entity.clone(),
                    http.clone(),
                    access_token,
                    reference.clone(),
                    clicked_comment,
                    cx,
                );
            } else {
                entity.update(cx, |this, cx| {
                    this.remote_source_context = RemoteSourceContextState::Failed {
                        comment_id: clicked_comment.id,
                        path: clicked_comment.path.clone(),
                        commit_sha: clicked_comment.original_commit_id.clone(),
                        message: "GitHub session expired; reconnect to load the reviewed source."
                            .into(),
                    };
                    cx.notify();
                });
            }
        })
}

fn render_remote_source_context(state: &RemoteSourceContextState) -> impl IntoElement {
    match state {
        RemoteSourceContextState::Idle => div(),
        RemoteSourceContextState::Loading {
            path, commit_sha, ..
        } => div().p_2().text_color(rgb(0x009b_aaba)).child(format!(
            "Loading reviewed source {path} at {}…",
            short_sha(commit_sha)
        )),
        RemoteSourceContextState::Loaded {
            comment_id,
            source,
            anchor_line,
        } => div()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .bg(rgb(0x0011_1820))
            .child(format!(
                "COMMENT #{comment_id} · {} · {}",
                source.path,
                short_sha(&source.commit_sha)
            ))
            .child(
                div()
                    .text_color(rgb(0x00b5_c0cb))
                    .child(source_excerpt(&source.content, *anchor_line)),
            ),
        RemoteSourceContextState::Failed {
            comment_id,
            path,
            commit_sha,
            message,
        } => div().p_2().text_color(rgb(0x00e5_b6ba)).child(format!(
            "Could not load context for comment #{comment_id} ({path} at {}): {message}",
            short_sha(commit_sha)
        )),
    }
}

fn short_sha(sha: &str) -> &str {
    sha.get(..8).unwrap_or(sha)
}

fn source_excerpt(content: &str, anchor_line: Option<u32>) -> String {
    let lines = content.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        return "(empty source file)".to_string();
    }
    let anchor = anchor_line
        .and_then(|line| usize::try_from(line).ok())
        .filter(|line| *line > 0)
        .unwrap_or(1);
    let start = anchor.saturating_sub(5).max(1);
    let end = lines.len().min(anchor.saturating_add(5));
    let mut excerpt = String::new();
    for line_number in start..=end {
        if let Some(line) = lines.get(line_number - 1) {
            use std::fmt::Write as _;
            let mut bounded_line = line.chars().take(500).collect::<String>();
            if line.chars().count() > 500 {
                bounded_line.push('…');
            }
            let _ = writeln!(excerpt, "{line_number:>5} {bounded_line}");
        }
    }
    excerpt
}

fn render_remote_conversation_comment(
    comment: &github_pr::PullRequestConversationComment,
) -> impl IntoElement {
    div()
        .p_2()
        .bg(rgb(0x0020_2b36))
        .child(format!(
            "CONVERSATION · @{} · {}",
            comment.author, comment.created_at
        ))
        .child(
            div()
                .mt_1()
                .text_color(rgb(0x00b5_c0cb))
                .child(comment.body.clone()),
        )
}

fn start_github_device_login(
    entity: Entity<ReviewWorkspace>,
    client_id: String,
    http: Arc<dyn gpui::http_client::HttpClient>,
    cx: &Context<ReviewWorkspace>,
) {
    cx.spawn(async move |_this, cx| {
        let task_entity = entity.clone();
        let result = async {
            let flow = github_auth::GitHubDeviceFlow::new(client_id, http)?;
            let authorization =
                with_network_timeout(flow.request_device_code(), cx.background_executor()).await?;
            let cancel = Arc::new(AtomicBool::new(false));
            let verification_url = authorization.verification_uri.to_string();
            cx.update(|app| app.open_url(&verification_url))
                .context("could not open the GitHub device verification page")?;
            entity.update(cx, |this, cx| {
                this.github_auth_state = GitHubAuthState::AwaitingDeviceAuthorization {
                    user_code: authorization.user_code.clone(),
                    verification_url,
                    expires_at: authorization.expires_at,
                    cancel: cancel.clone(),
                };
                cx.notify();
            })?;
            let Some(credentials) = poll_for_device_authorization(
                &flow,
                &authorization,
                &cancel,
                cx.background_executor(),
            )
            .await?
            else {
                entity.update(cx, |this, cx| {
                    this.github_auth_state = GitHubAuthState::SignedOut;
                    this.notice = Some("GitHub sign-in cancelled.".into());
                    cx.notify();
                })?;
                return Ok::<(), anyhow::Error>(());
            };
            entity.update(cx, |this, cx| {
                this.github_auth_state = GitHubAuthState::SavingCredentials;
                cx.notify();
            })?;
            let credentials_to_store = credentials.clone();
            cx.background_executor()
                .spawn(async move { github_auth::CredentialStore::save(&credentials_to_store) })
                .await?;
            entity.update(cx, |this, cx| {
                this.github_credentials = Some(credentials);
                this.github_auth_state = GitHubAuthState::SignedIn;
                this.notice =
                    Some("GitHub connected. Remote actions still need confirmation.".into());
                cx.notify();
            })?;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(error) = result {
            let _ = task_entity.update(cx, |this, cx| {
                this.github_auth_state =
                    GitHubAuthState::Failed(format!("GitHub sign-in failed: {error}").into());
                cx.notify();
            });
        }
    })
    .detach();
}

async fn poll_for_device_authorization(
    flow: &github_auth::GitHubDeviceFlow,
    authorization: &github_auth::DeviceAuthorization,
    cancel: &AtomicBool,
    executor: &BackgroundExecutor,
) -> anyhow::Result<Option<github_auth::StoredCredentials>> {
    let mut interval = authorization.polling_interval;
    loop {
        if wait_for_poll_interval(interval, authorization.expires_at, cancel, executor).await {
            return Ok(None);
        }
        if Instant::now() >= authorization.expires_at {
            anyhow::bail!("GitHub device code expired; start sign-in again");
        }
        let poll = with_network_timeout(flow.poll(authorization), executor).await?;
        if cancel.load(Ordering::Acquire) {
            return Ok(None);
        }
        match poll {
            github_auth::DevicePoll::Pending => {}
            github_auth::DevicePoll::SlowDown => {
                interval += Duration::from_secs(5);
            }
            github_auth::DevicePoll::Authorized(credentials) => return Ok(Some(credentials)),
        }
    }
}

async fn wait_for_poll_interval(
    interval: Duration,
    expires_at: Instant,
    cancel: &AtomicBool,
    executor: &BackgroundExecutor,
) -> bool {
    let poll_at = Instant::now() + interval;
    loop {
        if cancel.load(Ordering::Acquire) {
            return true;
        }
        let now = Instant::now();
        if now >= poll_at || now >= expires_at {
            return false;
        }
        let remaining = poll_at.saturating_duration_since(now);
        executor
            .timer(remaining.min(Duration::from_millis(100)))
            .await;
    }
}

fn start_github_refresh(
    entity: Entity<ReviewWorkspace>,
    client_id: String,
    http: Arc<dyn gpui::http_client::HttpClient>,
    credentials: github_auth::StoredCredentials,
    cx: &Context<ReviewWorkspace>,
) {
    cx.spawn(async move |_this, cx| {
        let task_entity = entity.clone();
        let result = async {
            let flow = github_auth::GitHubDeviceFlow::new(client_id, http)?;
            let refreshed =
                with_network_timeout(flow.refresh(&credentials), cx.background_executor()).await?;
            let refreshed_to_store = refreshed.clone();
            cx.background_executor()
                .spawn(async move { github_auth::CredentialStore::save(&refreshed_to_store) })
                .await?;
            entity.update(cx, |this, cx| {
                this.github_credentials = Some(refreshed);
                this.github_auth_state = GitHubAuthState::SignedIn;
                this.notice = Some("GitHub session refreshed.".into());
                cx.notify();
            })?;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(error) = result {
            let _ = task_entity.update(cx, |this, cx| {
                this.github_auth_state = GitHubAuthState::SignedOut;
                this.session_store_error = Some(
                    format!("Could not refresh GitHub session; sign in again: {error}").into(),
                );
                cx.notify();
            });
        }
    })
    .detach();
}

fn start_github_disconnect(entity: Entity<ReviewWorkspace>, cx: &Context<ReviewWorkspace>) {
    cx.spawn(async move |_this, cx| {
        let result = cx
            .background_executor()
            .spawn(async { github_auth::CredentialStore::delete() })
            .await;
        let _ = entity.update(cx, |this, cx| {
            match result {
                Ok(()) => {
                    this.github_credentials = None;
                    this.github_auth_state = GitHubAuthState::SignedOut;
                    this.notice = Some("GitHub disconnected and local credentials removed.".into());
                }
                Err(error) => {
                    this.github_auth_state = GitHubAuthState::Failed(
                        format!("Could not remove GitHub credentials: {error}").into(),
                    );
                }
            }
            cx.notify();
        });
    })
    .detach();
}

fn delete_comment_draft(
    entity: Entity<ReviewWorkspace>,
    store: session_store::SessionStore,
    root: PathBuf,
    draft_id: i64,
    cx: &mut App,
) {
    entity.update(cx, |this, cx| {
        this.deleting_draft_id = Some(draft_id);
        this.notice = Some("Deleting local draft…".into());
        cx.notify();
    });
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move { store.delete_comment_draft(&root, draft_id) })
            .await;
        let _ = entity.update(cx, |this, cx| {
            this.deleting_draft_id = None;
            match result {
                Ok(true) => {
                    this.comment_drafts.retain(|draft| draft.id != draft_id);
                    if this
                        .editing_draft
                        .as_ref()
                        .is_some_and(|draft| draft.id == draft_id)
                    {
                        this.editing_draft = None;
                    }
                    this.notice = Some("Local draft deleted.".into());
                }
                Ok(false) => {
                    this.comment_drafts.retain(|draft| draft.id != draft_id);
                    this.notice = Some("Draft was already deleted in another app instance.".into());
                }
                Err(error) => {
                    this.session_store_error =
                        Some(format!("Could not delete comment draft: {error}").into());
                }
            }
            cx.notify();
        });
    })
    .detach();
}

fn persist_comment_draft(entity: Entity<ReviewWorkspace>, save: DraftSave, cx: &mut App) {
    entity.update(cx, |this, cx| {
        this.is_saving_draft = true;
        this.notice = Some("Saving comment draft locally…".into());
        cx.notify();
    });
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move {
                save.store.save_comment_draft(
                    &save.repository_root,
                    save.base_head_oid.as_deref(),
                    save.current_head_oid.as_deref(),
                    &save.draft,
                    save.expected_revision,
                )
            })
            .await;
        let _ = entity.update(cx, |this, cx| {
            this.is_saving_draft = false;
            match result {
                Ok(saved) => {
                    this.comment_drafts.retain(|draft| draft.id != saved.id);
                    this.editing_draft = Some(saved.clone());
                    this.comment_drafts.push(saved);
                    this.session_store_error = None;
                    this.notice = Some("Draft saved locally · not published".into());
                }
                Err(error) => {
                    this.session_store_error =
                        Some(format!("Could not save comment draft: {error}").into());
                }
            }
            cx.notify();
        });
    })
    .detach();
}

fn main() {
    Application::new().run(|cx: &mut App| {
        gpui_component::init(cx);
        let bounds = Bounds::centered(None, size(px(1440.0), px(900.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| ReviewWorkspace::from_cli()),
        )
        .unwrap();
        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use super::{
        DIFF_LINE_COUNT, DiffKind, LocalReviewRequest, LocalReviewRequestContext,
        RemoteReviewState, ReviewMode, ReviewWorkspace, SAMPLE_FILE_PATHS,
        SuggestedChangePreviewDecision, append_reply_to_snapshot, locate_remote_comment,
        parse_diff_file, source_excerpt, suggested_change_application_is_current,
        suggested_change_preview_decision, validate_local_review_request,
        validate_local_review_snapshot,
    };
    use std::fs;

    #[test]
    fn suggestion_preview_does_not_replace_an_in_flight_application() {
        assert_eq!(
            suggested_change_preview_decision(true, true),
            SuggestedChangePreviewDecision::IgnoreBusy
        );
        assert_eq!(
            suggested_change_preview_decision(false, false),
            SuggestedChangePreviewDecision::RejectStale
        );
        assert_eq!(
            suggested_change_preview_decision(false, true),
            SuggestedChangePreviewDecision::Start
        );
    }

    #[test]
    fn suggestion_application_waits_for_repository_open_to_finish() {
        assert!(!suggested_change_application_is_current(true, true));
        assert!(suggested_change_application_is_current(false, true));
    }

    #[test]
    fn review_workspace_generates_large_diff_and_initial_selection() {
        let workspace = ReviewWorkspace::sample();

        assert_eq!(workspace.files.len(), SAMPLE_FILE_PATHS.len());
        assert_eq!(
            workspace
                .files
                .iter()
                .map(|file| file.lines.len())
                .sum::<usize>(),
            DIFF_LINE_COUNT
        );
        assert_eq!(workspace.selected_file, Some(0));
        assert_eq!(workspace.selected_line, 42);
        let file = &workspace.files[0];
        assert_eq!(file.path.as_ref(), SAMPLE_FILE_PATHS[0]);
        assert_eq!(file.lines[0].kind, DiffKind::Context);
        assert_eq!(file.lines[3].kind, DiffKind::Removed);
        assert_eq!(file.lines[5].kind, DiffKind::Added);
        assert_eq!(file.lines[0].old_number, Some(1));
        assert_eq!(file.lines[0].new_number, Some(1));
        assert_eq!(file.lines[5].old_number, None);
        assert_eq!(file.lines[5].new_number, Some(4));
        let mut anchored_lines = 0;
        for line in &file.lines {
            match line.kind {
                DiffKind::Context | DiffKind::Added => {
                    assert!(line.new_number.is_some());
                    anchored_lines += 1;
                }
                DiffKind::Removed => assert!(line.new_number.is_none()),
            }
        }
        assert_eq!(anchored_lines, file.lines.len() - file.removed);
    }

    #[test]
    fn local_drafts_are_blocked_while_loading_or_reviewing_a_remote_pull_request() {
        let reference = crate::github_pr::PullRequestRef::parse("octo/repo#9").unwrap();
        assert!(!RemoteReviewState::Idle.blocks_local_drafts());
        assert!(RemoteReviewState::Loading(reference.clone()).blocks_local_drafts());
        assert!(
            RemoteReviewState::Loaded(Box::new(crate::github_pr::PullRequestSnapshot {
                reference,
                title: "remote review".to_string(),
                state: "open".to_string(),
                author: "octocat".to_string(),
                html_url: "https://github.com/octo/repo/pull/9".to_string(),
                base_sha: "base".to_string(),
                head_sha: "head".to_string(),
                files: Vec::new(),
                reviews: Vec::new(),
                review_threads: Vec::new(),
                conversation_comments: Vec::new(),
            }))
            .blocks_local_drafts()
        );
    }

    #[test]
    fn selecting_a_left_side_comment_resolves_the_deleted_diff_line() {
        let diff = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -4,2 +4,2 @@\n context\n-removed\n+added\n";
        let files = cururu_core::parse_unified_diff(diff)
            .into_iter()
            .map(parse_diff_file)
            .collect::<Vec<_>>();
        let comment = crate::github_pr::PullRequestComment {
            id: 1,
            path: "src/lib.rs".to_string(),
            line: None,
            original_line: Some(5),
            body: "Why remove this?".to_string(),
            author: "reviewer".to_string(),
            html_url: "https://github.com/org/repo/pull/1#discussion_r1".to_string(),
            commit_id: "head".to_string(),
            original_commit_id: "old".to_string(),
            in_reply_to_id: None,
            is_outdated: true,
            subject_type: Some("line".to_string()),
            side: Some("LEFT".to_string()),
        };
        assert_eq!(locate_remote_comment(&files, &comment), Some((0, 1)));
    }

    #[test]
    fn source_excerpt_is_bounded_and_centers_on_the_reviewed_line() {
        let content = (1..=100)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let excerpt = source_excerpt(&content, Some(50));
        assert!(excerpt.contains("   45 line 45"));
        assert!(excerpt.contains("   50 line 50"));
        assert!(excerpt.contains("   55 line 55"));
        assert!(!excerpt.contains("line 44"));
        assert!(!excerpt.contains("line 56"));
    }

    #[test]
    fn published_reply_is_added_to_its_thread_without_creating_duplicates() {
        let reference = crate::github_pr::PullRequestRef::parse("octo/repo#9").unwrap();
        let target = crate::github_pr::PullRequestComment {
            id: 7,
            path: "src/lib.rs".to_string(),
            line: Some(8),
            original_line: Some(8),
            body: "Please keep this check.".to_string(),
            author: "reviewer".to_string(),
            html_url: "https://github.com/octo/repo/pull/9#discussion_r7".to_string(),
            commit_id: "head".to_string(),
            original_commit_id: "base".to_string(),
            in_reply_to_id: None,
            is_outdated: false,
            subject_type: Some("line".to_string()),
            side: Some("RIGHT".to_string()),
        };
        let mut snapshot = crate::github_pr::PullRequestSnapshot {
            reference,
            title: "Review parser".to_string(),
            state: "open".to_string(),
            author: "octocat".to_string(),
            html_url: "https://github.com/octo/repo/pull/9".to_string(),
            base_sha: "base".to_string(),
            head_sha: "head".to_string(),
            files: Vec::new(),
            reviews: Vec::new(),
            review_threads: vec![crate::github_pr::PullRequestReviewThread {
                root: target,
                replies: Vec::new(),
            }],
            conversation_comments: Vec::new(),
        };
        let reply = crate::github_pr::PullRequestComment {
            id: 8,
            path: "src/lib.rs".to_string(),
            line: Some(8),
            original_line: Some(8),
            body: "I kept it and added a test.".to_string(),
            author: "octocat".to_string(),
            html_url: "https://github.com/octo/repo/pull/9#discussion_r8".to_string(),
            commit_id: "head".to_string(),
            original_commit_id: "base".to_string(),
            in_reply_to_id: Some(7),
            is_outdated: false,
            subject_type: Some("line".to_string()),
            side: Some("RIGHT".to_string()),
        };

        assert!(append_reply_to_snapshot(&mut snapshot, 7, reply.clone()));
        assert!(append_reply_to_snapshot(&mut snapshot, 7, reply));
        assert_eq!(snapshot.review_threads[0].replies.len(), 1);
        assert_eq!(snapshot.review_threads[0].replies[0].id, 8);
        let orphaned_reply = snapshot.review_threads[0].replies[0].clone();
        assert!(!append_reply_to_snapshot(&mut snapshot, 99, orphaned_reply));
    }

    #[test]
    fn switching_review_modes_preserves_the_open_workspace_and_drafts() {
        let mut workspace = ReviewWorkspace::sample();
        let original_file_count = workspace.files.len();
        let original_selection = workspace.selected_file;
        let original_draft_count = workspace.comment_drafts.len();

        workspace.select_review_mode(ReviewMode::UnderstandReview);

        assert_eq!(workspace.review_mode, ReviewMode::UnderstandReview);
        assert_eq!(workspace.files.len(), original_file_count);
        assert_eq!(workspace.selected_file, original_selection);
        assert_eq!(workspace.comment_drafts.len(), original_draft_count);

        workspace.select_review_mode(ReviewMode::ReviewChanges);

        assert_eq!(workspace.review_mode, ReviewMode::ReviewChanges);
        assert_eq!(workspace.files.len(), original_file_count);
        assert_eq!(workspace.selected_file, original_selection);
        assert_eq!(workspace.comment_drafts.len(), original_draft_count);
    }

    #[test]
    fn selecting_a_file_resets_the_diff_selection_and_rejects_invalid_indices() {
        let mut workspace = ReviewWorkspace::sample();

        workspace.select_file(1);

        assert_eq!(workspace.selected_file, Some(1));
        assert_eq!(workspace.selected_line, 0);
        assert_eq!(
            workspace.files[workspace.selected_file.unwrap()]
                .path
                .as_ref(),
            SAMPLE_FILE_PATHS[1]
        );

        workspace.select_file(usize::MAX);

        assert_eq!(workspace.selected_file, Some(1));
        assert_eq!(workspace.selected_line, 0);
    }

    #[test]
    fn real_unified_hunk_offsets_are_used_for_old_side_line_numbers() {
        let diff = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -20,2 +40,2 @@\n-old\n+new\n context\n";
        let file = cururu_core::parse_unified_diff(diff).remove(0);
        let file = super::parse_diff_file(file);

        assert_eq!(file.lines[0].old_number, Some(20));
        assert_eq!(file.lines[0].new_number, None);
        assert_eq!(file.lines[1].old_number, None);
        assert_eq!(file.lines[1].new_number, Some(40));
        assert_eq!(file.lines[2].old_number, Some(21));
        assert_eq!(file.lines[2].new_number, Some(41));
    }

    #[test]
    fn opening_a_local_repository_replaces_the_sample_workspace() {
        let directory = tempfile::tempdir().unwrap();
        gix::init(directory.path()).unwrap();
        fs::write(directory.path().join("notes.txt"), "local change\n").unwrap();
        let opened = super::load_repository_session(directory.path().to_path_buf(), None).unwrap();
        let mut workspace = ReviewWorkspace::sample();

        workspace.apply_repository_result(Ok(opened));

        assert_eq!(workspace.files.len(), 1);
        assert_eq!(workspace.files[0].path.as_ref(), "notes.txt");
        assert_eq!(workspace.selected_file, Some(0));
        assert_eq!(workspace.selected_line, 0);
        assert!(workspace.repository_root.is_some());
        assert!(workspace.local_repository_snapshot.is_some());
        assert!(workspace.local_review_identity.is_some());
        assert!(workspace.notice.as_ref().unwrap().contains("read-only"));
    }

    #[test]
    fn opening_a_repository_records_it_for_explicit_session_resume() {
        let directory = tempfile::tempdir().unwrap();
        let repository_root = directory.path().join("repo");
        gix::init(&repository_root).unwrap();
        fs::write(repository_root.join("notes.txt"), "local change\n").unwrap();
        let store = super::session_store::SessionStore::open(
            directory.path().join("app-data/sessions.sqlite3"),
        )
        .unwrap();
        let settings = super::session_store::LocalReviewSettings {
            openai_endpoint: "https://api.example.test/v1".to_string(),
            openai_model: "review-model".to_string(),
        };
        store.save_local_review_settings(&settings).unwrap();
        let identity = super::local_review::repository_identity(&repository_root).unwrap();
        let fingerprint = super::local_review::config_fingerprint(&settings).unwrap();
        store
            .authorize_local_review(&identity, &fingerprint)
            .unwrap();

        let opened = super::load_repository_session(repository_root.clone(), Some(&store)).unwrap();
        let mut workspace = ReviewWorkspace::sample();
        workspace.apply_repository_result(Ok(opened));

        let saved = store.most_recent().unwrap().unwrap();
        assert_eq!(saved.repository_root, repository_root);
        assert_eq!(saved.branch.as_deref(), Some("main"));
        assert_eq!(workspace.last_session, Some(saved));
        assert_eq!(
            workspace.local_review_authorized_for,
            Some((identity, fingerprint))
        );
    }

    #[test]
    fn local_review_preflight_requires_current_consent_and_the_previewed_diff() {
        let directory = tempfile::tempdir().unwrap();
        let repository_root = directory.path().join("repo");
        gix::init(&repository_root).unwrap();
        let changed_file = repository_root.join("notes.txt");
        fs::write(&changed_file, "first local change\n").unwrap();
        let snapshot = super::repository::RepositorySnapshot::open(&repository_root).unwrap();
        let store = super::session_store::SessionStore::open(
            directory.path().join("app-data/sessions.sqlite3"),
        )
        .unwrap();
        let settings = super::session_store::LocalReviewSettings {
            openai_endpoint: "https://api.example.test/v1".to_string(),
            openai_model: "review-model".to_string(),
        };
        store.save_local_review_settings(&settings).unwrap();
        let identity = super::local_review::repository_identity(&repository_root).unwrap();
        let config_fingerprint = super::local_review::config_fingerprint(&settings).unwrap();
        let snapshot_fingerprint = super::local_review::snapshot_fingerprint(&snapshot);
        let request = LocalReviewRequest {
            context: LocalReviewRequestContext {
                request_id: 1,
                repository_identity: identity.clone(),
                config_fingerprint: config_fingerprint.clone(),
                snapshot_fingerprint,
            },
            repository_root: repository_root.clone(),
            settings,
        };

        store
            .authorize_local_review(&identity, &config_fingerprint)
            .unwrap();
        assert!(validate_local_review_request(&store, &request).is_ok());
        assert!(validate_local_review_snapshot(&request, &snapshot).is_ok());

        fs::write(&changed_file, "changed after preview\n").unwrap();
        let changed_snapshot =
            super::repository::RepositorySnapshot::open(&repository_root).unwrap();
        assert!(validate_local_review_snapshot(&request, &changed_snapshot).is_err());

        store.revoke_local_review_authorization(&identity).unwrap();
        assert!(validate_local_review_request(&store, &request).is_err());
    }
}
