use std::{fmt, path::Path, sync::Arc};

use anyhow::{Context as _, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use gpui::http_client::{
    AsyncBody, HttpClient, HttpRequestExt as _, Method, RedirectPolicy, Request, StatusCode, Url,
};
use http_body_util::BodyExt as _;
use serde::{Deserialize, Serialize};

const API_ROOT: &str = "https://api.github.com";
const API_VERSION: &str = "2022-11-28";
const PER_PAGE: usize = 100;
const MAX_PAGE_BYTES: usize = 1_048_576;
const MAX_COLLECTION_BYTES: usize = 8 * 1_048_576;
const MAX_COLLECTION_PAGES: usize = 50;
const MAX_REMOTE_DIFF_FILE_BYTES: usize = 45_000;
const MAX_REMOTE_DIFF_BYTES: usize = 180_000;
const MAX_SOURCE_FILE_BYTES: usize = 256 * 1024;
const MAX_THREAD_REPLY_BYTES: usize = 10_000;
const MAX_MUTATION_RESPONSE_BYTES: usize = 64_000;
pub(crate) const MAX_REMOTE_COMMIT_FILES: usize = 100;
pub(crate) const MAX_REMOTE_COMMIT_FILE_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const MAX_REMOTE_COMMIT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PullRequestRef {
    pub owner: String,
    pub repository: String,
    pub number: u64,
}

impl PullRequestRef {
    pub fn parse(input: &str) -> anyhow::Result<Self> {
        let (repository, number) = input
            .rsplit_once('#')
            .context("Use owner/repository#number to identify a pull request")?;
        let (owner, repository_name) = repository
            .split_once('/')
            .context("Use owner/repository#number to identify a pull request")?;
        ensure!(
            !repository_name.contains('/')
                && valid_segment(owner)
                && valid_segment(repository_name),
            "Pull request repository must be a GitHub owner/repository"
        );
        let number = number
            .parse::<u64>()
            .context("Pull request number must be a positive integer")?;
        ensure!(number > 0, "Pull request number must be positive");
        Ok(Self {
            owner: owner.to_string(),
            repository: repository_name.to_string(),
            number,
        })
    }

    fn api_path(&self, suffix: &str) -> String {
        format!(
            "/repos/{}/{}/pulls/{}{}?per_page={PER_PAGE}&page=1",
            self.owner, self.repository, self.number, suffix
        )
    }
}

fn valid_segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
}

fn validate_pull_request_reference(reference: &PullRequestRef) -> anyhow::Result<()> {
    ensure!(
        valid_segment(&reference.owner)
            && valid_segment(&reference.repository)
            && reference.number > 0,
        "pull request reference is invalid"
    );
    Ok(())
}

pub fn validate_thread_reply_body(body: &str) -> anyhow::Result<()> {
    ensure!(
        !body.trim().is_empty(),
        "a GitHub thread reply cannot be empty"
    );
    ensure!(
        body.len() <= MAX_THREAD_REPLY_BYTES,
        "a GitHub thread reply cannot exceed 10,000 bytes"
    );
    ensure!(
        !body.chars().any(|character| {
            character == '\0'
                || (character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
        }),
        "a GitHub thread reply contains unsupported control characters"
    );
    Ok(())
}

#[derive(Serialize)]
struct CreateThreadReplyRequest<'a> {
    body: &'a str,
    in_reply_to: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewReviewComment {
    pub path: String,
    pub line: u32,
    pub body: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewEvent {
    #[default]
    Comment,
    RequestChanges,
}

impl ReviewEvent {
    const fn api_value(self) -> &'static str {
        match self {
            Self::Comment => "COMMENT",
            Self::RequestChanges => "REQUEST_CHANGES",
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Comment => "COMMENT",
            Self::RequestChanges => "REQUEST_CHANGES",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingReview {
    pub reference: PullRequestRef,
    pub base_sha: String,
    pub head_sha: String,
    #[serde(default)]
    pub event: ReviewEvent,
    pub body: String,
    pub comments: Vec<NewReviewComment>,
}

pub fn validate_saved_review(review: &PendingReview) -> anyhow::Result<()> {
    validate_pull_request_reference(&review.reference)?;
    ensure!(
        review.reference.owner.len() <= 256 && review.reference.repository.len() <= 256,
        "saved review repository name exceeds the limit"
    );
    ensure!(
        valid_commit_sha(&review.base_sha) && valid_commit_sha(&review.head_sha),
        "saved review has invalid commit identifiers"
    );
    ensure!(
        review.event == ReviewEvent::Comment,
        "saved comment drafts cannot persist a review vote"
    );
    if !review.body.is_empty() {
        validate_thread_reply_body(&review.body)?;
    }
    ensure!(
        review.comments.len() == 1,
        "saved draft must contain exactly one inline comment"
    );
    for comment in &review.comments {
        validate_relative_path(&comment.path)?;
        ensure!(
            comment.path.len() <= 4_096 && comment.line > 0,
            "saved comment has an invalid anchor"
        );
        validate_thread_reply_body(&comment.body)?;
    }
    Ok(())
}

pub fn validate_review_comments(
    files: &[cururu_core::ChangedFile],
    comments: &[NewReviewComment],
) -> anyhow::Result<()> {
    ensure!(
        !comments.is_empty() && comments.len() <= 50,
        "a review requires 1 to 50 inline comments"
    );
    let mut total_bytes = 0_usize;
    for (index, comment) in comments.iter().enumerate() {
        validate_relative_path(&comment.path)?;
        validate_thread_reply_body(&comment.body)?;
        total_bytes = total_bytes.saturating_add(comment.body.len());
        ensure!(
            total_bytes <= 64_000,
            "review comments exceed the 64,000-byte aggregate limit"
        );
        ensure!(
            comment.line > 0
                && files.iter().any(|file| {
                    file.path == comment.path && file.right_lines.contains(&comment.line)
                }),
            "a review comment is not anchored to the loaded new-side diff"
        );
        ensure!(
            !comments[..index].contains(comment),
            "the review contains a duplicate comment"
        );
    }
    Ok(())
}

fn validate_review_metadata(
    event: ReviewEvent,
    body: &str,
    comments: &[NewReviewComment],
) -> anyhow::Result<()> {
    validate_thread_reply_body(body)?;
    ensure!(
        comments.len() <= 50,
        "a review supports at most 50 inline comments"
    );
    ensure!(
        event == ReviewEvent::RequestChanges || !comments.is_empty(),
        "a COMMENT review requires at least one inline comment"
    );
    let mut total_bytes = 0_usize;
    for comment in comments {
        validate_relative_path(&comment.path)?;
        validate_thread_reply_body(&comment.body)?;
        total_bytes = total_bytes.saturating_add(comment.body.len());
    }
    ensure!(
        total_bytes <= 64_000,
        "review comments exceed the aggregate limit"
    );
    Ok(())
}

pub(crate) fn validate_review_submission(
    event: ReviewEvent,
    files: &[cururu_core::ChangedFile],
    body: &str,
    comments: &[NewReviewComment],
) -> anyhow::Result<()> {
    validate_review_metadata(event, body, comments)?;
    if !comments.is_empty() {
        validate_review_comments(files, comments)?;
    }
    Ok(())
}

#[derive(Serialize)]
struct CreateReviewRequest<'a> {
    commit_id: &'a str,
    body: &'a str,
    event: &'static str,
    comments: Vec<CreateReviewComment<'a>>,
}

#[derive(Serialize)]
struct CreateReviewComment<'a> {
    path: &'a str,
    line: u32,
    side: &'static str,
    body: &'a str,
}

#[derive(Serialize)]
struct CreateGitTreeRequest<'a> {
    base_tree: &'a str,
    tree: Vec<CreateGitTreeEntry<'a>>,
}

#[derive(Serialize)]
struct CreateGitTreeEntry<'a> {
    path: &'a str,
    mode: &'a str,
    #[serde(rename = "type")]
    object_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<&'a str>,
    #[serde(skip_serializing_if = "GitTreeEntrySha::is_omitted")]
    sha: GitTreeEntrySha<'a>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum GitTreeEntrySha<'a> {
    Omitted,
    Null(Option<&'a str>),
}

impl GitTreeEntrySha<'_> {
    const fn is_omitted(&self) -> bool {
        matches!(self, Self::Omitted)
    }
}

#[derive(Serialize)]
struct CreateGitCommitRequest<'a> {
    message: &'a str,
    tree: &'a str,
    parents: Vec<&'a str>,
}

#[derive(Serialize)]
struct UpdateGitReferenceRequest<'a> {
    sha: &'a str,
    force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestSnapshot {
    pub reference: PullRequestRef,
    pub title: String,
    pub state: String,
    pub author: String,
    pub html_url: String,
    pub base_sha: String,
    pub head_sha: String,
    pub head_repository: Option<GitHubRepositoryRef>,
    pub head_branch: Option<String>,
    pub files: Vec<cururu_core::ChangedFile>,
    pub reviews: Vec<PullRequestReview>,
    pub review_threads: Vec<PullRequestReviewThread>,
    pub conversation_comments: Vec<PullRequestConversationComment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubRepositoryRef {
    pub owner: String,
    pub repository: String,
}

impl GitHubRepositoryRef {
    pub fn parse_full_name(value: &str) -> anyhow::Result<Self> {
        let (owner, repository) = value
            .split_once('/')
            .context("GitHub repository identity must be owner/repository")?;
        ensure!(
            valid_segment(owner) && valid_segment(repository) && !repository.contains('/'),
            "GitHub repository identity is invalid"
        );
        Ok(Self {
            owner: owner.to_string(),
            repository: repository.to_string(),
        })
    }

    fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            valid_segment(&self.owner) && valid_segment(&self.repository),
            "GitHub repository identity is invalid"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubCommitFile {
    pub path: String,
    pub mode: String,
    pub content: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCommitRequest {
    pub reference: PullRequestRef,
    pub repository: GitHubRepositoryRef,
    pub branch: String,
    pub base_sha: String,
    pub parent_sha: String,
    pub message: String,
    pub files: Vec<GitHubCommitFile>,
    pub expected_suggestions: Vec<PullRequestComment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubCommitObject {
    pub reference: PullRequestRef,
    pub repository: GitHubRepositoryRef,
    pub branch: String,
    pub base_sha: String,
    pub parent_sha: String,
    pub commit_sha: String,
    pub message: String,
    pub html_url: String,
    pub files: Vec<GitHubCommitFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubBranchUpdate {
    pub reference: PullRequestRef,
    pub repository: GitHubRepositoryRef,
    pub branch: String,
    pub previous_sha: String,
    pub commit_sha: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestComment {
    pub id: u64,
    pub path: String,
    pub line: Option<u32>,
    pub original_line: Option<u32>,
    pub body: String,
    pub author: String,
    pub html_url: String,
    pub commit_id: String,
    pub original_commit_id: String,
    pub in_reply_to_id: Option<u64>,
    pub is_outdated: bool,
    pub subject_type: Option<String>,
    pub side: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestReviewThread {
    pub root: PullRequestComment,
    pub replies: Vec<PullRequestComment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestReview {
    pub id: u64,
    pub state: String,
    pub body: String,
    pub author: String,
    pub submitted_at: Option<String>,
    pub commit_id: Option<String>,
    pub html_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestConversationComment {
    pub id: u64,
    pub body: String,
    pub author: String,
    pub created_at: String,
    pub html_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestSourceFile {
    pub path: String,
    pub commit_sha: String,
    pub content: String,
}

pub struct GitHubPullRequestClient {
    http: Arc<dyn HttpClient>,
    access_token: String,
}

impl fmt::Debug for GitHubPullRequestClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitHubPullRequestClient")
            .field("http", &"[HTTP client]")
            .field("access_token", &"[REDACTED]")
            .finish()
    }
}

impl GitHubPullRequestClient {
    pub fn new(http: Arc<dyn HttpClient>, access_token: String) -> anyhow::Result<Self> {
        ensure!(!access_token.is_empty(), "GitHub access token is required");
        ensure!(
            access_token
                .bytes()
                .all(|byte| byte.is_ascii_graphic() && byte != b'"' && byte != b'\\'),
            "GitHub access token contains invalid header characters"
        );
        Ok(Self { http, access_token })
    }

    pub async fn load_pull_request(
        &self,
        reference: PullRequestRef,
    ) -> anyhow::Result<PullRequestSnapshot> {
        validate_pull_request_reference(&reference)?;
        let pull_request: PullRequestResponse = self
            .get_json(&format!(
                "{API_ROOT}/repos/{}/{}/pulls/{}",
                reference.owner, reference.repository, reference.number
            ))
            .await
            .context("could not load the GitHub pull request")?;
        ensure!(
            pull_request.number == reference.number,
            "GitHub returned a different pull request number"
        );
        ensure!(
            pull_request.state == "open" || pull_request.state == "closed",
            "GitHub returned an unsupported pull request state"
        );
        ensure!(
            safe_web_url(&pull_request.html_url),
            "GitHub returned an invalid pull request URL"
        );

        let file_path = format!("{API_ROOT}{}", reference.api_path("/files"));
        let remote_files: Vec<PullRequestFile> = self.get_all_pages(&file_path).await?;
        let files = parse_pull_request_files(remote_files)?;
        let comments_path = format!("{API_ROOT}{}", reference.api_path("/comments"));
        let remote_comments: Vec<PullRequestReviewComment> =
            self.get_all_pages(&comments_path).await?;
        let comments = parse_pull_request_comments(remote_comments)?;
        let review_path = format!("{API_ROOT}{}", reference.api_path("/reviews"));
        let remote_reviews: Vec<PullRequestReviewResponse> =
            self.get_all_pages(&review_path).await?;
        let reviews = parse_pull_request_reviews(remote_reviews)?;
        let conversation_path = format!(
            "{API_ROOT}/repos/{}/{}/issues/{}/comments?per_page={PER_PAGE}&page=1",
            reference.owner, reference.repository, reference.number
        );
        let remote_conversation: Vec<PullRequestConversationResponse> =
            self.get_all_pages(&conversation_path).await?;
        let conversation_comments = parse_pull_request_conversation(remote_conversation)?;
        let head_repository = pull_request
            .head
            .repository
            .as_ref()
            .map(|repository| GitHubRepositoryRef::parse_full_name(&repository.full_name))
            .transpose()?;

        Ok(PullRequestSnapshot {
            reference,
            title: pull_request.title,
            state: pull_request.state,
            author: pull_request
                .user
                .map_or_else(|| "deleted user".to_string(), |user| user.login),
            html_url: pull_request.html_url,
            base_sha: pull_request.base.sha,
            head_sha: pull_request.head.sha,
            head_repository,
            head_branch: pull_request.head.reference,
            files,
            reviews,
            review_threads: group_review_threads(comments),
            conversation_comments,
        })
    }

    pub async fn load_source_file(
        &self,
        reference: &PullRequestRef,
        path: &str,
        commit_sha: &str,
    ) -> anyhow::Result<PullRequestSourceFile> {
        validate_pull_request_reference(reference)?;
        self.load_repository_file(
            &reference.owner,
            &reference.repository,
            path,
            commit_sha,
            MAX_SOURCE_FILE_BYTES,
        )
        .await
    }

    pub async fn load_shared_config(
        &self,
        source: &crate::shared_base::SharedBaseSource,
    ) -> anyhow::Result<String> {
        source.validate()?;
        let file = self
            .load_repository_file(
                &source.owner,
                &source.repository,
                &source.path,
                &source.commit,
                crate::shared_base::MAX_BASE_BYTES,
            )
            .await?;
        Ok(file.content)
    }

    async fn load_repository_file(
        &self,
        owner: &str,
        repository: &str,
        path: &str,
        commit_sha: &str,
        max_bytes: usize,
    ) -> anyhow::Result<PullRequestSourceFile> {
        validate_relative_path(path)?;
        ensure!(
            valid_commit_sha(commit_sha),
            "GitHub review comment has an invalid commit identifier"
        );
        let mut url = Url::parse(API_ROOT).context("invalid GitHub API root")?;
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|()| anyhow::anyhow!("could not build GitHub contents API path"))?;
            segments
                .push("repos")
                .push(owner)
                .push(repository)
                .push("contents");
            for segment in path.split('/') {
                segments.push(segment);
            }
        }
        url.query_pairs_mut().append_pair("ref", commit_sha);
        let response: PullRequestSourceFileResponse = self.get_json(url.as_str()).await?;
        ensure!(
            response.kind == "file" && response.encoding == "base64",
            "GitHub returned a non-file or unsupported source representation"
        );
        ensure!(
            response.path == path,
            "GitHub returned source for a different repository path"
        );
        ensure!(
            response.size <= max_bytes,
            "Reviewed source file exceeds the desktop context limit"
        );
        let decoded = STANDARD
            .decode(
                response
                    .content
                    .chars()
                    .filter(|character| !character.is_whitespace())
                    .collect::<String>(),
            )
            .context("GitHub returned invalid base64 source content")?;
        ensure!(
            decoded.len() == response.size,
            "GitHub source content size does not match its metadata"
        );
        let content =
            String::from_utf8(decoded).context("Reviewed source file is not valid UTF-8 text")?;
        Ok(PullRequestSourceFile {
            path: response.path,
            commit_sha: commit_sha.to_string(),
            content,
        })
    }

    pub async fn publish_thread_reply(
        &self,
        reference: &PullRequestRef,
        expected_head_sha: &str,
        expected_target: &PullRequestComment,
        body: &str,
    ) -> anyhow::Result<PullRequestComment> {
        validate_pull_request_reference(reference)?;
        validate_thread_reply_body(body)?;
        ensure!(
            valid_commit_sha(expected_head_sha),
            "the confirmed pull request head is not a valid commit identifier"
        );
        ensure!(
            expected_target.id > 0,
            "the reply target must be a positive comment id"
        );

        let target = self
            .revalidate_thread_reply_target(reference, expected_head_sha, expected_target, body)
            .await?;
        self.send_thread_reply(reference, &target, body).await
    }

    pub async fn publish_comment_review(
        &self,
        reference: &PullRequestRef,
        expected_base_sha: &str,
        expected_head_sha: &str,
        body: &str,
        comments: &[NewReviewComment],
    ) -> anyhow::Result<PullRequestReview> {
        self.publish_review(
            reference,
            expected_base_sha,
            expected_head_sha,
            ReviewEvent::Comment,
            body,
            comments,
        )
        .await
    }

    pub async fn publish_review(
        &self,
        reference: &PullRequestRef,
        expected_base_sha: &str,
        expected_head_sha: &str,
        event: ReviewEvent,
        body: &str,
        comments: &[NewReviewComment],
    ) -> anyhow::Result<PullRequestReview> {
        validate_pull_request_reference(reference)?;
        validate_review_metadata(event, body, comments)?;
        ensure!(
            valid_commit_sha(expected_base_sha) && valid_commit_sha(expected_head_sha),
            "the confirmed review has invalid commit identifiers"
        );
        let current = self.load_pull_request(reference.clone()).await?;
        ensure!(
            current.state == "open",
            "the pull request is no longer open; refresh before publishing"
        );
        ensure!(
            current.head_sha == expected_head_sha && current.base_sha == expected_base_sha,
            "the pull request base or head changed; refresh and preview again"
        );
        validate_review_submission(event, &current.files, body, comments)?;
        for comment in comments {
            ensure!(
                !current.review_threads.iter().any(|thread| {
                    let existing = &thread.root;
                    existing.path == comment.path
                        && existing.line == Some(comment.line)
                        && existing.side.as_deref() == Some("RIGHT")
                        && existing.commit_id == expected_head_sha
                        && existing.body == comment.body
                }),
                "an identical comment already exists at this anchor; refresh before publishing"
            );
        }
        self.send_review(reference, expected_head_sha, event, body, comments)
            .await
    }

    async fn send_review(
        &self,
        reference: &PullRequestRef,
        head_sha: &str,
        event: ReviewEvent,
        summary: &str,
        comments: &[NewReviewComment],
    ) -> anyhow::Result<PullRequestReview> {
        let body = serde_json::to_string(&CreateReviewRequest {
            commit_id: head_sha,
            body: summary,
            event: event.api_value(),
            comments: comments
                .iter()
                .map(|comment| CreateReviewComment {
                    path: &comment.path,
                    line: comment.line,
                    side: "RIGHT",
                    body: &comment.body,
                })
                .collect(),
        })?;
        let url = format!(
            "{API_ROOT}/repos/{}/{}/pulls/{}/reviews",
            reference.owner, reference.repository, reference.number
        );
        let request = Request::builder()
            .method(Method::POST)
            .uri(url)
            .follow_redirects(RedirectPolicy::NoFollow)
            .header("Accept", "application/vnd.github+json")
            .header("Authorization", format!("Bearer {}", self.access_token))
            .header("Content-Type", "application/json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .header("User-Agent", "Cururu-Desktop")
            .body(AsyncBody::from(body))?;
        let response = self.http.send(request).await.context(
            "GitHub did not confirm publication; refresh before deciding whether to retry",
        )?;
        let (parts, mut body) = response.into_parts();
        ensure!(
            parts.status == StatusCode::OK,
            "GitHub returned HTTP {} for review publication; refresh before retrying",
            parts.status
        );
        let mut bytes = Vec::new();
        while let Some(frame) = body.frame().await {
            if let Ok(data) = frame?.into_data() {
                ensure!(
                    bytes.len().saturating_add(data.len()) <= MAX_MUTATION_RESPONSE_BYTES,
                    "GitHub review response exceeded the size limit; refresh before retrying"
                );
                bytes.extend_from_slice(&data);
            }
        }
        let response: PullRequestReviewResponse = serde_json::from_slice(&bytes)?;
        let review = parse_pull_request_reviews(vec![response])?
            .pop()
            .context("GitHub did not return the created review")?;
        ensure!(
            review.id > 0
                && review.body == summary
                && review.state
                    == match event {
                        ReviewEvent::Comment => "COMMENTED",
                        ReviewEvent::RequestChanges => "CHANGES_REQUESTED",
                    }
                && review.commit_id.as_deref() == Some(head_sha),
            "GitHub returned a different review; refresh before retrying"
        );
        Ok(review)
    }

    pub async fn create_remote_commit_object(
        &self,
        request: &RemoteCommitRequest,
    ) -> anyhow::Result<GitHubCommitObject> {
        let RemoteCommitRequest {
            reference,
            repository,
            branch,
            base_sha,
            parent_sha,
            message,
            files,
            expected_suggestions,
        } = request;
        validate_remote_commit_target(reference, repository, branch, base_sha, parent_sha)?;
        validate_commit_message(message)?;
        validate_commit_files(files)?;

        let current = self.load_pull_request(reference.clone()).await?;
        ensure!(
            current.state == "open"
                && current.base_sha == *base_sha
                && current.head_sha == *parent_sha,
            "pull request base/head changed; refresh and preview the commit again"
        );
        ensure!(
            current.head_repository.as_ref() == Some(repository)
                && current.head_branch.as_deref() == Some(branch.as_str()),
            "pull request source repository or branch changed; refresh and preview again"
        );
        for expected_suggestion in expected_suggestions {
            let line = crate::remote_suggestion::validate_comment(&current, expected_suggestion)?;
            let source = self
                .load_repository_file(
                    &repository.owner,
                    &repository.repository,
                    &expected_suggestion.path,
                    parent_sha,
                    MAX_SOURCE_FILE_BYTES,
                )
                .await
                .context("could not revalidate GitHub suggestion source at the reviewed head")?;
            let replacement =
                crate::remote_suggestion::parse_suggestion(&expected_suggestion.body)?;
            let expected_content = crate::suggested_change::remote_suggestion_content(
                &source.content,
                line,
                &replacement,
            )?;
            let selected_file = files
                .iter()
                .find(|file| file.path == expected_suggestion.path)
                .context("selected suggestion file is missing from the remote commit preview")?;
            ensure!(
                selected_file.content.as_deref() == Some(expected_content.as_str()),
                "selected suggestion content differs from the exact suggestion at the reviewed head"
            );
        }

        let (parent_tree_sha, parent_tree) =
            self.load_remote_parent_tree(repository, parent_sha).await?;
        validate_commit_files_against_tree(files, &parent_tree)?;
        let mut sorted_files = files.clone();
        sorted_files.sort_by(|left, right| left.path.cmp(&right.path));
        let tree_sha = self
            .create_remote_tree(repository, &parent_tree_sha, &sorted_files)
            .await?;
        let commit_response = self
            .create_remote_git_commit(repository, message, &tree_sha, parent_sha)
            .await?;
        ensure!(
            valid_commit_sha(&commit_response.sha)
                && commit_response.parents.len() == 1
                && commit_response.parents[0].sha == *parent_sha
                && safe_web_url(&commit_response.html_url),
            "GitHub returned a commit that does not match the confirmed parent"
        );
        Ok(GitHubCommitObject {
            reference: reference.clone(),
            repository: repository.clone(),
            branch: branch.clone(),
            base_sha: base_sha.clone(),
            parent_sha: parent_sha.clone(),
            commit_sha: commit_response.sha,
            message: message.clone(),
            html_url: commit_response.html_url,
            files: sorted_files,
        })
    }

    async fn load_remote_parent_tree(
        &self,
        repository: &GitHubRepositoryRef,
        parent_sha: &str,
    ) -> anyhow::Result<(String, Vec<GitHubTreeEntryResponse>)> {
        let parent_commit: GitHubCommitDetailsResponse = self
            .get_json(&format!(
                "{API_ROOT}/repos/{}/{}/git/commits/{parent_sha}",
                repository.owner, repository.repository
            ))
            .await
            .context("could not load the reviewed GitHub commit tree")?;
        ensure!(
            parent_commit.sha == parent_sha && valid_commit_sha(&parent_commit.tree.sha),
            "GitHub returned a different reviewed commit or an invalid tree"
        );
        let parent_tree: GitHubTreeDetailsResponse = self
            .get_json(&format!(
                "{API_ROOT}/repos/{}/{}/git/trees/{}?recursive=1",
                repository.owner, repository.repository, parent_commit.tree.sha
            ))
            .await
            .context("could not inspect the reviewed GitHub tree")?;
        ensure!(
            parent_tree.sha == parent_commit.tree.sha && !parent_tree.truncated,
            "reviewed GitHub tree is truncated or does not match the parent"
        );
        Ok((parent_commit.tree.sha, parent_tree.tree))
    }

    async fn create_remote_tree(
        &self,
        repository: &GitHubRepositoryRef,
        base_tree: &str,
        files: &[GitHubCommitFile],
    ) -> anyhow::Result<String> {
        let tree = CreateGitTreeRequest {
            base_tree,
            tree: files
                .iter()
                .map(|file| CreateGitTreeEntry {
                    path: &file.path,
                    mode: &file.mode,
                    object_type: "blob",
                    content: file.content.as_deref(),
                    sha: if file.content.is_none() {
                        GitTreeEntrySha::Null(None)
                    } else {
                        GitTreeEntrySha::Omitted
                    },
                })
                .collect(),
        };
        let body = serde_json::to_vec(&tree)?;
        ensure!(
            body.len() <= MAX_REMOTE_COMMIT_BYTES + 128 * 1024,
            "remote commit tree request exceeds the size limit"
        );
        let response: GitHubTreeResponse = self
            .send_mutation_json(
                Method::POST,
                &format!(
                    "{API_ROOT}/repos/{}/{}/git/trees",
                    repository.owner, repository.repository
                ),
                body,
                StatusCode::CREATED,
                "GitHub did not confirm tree creation; refresh before deciding whether to retry",
            )
            .await?;
        ensure!(
            valid_commit_sha(&response.sha),
            "GitHub returned an invalid created tree identifier"
        );
        Ok(response.sha)
    }

    async fn create_remote_git_commit(
        &self,
        repository: &GitHubRepositoryRef,
        message: &str,
        tree_sha: &str,
        parent_sha: &str,
    ) -> anyhow::Result<GitHubCommitResponse> {
        let request = CreateGitCommitRequest {
            message,
            tree: tree_sha,
            parents: vec![parent_sha],
        };
        self.send_mutation_json(
            Method::POST,
            &format!(
                "{API_ROOT}/repos/{}/{}/git/commits",
                repository.owner, repository.repository
            ),
            serde_json::to_vec(&request)?,
            StatusCode::CREATED,
            "GitHub did not confirm commit creation; refresh before deciding whether to retry",
        )
        .await
    }

    pub async fn update_remote_branch(
        &self,
        commit: &GitHubCommitObject,
    ) -> anyhow::Result<GitHubBranchUpdate> {
        validate_remote_commit_target(
            &commit.reference,
            &commit.repository,
            &commit.branch,
            &commit.base_sha,
            &commit.parent_sha,
        )?;
        validate_commit_message(&commit.message)?;
        validate_commit_files(&commit.files)?;
        ensure!(
            valid_commit_sha(&commit.commit_sha) && safe_web_url(&commit.html_url),
            "created remote commit identity is invalid"
        );

        let current = self.load_pull_request(commit.reference.clone()).await?;
        ensure!(
            current.state == "open"
                && current.base_sha == commit.base_sha
                && current.head_sha == commit.parent_sha
                && current.head_repository.as_ref() == Some(&commit.repository)
                && current.head_branch.as_deref() == Some(commit.branch.as_str()),
            "pull request changed after commit creation; refresh before updating its branch"
        );

        let created_commit: GitHubCommitDetailsResponse = self
            .get_json(&format!(
                "{API_ROOT}/repos/{}/{}/git/commits/{}",
                commit.repository.owner, commit.repository.repository, commit.commit_sha
            ))
            .await
            .context("could not revalidate the created commit object")?;
        ensure!(
            created_commit.sha == commit.commit_sha
                && created_commit.parents.len() == 1
                && created_commit.parents[0].sha == commit.parent_sha,
            "created commit no longer has the confirmed parent; branch was not updated"
        );

        let reference_url = format!(
            "{API_ROOT}/repos/{}/{}/git/ref/heads/{}",
            commit.repository.owner, commit.repository.repository, commit.branch
        );
        let reference_response: GitHubReferenceResponse = self
            .get_json(&reference_url)
            .await
            .context("could not revalidate the remote branch before update")?;
        ensure!(
            reference_response.reference == format!("refs/heads/{}", commit.branch)
                && reference_response.object.object_type == "commit"
                && reference_response.object.sha == commit.parent_sha,
            "remote branch no longer points to the reviewed parent; refresh before updating"
        );

        let update_request = UpdateGitReferenceRequest {
            sha: &commit.commit_sha,
            force: false,
        };
        let update_body = serde_json::to_vec(&update_request)?;
        let update_url = format!(
            "{API_ROOT}/repos/{}/{}/git/refs/heads/{}",
            commit.repository.owner, commit.repository.repository, commit.branch
        );
        let updated: GitHubReferenceResponse = self
            .send_mutation_json(
                Method::PATCH,
                &update_url,
                update_body,
                StatusCode::OK,
                "GitHub did not confirm the branch update; refresh before deciding whether to retry",
            )
            .await?;
        ensure!(
            updated.reference == format!("refs/heads/{}", commit.branch)
                && updated.object.object_type == "commit"
                && updated.object.sha == commit.commit_sha,
            "GitHub returned a different branch update; refresh before retrying"
        );
        Ok(GitHubBranchUpdate {
            reference: commit.reference.clone(),
            repository: commit.repository.clone(),
            branch: commit.branch.clone(),
            previous_sha: commit.parent_sha.clone(),
            commit_sha: commit.commit_sha.clone(),
        })
    }

    async fn revalidate_thread_reply_target(
        &self,
        reference: &PullRequestRef,
        expected_head_sha: &str,
        expected_target: &PullRequestComment,
        body: &str,
    ) -> anyhow::Result<PullRequestComment> {
        let pull_request: PullRequestResponse = self
            .get_json(&format!(
                "{API_ROOT}/repos/{}/{}/pulls/{}",
                reference.owner, reference.repository, reference.number
            ))
            .await
            .context("could not revalidate the GitHub pull request before publishing")?;
        ensure!(
            pull_request.number == reference.number,
            "GitHub returned a different pull request during publication preflight"
        );
        ensure!(
            pull_request.state == "open",
            "the pull request is no longer open; refresh it before publishing"
        );
        ensure!(
            safe_web_url(&pull_request.html_url),
            "GitHub returned an invalid pull request URL during publication preflight"
        );
        ensure!(
            pull_request.head.sha == expected_head_sha,
            "the pull request head changed; refresh it and review the reply again"
        );
        let comments_path = format!("{API_ROOT}{}", reference.api_path("/comments"));
        let remote_comments: Vec<PullRequestReviewComment> = self
            .get_all_pages(&comments_path)
            .await
            .context("could not revalidate the selected GitHub review comment")?;
        let comments = parse_pull_request_comments(remote_comments)?;
        let target = comments
            .iter()
            .find(|comment| comment.id == expected_target.id)
            .context("the selected review comment no longer exists; refresh the pull request")?;
        ensure!(
            target == expected_target,
            "the selected review comment changed; refresh it before publishing"
        );
        ensure!(
            !comments.iter().any(|comment| {
                comment.in_reply_to_id == Some(expected_target.id) && comment.body == body
            }),
            "an identical reply already exists in this thread; refresh the pull request before publishing"
        );
        Ok(target.clone())
    }

    async fn send_thread_reply(
        &self,
        reference: &PullRequestRef,
        target: &PullRequestComment,
        body: &str,
    ) -> anyhow::Result<PullRequestComment> {
        let request_body = serde_json::to_string(&CreateThreadReplyRequest {
            body,
            in_reply_to: target.id,
        })
        .context("could not encode the GitHub reply request")?;
        let url = format!(
            "{API_ROOT}/repos/{}/{}/pulls/{}/comments",
            reference.owner, reference.repository, reference.number
        );
        let request = Request::builder()
            .method(Method::POST)
            .uri(&url)
            .follow_redirects(RedirectPolicy::NoFollow)
            .header("Accept", "application/vnd.github+json")
            .header("Authorization", format!("Bearer {}", self.access_token))
            .header("Content-Type", "application/json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .header("User-Agent", "Cururu-Desktop")
            .body(AsyncBody::from(request_body))
            .context("could not build the GitHub thread-reply request")?;
        let response = self
            .http
            .send(request)
            .await
            .context("GitHub did not confirm whether the reply was published")?;
        let (parts, mut response_body) = response.into_parts();
        ensure!(
            parts.status == StatusCode::CREATED,
            "GitHub API returned HTTP {} while publishing the reply",
            parts.status
        );
        let mut response_bytes = Vec::new();
        while let Some(frame) = response_body.frame().await {
            let frame = frame.context("could not read GitHub's reply response")?;
            let Ok(data) = frame.into_data() else {
                continue;
            };
            ensure!(
                response_bytes.len().saturating_add(data.len()) <= MAX_MUTATION_RESPONSE_BYTES,
                "GitHub reply response exceeded the size limit"
            );
            response_bytes.extend_from_slice(&data);
        }
        let response_comment: PullRequestReviewComment = serde_json::from_slice(&response_bytes)
            .context("GitHub did not return a valid created-reply record")?;
        let mut comments = parse_pull_request_comments(vec![response_comment])?;
        let comment = comments
            .pop()
            .context("GitHub did not return the created reply")?;
        ensure!(
            comment.in_reply_to_id == Some(target.id)
                && comment.path == target.path
                && comment.body == body,
            "GitHub returned a reply that did not match the confirmed target and body"
        );
        Ok(comment)
    }

    async fn get_all_pages<T>(&self, first_url: &str) -> anyhow::Result<Vec<T>>
    where
        T: for<'de> Deserialize<'de>,
    {
        let mut next_url = Some(first_url.to_string());
        let mut collection_bytes = 0_usize;
        let mut values = Vec::new();
        let mut page_count = 0_usize;
        while let Some(url) = next_url {
            page_count += 1;
            ensure!(
                page_count <= MAX_COLLECTION_PAGES,
                "GitHub pull request collection exceeds the page limit"
            );
            let expected_path = Url::parse(&url)
                .context("invalid GitHub collection URL")?
                .path()
                .to_string();
            let (body, next, body_size) = self.get_page(&url).await?;
            if let Some(next) = next.as_deref() {
                let next_path = Url::parse(next)
                    .context("invalid GitHub next-page URL")?
                    .path()
                    .to_string();
                ensure!(
                    next_path == expected_path,
                    "GitHub pagination link changed the collection endpoint"
                );
            }
            collection_bytes = collection_bytes.saturating_add(body_size);
            ensure!(
                collection_bytes <= MAX_COLLECTION_BYTES,
                "GitHub pull request collection exceeds the response limit"
            );
            let mut page: Vec<T> = serde_json::from_slice(&body)
                .context("GitHub returned an invalid pull request collection")?;
            values.append(&mut page);
            next_url = next;
        }
        Ok(values)
    }

    async fn get_json<T>(&self, url: &str) -> anyhow::Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        let (body, _, _) = self.get_page(url).await?;
        serde_json::from_slice(&body).context("GitHub returned an invalid pull request response")
    }

    async fn send_mutation_json<T>(
        &self,
        method: Method,
        url: &str,
        body: Vec<u8>,
        expected_status: StatusCode,
        uncertain_context: &'static str,
    ) -> anyhow::Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        let parsed = Url::parse(url).context("invalid GitHub mutation URL")?;
        ensure!(
            parsed.scheme() == "https"
                && parsed.host_str() == Some("api.github.com")
                && parsed.username().is_empty()
                && parsed.password().is_none()
                && parsed.port().is_none(),
            "GitHub mutation URL is outside the allowed API host"
        );
        let request = Request::builder()
            .method(method)
            .uri(url)
            .follow_redirects(RedirectPolicy::NoFollow)
            .header("Accept", "application/vnd.github+json")
            .header("Authorization", format!("Bearer {}", self.access_token))
            .header("Content-Type", "application/json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .header("User-Agent", "Cururu-Desktop")
            .body(AsyncBody::from(body))
            .context("could not build GitHub mutation request")?;
        let response = self.http.send(request).await.context(uncertain_context)?;
        let (parts, mut response_body) = response.into_parts();
        ensure!(
            parts.status == expected_status,
            "GitHub returned HTTP {} for a mutation; refresh before deciding whether to retry",
            parts.status
        );
        let mut response_bytes = Vec::new();
        while let Some(frame) = response_body.frame().await {
            let frame = frame.context("could not read GitHub mutation response")?;
            let Ok(data) = frame.into_data() else {
                continue;
            };
            ensure!(
                response_bytes.len().saturating_add(data.len()) <= MAX_MUTATION_RESPONSE_BYTES,
                "GitHub mutation response exceeded the size limit; refresh before retrying"
            );
            response_bytes.extend_from_slice(&data);
        }
        serde_json::from_slice(&response_bytes)
            .context("GitHub returned an invalid mutation response; refresh before retrying")
    }

    async fn get_page(&self, url: &str) -> anyhow::Result<(Vec<u8>, Option<String>, usize)> {
        let parsed = Url::parse(url).context("invalid GitHub API URL")?;
        ensure!(
            parsed.scheme() == "https"
                && parsed.host_str() == Some("api.github.com")
                && parsed.username().is_empty()
                && parsed.password().is_none()
                && parsed.port().is_none(),
            "GitHub API request URL is outside the allowed host"
        );
        let request = Request::builder()
            .method(Method::GET)
            .uri(url)
            .follow_redirects(RedirectPolicy::NoFollow)
            .header("Accept", "application/vnd.github+json")
            .header("Authorization", format!("Bearer {}", self.access_token))
            .header("X-GitHub-Api-Version", API_VERSION)
            .header("User-Agent", "Cururu-Desktop")
            .body(AsyncBody::empty())
            .context("could not build the GitHub API request")?;
        let response = self
            .http
            .send(request)
            .await
            .context("could not reach the GitHub API")?;
        let (parts, mut body) = response.into_parts();
        ensure!(
            parts.status == StatusCode::OK,
            "GitHub API returned HTTP {}",
            parts.status
        );
        let mut bytes = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.context("could not read the GitHub API response")?;
            let Ok(data) = frame.into_data() else {
                continue;
            };
            ensure!(
                bytes.len().saturating_add(data.len()) <= MAX_PAGE_BYTES,
                "GitHub API response exceeded the per-page limit"
            );
            bytes.extend_from_slice(&data);
        }
        let size = bytes.len();
        let next = next_page_url(&parts.headers)?;
        Ok((bytes, next, size))
    }
}

fn valid_commit_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn next_page_url(headers: &gpui::http_client::http::HeaderMap) -> anyhow::Result<Option<String>> {
    let Some(link_header) = headers.get(gpui::http_client::http::header::LINK) else {
        return Ok(None);
    };
    let link_header = link_header
        .to_str()
        .context("GitHub returned an invalid pagination header")?;
    for link in link_header.split(',') {
        let mut parts = link.split(';');
        let target = parts.next().unwrap_or_default().trim();
        let has_next_relation = parts.any(|part| {
            part.trim()
                .strip_prefix("rel=")
                .is_some_and(|relation| relation.trim_matches('"') == "next")
        });
        if has_next_relation {
            let target = target
                .strip_prefix('<')
                .and_then(|target| target.strip_suffix('>'))
                .context("GitHub returned an invalid next-page link")?;
            let url = Url::parse(target).context("GitHub returned an invalid next-page URL")?;
            ensure!(
                url.scheme() == "https"
                    && url.host_str() == Some("api.github.com")
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.port().is_none()
                    && url.fragment().is_none()
                    && url.path().starts_with("/repos/"),
                "GitHub returned a pagination link outside the allowed API"
            );
            return Ok(Some(url.to_string()));
        }
    }
    Ok(None)
}

fn safe_web_url(value: &str) -> bool {
    Url::parse(value).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str() == Some("github.com")
            && url.username().is_empty()
            && url.password().is_none()
    })
}

fn parse_pull_request_files(
    remote_files: Vec<PullRequestFile>,
) -> anyhow::Result<Vec<cururu_core::ChangedFile>> {
    let mut files = Vec::with_capacity(remote_files.len());
    let mut total_patch_bytes = 0_usize;
    for remote_file in remote_files {
        let path = remote_file.filename;
        validate_relative_path(&path)?;
        let old_path = remote_file.previous_filename.as_deref().unwrap_or(&path);
        validate_relative_path(old_path)?;
        let patch_text = remote_file
            .patch
            .context("GitHub omitted a file patch; refusing an incomplete review diff")?;
        ensure!(
            patch_text.len() <= MAX_REMOTE_DIFF_FILE_BYTES,
            "GitHub pull request file patch exceeds the desktop diff limit"
        );
        total_patch_bytes = total_patch_bytes.saturating_add(patch_text.len());
        ensure!(
            total_patch_bytes <= MAX_REMOTE_DIFF_BYTES,
            "GitHub pull request patches exceed the desktop diff limit"
        );
        let added_lines = patch_text
            .lines()
            .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
            .count();
        let removed_lines = patch_text
            .lines()
            .filter(|line| line.starts_with('-') && !line.starts_with("---"))
            .count();
        let expected_additions = usize::try_from(remote_file.additions)
            .context("GitHub returned an invalid additions count")?;
        let expected_deletions = usize::try_from(remote_file.deletions)
            .context("GitHub returned an invalid deletions count")?;
        ensure!(
            added_lines == expected_additions && removed_lines == expected_deletions,
            "GitHub returned an incomplete patch for a changed file"
        );
        let unified_diff = format!(
            "diff --git a/{old_path} b/{path}\n--- a/{old_path}\n+++ b/{path}\n{patch_text}\n"
        );
        let parsed = cururu_core::parse_unified_diff(&unified_diff);
        ensure!(
            parsed.len() == 1,
            "GitHub returned an invalid patch for a changed file"
        );
        files.extend(parsed);
    }
    cururu_core::ensure_review_diff_limits(
        &files,
        MAX_REMOTE_DIFF_FILE_BYTES,
        MAX_REMOTE_DIFF_BYTES,
    )
    .map_err(|error| anyhow::anyhow!(error).context("pull request diff exceeds desktop limits"))?;
    Ok(files)
}

fn validate_relative_path(value: &str) -> anyhow::Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 4_096
            && !value.chars().any(char::is_control)
            && !value.contains('\\')
            && value
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != "..")
            && Path::new(value)
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_))),
        "GitHub returned an unsafe changed-file path"
    );
    Ok(())
}

fn validate_branch_name(value: &str) -> anyhow::Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 255
            && !value.starts_with('/')
            && !value.ends_with('/')
            && !value.ends_with('.')
            && !value.contains("..")
            && !value.contains("@{")
            && value.split('/').all(|part| {
                !part.is_empty()
                    && part != "."
                    && !part.starts_with('.')
                    && !Path::new(part)
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("lock"))
                    && part
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
            }),
        "GitHub branch name is invalid or unsupported"
    );
    Ok(())
}

pub(crate) fn validate_remote_commit_target(
    reference: &PullRequestRef,
    repository: &GitHubRepositoryRef,
    branch: &str,
    base_sha: &str,
    head_sha: &str,
) -> anyhow::Result<()> {
    validate_pull_request_reference(reference)?;
    repository.validate()?;
    validate_branch_name(branch)?;
    ensure!(
        valid_commit_sha(base_sha) && valid_commit_sha(head_sha),
        "remote commit target has invalid base/head identifiers"
    );
    Ok(())
}

fn validate_commit_message(value: &str) -> anyhow::Result<()> {
    ensure!(
        !value.trim().is_empty() && value.len() <= 1_000,
        "remote commit message must contain 1 to 1,000 bytes"
    );
    ensure!(
        !value.chars().any(|character| {
            character == '\0'
                || (character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
        }),
        "remote commit message contains unsupported control characters"
    );
    Ok(())
}

fn validate_commit_files(files: &[GitHubCommitFile]) -> anyhow::Result<()> {
    ensure!(
        !files.is_empty() && files.len() <= MAX_REMOTE_COMMIT_FILES,
        "remote commit must contain 1 to 100 selected files"
    );
    let mut total_bytes = 0_usize;
    for (index, file) in files.iter().enumerate() {
        validate_relative_path(&file.path)?;
        ensure!(
            matches!(file.mode.as_str(), "100644" | "100755"),
            "remote commits support only regular, non-symlink files"
        );
        if let Some(content) = &file.content {
            ensure!(
                !content.contains('\0'),
                "remote commits do not support binary file content"
            );
            ensure!(
                content.len() <= MAX_REMOTE_COMMIT_FILE_BYTES,
                "a selected file exceeds the 2 MiB remote commit limit"
            );
            total_bytes = total_bytes.saturating_add(content.len());
            ensure!(
                total_bytes <= MAX_REMOTE_COMMIT_BYTES,
                "selected files exceed the 4 MiB remote commit limit"
            );
        }
        ensure!(
            !files[..index]
                .iter()
                .any(|previous| previous.path == file.path),
            "remote commit contains a duplicate file path"
        );
        ensure!(
            !files[..index].iter().any(|previous| {
                previous.path.starts_with(&format!("{}/", file.path))
                    || file.path.starts_with(&format!("{}/", previous.path))
            }),
            "remote commit contains conflicting file and directory paths"
        );
    }
    Ok(())
}

pub(crate) fn validate_remote_commit_preview(
    message: &str,
    files: &[GitHubCommitFile],
) -> anyhow::Result<()> {
    validate_commit_message(message)?;
    validate_commit_files(files)
}

fn validate_commit_files_against_tree(
    files: &[GitHubCommitFile],
    tree: &[GitHubTreeEntryResponse],
) -> anyhow::Result<()> {
    for file in files {
        for entry in tree {
            let entry_prefix = format!("{}/", entry.path);
            if file.path.starts_with(&entry_prefix) {
                ensure!(
                    entry.kind == "tree" && entry.mode == "040000",
                    "selected path is nested under a non-directory at the reviewed head"
                );
            }
        }
        let existing = tree.iter().find(|entry| entry.path == file.path);
        match (file.content.as_ref(), existing) {
            (Some(_), Some(entry)) => {
                ensure!(
                    entry.kind == "blob"
                        && matches!(entry.mode.as_str(), "100644" | "100755")
                        && entry.mode == file.mode
                        && valid_commit_sha(&entry.sha),
                    "selected file type or mode changed; refresh before committing"
                );
            }
            (Some(_), None) => {}
            (None, Some(entry)) => {
                ensure!(
                    entry.kind == "blob"
                        && matches!(entry.mode.as_str(), "100644" | "100755")
                        && entry.mode == file.mode
                        && valid_commit_sha(&entry.sha),
                    "selected deletion no longer matches a regular file at the reviewed head"
                );
            }
            (None, None) => {
                anyhow::bail!(
                    "selected deletion no longer exists at the reviewed head; refresh before committing"
                );
            }
        }
    }
    Ok(())
}

fn parse_pull_request_comments(
    remote_comments: Vec<PullRequestReviewComment>,
) -> anyhow::Result<Vec<PullRequestComment>> {
    let mut comments = Vec::with_capacity(remote_comments.len());
    for comment in remote_comments {
        validate_relative_path(&comment.path)?;
        ensure!(
            comment.body.len() <= 64_000,
            "GitHub review comment exceeds the size limit"
        );
        ensure!(
            safe_web_url(&comment.html_url),
            "GitHub returned an invalid review comment URL"
        );
        let author = comment
            .user
            .map_or_else(|| "deleted user".to_string(), |user| user.login);
        comments.push(PullRequestComment {
            id: comment.id,
            path: comment.path,
            line: comment.line.and_then(|line| u32::try_from(line).ok()),
            original_line: comment
                .original_line
                .and_then(|line| u32::try_from(line).ok()),
            body: comment.body,
            author,
            html_url: comment.html_url,
            commit_id: comment.commit_id,
            original_commit_id: comment.original_commit_id,
            in_reply_to_id: comment.in_reply_to_id,
            is_outdated: comment.line.is_none() && comment.original_line.is_some(),
            subject_type: comment.subject_type,
            side: comment.side,
        });
    }
    Ok(comments)
}

fn group_review_threads(comments: Vec<PullRequestComment>) -> Vec<PullRequestReviewThread> {
    let mut threads: Vec<PullRequestReviewThread> = Vec::new();
    let mut replies = Vec::new();
    for comment in comments {
        if comment.in_reply_to_id.is_some() {
            replies.push(comment);
        } else {
            threads.push(PullRequestReviewThread {
                root: comment,
                replies: Vec::new(),
            });
        }
    }
    for reply in replies {
        if let Some(thread) = threads
            .iter_mut()
            .find(|thread| Some(thread.root.id) == reply.in_reply_to_id)
        {
            thread.replies.push(reply);
        } else {
            // Preserve an orphaned reply rather than silently dropping remote discussion.
            threads.push(PullRequestReviewThread {
                root: reply,
                replies: Vec::new(),
            });
        }
    }
    threads
}

fn parse_pull_request_reviews(
    remote_reviews: Vec<PullRequestReviewResponse>,
) -> anyhow::Result<Vec<PullRequestReview>> {
    let mut reviews = Vec::with_capacity(remote_reviews.len());
    for review in remote_reviews {
        ensure!(
            review.body.len() <= 64_000,
            "GitHub review summary exceeds the size limit"
        );
        ensure!(
            safe_web_url(&review.html_url),
            "GitHub returned an invalid review URL"
        );
        let author = review
            .user
            .map_or_else(|| "deleted user".to_string(), |user| user.login);
        reviews.push(PullRequestReview {
            id: review.id,
            state: review.state,
            body: review.body,
            author,
            submitted_at: review.submitted_at,
            commit_id: review.commit_id,
            html_url: review.html_url,
        });
    }
    Ok(reviews)
}

fn parse_pull_request_conversation(
    remote_comments: Vec<PullRequestConversationResponse>,
) -> anyhow::Result<Vec<PullRequestConversationComment>> {
    let mut comments = Vec::with_capacity(remote_comments.len());
    for comment in remote_comments {
        ensure!(
            comment.body.len() <= 64_000,
            "GitHub conversation comment exceeds the size limit"
        );
        ensure!(
            safe_web_url(&comment.html_url),
            "GitHub returned an invalid conversation comment URL"
        );
        let author = comment
            .user
            .map_or_else(|| "deleted user".to_string(), |user| user.login);
        comments.push(PullRequestConversationComment {
            id: comment.id,
            body: comment.body,
            author,
            created_at: comment.created_at,
            html_url: comment.html_url,
        });
    }
    Ok(comments)
}

#[derive(Deserialize)]
struct PullRequestResponse {
    number: u64,
    title: String,
    state: String,
    html_url: String,
    user: Option<GitHubUser>,
    base: PullRequestCommit,
    head: PullRequestCommit,
}

#[derive(Deserialize)]
struct PullRequestCommit {
    sha: String,
    #[serde(rename = "ref", default)]
    reference: Option<String>,
    #[serde(rename = "repo", default)]
    repository: Option<PullRequestRepository>,
}

#[derive(Deserialize)]
struct PullRequestRepository {
    full_name: String,
}

#[derive(Deserialize)]
struct GitHubCommitDetailsResponse {
    sha: String,
    tree: GitHubTreeObjectResponse,
    #[serde(default)]
    parents: Vec<GitHubCommitParentResponse>,
}

#[derive(Deserialize)]
struct GitHubTreeObjectResponse {
    sha: String,
}

#[derive(Deserialize)]
struct GitHubTreeDetailsResponse {
    sha: String,
    truncated: bool,
    tree: Vec<GitHubTreeEntryResponse>,
}

#[derive(Deserialize)]
struct GitHubTreeEntryResponse {
    path: String,
    mode: String,
    #[serde(rename = "type")]
    kind: String,
    sha: String,
}

#[derive(Deserialize)]
struct GitHubTreeResponse {
    sha: String,
}

#[derive(Deserialize)]
struct GitHubCommitResponse {
    sha: String,
    html_url: String,
    parents: Vec<GitHubCommitParentResponse>,
}

#[derive(Deserialize)]
struct GitHubCommitParentResponse {
    sha: String,
}

#[derive(Deserialize)]
struct GitHubReferenceResponse {
    #[serde(rename = "ref")]
    reference: String,
    object: GitHubReferenceObjectResponse,
}

#[derive(Deserialize)]
struct GitHubReferenceObjectResponse {
    sha: String,
    #[serde(rename = "type")]
    object_type: String,
}

#[derive(Deserialize)]
struct GitHubUser {
    login: String,
}

#[derive(Deserialize)]
struct PullRequestFile {
    filename: String,
    previous_filename: Option<String>,
    additions: u32,
    deletions: u32,
    patch: Option<String>,
}

#[derive(Deserialize)]
struct PullRequestReviewComment {
    id: u64,
    path: String,
    line: Option<usize>,
    original_line: Option<usize>,
    body: String,
    html_url: String,
    commit_id: String,
    original_commit_id: String,
    in_reply_to_id: Option<u64>,
    subject_type: Option<String>,
    side: Option<String>,
    user: Option<GitHubUser>,
}

#[derive(Deserialize)]
struct PullRequestReviewResponse {
    id: u64,
    state: String,
    body: String,
    html_url: String,
    submitted_at: Option<String>,
    commit_id: Option<String>,
    user: Option<GitHubUser>,
}

#[derive(Deserialize)]
struct PullRequestConversationResponse {
    id: u64,
    body: String,
    html_url: String,
    created_at: String,
    user: Option<GitHubUser>,
}

#[derive(Deserialize)]
struct PullRequestSourceFileResponse {
    #[serde(rename = "type")]
    kind: String,
    encoding: String,
    size: usize,
    path: String,
    content: String,
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use gpui::http_client::{AsyncBody, HttpClient, Method, Request, Response, StatusCode, Url};
    use http_body_util::BodyExt as _;

    use super::{
        GitHubPullRequestClient, PullRequestRef, ReviewEvent, group_review_threads, next_page_url,
        parse_pull_request_comments, parse_pull_request_files, validate_thread_reply_body,
    };

    struct FakeHttpClient {
        responses: Mutex<VecDeque<Response<AsyncBody>>>,
        requests: Arc<Mutex<Vec<RequestInfo>>>,
    }

    struct RequestInfo {
        uri: String,
        method: String,
        authorization: Option<String>,
        api_version: Option<String>,
        content_type: Option<String>,
        body: Vec<u8>,
    }

    impl HttpClient for FakeHttpClient {
        fn type_name(&self) -> &'static str {
            "fake-github-api-client"
        }

        fn user_agent(&self) -> Option<&gpui::http_client::http::HeaderValue> {
            None
        }

        fn send(
            &self,
            request: Request<AsyncBody>,
        ) -> futures::future::BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
            let (parts, body) = request.into_parts();
            let mut request_info = RequestInfo {
                uri: parts.uri.to_string(),
                method: parts.method.to_string(),
                authorization: parts
                    .headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string),
                api_version: parts
                    .headers
                    .get("x-github-api-version")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string),
                content_type: parts
                    .headers
                    .get("content-type")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string),
                body: Vec::new(),
            };
            let response = self.responses.lock().unwrap().pop_front();
            let requests = Arc::clone(&self.requests);
            Box::pin(async move {
                request_info.body = body.collect().await?.to_bytes().to_vec();
                requests.lock().unwrap().push(request_info);
                response.ok_or_else(|| anyhow::anyhow!("no fake response"))
            })
        }

        fn proxy(&self) -> Option<&Url> {
            None
        }
    }

    fn response(body: &'static str) -> Response<AsyncBody> {
        response_with_status(StatusCode::OK, body)
    }

    fn response_with_status(status: StatusCode, body: &'static str) -> Response<AsyncBody> {
        Response::builder()
            .status(status)
            .body(AsyncBody::from(body))
            .unwrap()
    }

    fn response_with_link(body: &'static str, link: &'static str) -> Response<AsyncBody> {
        Response::builder()
            .status(200)
            .header("Link", link)
            .body(AsyncBody::from(body))
            .unwrap()
    }

    fn api_client(responses: &[&'static str]) -> (GitHubPullRequestClient, Arc<FakeHttpClient>) {
        let http = Arc::new(FakeHttpClient {
            responses: Mutex::new(responses.iter().map(|body| response(body)).collect()),
            requests: Arc::new(Mutex::new(Vec::new())),
        });
        let client =
            GitHubPullRequestClient::new(http.clone(), "test-token-not-real".to_string()).unwrap();
        (client, http)
    }

    fn expected_thread_comment() -> super::PullRequestComment {
        super::PullRequestComment {
            id: 7,
            path: "src/lib.rs".to_string(),
            line: Some(8),
            original_line: Some(8),
            body: "Please keep this check.".to_string(),
            author: "reviewer".to_string(),
            html_url: "https://github.com/octo-org/review_repo/pull/42#discussion_r7".to_string(),
            commit_id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            original_commit_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
            in_reply_to_id: None,
            is_outdated: false,
            subject_type: None,
            side: None,
        }
    }

    const REVIEW_PR: &str = r#"{"number":42,"title":"Parser review","state":"open","html_url":"https://github.com/octo-org/review_repo/pull/42","user":{"login":"author"},"base":{"sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"},"head":{"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}"#;
    const REMOTE_COMMIT_PR: &str = r#"{"number":42,"title":"Parser review","state":"open","html_url":"https://github.com/octo-org/review_repo/pull/42","user":{"login":"author"},"base":{"sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"},"head":{"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","ref":"feature/topic","repo":{"full_name":"octo-org/review_repo"}}}"#;
    const REVIEW_FILES: &str = r#"[{"filename":"src/lib.rs","status":"modified","additions":1,"deletions":1,"patch":"@@ -1 +1 @@\n-before\n+after"}]"#;
    const CREATED_REVIEW: &str = r#"{"id":91,"state":"COMMENTED","body":"Review notes","commit_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","html_url":"https://github.com/octo-org/review_repo/pull/42#pullrequestreview-91","user":{"login":"reviewer"}}"#;
    const CREATED_REQUEST_CHANGES_REVIEW: &str = r#"{"id":92,"state":"CHANGES_REQUESTED","body":"Please fix this before merge.","commit_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","html_url":"https://github.com/octo-org/review_repo/pull/42#pullrequestreview-92","user":{"login":"reviewer"}}"#;
    const REVIEWED_PARENT_COMMIT: &str = r#"{"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","tree":{"sha":"cccccccccccccccccccccccccccccccccccccccc"}}"#;
    const REVIEWED_PARENT_TREE: &str = r#"{"sha":"cccccccccccccccccccccccccccccccccccccccc","truncated":false,"tree":[{"path":"src/lib.rs","mode":"100644","type":"blob","sha":"dddddddddddddddddddddddddddddddddddddddd"}]}"#;
    const CREATED_REMOTE_TREE: &str = r#"{"sha":"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"}"#;
    const CREATED_REMOTE_COMMIT: &str = r#"{"sha":"ffffffffffffffffffffffffffffffffffffffff","html_url":"https://github.com/octo-org/review_repo/commit/ffffffffffffffffffffffffffffffffffffffff","parents":[{"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}]}"#;
    const CREATED_REMOTE_COMMIT_DETAILS: &str = r#"{"sha":"ffffffffffffffffffffffffffffffffffffffff","tree":{"sha":"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"},"parents":[{"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}]}"#;
    const REVIEWED_BRANCH_REF: &str = r#"{"ref":"refs/heads/feature/topic","object":{"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","type":"commit"}}"#;
    const UPDATED_BRANCH_REF: &str = r#"{"ref":"refs/heads/feature/topic","object":{"sha":"ffffffffffffffffffffffffffffffffffffffff","type":"commit"}}"#;
    const REMOTE_SUGGESTION_COMMENT: &str = r#"[{"id":7,"path":"src/lib.rs","line":1,"original_line":1,"body":"Please use the checked call.\n```suggestion\nchecked_call()\n```","html_url":"https://github.com/octo-org/review_repo/pull/42#discussion_r7","commit_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","original_commit_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","in_reply_to_id":null,"subject_type":"line","side":"RIGHT","user":{"login":"reviewer"}}]"#;
    const REMOTE_SUGGESTION_SOURCE: &str = r#"{"type":"file","encoding":"base64","size":14,"path":"src/lib.rs","content":"dW5zYWZlX2NhbGwoKQo="}"#;

    fn review_comment() -> super::NewReviewComment {
        super::NewReviewComment {
            path: "src/lib.rs".to_string(),
            line: 1,
            body: "Please preserve this check.".to_string(),
        }
    }

    fn review_client(
        comments: &'static str,
        post_status: StatusCode,
    ) -> (GitHubPullRequestClient, Arc<FakeHttpClient>) {
        review_client_with_response(comments, post_status, CREATED_REVIEW)
    }

    fn review_client_with_response(
        comments: &'static str,
        post_status: StatusCode,
        created_review: &'static str,
    ) -> (GitHubPullRequestClient, Arc<FakeHttpClient>) {
        let http = Arc::new(FakeHttpClient {
            responses: Mutex::new(VecDeque::from([
                response(REVIEW_PR),
                response(REVIEW_FILES),
                response(comments),
                response("[]"),
                response("[]"),
                response_with_status(post_status, created_review),
            ])),
            requests: Arc::new(Mutex::new(Vec::new())),
        });
        (
            GitHubPullRequestClient::new(http.clone(), "test-token-not-real".to_string()).unwrap(),
            http,
        )
    }

    fn remote_commit_create_client() -> (GitHubPullRequestClient, Arc<FakeHttpClient>) {
        let http = Arc::new(FakeHttpClient {
            responses: Mutex::new(VecDeque::from([
                response(REMOTE_COMMIT_PR),
                response(REVIEW_FILES),
                response("[]"),
                response("[]"),
                response("[]"),
                response(REVIEWED_PARENT_COMMIT),
                response(REVIEWED_PARENT_TREE),
                response_with_status(StatusCode::CREATED, CREATED_REMOTE_TREE),
                response_with_status(StatusCode::CREATED, CREATED_REMOTE_COMMIT),
            ])),
            requests: Arc::new(Mutex::new(Vec::new())),
        });
        (
            GitHubPullRequestClient::new(http.clone(), "test-token-not-real".to_string()).unwrap(),
            http,
        )
    }

    fn remote_commit_suggestion_client() -> (GitHubPullRequestClient, Arc<FakeHttpClient>) {
        let http = Arc::new(FakeHttpClient {
            responses: Mutex::new(VecDeque::from([
                response(REMOTE_COMMIT_PR),
                response(REVIEW_FILES),
                response(REMOTE_SUGGESTION_COMMENT),
                response("[]"),
                response("[]"),
                response(REMOTE_SUGGESTION_SOURCE),
                response(REVIEWED_PARENT_COMMIT),
                response(REVIEWED_PARENT_TREE),
                response_with_status(StatusCode::CREATED, CREATED_REMOTE_TREE),
                response_with_status(StatusCode::CREATED, CREATED_REMOTE_COMMIT),
            ])),
            requests: Arc::new(Mutex::new(Vec::new())),
        });
        (
            GitHubPullRequestClient::new(http.clone(), "test-token-not-real".to_string()).unwrap(),
            http,
        )
    }

    fn expected_suggestion() -> super::PullRequestComment {
        super::PullRequestComment {
            id: 7,
            path: "src/lib.rs".to_string(),
            line: Some(1),
            original_line: Some(1),
            body: "Please use the checked call.\n```suggestion\nchecked_call()\n```".to_string(),
            author: "reviewer".to_string(),
            html_url: "https://github.com/octo-org/review_repo/pull/42#discussion_r7".to_string(),
            commit_id: "a".repeat(40),
            original_commit_id: "a".repeat(40),
            in_reply_to_id: None,
            is_outdated: false,
            subject_type: Some("line".to_string()),
            side: Some("RIGHT".to_string()),
        }
    }

    fn remote_commit_request(
        base_sha: String,
        parent_sha: String,
        files: Vec<super::GitHubCommitFile>,
        expected_suggestions: Vec<super::PullRequestComment>,
    ) -> super::RemoteCommitRequest {
        super::RemoteCommitRequest {
            reference: PullRequestRef::parse("octo-org/review_repo#42").unwrap(),
            repository: super::GitHubRepositoryRef::parse_full_name("octo-org/review_repo")
                .unwrap(),
            branch: "feature/topic".to_string(),
            base_sha,
            parent_sha,
            message: "Apply reviewed changes".to_string(),
            files,
            expected_suggestions,
        }
    }

    fn remote_commit_object() -> super::GitHubCommitObject {
        super::GitHubCommitObject {
            reference: PullRequestRef::parse("octo-org/review_repo#42").unwrap(),
            repository: super::GitHubRepositoryRef::parse_full_name("octo-org/review_repo")
                .unwrap(),
            branch: "feature/topic".to_string(),
            base_sha: "b".repeat(40),
            parent_sha: "a".repeat(40),
            commit_sha: "f".repeat(40),
            message: "Apply reviewed changes".to_string(),
            html_url: "https://github.com/octo-org/review_repo/commit/ffffffffffffffffffffffffffffffffffffffff".to_string(),
            files: vec![super::GitHubCommitFile {
                path: "src/lib.rs".to_string(),
                mode: "100644".to_string(),
                content: Some("after\n".to_string()),
            }],
        }
    }

    fn remote_branch_update_client(
        branch_ref: &'static str,
        update_ref: Option<&'static str>,
    ) -> (GitHubPullRequestClient, Arc<FakeHttpClient>) {
        let mut responses = VecDeque::from([
            response(REMOTE_COMMIT_PR),
            response(REVIEW_FILES),
            response("[]"),
            response("[]"),
            response("[]"),
            response(CREATED_REMOTE_COMMIT_DETAILS),
            response(branch_ref),
        ]);
        if let Some(updated) = update_ref {
            responses.push_back(response(updated));
        }
        let http = Arc::new(FakeHttpClient {
            responses: Mutex::new(responses),
            requests: Arc::new(Mutex::new(Vec::new())),
        });
        (
            GitHubPullRequestClient::new(http.clone(), "test-token-not-real".to_string()).unwrap(),
            http,
        )
    }

    #[test]
    fn comment_review_sends_one_head_bound_request_after_full_revalidation() {
        futures::executor::block_on(async {
            let (client, http) = review_client("[]", StatusCode::OK);
            let reference = PullRequestRef::parse("octo-org/review_repo#42").unwrap();
            let review = client
                .publish_comment_review(
                    &reference,
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "Review notes",
                    &[review_comment()],
                )
                .await
                .unwrap();
            assert_eq!(review.id, 91);
            let requests = http.requests.lock().unwrap();
            assert_eq!(requests.len(), 6);
            let post = &requests[5];
            assert_eq!(post.method, "POST");
            assert!(post.uri.ends_with("/pulls/42/reviews"));
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&post.body).unwrap(),
                serde_json::json!({
                    "commit_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "event": "COMMENT", "body": "Review notes",
                    "comments": [{"path": "src/lib.rs", "line": 1, "side": "RIGHT", "body": "Please preserve this check."}]
                })
            );
            drop(requests);
        });
    }

    #[test]
    fn request_changes_can_be_standalone_with_a_required_summary() {
        futures::executor::block_on(async {
            let (client, http) =
                review_client_with_response("[]", StatusCode::OK, CREATED_REQUEST_CHANGES_REVIEW);
            let reference = PullRequestRef::parse("octo-org/review_repo#42").unwrap();
            assert!(
                client
                    .publish_review(
                        &reference,
                        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                        ReviewEvent::RequestChanges,
                        "  \n",
                        &[],
                    )
                    .await
                    .is_err()
            );
            assert!(http.requests.lock().unwrap().is_empty());

            let review = client
                .publish_review(
                    &reference,
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    ReviewEvent::RequestChanges,
                    "Please fix this before merge.",
                    &[],
                )
                .await
                .unwrap();

            assert_eq!(review.state, "CHANGES_REQUESTED");
            let requests = http.requests.lock().unwrap();
            assert_eq!(requests.len(), 6);
            let post = &requests[5];
            assert_eq!(post.method, "POST");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&post.body).unwrap(),
                serde_json::json!({
                    "commit_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "event": "REQUEST_CHANGES",
                    "body": "Please fix this before merge.",
                    "comments": []
                })
            );
            drop(requests);
        });
    }

    #[test]
    fn request_changes_can_include_a_revalidated_inline_comment() {
        futures::executor::block_on(async {
            let (client, http) =
                review_client_with_response("[]", StatusCode::OK, CREATED_REQUEST_CHANGES_REVIEW);
            client
                .publish_review(
                    &PullRequestRef::parse("octo-org/review_repo#42").unwrap(),
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    ReviewEvent::RequestChanges,
                    "Please fix this before merge.",
                    &[review_comment()],
                )
                .await
                .unwrap();

            let requests = http.requests.lock().unwrap();
            assert_eq!(requests.len(), 6);
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&requests[5].body).unwrap(),
                serde_json::json!({
                    "commit_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "event": "REQUEST_CHANGES",
                    "body": "Please fix this before merge.",
                    "comments": [{
                        "path": "src/lib.rs",
                        "line": 1,
                        "side": "RIGHT",
                        "body": "Please preserve this check."
                    }]
                })
            );
            drop(requests);
        });
    }

    #[test]
    fn creating_remote_commit_objects_does_not_update_the_branch_reference() {
        futures::executor::block_on(async {
            let (client, http) = remote_commit_create_client();
            let commit = client
                .create_remote_commit_object(&remote_commit_request(
                    "b".repeat(40),
                    "a".repeat(40),
                    vec![super::GitHubCommitFile {
                        path: "src/lib.rs".to_string(),
                        mode: "100644".to_string(),
                        content: Some("after\n".to_string()),
                    }],
                    Vec::new(),
                ))
                .await
                .unwrap();

            assert_eq!(commit.commit_sha, "f".repeat(40));
            let requests = http.requests.lock().unwrap();
            assert_eq!(requests.len(), 9);
            assert_eq!(requests[5].method, "GET");
            assert!(
                requests[5]
                    .uri
                    .ends_with("/git/commits/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            );
            assert_eq!(requests[6].method, "GET");
            assert!(
                requests[6]
                    .uri
                    .ends_with("/git/trees/cccccccccccccccccccccccccccccccccccccccc?recursive=1")
            );
            assert_eq!(requests[7].method, "POST");
            assert!(requests[7].uri.ends_with("/git/trees"));
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&requests[7].body).unwrap(),
                serde_json::json!({
                    "base_tree": "cccccccccccccccccccccccccccccccccccccccc",
                    "tree": [{
                        "path": "src/lib.rs",
                        "mode": "100644",
                        "type": "blob",
                        "content": "after\n"
                    }]
                })
            );
            assert_eq!(requests[8].method, "POST");
            assert!(requests[8].uri.ends_with("/git/commits"));
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&requests[8].body).unwrap(),
                serde_json::json!({
                    "message": "Apply reviewed changes",
                    "tree": "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
                    "parents": ["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]
                })
            );
            assert!(requests.iter().all(|request| request.method != "PATCH"));
            drop(requests);
        });
    }

    #[test]
    fn remote_commit_rejects_stale_parent_before_object_creation() {
        futures::executor::block_on(async {
            let (client, http) = remote_commit_create_client();
            let error = client
                .create_remote_commit_object(&remote_commit_request(
                    "b".repeat(40),
                    "c".repeat(40),
                    vec![super::GitHubCommitFile {
                        path: "src/lib.rs".to_string(),
                        mode: "100644".to_string(),
                        content: Some("after\n".to_string()),
                    }],
                    Vec::new(),
                ))
                .await
                .unwrap_err();

            assert!(error.to_string().contains("base/head changed"));
            let requests = http.requests.lock().unwrap();
            assert_eq!(requests.len(), 5);
            assert!(requests.iter().all(|request| request.method == "GET"));
            drop(requests);
        });
    }

    #[test]
    fn remote_commit_revalidates_the_exact_github_suggestion_before_creation() {
        futures::executor::block_on(async {
            let (client, http) = remote_commit_suggestion_client();
            client
                .create_remote_commit_object(&super::RemoteCommitRequest {
                    reference: PullRequestRef::parse("octo-org/review_repo#42").unwrap(),
                    repository: super::GitHubRepositoryRef::parse_full_name("octo-org/review_repo")
                        .unwrap(),
                    branch: "feature/topic".to_string(),
                    base_sha: "b".repeat(40),
                    parent_sha: "a".repeat(40),
                    message: "Apply reviewed suggestion".to_string(),
                    files: vec![super::GitHubCommitFile {
                        path: "src/lib.rs".to_string(),
                        mode: "100644".to_string(),
                        content: Some("checked_call()\n".to_string()),
                    }],
                    expected_suggestions: vec![expected_suggestion()],
                })
                .await
                .unwrap();

            let requests = http.requests.lock().unwrap();
            assert_eq!(requests.len(), 10);
            assert_eq!(requests[5].method, "GET");
            assert!(requests[5].uri.contains("/contents/src/lib.rs?ref="));
            drop(requests);
        });
    }

    #[test]
    fn remote_commit_refuses_a_suggestion_missing_during_fresh_revalidation() {
        futures::executor::block_on(async {
            let (client, http) = remote_commit_create_client();
            assert!(
                client
                    .create_remote_commit_object(&remote_commit_request(
                        "b".repeat(40),
                        "a".repeat(40),
                        vec![super::GitHubCommitFile {
                            path: "src/lib.rs".to_string(),
                            mode: "100644".to_string(),
                            content: Some("checked_call()\n".to_string()),
                        }],
                        vec![expected_suggestion()],
                    ),)
                    .await
                    .is_err()
            );
            let requests = http.requests.lock().unwrap();
            assert_eq!(requests.len(), 5);
            assert!(requests.iter().all(|request| request.method == "GET"));
            drop(requests);
        });
    }

    #[test]
    fn remote_commit_refuses_content_that_does_not_match_the_revalidated_suggestion() {
        futures::executor::block_on(async {
            let (client, http) = remote_commit_suggestion_client();
            let request = super::RemoteCommitRequest {
                reference: PullRequestRef::parse("octo-org/review_repo#42").unwrap(),
                repository: super::GitHubRepositoryRef::parse_full_name("octo-org/review_repo")
                    .unwrap(),
                branch: "feature/topic".to_string(),
                base_sha: "b".repeat(40),
                parent_sha: "a".repeat(40),
                message: "Apply reviewed suggestion".to_string(),
                files: vec![super::GitHubCommitFile {
                    path: "src/lib.rs".to_string(),
                    mode: "100644".to_string(),
                    content: Some("different_call()\n".to_string()),
                }],
                expected_suggestions: vec![expected_suggestion()],
            };

            assert!(client.create_remote_commit_object(&request).await.is_err());
            let requests = http.requests.lock().unwrap();
            assert_eq!(requests.len(), 6);
            assert!(requests.iter().all(|request| request.method == "GET"));
            drop(requests);
        });
    }

    #[test]
    fn branch_update_requires_the_exact_parent_and_uses_non_force_patch() {
        futures::executor::block_on(async {
            let (client, http) =
                remote_branch_update_client(REVIEWED_BRANCH_REF, Some(UPDATED_BRANCH_REF));
            let result = client
                .update_remote_branch(&remote_commit_object())
                .await
                .unwrap();
            assert_eq!(result.previous_sha, "a".repeat(40));
            assert_eq!(result.commit_sha, "f".repeat(40));
            let requests = http.requests.lock().unwrap();
            assert_eq!(requests.len(), 8);
            assert_eq!(requests[5].method, "GET");
            assert!(
                requests[5]
                    .uri
                    .ends_with("/git/commits/ffffffffffffffffffffffffffffffffffffffff")
            );
            assert_eq!(requests[6].method, "GET");
            assert!(requests[6].uri.ends_with("/git/ref/heads/feature/topic"));
            assert_eq!(requests[7].method, "PATCH");
            assert!(requests[7].uri.ends_with("/git/refs/heads/feature/topic"));
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&requests[7].body).unwrap(),
                serde_json::json!({
                    "sha": "ffffffffffffffffffffffffffffffffffffffff",
                    "force": false
                })
            );
            drop(requests);
        });
    }

    #[test]
    fn branch_update_rejects_stale_ref_without_patch() {
        futures::executor::block_on(async {
            let stale_ref = r#"{"ref":"refs/heads/feature/topic","object":{"sha":"9999999999999999999999999999999999999999","type":"commit"}}"#;
            let (client, http) = remote_branch_update_client(stale_ref, None);
            assert!(
                client
                    .update_remote_branch(&remote_commit_object())
                    .await
                    .is_err()
            );
            let requests = http.requests.lock().unwrap();
            assert_eq!(requests.len(), 7);
            assert!(requests.iter().all(|request| request.method == "GET"));
            drop(requests);
        });
    }

    #[test]
    fn remote_commit_input_rejects_unsafe_paths_modes_and_messages() {
        assert!(super::validate_branch_name("feature/topic").is_ok());
        assert!(super::validate_branch_name("feature/../main").is_err());
        assert!(super::validate_branch_name("feature/name.lock").is_err());
        assert!(super::validate_commit_message("  ").is_err());
        assert!(
            super::validate_commit_files(&[super::GitHubCommitFile {
                path: "../outside.rs".to_string(),
                mode: "100644".to_string(),
                content: Some("safe".to_string()),
            }])
            .is_err()
        );
        assert!(
            super::validate_commit_files(&[super::GitHubCommitFile {
                path: "link".to_string(),
                mode: "120000".to_string(),
                content: Some("target".to_string()),
            }])
            .is_err()
        );
    }

    #[test]
    fn remote_commit_rejects_a_file_nested_under_a_remote_blob() {
        let files = [super::GitHubCommitFile {
            path: "occupied/child.rs".to_string(),
            mode: "100644".to_string(),
            content: Some("content\n".to_string()),
        }];
        let tree = [super::GitHubTreeEntryResponse {
            path: "occupied".to_string(),
            mode: "100644".to_string(),
            kind: "blob".to_string(),
            sha: "d".repeat(40),
        }];

        assert!(super::validate_commit_files_against_tree(&files, &tree).is_err());
    }

    #[test]
    fn remote_commit_serializes_file_deletions_as_a_null_blob_sha() {
        let entry = super::CreateGitTreeEntry {
            path: "removed.rs",
            mode: "100644",
            object_type: "blob",
            content: None,
            sha: super::GitTreeEntrySha::Null(None),
        };

        assert_eq!(
            serde_json::to_value(entry).unwrap(),
            serde_json::json!({
                "path": "removed.rs",
                "mode": "100644",
                "type": "blob",
                "sha": null
            })
        );
    }

    #[test]
    fn comment_review_rejects_changed_base_head_and_missing_anchors_without_a_post() {
        futures::executor::block_on(async {
            for (base, head, line) in [
                (
                    "cccccccccccccccccccccccccccccccccccccccc",
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    1,
                ),
                (
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    "cccccccccccccccccccccccccccccccccccccccc",
                    1,
                ),
                (
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    999,
                ),
            ] {
                let (client, http) = review_client("[]", StatusCode::OK);
                let mut comment = review_comment();
                comment.line = line;
                assert!(
                    client
                        .publish_comment_review(
                            &PullRequestRef::parse("octo-org/review_repo#42").unwrap(),
                            base,
                            head,
                            "Review notes",
                            &[comment]
                        )
                        .await
                        .is_err()
                );
                assert!(
                    http.requests
                        .lock()
                        .unwrap()
                        .iter()
                        .all(|request| request.method == "GET")
                );
            }
        });
    }

    #[test]
    fn comment_review_blocks_duplicates_and_never_retries_a_rejected_write() {
        futures::executor::block_on(async {
            let existing = r#"[{"id":7,"path":"src/lib.rs","line":1,"body":"Please preserve this check.","html_url":"https://github.com/octo-org/review_repo/pull/42#discussion_r7","commit_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","original_commit_id":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","side":"RIGHT","user":{"login":"reviewer"}}]"#;
            for (comments, status, expected_posts) in [
                (existing, StatusCode::OK, 0),
                ("[]", StatusCode::UNPROCESSABLE_ENTITY, 1),
                ("[]", StatusCode::TEMPORARY_REDIRECT, 1),
            ] {
                let (client, http) = review_client(comments, status);
                assert!(
                    client
                        .publish_comment_review(
                            &PullRequestRef::parse("octo-org/review_repo#42").unwrap(),
                            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                            "Review notes",
                            &[review_comment()]
                        )
                        .await
                        .is_err()
                );
                assert_eq!(
                    http.requests
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|request| request.method == "POST")
                        .count(),
                    expected_posts
                );
            }
        });
    }

    #[test]
    fn comment_review_rejects_closed_pr_and_unconfirmed_success_records() {
        futures::executor::block_on(async {
            let reference = PullRequestRef::parse("octo-org/review_repo#42").unwrap();
            let (client, http) = review_client("[]", StatusCode::OK);
            http.responses.lock().unwrap()[0] = response(
                r#"{"number":42,"title":"Parser review","state":"closed","html_url":"https://github.com/octo-org/review_repo/pull/42","user":{"login":"author"},"base":{"sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"},"head":{"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}"#,
            );
            assert!(
                client
                    .publish_comment_review(
                        &reference,
                        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                        "Review notes",
                        &[review_comment()]
                    )
                    .await
                    .is_err()
            );
            assert!(
                http.requests
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|request| request.method == "GET")
            );
            let (client, http) = review_client("[]", StatusCode::OK);
            // A 200 alone is not proof the confirmed review was created.
            http.responses.lock().unwrap()[5] = response("{}");
            assert!(
                client
                    .publish_comment_review(
                        &reference,
                        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                        "Review notes",
                        &[review_comment()]
                    )
                    .await
                    .is_err()
            );
            assert_eq!(
                http.requests
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|request| request.method == "POST")
                    .count(),
                1
            );
        });
    }

    #[test]
    fn comment_review_validation_is_bounded_and_rejects_invalid_input_before_network() {
        let files =
            super::parse_pull_request_files(serde_json::from_str(REVIEW_FILES).unwrap()).unwrap();
        assert!(super::validate_review_comments(&files, &[]).is_err());
        assert!(
            super::validate_review_comments(&files, &[review_comment(), review_comment()]).is_err()
        );
        let mut oversized = review_comment();
        oversized.body = "x".repeat(10_001);
        assert!(super::validate_review_comments(&files, &[oversized]).is_err());
        futures::executor::block_on(async {
            let (client, http) = api_client(&[]);
            let mut invalid = review_comment();
            invalid.path = "../secret".to_string();
            assert!(
                client
                    .publish_comment_review(
                        &PullRequestRef::parse("octo/repo#1").unwrap(),
                        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                        "Review notes",
                        &[invalid]
                    )
                    .await
                    .is_err()
            );
            assert!(http.requests.lock().unwrap().is_empty());
        });
    }

    #[test]
    fn pull_request_reference_requires_an_explicit_safe_owner_repo_and_number() {
        let parsed = PullRequestRef::parse("octo-org/review_repo#42").unwrap();
        assert_eq!(parsed.owner, "octo-org");
        assert_eq!(parsed.repository, "review_repo");
        assert_eq!(parsed.number, 42);
        assert!(PullRequestRef::parse("octo-org/review_repo/other#42").is_err());
        assert!(PullRequestRef::parse("octo-org/review_repo#0").is_err());
        assert!(PullRequestRef::parse("octo-org/review_repo#4%2Fpulls").is_err());
    }

    #[test]
    fn shared_config_fetch_is_pinned_bounded_and_read_only_without_a_pull_request() {
        futures::executor::block_on(async {
            let (client, http) = api_client(&[
                r#"{"type":"file","encoding":"base64","size":12,"path":".cururu.toml","content":"dmVyc2lvbiA9IDEK"}"#,
            ]);
            let source = crate::shared_base::SharedBaseSource {
                owner: "org".to_string(),
                repository: "policy".to_string(),
                commit: "a".repeat(40),
                path: ".cururu.toml".to_string(),
            };
            assert_eq!(
                client.load_shared_config(&source).await.unwrap(),
                "version = 1\n"
            );
            {
                let requests = http.requests.lock().unwrap();
                assert_eq!(requests.len(), 1);
                assert_eq!(requests[0].method, "GET");
                assert_eq!(
                    requests[0].uri,
                    format!(
                        "https://api.github.com/repos/org/policy/contents/.cururu.toml?ref={}",
                        "a".repeat(40)
                    )
                );
            }
            let (client, _) = api_client(&[
                r#"{"type":"file","encoding":"base64","size":65537,"path":".cururu.toml","content":""}"#,
            ]);
            assert!(client.load_shared_config(&source).await.is_err());
            let (client, _) = api_client(&[
                r#"{"type":"symlink","encoding":"base64","size":12,"path":".cururu.toml","content":"dmVyc2lvbiA9IDEK"}"#,
            ]);
            assert!(client.load_shared_config(&source).await.is_err());
            let (client, _) = api_client(&[
                r#"{"type":"file","encoding":"base64","size":12,"path":"other.toml","content":"dmVyc2lvbiA9IDEK"}"#,
            ]);
            assert!(client.load_shared_config(&source).await.is_err());
        });
    }

    #[test]
    fn client_loads_pr_files_and_comments_with_bearer_auth() {
        futures::executor::block_on(async {
            let (client, http) = api_client(&[
                r#"{"number":42,"title":"Review a parser","state":"open","html_url":"https://github.com/octo-org/review_repo/pull/42","user":{"login":"octocat"},"base":{"sha":"base-sha"},"head":{"sha":"head-sha"}}"#,
                r#"[{"filename":"src/lib.rs","status":"modified","additions":1,"deletions":1,"patch":"@@ -1 +1 @@\n-before\n+after"}]"#,
                r#"[{"id":7,"path":"src/lib.rs","line":1,"original_line":1,"body":"Please keep this check.","html_url":"https://github.com/octo-org/review_repo/pull/42#discussion_r7","commit_id":"head-sha","original_commit_id":"base-sha","user":{"login":"reviewer"}}]"#,
                r#"[{"id":13,"state":"COMMENTED","body":"Review summary","html_url":"https://github.com/octo-org/review_repo/pull/42#pullrequestreview-13","submitted_at":"2026-10-09T12:00:00Z","commit_id":"head-sha","user":{"login":"reviewer"}}]"#,
                r#"[{"id":17,"body":"Thanks for the report.","html_url":"https://github.com/octo-org/review_repo/pull/42#issuecomment-17","created_at":"2026-10-09T12:01:00Z","user":{"login":"author"}}]"#,
            ]);
            let snapshot = client
                .load_pull_request(PullRequestRef::parse("octo-org/review_repo#42").unwrap())
                .await
                .unwrap();
            assert_eq!(snapshot.title, "Review a parser");
            assert_eq!(snapshot.base_sha, "base-sha");
            assert_eq!(snapshot.head_sha, "head-sha");
            assert_eq!(snapshot.files.len(), 1);
            assert!(snapshot.files[0].patch.contains("+after"));
            assert_eq!(snapshot.review_threads.len(), 1);
            assert_eq!(snapshot.review_threads[0].root.author, "reviewer");
            assert!(
                snapshot.review_threads[0]
                    .root
                    .body
                    .contains("keep this check")
            );
            assert_eq!(snapshot.reviews[0].state, "COMMENTED");
            assert_eq!(snapshot.conversation_comments[0].author, "author");
            {
                let requests = http.requests.lock().unwrap();
                assert_eq!(requests.len(), 5);
                assert_eq!(
                    requests[0].uri,
                    "https://api.github.com/repos/octo-org/review_repo/pulls/42"
                );
                assert_eq!(
                    requests[0].authorization.as_deref(),
                    Some("Bearer test-token-not-real")
                );
                assert_eq!(requests[0].api_version.as_deref(), Some("2022-11-28"));
                drop(requests);
            }
        });
    }

    #[test]
    fn publishing_a_thread_reply_revalidates_the_pr_and_posts_one_bounded_reply() {
        futures::executor::block_on(async {
            let http = Arc::new(FakeHttpClient {
                responses: Mutex::new(VecDeque::from([
                    response(
                        r#"{"number":42,"title":"Review a parser","state":"open","html_url":"https://github.com/octo-org/review_repo/pull/42","user":{"login":"octocat"},"base":{"sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"},"head":{"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}"#,
                    ),
                    response(
                        r#"[{"id":7,"path":"src/lib.rs","line":8,"original_line":8,"body":"Please keep this check.","html_url":"https://github.com/octo-org/review_repo/pull/42#discussion_r7","commit_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","original_commit_id":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","user":{"login":"reviewer"}}]"#,
                    ),
                    response_with_status(
                        StatusCode::CREATED,
                        r#"{"id":8,"path":"src/lib.rs","line":8,"original_line":8,"body":"I kept the check and added a regression test.","html_url":"https://github.com/octo-org/review_repo/pull/42#discussion_r8","commit_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","original_commit_id":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","in_reply_to_id":7,"user":{"login":"octocat"}}"#,
                    ),
                ])),
                requests: Arc::new(Mutex::new(Vec::new())),
            });
            let client =
                GitHubPullRequestClient::new(http.clone(), "test-token-not-real".to_string())
                    .unwrap();
            let reference = PullRequestRef::parse("octo-org/review_repo#42").unwrap();
            let body = "I kept the check and added a regression test.";
            let target = expected_thread_comment();

            let created = client
                .publish_thread_reply(
                    &reference,
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    &target,
                    body,
                )
                .await
                .unwrap();

            assert_eq!(created.id, 8);
            assert_eq!(created.in_reply_to_id, Some(7));
            assert_eq!(created.body, body);
            let requests = http.requests.lock().unwrap();
            assert_eq!(requests.len(), 3);
            let post = &requests[2];
            assert_eq!(post.method, Method::POST.as_str());
            assert_eq!(
                post.uri,
                "https://api.github.com/repos/octo-org/review_repo/pulls/42/comments"
            );
            assert_eq!(
                post.authorization.as_deref(),
                Some("Bearer test-token-not-real")
            );
            assert_eq!(post.api_version.as_deref(), Some("2022-11-28"));
            assert_eq!(post.content_type.as_deref(), Some("application/json"));
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&post.body).unwrap(),
                serde_json::json!({"body": body, "in_reply_to": 7})
            );
            drop(requests);
        });
    }

    #[test]
    fn publishing_a_thread_reply_does_not_retry_after_a_github_rejection() {
        futures::executor::block_on(async {
            let http = Arc::new(FakeHttpClient {
                responses: Mutex::new(VecDeque::from([
                    response(
                        r#"{"number":42,"title":"Review a parser","state":"open","html_url":"https://github.com/octo-org/review_repo/pull/42","user":{"login":"octocat"},"base":{"sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"},"head":{"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}"#,
                    ),
                    response(
                        r#"[{"id":7,"path":"src/lib.rs","line":8,"original_line":8,"body":"Please keep this check.","html_url":"https://github.com/octo-org/review_repo/pull/42#discussion_r7","commit_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","original_commit_id":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","user":{"login":"reviewer"}}]"#,
                    ),
                    response_with_status(StatusCode::UNPROCESSABLE_ENTITY, "{}"),
                ])),
                requests: Arc::new(Mutex::new(Vec::new())),
            });
            let client =
                GitHubPullRequestClient::new(http.clone(), "test-token-not-real".to_string())
                    .unwrap();
            let reference = PullRequestRef::parse("octo-org/review_repo#42").unwrap();
            let target = expected_thread_comment();

            let result = client
                .publish_thread_reply(
                    &reference,
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    &target,
                    "A safe reply",
                )
                .await;

            assert!(result.is_err());
            let requests = http.requests.lock().unwrap();
            assert_eq!(requests.len(), 3);
            assert_eq!(
                requests
                    .iter()
                    .filter(|request| request.method == Method::POST.as_str())
                    .count(),
                1
            );
            drop(requests);
        });
    }

    #[test]
    fn publishing_a_thread_reply_blocks_an_identical_existing_reply() {
        futures::executor::block_on(async {
            let http = Arc::new(FakeHttpClient {
                responses: Mutex::new(VecDeque::from([
                    response(
                        r#"{"number":42,"title":"Review a parser","state":"open","html_url":"https://github.com/octo-org/review_repo/pull/42","user":{"login":"octocat"},"base":{"sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"},"head":{"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}"#,
                    ),
                    response(
                        r#"[{"id":7,"path":"src/lib.rs","line":8,"original_line":8,"body":"Please keep this check.","html_url":"https://github.com/octo-org/review_repo/pull/42#discussion_r7","commit_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","original_commit_id":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","user":{"login":"reviewer"}},{"id":8,"path":"src/lib.rs","line":8,"original_line":8,"body":"A safe reply","html_url":"https://github.com/octo-org/review_repo/pull/42#discussion_r8","commit_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","original_commit_id":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","in_reply_to_id":7,"user":{"login":"octocat"}}]"#,
                    ),
                ])),
                requests: Arc::new(Mutex::new(Vec::new())),
            });
            let client =
                GitHubPullRequestClient::new(http.clone(), "test-token-not-real".to_string())
                    .unwrap();
            let reference = PullRequestRef::parse("octo-org/review_repo#42").unwrap();
            let target = expected_thread_comment();

            let result = client
                .publish_thread_reply(
                    &reference,
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    &target,
                    "A safe reply",
                )
                .await;

            assert!(result.is_err());
            let requests = http.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert!(
                requests
                    .iter()
                    .all(|request| request.method == Method::GET.as_str())
            );
            drop(requests);
        });
    }

    #[test]
    fn publishing_a_thread_reply_fails_closed_if_the_pull_request_head_changed() {
        futures::executor::block_on(async {
            let http = Arc::new(FakeHttpClient {
                responses: Mutex::new(VecDeque::from([
                    response(
                        r#"{"number":42,"title":"Review a parser","state":"open","html_url":"https://github.com/octo-org/review_repo/pull/42","user":{"login":"octocat"},"base":{"sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"},"head":{"sha":"cccccccccccccccccccccccccccccccccccccccc"}}"#,
                    ),
                    response(
                        r#"[{"id":7,"path":"src/lib.rs","line":8,"original_line":8,"body":"Please keep this check.","html_url":"https://github.com/octo-org/review_repo/pull/42#discussion_r7","commit_id":"cccccccccccccccccccccccccccccccccccccccc","original_commit_id":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","user":{"login":"reviewer"}}]"#,
                    ),
                ])),
                requests: Arc::new(Mutex::new(Vec::new())),
            });
            let client =
                GitHubPullRequestClient::new(http.clone(), "test-token-not-real".to_string())
                    .unwrap();
            let reference = PullRequestRef::parse("octo-org/review_repo#42").unwrap();
            let target = expected_thread_comment();

            let result = client
                .publish_thread_reply(
                    &reference,
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    &target,
                    "A safe reply",
                )
                .await;

            assert!(result.is_err());
            let requests = http.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert!(
                requests
                    .iter()
                    .all(|request| request.method == Method::GET.as_str())
            );
            drop(requests);
        });
    }

    #[test]
    fn publishing_a_thread_reply_fails_closed_if_the_pull_request_is_closed() {
        futures::executor::block_on(async {
            let http = Arc::new(FakeHttpClient {
                responses: Mutex::new(VecDeque::from([response(
                    r#"{"number":42,"title":"Review a parser","state":"closed","html_url":"https://github.com/octo-org/review_repo/pull/42","user":{"login":"octocat"},"base":{"sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"},"head":{"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}"#,
                )])),
                requests: Arc::new(Mutex::new(Vec::new())),
            });
            let client =
                GitHubPullRequestClient::new(http.clone(), "test-token-not-real".to_string())
                    .unwrap();
            let reference = PullRequestRef::parse("octo-org/review_repo#42").unwrap();
            let target = expected_thread_comment();

            let result = client
                .publish_thread_reply(
                    &reference,
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    &target,
                    "A safe reply",
                )
                .await;

            assert!(result.is_err());
            let requests = http.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].method, Method::GET.as_str());
            drop(requests);
        });
    }

    #[test]
    fn publishing_a_thread_reply_fails_closed_if_the_target_was_removed() {
        futures::executor::block_on(async {
            let http = Arc::new(FakeHttpClient {
                responses: Mutex::new(VecDeque::from([
                    response(
                        r#"{"number":42,"title":"Review a parser","state":"open","html_url":"https://github.com/octo-org/review_repo/pull/42","user":{"login":"octocat"},"base":{"sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"},"head":{"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}"#,
                    ),
                    response("[]"),
                ])),
                requests: Arc::new(Mutex::new(Vec::new())),
            });
            let client =
                GitHubPullRequestClient::new(http.clone(), "test-token-not-real".to_string())
                    .unwrap();
            let reference = PullRequestRef::parse("octo-org/review_repo#42").unwrap();
            let target = expected_thread_comment();

            let result = client
                .publish_thread_reply(
                    &reference,
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    &target,
                    "A safe reply",
                )
                .await;

            assert!(result.is_err());
            let requests = http.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert!(
                requests
                    .iter()
                    .all(|request| request.method == Method::GET.as_str())
            );
            drop(requests);
        });
    }

    #[test]
    fn publishing_a_thread_reply_fails_closed_if_the_target_changed_since_preview() {
        futures::executor::block_on(async {
            let http = Arc::new(FakeHttpClient {
                responses: Mutex::new(VecDeque::from([
                    response(
                        r#"{"number":42,"title":"Review a parser","state":"open","html_url":"https://github.com/octo-org/review_repo/pull/42","user":{"login":"octocat"},"base":{"sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"},"head":{"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}"#,
                    ),
                    response(
                        r#"[{"id":7,"path":"src/lib.rs","line":8,"original_line":8,"body":"Updated comment text.","html_url":"https://github.com/octo-org/review_repo/pull/42#discussion_r7","commit_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","original_commit_id":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","user":{"login":"reviewer"}}]"#,
                    ),
                ])),
                requests: Arc::new(Mutex::new(Vec::new())),
            });
            let client =
                GitHubPullRequestClient::new(http.clone(), "test-token-not-real".to_string())
                    .unwrap();
            let reference = PullRequestRef::parse("octo-org/review_repo#42").unwrap();
            let target = expected_thread_comment();

            let result = client
                .publish_thread_reply(
                    &reference,
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    &target,
                    "A safe reply",
                )
                .await;

            assert!(result.is_err());
            let requests = http.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert!(
                requests
                    .iter()
                    .all(|request| request.method == Method::GET.as_str())
            );
            drop(requests);
        });
    }

    #[test]
    fn invalid_thread_reply_is_rejected_before_any_network_request() {
        futures::executor::block_on(async {
            let (client, http) = api_client(&[]);
            let reference = PullRequestRef::parse("octo-org/review_repo#42").unwrap();
            let target = expected_thread_comment();

            let result = client
                .publish_thread_reply(
                    &reference,
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    &target,
                    " \n\t",
                )
                .await;

            assert!(result.is_err());
            assert!(http.requests.lock().unwrap().is_empty());
        });
    }

    #[test]
    fn thread_reply_body_is_bounded_and_allows_markdown_line_breaks() {
        assert!(validate_thread_reply_body("A reply\nwith Markdown.").is_ok());
        assert!(validate_thread_reply_body(" \n\t").is_err());
        assert!(validate_thread_reply_body("bad\0body").is_err());
        assert!(validate_thread_reply_body(&"x".repeat(10_001)).is_err());
    }

    #[test]
    fn source_file_is_loaded_at_the_comment_commit_with_a_strict_size_limit() {
        futures::executor::block_on(async {
            let (client, http) = api_client(&[
                r#"{"type":"file","encoding":"base64","size":13,"path":"src/lib.rs","content":"Zm4gbWFpbigpIHt9Cg==","sha":"blob-sha"}"#,
            ]);
            let commit_sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
            let source = client
                .load_source_file(
                    &PullRequestRef::parse("octo-org/review_repo#42").unwrap(),
                    "src/lib.rs",
                    commit_sha,
                )
                .await
                .unwrap();
            assert_eq!(source.path, "src/lib.rs");
            assert_eq!(source.commit_sha, commit_sha);
            assert_eq!(source.content, "fn main() {}\n");
            {
                let requests = http.requests.lock().unwrap();
                assert_eq!(
                    requests[0].uri,
                    format!(
                        "https://api.github.com/repos/octo-org/review_repo/contents/src/lib.rs?ref={commit_sha}"
                    )
                );
                drop(requests);
            }
        });
    }

    #[test]
    fn source_file_rejects_unsafe_paths_and_large_files() {
        futures::executor::block_on(async {
            let (client, _) = api_client(&[]);
            let reference = PullRequestRef::parse("octo-org/review_repo#42").unwrap();
            let commit_sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
            assert!(
                client
                    .load_source_file(&reference, "../secrets.txt", commit_sha)
                    .await
                    .is_err()
            );

            let (client, _) = api_client(&[
                r#"{"type":"file","encoding":"base64","size":262145,"path":"src/lib.rs","content":"","sha":"blob-sha"}"#,
            ]);
            assert!(
                client
                    .load_source_file(&reference, "src/lib.rs", commit_sha)
                    .await
                    .is_err()
            );
        });
    }

    #[test]
    fn client_follows_only_github_api_next_links() {
        let mut headers = gpui::http_client::http::HeaderMap::new();
        headers.insert(
            gpui::http_client::http::header::LINK,
            "<https://api.github.com/repos/org/repo/pulls/1/comments?page=2>; rel=\"next\", <https://api.github.com/repos/org/repo/pulls/1/comments?page=5>; rel=\"last\""
                .parse()
                .unwrap(),
        );
        assert_eq!(
            next_page_url(&headers).unwrap().as_deref(),
            Some("https://api.github.com/repos/org/repo/pulls/1/comments?page=2")
        );
        headers.insert(
            gpui::http_client::http::header::LINK,
            "<https://attacker.invalid/steal>; rel=\"next\""
                .parse()
                .unwrap(),
        );
        assert!(next_page_url(&headers).is_err());
    }

    #[test]
    fn collection_pagination_uses_the_same_api_endpoint_and_does_not_retry() {
        futures::executor::block_on(async {
            let http = Arc::new(FakeHttpClient {
                responses: Mutex::new(VecDeque::from([
                    response_with_link(
                        "[]",
                        "<https://api.github.com/repos/org/repo/pulls/1/comments?page=2>; rel=\"next\"",
                    ),
                    response("[]"),
                ])),
                requests: Arc::new(Mutex::new(Vec::new())),
            });
            let client =
                GitHubPullRequestClient::new(http.clone(), "test-token-not-real".to_string())
                    .unwrap();
            let comments: Vec<super::PullRequestReviewComment> = client
                .get_all_pages(
                    "https://api.github.com/repos/org/repo/pulls/1/comments?per_page=100&page=1",
                )
                .await
                .unwrap();
            assert!(comments.is_empty());
            {
                let requests = http.requests.lock().unwrap();
                assert_eq!(requests.len(), 2);
                assert_eq!(
                    requests[1].uri,
                    "https://api.github.com/repos/org/repo/pulls/1/comments?page=2"
                );
                drop(requests);
            }
        });
    }

    #[test]
    fn response_body_is_bounded_while_streaming() {
        futures::executor::block_on(async {
            let oversized = "x".repeat(super::MAX_PAGE_BYTES + 1);
            let response = Response::builder()
                .status(200)
                .body(AsyncBody::from(oversized))
                .unwrap();
            let http = Arc::new(FakeHttpClient {
                responses: Mutex::new(VecDeque::from([response])),
                requests: Arc::new(Mutex::new(Vec::new())),
            });
            let client =
                GitHubPullRequestClient::new(http, "test-token-not-real".to_string()).unwrap();
            assert!(
                client
                    .get_page("https://api.github.com/repos/org/repo/pulls/1")
                    .await
                    .is_err()
            );
        });
    }

    #[test]
    fn missing_patch_fails_closed_instead_of_creating_partial_review_data() {
        let files = serde_json::from_str::<Vec<super::PullRequestFile>>(
            r#"[{"filename":"large.bin","status":"modified","additions":3,"deletions":4}]"#,
        )
        .unwrap();
        assert!(parse_pull_request_files(files).is_err());
    }

    #[test]
    fn incomplete_hunk_counts_are_rejected() {
        let files = serde_json::from_str::<Vec<super::PullRequestFile>>(
            r#"[{"filename":"src/lib.rs","status":"modified","additions":2,"deletions":1,"patch":"@@ -1 +1,2 @@\n-before\n+after"}]"#,
        )
        .unwrap();
        assert!(parse_pull_request_files(files).is_err());
    }

    #[test]
    fn inline_comments_are_grouped_and_outdated_locations_are_not_called_resolved() {
        let comments = parse_pull_request_comments(
            serde_json::from_str::<Vec<super::PullRequestReviewComment>>(
                r#"[
                    {"id":1,"path":"src/lib.rs","line":null,"original_line":8,"side":"RIGHT","body":"Original point","html_url":"https://github.com/org/repo/pull/1#discussion_r1","commit_id":"head","original_commit_id":"old","user":{"login":"reviewer"}},
                    {"id":2,"path":"src/lib.rs","line":null,"original_line":8,"side":"RIGHT","body":"Reply","html_url":"https://github.com/org/repo/pull/1#discussion_r2","commit_id":"head","original_commit_id":"old","in_reply_to_id":1,"user":{"login":"author"}}
                ]"#,
            )
            .unwrap(),
        )
        .unwrap();
        let threads = group_review_threads(comments);
        assert_eq!(threads.len(), 1);
        assert!(threads[0].root.is_outdated);
        assert_eq!(threads[0].replies.len(), 1);
        assert_eq!(threads[0].replies[0].in_reply_to_id, Some(1));
    }

    #[test]
    fn client_debug_redacts_the_access_token() {
        let (client, _) = api_client(&[]);
        assert!(!format!("{client:?}").contains("test-token-not-real"));
    }
}
