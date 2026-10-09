use std::{fmt, path::Path, sync::Arc};

use anyhow::{Context as _, ensure};
use gpui::http_client::{
    AsyncBody, HttpClient, HttpRequestExt as _, Method, RedirectPolicy, Request, StatusCode, Url,
};
use http_body_util::BodyExt as _;
use serde::Deserialize;

const API_ROOT: &str = "https://api.github.com";
const API_VERSION: &str = "2022-11-28";
const PER_PAGE: usize = 100;
const MAX_PAGE_BYTES: usize = 1_048_576;
const MAX_COLLECTION_BYTES: usize = 8 * 1_048_576;
const MAX_COLLECTION_PAGES: usize = 50;
const MAX_REMOTE_DIFF_FILE_BYTES: usize = 45_000;
const MAX_REMOTE_DIFF_BYTES: usize = 180_000;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestSnapshot {
    pub reference: PullRequestRef,
    pub title: String,
    pub state: String,
    pub author: String,
    pub html_url: String,
    pub base_sha: String,
    pub head_sha: String,
    pub files: Vec<cururu_core::ChangedFile>,
    pub comments: Vec<PullRequestComment>,
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

        Ok(PullRequestSnapshot {
            reference,
            title: pull_request.title,
            state: pull_request.state,
            author: pull_request.user.login,
            html_url: pull_request.html_url,
            base_sha: pull_request.base.sha,
            head_sha: pull_request.head.sha,
            files,
            comments,
        })
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
            && !value.chars().any(char::is_control)
            && Path::new(value)
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_))),
        "GitHub returned an unsafe changed-file path"
    );
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
        comments.push(PullRequestComment {
            id: comment.id,
            path: comment.path,
            line: comment.line.and_then(|line| u32::try_from(line).ok()),
            original_line: comment
                .original_line
                .and_then(|line| u32::try_from(line).ok()),
            body: comment.body,
            author: comment.user.login,
            html_url: comment.html_url,
            commit_id: comment.commit_id,
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
    user: GitHubUser,
    base: PullRequestCommit,
    head: PullRequestCommit,
}

#[derive(Deserialize)]
struct PullRequestCommit {
    sha: String,
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
    user: GitHubUser,
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use gpui::http_client::{AsyncBody, HttpClient, Request, Response, Url};

    use super::{GitHubPullRequestClient, PullRequestRef, next_page_url, parse_pull_request_files};

    struct FakeHttpClient {
        responses: Mutex<VecDeque<Response<AsyncBody>>>,
        requests: Mutex<Vec<RequestInfo>>,
    }

    struct RequestInfo {
        uri: String,
        authorization: Option<String>,
        api_version: Option<String>,
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
            let request_info = RequestInfo {
                uri: request.uri().to_string(),
                authorization: request
                    .headers()
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string),
                api_version: request
                    .headers()
                    .get("x-github-api-version")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string),
            };
            let response = self.responses.lock().unwrap().pop_front();
            self.requests.lock().unwrap().push(request_info);
            Box::pin(async move { response.ok_or_else(|| anyhow::anyhow!("no fake response")) })
        }

        fn proxy(&self) -> Option<&Url> {
            None
        }
    }

    fn response(body: &'static str) -> Response<AsyncBody> {
        Response::builder()
            .status(200)
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
            requests: Mutex::new(Vec::new()),
        });
        let client =
            GitHubPullRequestClient::new(http.clone(), "test-token-not-real".to_string()).unwrap();
        (client, http)
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
    fn client_loads_pr_files_and_comments_with_bearer_auth() {
        futures::executor::block_on(async {
            let (client, http) = api_client(&[
                r#"{"number":42,"title":"Review a parser","state":"open","html_url":"https://github.com/octo-org/review_repo/pull/42","user":{"login":"octocat"},"base":{"sha":"base-sha"},"head":{"sha":"head-sha"}}"#,
                r#"[{"filename":"src/lib.rs","status":"modified","additions":1,"deletions":1,"patch":"@@ -1 +1 @@\n-before\n+after"}]"#,
                r#"[{"id":7,"path":"src/lib.rs","line":1,"original_line":1,"body":"Please keep this check.","html_url":"https://github.com/octo-org/review_repo/pull/42#discussion_r7","commit_id":"head-sha","user":{"login":"reviewer"}}]"#,
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
            assert_eq!(snapshot.comments[0].author, "reviewer");
            assert!(snapshot.comments[0].body.contains("keep this check"));
            let requests = http.requests.lock().unwrap();
            assert_eq!(requests.len(), 3);
            assert_eq!(
                requests[0].uri,
                "https://api.github.com/repos/octo-org/review_repo/pulls/42"
            );
            assert_eq!(
                requests[0].authorization.as_deref(),
                Some("Bearer test-token-not-real")
            );
            assert_eq!(requests[0].api_version.as_deref(), Some("2022-11-28"));
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
                requests: Mutex::new(Vec::new()),
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
            let requests = http.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(
                requests[1].uri,
                "https://api.github.com/repos/org/repo/pulls/1/comments?page=2"
            );
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
                requests: Mutex::new(Vec::new()),
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
    fn client_debug_redacts_the_access_token() {
        let (client, _) = api_client(&[]);
        assert!(!format!("{client:?}").contains("test-token-not-real"));
    }
}
