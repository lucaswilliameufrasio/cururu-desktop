use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, ensure};
use gpui::http_client::{AsyncBody, HttpClient, Method, Request, StatusCode};
use http_body_util::BodyExt as _;
use serde::{Deserialize, Serialize};
use url::Url;

const DEVICE_CODE_ENDPOINT: &str = "https://github.com/login/device/code";
const ACCESS_TOKEN_ENDPOINT: &str = "https://github.com/login/oauth/access_token";
const CREDENTIAL_SERVICE: &str = "com.cururu.desktop.github";
const CREDENTIAL_ACCOUNT: &str = "github.com";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const MAX_AUTH_RESPONSE_BYTES: usize = 16_384;

#[derive(Clone, PartialEq, Eq)]
pub struct DeviceAuthorization {
    pub user_code: String,
    pub verification_uri: Url,
    pub expires_at: Instant,
    pub polling_interval: Duration,
    device_code: String,
}

impl std::fmt::Debug for DeviceAuthorization {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeviceAuthorization")
            .field("user_code", &"[REDACTED]")
            .field("verification_uri", &self.verification_uri)
            .field("expires_at", &self.expires_at)
            .field("polling_interval", &self.polling_interval)
            .field("device_code", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevicePoll {
    Pending,
    SlowDown,
    Authorized(StoredCredentials),
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredCredentials {
    pub access_token: String,
    pub refresh_token: String,
    pub access_token_expires_in: u64,
    pub refresh_token_expires_in: u64,
    pub issued_at_unix_seconds: u64,
}

/// Session-only union: only the Device Flow payload can reach `CredentialStore`.
#[derive(Debug, Clone)]
pub enum SessionCredentials {
    DeviceFlow(StoredCredentials),
    GhCli(crate::gh_cli::GhToken),
}

impl SessionCredentials {
    #[must_use]
    pub fn access_token(&self) -> &str {
        match self {
            Self::DeviceFlow(credentials) => &credentials.access_token,
            Self::GhCli(token) => token.as_str(),
        }
    }

    #[must_use]
    pub fn access_token_is_expired(&self) -> bool {
        match self {
            Self::DeviceFlow(credentials) => credentials.access_token_is_expired(),
            Self::GhCli(_) => false,
        }
    }

    #[must_use]
    pub fn device_flow_credentials(&self) -> Option<StoredCredentials> {
        match self {
            Self::DeviceFlow(credentials) => Some(credentials.clone()),
            Self::GhCli(_) => None,
        }
    }

    #[must_use]
    pub const fn is_gh_cli(&self) -> bool {
        matches!(self, Self::GhCli(_))
    }
}

impl StoredCredentials {
    #[must_use]
    pub fn access_token_is_expired(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(u64::MAX, |duration| duration.as_secs());
        now >= self
            .issued_at_unix_seconds
            .saturating_add(self.access_token_expires_in)
    }
}

impl std::fmt::Debug for StoredCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StoredCredentials")
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &"[REDACTED]")
            .field("access_token_expires_in", &self.access_token_expires_in)
            .field("refresh_token_expires_in", &self.refresh_token_expires_in)
            .field("issued_at_unix_seconds", &self.issued_at_unix_seconds)
            .finish()
    }
}

pub struct GitHubDeviceFlow {
    client_id: String,
    http: Arc<dyn HttpClient>,
}

impl GitHubDeviceFlow {
    pub fn new(client_id: String, http: Arc<dyn HttpClient>) -> anyhow::Result<Self> {
        ensure!(
            !client_id.is_empty()
                && client_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte)),
            "GitHub App client ID is missing or malformed"
        );
        Ok(Self { client_id, http })
    }

    pub async fn request_device_code(&self) -> anyhow::Result<DeviceAuthorization> {
        let form = form_body(&[
            ("client_id", self.client_id.as_str()),
            ("scope", "offline_access"),
        ]);
        let response: DeviceCodeResponse = self
            .post_form(DEVICE_CODE_ENDPOINT, form)
            .await
            .context("GitHub device authorization request failed")?;
        let verification_uri = Url::parse(&response.verification_uri)
            .context("GitHub returned an invalid device verification URL")?;
        ensure!(
            verification_uri.scheme() == "https"
                && verification_uri.host_str() == Some("github.com")
                && verification_uri.path() == "/login/device",
            "GitHub returned an unexpected device verification URL"
        );
        ensure!(
            !response.user_code.is_empty() && !response.device_code.is_empty(),
            "GitHub returned an incomplete device authorization"
        );
        ensure!(
            response.expires_in > 0,
            "GitHub returned an expired device code"
        );
        ensure!(
            response.expires_in <= 3_600 && response.interval <= 60,
            "GitHub returned unreasonable device authorization timing"
        );
        Ok(DeviceAuthorization {
            user_code: response.user_code,
            verification_uri,
            expires_at: Instant::now() + Duration::from_secs(response.expires_in),
            polling_interval: Duration::from_secs(response.interval.max(1)),
            device_code: response.device_code,
        })
    }

    pub async fn poll(&self, authorization: &DeviceAuthorization) -> anyhow::Result<DevicePoll> {
        let form = form_body(&[
            ("client_id", self.client_id.as_str()),
            ("device_code", authorization.device_code.as_str()),
            ("grant_type", DEVICE_GRANT),
        ]);
        let response: TokenPollResponse = self
            .post_form(ACCESS_TOKEN_ENDPOINT, form)
            .await
            .context("GitHub device authorization polling failed")?;
        match response.error.as_deref() {
            Some("authorization_pending") => Ok(DevicePoll::Pending),
            Some("slow_down") => Ok(DevicePoll::SlowDown),
            Some("access_denied") => anyhow::bail!("GitHub authorization was denied"),
            Some("expired_token" | "token_expired") => {
                anyhow::bail!("GitHub device authorization expired")
            }
            Some(_) => anyhow::bail!("GitHub rejected the device authorization request"),
            None => {
                let (
                    Some(access_token),
                    Some(refresh_token),
                    Some(expires_in),
                    Some(refresh_token_expires_in),
                ) = (
                    response.access_token,
                    response.refresh_token,
                    response.expires_in,
                    response.refresh_token_expires_in,
                )
                else {
                    anyhow::bail!("GitHub returned an incomplete token response");
                };
                ensure!(
                    response.token_type.as_deref() == Some("bearer"),
                    "GitHub returned an unsupported token type"
                );
                ensure!(
                    !access_token.is_empty()
                        && !refresh_token.is_empty()
                        && expires_in > 0
                        && refresh_token_expires_in > 0,
                    "GitHub returned invalid expiring credentials"
                );
                Ok(DevicePoll::Authorized(StoredCredentials {
                    access_token,
                    refresh_token,
                    access_token_expires_in: expires_in,
                    refresh_token_expires_in,
                    issued_at_unix_seconds: current_unix_seconds()?,
                }))
            }
        }
    }

    pub async fn refresh(
        &self,
        credentials: &StoredCredentials,
    ) -> anyhow::Result<StoredCredentials> {
        let form = form_body(&[
            ("client_id", self.client_id.as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", credentials.refresh_token.as_str()),
        ]);
        let response: TokenPollResponse = self
            .post_form(ACCESS_TOKEN_ENDPOINT, form)
            .await
            .context("GitHub token refresh failed")?;
        if response.error.is_some() {
            anyhow::bail!("GitHub rejected the saved refresh token; sign in again");
        }
        let (
            Some(access_token),
            Some(refresh_token),
            Some(expires_in),
            Some(refresh_token_expires_in),
        ) = (
            response.access_token,
            response.refresh_token,
            response.expires_in,
            response.refresh_token_expires_in,
        )
        else {
            anyhow::bail!("GitHub returned an incomplete refreshed-token response");
        };
        ensure!(
            response.token_type.as_deref() == Some("bearer"),
            "GitHub returned an unsupported refreshed-token type"
        );
        ensure!(
            !access_token.is_empty()
                && !refresh_token.is_empty()
                && expires_in > 0
                && refresh_token_expires_in > 0,
            "GitHub returned invalid refreshed credentials"
        );
        Ok(StoredCredentials {
            access_token,
            refresh_token,
            access_token_expires_in: expires_in,
            refresh_token_expires_in,
            issued_at_unix_seconds: current_unix_seconds()?,
        })
    }

    async fn post_form<T>(&self, endpoint: &str, body: String) -> anyhow::Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        let request = Request::builder()
            .method(Method::POST)
            .uri(endpoint)
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(AsyncBody::from(body))
            .context("failed to build GitHub authorization request")?;
        let response = self
            .http
            .send(request)
            .await
            .context("could not reach GitHub authorization service")?;
        let (parts, body) = response.into_parts();
        ensure!(
            parts.status == StatusCode::OK,
            "GitHub authorization service returned HTTP {}",
            parts.status
        );
        if let Some(content_length) = parts
            .headers
            .get(gpui::http_client::http::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok())
        {
            ensure!(
                content_length <= MAX_AUTH_RESPONSE_BYTES,
                "GitHub authorization response exceeded the size limit"
            );
        }
        let mut bytes = Vec::new();
        let mut body = body;
        while let Some(frame) = body.frame().await {
            let frame = frame.context("could not read GitHub authorization response")?;
            let Ok(data) = frame.into_data() else {
                continue;
            };
            ensure!(
                bytes.len().saturating_add(data.len()) <= MAX_AUTH_RESPONSE_BYTES,
                "GitHub authorization response exceeded the size limit"
            );
            bytes.extend_from_slice(&data);
        }
        serde_json::from_slice(&bytes).context("GitHub returned an invalid authorization response")
    }
}

pub struct CredentialStore;

impl CredentialStore {
    pub fn load() -> anyhow::Result<Option<StoredCredentials>> {
        let entry = keyring::Entry::new(CREDENTIAL_SERVICE, CREDENTIAL_ACCOUNT)
            .map_err(|_| anyhow::anyhow!("operating-system credential store is unavailable"))?;
        let value = match entry.get_password() {
            Ok(value) => value,
            Err(keyring::Error::NoEntry) => return Ok(None),
            Err(_) => {
                anyhow::bail!("could not read GitHub credentials from the operating-system store")
            }
        };
        serde_json::from_str(&value).context("stored GitHub credentials have an invalid format")
    }

    pub fn save(credentials: &StoredCredentials) -> anyhow::Result<()> {
        let serialized =
            serde_json::to_string(credentials).context("could not serialize GitHub credentials")?;
        let entry = keyring::Entry::new(CREDENTIAL_SERVICE, CREDENTIAL_ACCOUNT)
            .map_err(|_| anyhow::anyhow!("operating-system credential store is unavailable"))?;
        entry.set_password(&serialized).map_err(|_| {
            anyhow::anyhow!("could not save credentials to the operating-system store")
        })
    }

    pub fn delete() -> anyhow::Result<()> {
        let entry = keyring::Entry::new(CREDENTIAL_SERVICE, CREDENTIAL_ACCOUNT)
            .map_err(|_| anyhow::anyhow!("operating-system credential store is unavailable"))?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => {
                anyhow::bail!("could not delete GitHub credentials from the operating-system store")
            }
        }
    }
}

fn form_body(fields: &[(&str, &str)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in fields {
        serializer.append_pair(key, value);
    }
    serializer.finish()
}

fn current_unix_seconds() -> anyhow::Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")
        .map(|duration| duration.as_secs())
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Deserialize)]
struct TokenPollResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    refresh_token_expires_in: Option<u64>,
    token_type: Option<String>,
    error: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use gpui::http_client::{AsyncBody, HttpClient, Request, Response, Url};

    use super::{DevicePoll, GitHubDeviceFlow, StoredCredentials, form_body};

    struct FakeHttpClient {
        responses: Mutex<VecDeque<Response<AsyncBody>>>,
    }

    impl HttpClient for FakeHttpClient {
        fn type_name(&self) -> &'static str {
            "fake-github-auth-client"
        }

        fn user_agent(&self) -> Option<&gpui::http_client::http::HeaderValue> {
            None
        }

        fn send(
            &self,
            _request: Request<AsyncBody>,
        ) -> futures::future::BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
            let response = self.responses.lock().unwrap().pop_front();
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

    fn client(responses: &[&'static str]) -> GitHubDeviceFlow {
        let http = Arc::new(FakeHttpClient {
            responses: Mutex::new(responses.iter().map(|body| response(body)).collect()),
        });
        GitHubDeviceFlow::new("public-app-id".to_string(), http).unwrap()
    }

    #[test]
    fn device_flow_validates_github_verification_url_and_exposes_user_code() {
        futures::executor::block_on(async {
            let client = client(&[
                r#"{"device_code":"device-secret","user_code":"ABCD-EFGH","verification_uri":"https://github.com/login/device","expires_in":900,"interval":5}"#,
            ]);
            let authorization = client.request_device_code().await.unwrap();
            assert_eq!(authorization.user_code, "ABCD-EFGH");
            assert_eq!(
                authorization.verification_uri.as_str(),
                "https://github.com/login/device"
            );
            assert_eq!(authorization.polling_interval, Duration::from_secs(5));
        });
    }

    #[test]
    fn device_flow_handles_pending_slow_down_and_expired_tokens() {
        futures::executor::block_on(async {
            let client = client(&[
                r#"{"device_code":"device-secret","user_code":"ABCD-EFGH","verification_uri":"https://github.com/login/device","expires_in":900,"interval":5}"#,
                r#"{"error":"authorization_pending"}"#,
                r#"{"error":"slow_down"}"#,
                r#"{"error":"expired_token"}"#,
            ]);
            let authorization = client.request_device_code().await.unwrap();
            assert_eq!(
                client.poll(&authorization).await.unwrap(),
                DevicePoll::Pending
            );
            assert_eq!(
                client.poll(&authorization).await.unwrap(),
                DevicePoll::SlowDown
            );
            assert!(client.poll(&authorization).await.is_err());
        });
    }

    #[test]
    fn device_flow_redacts_device_and_user_tokens_from_debug_output() {
        futures::executor::block_on(async {
            let client = client(&[
                r#"{"device_code":"device-secret","user_code":"ABCD-EFGH","verification_uri":"https://github.com/login/device","expires_in":900,"interval":5}"#,
                r#"{"access_token":"access-secret","refresh_token":"refresh-secret","expires_in":28800,"refresh_token_expires_in":15897600,"scope":"","token_type":"bearer"}"#,
            ]);
            let authorization = client.request_device_code().await.unwrap();
            let poll = client.poll(&authorization).await.unwrap();
            let debug_output = format!("{authorization:?} {poll:?}");
            assert!(!debug_output.contains("device-secret"));
            assert!(!debug_output.contains("access-secret"));
            assert!(!debug_output.contains("refresh-secret"));
            assert!(matches!(poll, DevicePoll::Authorized(_)));
        });
    }

    #[test]
    fn refresh_replaces_both_expiring_tokens_and_tracks_expiry() {
        futures::executor::block_on(async {
            let client = client(&[
                r#"{"access_token":"new-access-secret","refresh_token":"new-refresh-secret","expires_in":28800,"refresh_token_expires_in":15897600,"scope":"","token_type":"bearer"}"#,
            ]);
            let credentials = StoredCredentials {
                access_token: "old-access-secret".to_string(),
                refresh_token: "old-refresh-secret".to_string(),
                access_token_expires_in: 1,
                refresh_token_expires_in: 60,
                issued_at_unix_seconds: 0,
            };
            assert!(credentials.access_token_is_expired());
            let refreshed = client.refresh(&credentials).await.unwrap();
            assert_eq!(refreshed.access_token, "new-access-secret");
            assert_eq!(refreshed.refresh_token, "new-refresh-secret");
            assert!(!refreshed.access_token_is_expired());
            assert!(!format!("{refreshed:?}").contains("new-access-secret"));
        });
    }

    #[test]
    fn oauth_form_encoding_escapes_untrusted_values() {
        assert_eq!(
            form_body(&[("device_code", "unsafe&grant_type=wrong")]),
            "device_code=unsafe%26grant_type%3Dwrong"
        );
    }

    #[test]
    fn device_flow_rejects_unexpected_verification_hosts_and_invalid_client_ids() {
        futures::executor::block_on(async {
            let client = client(&[
                r#"{"device_code":"device-secret","user_code":"ABCD-EFGH","verification_uri":"https://attacker.invalid/login/device","expires_in":900,"interval":5}"#,
            ]);
            assert!(client.request_device_code().await.is_err());
            assert!(
                GitHubDeviceFlow::new(
                    "id&scope=repo".to_string(),
                    Arc::new(FakeHttpClient {
                        responses: Mutex::new(VecDeque::new()),
                    })
                )
                .is_err()
            );
        });
    }
}
