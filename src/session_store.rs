use std::{
    fs,
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Context as _;
use directories::ProjectDirs;
use rusqlite::{Connection, OptionalExtension, params};

#[path = "evaluation_history_store.rs"]
mod evaluation_history_store;

const SCHEMA_VERSION: i64 = 7;
const MAX_DRAFT_BODY_BYTES: usize = 64_000;
const MAX_EVALUATOR_SETTING_BYTES: usize = 2_048;
const MAX_LOCAL_REVIEW_REPOSITORY_IDENTITY_BYTES: usize = 4_096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSession {
    pub repository_root: PathBuf,
    pub branch: Option<String>,
    pub head_oid: Option<String>,
    pub created_at: i64,
    pub last_opened_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredCommentDraft {
    pub id: i64,
    pub draft: cururu_core::ReviewCommentDraft,
    pub base_head_oid: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub revision: i64,
    pub is_stale: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct GitHubDraftQueue {
    pub drafts: Vec<crate::github_pr::PendingReview>,
    pub summary: String,
}

impl GitHubDraftQueue {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.drafts.len() <= 50,
            "saved GitHub queue exceeds 50 comments"
        );
        if !self.summary.is_empty() {
            crate::github_pr::validate_thread_reply_body(&self.summary)?;
        }
        for draft in &self.drafts {
            crate::github_pr::validate_saved_review(draft)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvaluatorSettings {
    pub openai_endpoint: String,
    pub openai_model: String,
    pub jev_model: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalReviewSettings {
    pub openai_endpoint: String,
    pub openai_model: String,
}

impl LocalReviewSettings {
    pub fn validate(&self) -> anyhow::Result<()> {
        let endpoint = url::Url::parse(&self.openai_endpoint)
            .context("local review endpoint must be a valid HTTPS URL")?;
        anyhow::ensure!(
            endpoint.scheme() == "https"
                && endpoint.host_str().is_some()
                && endpoint.username().is_empty()
                && endpoint.password().is_none()
                && endpoint.query().is_none()
                && endpoint.fragment().is_none(),
            "local review endpoint must use HTTPS without credentials, query, or fragment"
        );
        validate_setting_value("local review model", &self.openai_model)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStore {
    database_path: PathBuf,
}

impl SessionStore {
    pub fn open_default() -> anyhow::Result<Self> {
        let directories = ProjectDirs::from("com", "Cururu", "Cururu Desktop")
            .ok_or_else(|| anyhow::anyhow!("could not locate the application data directory"))?;
        let data_directory = directories.data_local_dir();
        fs::create_dir_all(data_directory)
            .context("failed to create the application data directory")?;
        Self::open(data_directory.join("sessions.sqlite3"))
    }

    pub fn open(database_path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let database_path = database_path.as_ref().to_path_buf();
        if let Some(parent) = database_path.parent() {
            fs::create_dir_all(parent)
                .context("failed to create the session database directory")?;
        }
        let store = Self { database_path };
        store.initialize()?;
        Ok(store)
    }

    pub fn most_recent(&self) -> anyhow::Result<Option<StoredSession>> {
        let connection = self.connect()?;
        connection
            .query_row(
                "SELECT repository_root, branch, head_oid, created_at, last_opened_at \
                 FROM local_sessions \
                 ORDER BY last_opened_at DESC, repository_root ASC LIMIT 1",
                [],
                stored_session_from_row,
            )
            .optional()
            .context("failed to read the most recent local session")
    }

    pub fn github_drafts(&self) -> anyhow::Result<(i64, GitHubDraftQueue)> {
        let connection = self.connect()?;
        let record: Option<(i64, String)> = connection.query_row(
            "SELECT revision, payload FROM github_draft_queue WHERE id = 1 AND length(CAST(payload AS BLOB)) <= 1048576",
            [], |row| Ok((row.get(0)?, row.get(1)?))).optional()?;
        let Some((revision, payload)) = record else {
            let count: i64 =
                connection.query_row("SELECT count(*) FROM github_draft_queue", [], |row| {
                    row.get(0)
                })?;
            anyhow::ensure!(
                count == 0,
                "saved GitHub drafts exceed the storage size limit"
            );
            return Ok((0, GitHubDraftQueue::default()));
        };
        let queue: GitHubDraftQueue = serde_json::from_str(&payload)?;
        queue.validate()?;
        Ok((revision, queue))
    }

    pub fn save_github_drafts(
        &self,
        expected_revision: i64,
        queue: &GitHubDraftQueue,
    ) -> anyhow::Result<i64> {
        queue.validate()?;
        let payload = serde_json::to_string(queue)?;
        anyhow::ensure!(
            payload.len() <= 1_048_576,
            "GitHub draft queue exceeds the storage size limit"
        );
        let mut connection = self.connect()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current: i64 = transaction
            .query_row(
                "SELECT revision FROM github_draft_queue WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        anyhow::ensure!(
            current == expected_revision,
            "GitHub drafts changed in another app instance; restart to restore them before saving"
        );
        let revision = current
            .checked_add(1)
            .context("GitHub draft revision overflow")?;
        transaction.execute(
            "INSERT INTO github_draft_queue(id, revision, payload) VALUES (1, ?1, ?2) \
            ON CONFLICT(id) DO UPDATE SET revision = excluded.revision, payload = excluded.payload",
            params![revision, payload],
        )?;
        transaction.commit()?;
        Ok(revision)
    }

    pub fn record_opened(
        &self,
        repository_root: &Path,
        branch: Option<&str>,
        head_oid: Option<&str>,
    ) -> anyhow::Result<StoredSession> {
        let repository_root = repository_root
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("repository path is not UTF-8 and cannot be resumed"))?;
        let now = unix_timestamp()?;
        let connection = self.connect()?;
        connection
            .execute(
                "INSERT INTO local_sessions \
                 (repository_root, branch, head_oid, created_at, last_opened_at) \
                 VALUES (?1, ?2, ?3, ?4, ?4) \
                 ON CONFLICT(repository_root) DO UPDATE SET \
                 branch = excluded.branch, \
                 head_oid = excluded.head_oid, \
                 last_opened_at = excluded.last_opened_at",
                params![repository_root, branch, head_oid, now],
            )
            .context("failed to save the local review session")?;
        connection
            .query_row(
                "SELECT repository_root, branch, head_oid, created_at, last_opened_at \
                 FROM local_sessions WHERE repository_root = ?1",
                [repository_root],
                stored_session_from_row,
            )
            .context("failed to read the saved local review session")
    }

    pub fn save_comment_draft(
        &self,
        repository_root: &Path,
        base_head_oid: Option<&str>,
        current_head_oid: Option<&str>,
        draft: &cururu_core::ReviewCommentDraft,
        expected_revision: Option<i64>,
    ) -> anyhow::Result<StoredCommentDraft> {
        let repository_root = repository_root
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("repository path is not UTF-8 and cannot be resumed"))?;
        validate_draft(draft)?;
        let line =
            i64::from(draft.line.ok_or_else(|| {
                anyhow::anyhow!("a local comment draft requires a new-side line")
            })?);
        let stored_head = base_head_oid.unwrap_or_default();
        let now = unix_timestamp_millis()?;
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .context("failed to begin local comment draft save")?;
        let existing = transaction
            .query_row(
                "SELECT id, revision FROM comment_drafts \
                 WHERE repository_root = ?1 AND path = ?2 AND line = ?3 AND base_head_oid = ?4",
                params![repository_root, draft.path, line, stored_head],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .context("failed to check the existing local comment draft")?;
        if let Some((id, revision)) = existing {
            anyhow::ensure!(
                expected_revision == Some(revision),
                "comment draft changed in another app instance; reload it before saving"
            );
            let changed = transaction
                .execute(
                    "UPDATE comment_drafts SET body = ?1, updated_at = ?2, revision = revision + 1 \
                     WHERE id = ?3 AND revision = ?4",
                    params![draft.body, now, id, revision],
                )
                .context("failed to update the local comment draft")?;
            anyhow::ensure!(
                changed == 1,
                "comment draft changed before it could be saved"
            );
        } else {
            anyhow::ensure!(
                expected_revision.is_none(),
                "comment draft was deleted in another app instance; reload it before saving"
            );
            transaction
                .execute(
                    "INSERT INTO comment_drafts \
                     (repository_root, path, line, base_head_oid, body, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                    params![
                        repository_root,
                        draft.path,
                        line,
                        stored_head,
                        draft.body,
                        now
                    ],
                )
                .context("failed to save the local comment draft")?;
        }
        let saved = transaction
            .query_row(
                "SELECT id, path, line, base_head_oid, body, created_at, updated_at, revision \
                 FROM comment_drafts \
                 WHERE repository_root = ?1 AND path = ?2 AND line = ?3 AND base_head_oid = ?4",
                params![repository_root, draft.path, line, stored_head],
                |row| stored_comment_draft_from_row(row, current_head_oid),
            )
            .context("failed to read the saved local comment draft")?;
        transaction
            .commit()
            .context("failed to commit the local comment draft")?;
        Ok(saved)
    }

    pub fn comment_drafts(
        &self,
        repository_root: &Path,
        current_head_oid: Option<&str>,
    ) -> anyhow::Result<Vec<StoredCommentDraft>> {
        let repository_root = repository_root
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("repository path is not UTF-8 and cannot be resumed"))?;
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                "SELECT id, path, line, base_head_oid, body, created_at, updated_at, revision \
                 FROM comment_drafts WHERE repository_root = ?1 \
                 ORDER BY updated_at DESC, id ASC",
            )
            .context("failed to prepare local comment draft lookup")?;
        let rows = statement
            .query_map([repository_root], |row| {
                let base_head: String = row.get(3)?;
                let mut draft = stored_comment_draft_from_row(row, None)?;
                draft.is_stale = base_head != current_head_oid.unwrap_or_default();
                Ok(draft)
            })
            .context("failed to query local comment drafts")?;
        let mut drafts = Vec::new();
        for row in rows {
            drafts.push(row.context("failed to read a local comment draft")?);
        }
        Ok(drafts)
    }

    pub fn delete_comment_draft(
        &self,
        repository_root: &Path,
        draft_id: i64,
    ) -> anyhow::Result<bool> {
        let repository_root = repository_root
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("repository path is not UTF-8 and cannot be resumed"))?;
        let connection = self.connect()?;
        connection
            .execute(
                "DELETE FROM comment_drafts WHERE repository_root = ?1 AND id = ?2",
                params![repository_root, draft_id],
            )
            .map(|rows_changed| rows_changed > 0)
            .context("failed to delete the local comment draft")
    }

    pub fn evaluator_settings(&self) -> anyhow::Result<Option<EvaluatorSettings>> {
        let connection = self.connect()?;
        connection
            .query_row(
                "SELECT openai_endpoint, openai_model, jev_model \
                 FROM evaluator_settings WHERE id = 1",
                [],
                |row| {
                    Ok(EvaluatorSettings {
                        openai_endpoint: row.get(0)?,
                        openai_model: row.get(1)?,
                        jev_model: row.get(2)?,
                    })
                },
            )
            .optional()
            .context("failed to read local evaluator settings")
    }

    pub fn save_evaluator_settings(&self, settings: &EvaluatorSettings) -> anyhow::Result<()> {
        validate_evaluator_settings(settings)?;
        let connection = self.connect()?;
        connection
            .execute(
                "INSERT INTO evaluator_settings (id, openai_endpoint, openai_model, jev_model) \
                 VALUES (1, ?1, ?2, ?3) \
                 ON CONFLICT(id) DO UPDATE SET \
                 openai_endpoint = excluded.openai_endpoint, \
                 openai_model = excluded.openai_model, \
                 jev_model = excluded.jev_model",
                params![
                    settings.openai_endpoint,
                    settings.openai_model,
                    settings.jev_model
                ],
            )
            .context("failed to save local evaluator settings")?;
        Ok(())
    }

    pub fn evaluator_is_authorized(
        &self,
        repository_identity: &str,
        config_fingerprint: &str,
    ) -> anyhow::Result<bool> {
        let repository_identity = normalize_evaluator_repository_identity(repository_identity)?;
        validate_config_fingerprint(config_fingerprint)?;
        let connection = self.connect()?;
        connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM evaluator_authorizations \
                 WHERE repository_identity = ?1 AND config_fingerprint = ?2)",
                params![repository_identity, config_fingerprint],
                |row| row.get(0),
            )
            .context("failed to check local evaluator authorization")
    }

    pub fn authorize_evaluator(
        &self,
        repository_identity: &str,
        config_fingerprint: &str,
    ) -> anyhow::Result<()> {
        let repository_identity = normalize_evaluator_repository_identity(repository_identity)?;
        validate_config_fingerprint(config_fingerprint)?;
        let now = unix_timestamp()?;
        let connection = self.connect()?;
        connection
            .execute(
                "INSERT INTO evaluator_authorizations \
                 (repository_identity, config_fingerprint, authorized_at) \
                 VALUES (?1, ?2, ?3) \
                 ON CONFLICT(repository_identity) DO UPDATE SET \
                 config_fingerprint = excluded.config_fingerprint, \
                 authorized_at = excluded.authorized_at",
                params![repository_identity, config_fingerprint, now],
            )
            .context("failed to save local evaluator authorization")?;
        Ok(())
    }

    pub fn revoke_evaluator_authorization(
        &self,
        repository_identity: &str,
    ) -> anyhow::Result<bool> {
        let repository_identity = normalize_evaluator_repository_identity(repository_identity)?;
        let connection = self.connect()?;
        connection
            .execute(
                "DELETE FROM evaluator_authorizations WHERE repository_identity = ?1",
                [repository_identity],
            )
            .map(|rows_changed| rows_changed > 0)
            .context("failed to revoke local evaluator authorization")
    }

    pub fn local_review_settings(&self) -> anyhow::Result<Option<LocalReviewSettings>> {
        let connection = self.connect()?;
        connection
            .query_row(
                "SELECT openai_endpoint, openai_model \
                 FROM local_review_settings WHERE id = 1",
                [],
                |row| {
                    Ok(LocalReviewSettings {
                        openai_endpoint: row.get(0)?,
                        openai_model: row.get(1)?,
                    })
                },
            )
            .optional()
            .context("failed to read local review settings")
    }

    pub fn save_local_review_settings(&self, settings: &LocalReviewSettings) -> anyhow::Result<()> {
        settings.validate()?;
        let connection = self.connect()?;
        connection
            .execute(
                "INSERT INTO local_review_settings (id, openai_endpoint, openai_model) \
                 VALUES (1, ?1, ?2) \
                 ON CONFLICT(id) DO UPDATE SET \
                 openai_endpoint = excluded.openai_endpoint, \
                 openai_model = excluded.openai_model",
                params![settings.openai_endpoint, settings.openai_model],
            )
            .context("failed to save local review settings")?;
        Ok(())
    }

    pub fn local_review_is_authorized(
        &self,
        repository_identity: &str,
        config_fingerprint: &str,
    ) -> anyhow::Result<bool> {
        let repository_identity = normalize_local_review_repository_identity(repository_identity)?;
        validate_config_fingerprint(config_fingerprint)?;
        let connection = self.connect()?;
        connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM local_review_authorizations \
                 WHERE repository_identity = ?1 AND config_fingerprint = ?2)",
                params![repository_identity, config_fingerprint],
                |row| row.get(0),
            )
            .context("failed to check local review authorization")
    }

    pub fn authorize_local_review(
        &self,
        repository_identity: &str,
        config_fingerprint: &str,
    ) -> anyhow::Result<()> {
        let repository_identity = normalize_local_review_repository_identity(repository_identity)?;
        validate_config_fingerprint(config_fingerprint)?;
        let now = unix_timestamp()?;
        let connection = self.connect()?;
        connection
            .execute(
                "INSERT INTO local_review_authorizations \
                 (repository_identity, config_fingerprint, authorized_at) \
                 VALUES (?1, ?2, ?3) \
                 ON CONFLICT(repository_identity) DO UPDATE SET \
                 config_fingerprint = excluded.config_fingerprint, \
                 authorized_at = excluded.authorized_at",
                params![repository_identity, config_fingerprint, now],
            )
            .context("failed to save local review authorization")?;
        Ok(())
    }

    pub fn revoke_local_review_authorization(
        &self,
        repository_identity: &str,
    ) -> anyhow::Result<bool> {
        let repository_identity = normalize_local_review_repository_identity(repository_identity)?;
        let connection = self.connect()?;
        connection
            .execute(
                "DELETE FROM local_review_authorizations WHERE repository_identity = ?1",
                [repository_identity],
            )
            .map(|rows_changed| rows_changed > 0)
            .context("failed to revoke local review authorization")
    }

    fn initialize(&self) -> anyhow::Result<()> {
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .context("failed to begin the session database migration transaction")?;
        let version = transaction
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .context("failed to read the session database schema version")?;
        anyhow::ensure!(
            (0..=SCHEMA_VERSION).contains(&version),
            "session database schema version {version} is outside the supported range 0..={SCHEMA_VERSION}"
        );
        if version == 0 {
            transaction
                .execute_batch(
                    "CREATE TABLE local_sessions (\
                         repository_root TEXT PRIMARY KEY NOT NULL,\
                         branch TEXT,\
                         head_oid TEXT,\
                         created_at INTEGER NOT NULL,\
                         last_opened_at INTEGER NOT NULL\
                     );\
                     PRAGMA user_version = 1;",
                )
                .context("failed to apply local session database migration 1")?;
        }
        if version < 2 {
            transaction
                .execute_batch(
                    "CREATE TABLE comment_drafts (\
                         id INTEGER PRIMARY KEY,\
                         repository_root TEXT NOT NULL REFERENCES local_sessions(repository_root) ON DELETE CASCADE,\
                         path TEXT NOT NULL,\
                         line INTEGER NOT NULL CHECK(line > 0),\
                         base_head_oid TEXT NOT NULL,\
                         body TEXT NOT NULL,\
                         created_at INTEGER NOT NULL,\
                         updated_at INTEGER NOT NULL,\
                         UNIQUE(repository_root, path, line, base_head_oid)\
                     );\
                     CREATE INDEX comment_drafts_by_repository_updated \
                         ON comment_drafts(repository_root, updated_at DESC);\
                     PRAGMA user_version = 2;",
                )
                .context("failed to apply local session database migration 2")?;
        }
        if version < 3 {
            transaction
                .execute_batch(
                    "ALTER TABLE comment_drafts ADD COLUMN revision INTEGER NOT NULL DEFAULT 1;\
                     PRAGMA user_version = 3;",
                )
                .context("failed to apply local session database migration 3")?;
        }
        if version < 4 {
            transaction
                .execute_batch(
                    "CREATE TABLE evaluator_settings (\
                         id INTEGER PRIMARY KEY CHECK(id = 1),\
                         openai_endpoint TEXT NOT NULL,\
                         openai_model TEXT NOT NULL,\
                         jev_model TEXT NOT NULL\
                     );\
                     CREATE TABLE evaluator_authorizations (\
                         repository_identity TEXT PRIMARY KEY NOT NULL,\
                         config_fingerprint TEXT NOT NULL,\
                         authorized_at INTEGER NOT NULL\
                     );\
                     PRAGMA user_version = 4;",
                )
                .context("failed to apply local session database migration 4")?;
        }
        if version < 5 {
            transaction
                .execute_batch(
                    "CREATE TABLE local_review_settings (\
                         id INTEGER PRIMARY KEY CHECK(id = 1),\
                         openai_endpoint TEXT NOT NULL,\
                         openai_model TEXT NOT NULL\
                     );\
                     CREATE TABLE local_review_authorizations (\
                         repository_identity TEXT PRIMARY KEY NOT NULL,\
                         config_fingerprint TEXT NOT NULL,\
                         authorized_at INTEGER NOT NULL\
                     );\
                     PRAGMA user_version = 5;",
                )
                .context("failed to apply local session database migration 5")?;
        }
        if version < 6 {
            transaction.execute_batch("CREATE TABLE github_draft_queue (id INTEGER PRIMARY KEY CHECK(id = 1), revision INTEGER NOT NULL CHECK(revision > 0), payload TEXT NOT NULL); PRAGMA user_version = 6;")?;
        }
        if version < 7 {
            evaluation_history_store::migrate(&transaction)?;
        }
        transaction
            .commit()
            .context("failed to commit the session database migration")
    }

    fn connect(&self) -> anyhow::Result<Connection> {
        let connection = Connection::open(&self.database_path)
            .context("failed to open the local session database")?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .context("failed to configure local session database locking")?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .context("failed to enable local session database constraints")?;
        Ok(connection)
    }
}

fn validate_draft(draft: &cururu_core::ReviewCommentDraft) -> anyhow::Result<()> {
    anyhow::ensure!(!draft.path.is_empty(), "draft path must not be empty");
    anyhow::ensure!(
        Path::new(&draft.path)
            .components()
            .all(|component| matches!(component, Component::Normal(_))),
        "draft path must be repository-relative and safe"
    );
    anyhow::ensure!(
        !draft.path.chars().any(char::is_control),
        "draft path must not contain control characters"
    );
    anyhow::ensure!(
        !draft.body.trim().is_empty(),
        "draft body must not be empty"
    );
    anyhow::ensure!(
        draft.body.len() <= MAX_DRAFT_BODY_BYTES,
        "draft body exceeds the 64,000-byte local limit"
    );
    anyhow::ensure!(
        draft.line.is_some_and(|line| line > 0),
        "draft line must be positive"
    );
    Ok(())
}

fn validate_evaluator_settings(settings: &EvaluatorSettings) -> anyhow::Result<()> {
    for (name, value) in [
        (
            "OpenAI-compatible endpoint",
            settings.openai_endpoint.as_str(),
        ),
        ("OpenAI-compatible model", settings.openai_model.as_str()),
        ("Jev model", settings.jev_model.as_str()),
    ] {
        anyhow::ensure!(!value.trim().is_empty(), "{name} must not be empty");
        anyhow::ensure!(
            value.len() <= MAX_EVALUATOR_SETTING_BYTES,
            "{name} exceeds the local settings size limit"
        );
        anyhow::ensure!(
            !value.chars().any(char::is_control),
            "{name} must not contain control characters"
        );
    }
    Ok(())
}

fn validate_setting_value(name: &str, value: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!value.trim().is_empty(), "{name} must not be empty");
    anyhow::ensure!(
        value.len() <= MAX_EVALUATOR_SETTING_BYTES,
        "{name} exceeds the local settings size limit"
    );
    anyhow::ensure!(
        !value.chars().any(char::is_control),
        "{name} must not contain control characters"
    );
    Ok(())
}

fn normalize_evaluator_repository_identity(value: &str) -> anyhow::Result<String> {
    let (owner, repository) = value
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("evaluator authorization requires owner/repository"))?;
    anyhow::ensure!(
        !repository.contains('/')
            && valid_github_repository_segment(owner)
            && valid_github_repository_segment(repository),
        "evaluator authorization requires a safe GitHub owner/repository"
    );
    Ok(format!(
        "{}/{}",
        owner.to_ascii_lowercase(),
        repository.to_ascii_lowercase()
    ))
}

fn normalize_local_review_repository_identity(value: &str) -> anyhow::Result<String> {
    anyhow::ensure!(
        !value.is_empty()
            && value.len() <= MAX_LOCAL_REVIEW_REPOSITORY_IDENTITY_BYTES
            && !value.chars().any(char::is_control)
            && Path::new(value).is_absolute()
            && Path::new(value).components().all(|component| {
                matches!(
                    component,
                    Component::Prefix(_) | Component::RootDir | Component::Normal(_)
                )
            }),
        "local review authorization requires a canonical absolute repository path"
    );
    Ok(value.to_string())
}

fn valid_github_repository_segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
}

fn validate_config_fingerprint(value: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "evaluator configuration fingerprint is invalid"
    );
    Ok(())
}

fn stored_comment_draft_from_row(
    row: &rusqlite::Row<'_>,
    current_head_oid: Option<&str>,
) -> rusqlite::Result<StoredCommentDraft> {
    let path: String = row.get(1)?;
    let line: i64 = row.get(2)?;
    let base_head: String = row.get(3)?;
    let body: String = row.get(4)?;
    Ok(StoredCommentDraft {
        id: row.get(0)?,
        draft: cururu_core::ReviewCommentDraft {
            path,
            line: u32::try_from(line).ok(),
            body,
        },
        base_head_oid: (!base_head.is_empty()).then_some(base_head.clone()),
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
        revision: row.get(7)?,
        is_stale: base_head != current_head_oid.unwrap_or_default(),
    })
}

fn unix_timestamp_millis() -> anyhow::Result<i64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?;
    i64::try_from(duration.as_millis()).context("system timestamp exceeds the supported range")
}

fn stored_session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredSession> {
    let repository_root: String = row.get(0)?;
    Ok(StoredSession {
        repository_root: PathBuf::from(repository_root),
        branch: row.get(1)?,
        head_oid: row.get(2)?,
        created_at: row.get(3)?,
        last_opened_at: row.get(4)?,
    })
}

fn unix_timestamp() -> anyhow::Result<i64> {
    let milliseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_millis();
    i64::try_from(milliseconds).context("system clock timestamp exceeds the supported range")
}

#[cfg(test)]
mod tests {
    use super::{EvaluatorSettings, SessionStore};

    fn draft(path: &str, line: Option<u32>, body: &str) -> cururu_core::ReviewCommentDraft {
        cururu_core::ReviewCommentDraft {
            path: path.to_string(),
            line,
            body: body.to_string(),
        }
    }

    #[test]
    fn local_sessions_round_trip_and_update_last_opened_repository_state() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("sessions.sqlite3");
        let repository_root = directory.path().join("repo");
        let store = SessionStore::open(&database_path).unwrap();

        assert_eq!(store.most_recent().unwrap(), None);

        let created = store
            .record_opened(&repository_root, Some("main"), Some("abc123"))
            .unwrap();
        let loaded = store.most_recent().unwrap().unwrap();
        assert_eq!(loaded.repository_root, repository_root);
        assert_eq!(loaded.branch.as_deref(), Some("main"));
        assert_eq!(loaded.head_oid.as_deref(), Some("abc123"));
        assert_eq!(loaded.created_at, created.created_at);

        store
            .record_opened(&repository_root, Some("feature/local"), Some("def456"))
            .unwrap();
        let updated = store.most_recent().unwrap().unwrap();
        assert_eq!(updated.branch.as_deref(), Some("feature/local"));
        assert_eq!(updated.head_oid.as_deref(), Some("def456"));
        assert_eq!(updated.created_at, created.created_at);
        assert!(updated.last_opened_at >= created.last_opened_at);
    }

    #[test]
    fn opening_a_database_with_a_newer_schema_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("newer.sqlite3");
        let connection = rusqlite::Connection::open(&database_path).unwrap();
        connection.pragma_update(None, "user_version", 999).unwrap();
        drop(connection);

        let result = SessionStore::open(&database_path);

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("outside the supported range")
        );
    }

    #[test]
    fn evaluator_settings_round_trip_without_storing_provider_secrets() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::open(directory.path().join("settings.sqlite3")).unwrap();
        assert_eq!(store.evaluator_settings().unwrap(), None);

        let settings = EvaluatorSettings {
            openai_endpoint: "https://api.example.test/v1/chat/completions".to_string(),
            openai_model: "configured-model".to_string(),
            jev_model: "configured-jev-model".to_string(),
        };
        store.save_evaluator_settings(&settings).unwrap();

        assert_eq!(store.evaluator_settings().unwrap(), Some(settings));
    }

    #[test]
    fn local_review_settings_round_trip_without_storing_provider_secrets() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::open(directory.path().join("local-review.sqlite3")).unwrap();
        assert_eq!(store.local_review_settings().unwrap(), None);

        let settings = super::LocalReviewSettings {
            openai_endpoint: "https://api.example.test/v1".to_string(),
            openai_model: "review-model".to_string(),
        };
        store.save_local_review_settings(&settings).unwrap();

        assert_eq!(store.local_review_settings().unwrap(), Some(settings));
    }

    #[test]
    fn local_review_authorization_is_scoped_to_exact_checkout_and_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::open(directory.path().join("local-review-auth.sqlite3")).unwrap();
        let first_repository = directory.path().join("repo").to_string_lossy().into_owned();
        let second_repository = directory
            .path()
            .join("repo-clone")
            .to_string_lossy()
            .into_owned();
        let fingerprint = "a".repeat(64);

        assert!(
            !store
                .local_review_is_authorized(&first_repository, &fingerprint)
                .unwrap()
        );
        store
            .authorize_local_review(&first_repository, &fingerprint)
            .unwrap();

        assert!(
            store
                .local_review_is_authorized(&first_repository, &fingerprint)
                .unwrap()
        );
        assert!(
            !store
                .local_review_is_authorized(&second_repository, &fingerprint)
                .unwrap()
        );
        assert!(
            !store
                .local_review_is_authorized(&first_repository, &"b".repeat(64))
                .unwrap()
        );
        assert!(
            store
                .revoke_local_review_authorization(&first_repository)
                .unwrap()
        );
        assert!(
            !store
                .local_review_is_authorized(&first_repository, &fingerprint)
                .unwrap()
        );
    }

    #[test]
    fn local_review_settings_and_authorizations_reject_unsafe_values() {
        let directory = tempfile::tempdir().unwrap();
        let store =
            SessionStore::open(directory.path().join("local-review-invalid.sqlite3")).unwrap();
        for endpoint in [
            "http://api.example.test/v1",
            "https://user:password@api.example.test/v1",
            "https://api.example.test/v1?token=unsafe",
        ] {
            let settings = super::LocalReviewSettings {
                openai_endpoint: endpoint.to_string(),
                openai_model: "review-model".to_string(),
            };
            assert!(store.save_local_review_settings(&settings).is_err());
        }
        assert!(
            store
                .authorize_local_review("relative/repo", &"a".repeat(64))
                .is_err()
        );
        assert!(
            store
                .authorize_local_review("/canonical/repo", "invalid")
                .is_err()
        );
    }

    #[test]
    fn opening_a_version_four_database_adds_local_review_and_github_draft_tables() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("version-four.sqlite3");
        let connection = rusqlite::Connection::open(&database_path).unwrap();
        connection.pragma_update(None, "user_version", 4).unwrap();
        drop(connection);

        let store = SessionStore::open(&database_path).unwrap();

        assert_eq!(store.local_review_settings().unwrap(), None);
        assert_eq!(
            rusqlite::Connection::open(database_path)
                .unwrap()
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            super::SCHEMA_VERSION
        );
    }

    fn github_queue() -> super::GitHubDraftQueue {
        super::GitHubDraftQueue {
            summary: "My review summary".to_string(),
            drafts: vec![crate::github_pr::PendingReview {
                reference: crate::github_pr::PullRequestRef::parse("octo/repo#42").unwrap(),
                base_sha: "b".repeat(40),
                head_sha: "a".repeat(40),
                body: String::new(),
                comments: vec![crate::github_pr::NewReviewComment {
                    path: "src/lib.rs".to_string(),
                    line: 17,
                    body: "Please check this anchor".to_string(),
                }],
            }],
        }
    }

    #[test]
    fn github_drafts_round_trip_across_restarts_and_reject_stale_writers() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("github-drafts.sqlite3");
        let first = SessionStore::open(&path).unwrap();
        let second = SessionStore::open(&path).unwrap();
        assert_eq!(
            first.github_drafts().unwrap(),
            (0, super::GitHubDraftQueue::default())
        );
        let queue = github_queue();
        assert_eq!(first.save_github_drafts(0, &queue).unwrap(), 1);
        assert!(
            second
                .save_github_drafts(0, &super::GitHubDraftQueue::default())
                .is_err()
        );
        drop(first);
        let reopened = SessionStore::open(&path).unwrap();
        assert_eq!(reopened.github_drafts().unwrap(), (1, queue));
        assert_eq!(
            reopened
                .save_github_drafts(1, &super::GitHubDraftQueue::default())
                .unwrap(),
            2
        );
        assert_eq!(
            second.github_drafts().unwrap(),
            (2, super::GitHubDraftQueue::default())
        );
    }

    #[test]
    fn github_draft_migration_preserves_local_drafts_and_rejects_invalid_records() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("migration.sqlite3");
        let store = SessionStore::open(&path).unwrap();
        let repo = directory.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        store
            .record_opened(&repo, Some("feature/test"), Some("head"))
            .unwrap();
        let draft = cururu_core::ReviewCommentDraft {
            path: "src/lib.rs".to_string(),
            line: Some(1),
            body: "Existing local draft".to_string(),
        };
        store
            .save_comment_draft(&repo, Some("head"), Some("head"), &draft, None)
            .unwrap();
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch("DROP TABLE evaluation_history; DROP TABLE github_draft_queue; PRAGMA user_version = 5;")
            .unwrap();
        let migrated = SessionStore::open(&path).unwrap();
        assert_eq!(
            migrated.most_recent().unwrap().unwrap().head_oid.as_deref(),
            Some("head")
        );
        assert_eq!(
            migrated.comment_drafts(&repo, Some("head")).unwrap()[0].draft,
            draft
        );
        let mut invalid = github_queue();
        invalid.drafts[0].comments[0].path = "../credential".to_string();
        assert!(migrated.save_github_drafts(0, &invalid).is_err());
        let payload = serde_json::to_string(&invalid).unwrap();
        connection
            .execute(
                "INSERT INTO github_draft_queue(id,revision,payload) VALUES(1,1,?1)",
                [payload],
            )
            .unwrap();
        assert!(migrated.github_drafts().is_err());
        let count: i64 = connection
            .query_row("SELECT count(*) FROM github_draft_queue", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            count, 1,
            "invalid persisted data must not be silently deleted"
        );
    }

    #[test]
    fn evaluator_authorization_is_exact_repository_and_configuration_scoped() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::open(directory.path().join("authorization.sqlite3")).unwrap();
        let fingerprint = "a".repeat(64);

        assert!(
            !store
                .evaluator_is_authorized("Owner/Repository", &fingerprint)
                .unwrap()
        );
        store
            .authorize_evaluator("Owner/Repository", &fingerprint)
            .unwrap();
        assert!(
            store
                .evaluator_is_authorized("owner/repository", &fingerprint)
                .unwrap()
        );
        assert!(
            !store
                .evaluator_is_authorized("owner/another", &fingerprint)
                .unwrap()
        );
        assert!(
            !store
                .evaluator_is_authorized("owner/repository", &"b".repeat(64))
                .unwrap()
        );
        assert!(
            store
                .revoke_evaluator_authorization("OWNER/REPOSITORY")
                .unwrap()
        );
        assert!(
            !store
                .evaluator_is_authorized("owner/repository", &fingerprint)
                .unwrap()
        );
    }

    #[test]
    fn evaluator_settings_and_authorizations_reject_malformed_values() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::open(directory.path().join("invalid.sqlite3")).unwrap();
        let invalid_settings = EvaluatorSettings {
            openai_endpoint: "https://api.example.test/\nAuthorization: leak".to_string(),
            openai_model: "model".to_string(),
            jev_model: "model".to_string(),
        };
        assert!(store.save_evaluator_settings(&invalid_settings).is_err());
        assert!(
            store
                .authorize_evaluator("owner/../repo", &"a".repeat(64))
                .is_err()
        );
        assert!(
            store
                .authorize_evaluator("owner/repo", "bad-fingerprint")
                .is_err()
        );
    }

    #[test]
    fn opening_a_version_one_database_migrates_it_to_comment_drafts() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("legacy.sqlite3");
        let connection = rusqlite::Connection::open(&database_path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE local_sessions (\
                     repository_root TEXT PRIMARY KEY NOT NULL,\
                     branch TEXT,\
                     head_oid TEXT,\
                     created_at INTEGER NOT NULL,\
                     last_opened_at INTEGER NOT NULL\
                 );\
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        drop(connection);

        let store = SessionStore::open(&database_path).unwrap();
        let repository_root = directory.path().join("repo");
        store
            .record_opened(&repository_root, Some("main"), Some("head"))
            .unwrap();
        let saved = store
            .save_comment_draft(
                &repository_root,
                Some("head"),
                Some("head"),
                &draft("src/lib.rs", Some(7), "Legacy schema migration works."),
                None,
            )
            .unwrap();

        assert_eq!(saved.draft.line, Some(7));
        assert!(!saved.is_stale);
    }

    #[cfg(unix)]
    #[test]
    fn paths_that_cannot_be_represented_as_utf8_are_not_saved_as_lossy_paths() {
        use std::os::unix::ffi::OsStringExt;

        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::open(directory.path().join("sessions.sqlite3")).unwrap();
        let invalid_path = directory
            .path()
            .join(std::ffi::OsString::from_vec(vec![b'r', b'e', b'p', 0xff]));

        let result = store.record_opened(&invalid_path, None, None);

        assert!(result.is_err());
    }

    #[test]
    fn comment_drafts_round_trip_update_and_become_stale_without_being_discarded() {
        let directory = tempfile::tempdir().unwrap();
        let repository_root = directory.path().join("repo");
        let store = SessionStore::open(directory.path().join("sessions.sqlite3")).unwrap();
        store
            .record_opened(&repository_root, Some("main"), Some("base-1"))
            .unwrap();

        let first = store
            .save_comment_draft(
                &repository_root,
                Some("base-1"),
                Some("base-1"),
                &draft("src/lib.rs", Some(12), "Please handle this case."),
                None,
            )
            .unwrap();
        assert!(!first.is_stale);
        assert_eq!(first.draft.body, "Please handle this case.");

        let updated = store
            .save_comment_draft(
                &repository_root,
                Some("base-1"),
                Some("base-1"),
                &draft("src/lib.rs", Some(12), "Please handle this case safely."),
                Some(first.revision),
            )
            .unwrap();
        assert_eq!(updated.id, first.id);
        assert_eq!(updated.revision, first.revision + 1);
        assert_eq!(updated.created_at, first.created_at);
        assert_eq!(updated.draft.body, "Please handle this case safely.");

        let stale = store
            .comment_drafts(&repository_root, Some("base-2"))
            .unwrap();
        assert_eq!(stale.len(), 1);
        assert!(stale[0].is_stale);
        assert_eq!(stale[0].draft.line, Some(12));
        assert_eq!(stale[0].draft.body, "Please handle this case safely.");

        let stale_edit = store
            .save_comment_draft(
                &repository_root,
                Some("base-1"),
                Some("base-2"),
                &draft("src/lib.rs", Some(12), "Updated without reanchoring."),
                Some(stale[0].revision),
            )
            .unwrap();
        assert!(stale_edit.is_stale);
        assert_eq!(stale_edit.base_head_oid.as_deref(), Some("base-1"));

        assert!(
            store
                .delete_comment_draft(&repository_root, first.id)
                .unwrap()
        );
        assert_eq!(
            store
                .comment_drafts(&repository_root, Some("base-1"))
                .unwrap(),
            []
        );
    }

    #[test]
    fn comment_drafts_reject_unsafe_paths_missing_lines_and_empty_bodies() {
        let directory = tempfile::tempdir().unwrap();
        let repository_root = directory.path().join("repo");
        let store = SessionStore::open(directory.path().join("sessions.sqlite3")).unwrap();
        store.record_opened(&repository_root, None, None).unwrap();

        for invalid in [
            draft("../outside.rs", Some(1), "comment"),
            draft("src/lib.rs", None, "comment"),
            draft("src/lib.rs", Some(1), " \n "),
        ] {
            assert!(
                store
                    .save_comment_draft(&repository_root, None, None, &invalid, None)
                    .is_err()
            );
        }
    }

    #[test]
    fn concurrent_draft_edits_fail_instead_of_overwriting_newer_content() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("sessions.sqlite3");
        let repository_root = directory.path().join("repo");
        let first_store = SessionStore::open(&database_path).unwrap();
        let second_store = SessionStore::open(&database_path).unwrap();
        first_store
            .record_opened(&repository_root, Some("main"), Some("head"))
            .unwrap();
        let original = first_store
            .save_comment_draft(
                &repository_root,
                Some("head"),
                Some("head"),
                &draft("src/lib.rs", Some(3), "Original draft"),
                None,
            )
            .unwrap();

        let second_instance_copy = second_store
            .comment_drafts(&repository_root, Some("head"))
            .unwrap()
            .remove(0);
        let updated = second_store
            .save_comment_draft(
                &repository_root,
                Some("head"),
                Some("head"),
                &draft("src/lib.rs", Some(3), "Newer draft"),
                Some(second_instance_copy.revision),
            )
            .unwrap();
        assert_eq!(updated.revision, original.revision + 1);

        let conflict = first_store.save_comment_draft(
            &repository_root,
            Some("head"),
            Some("head"),
            &draft("src/lib.rs", Some(3), "Stale instance edit"),
            Some(original.revision),
        );
        assert!(conflict.is_err());
        assert_eq!(
            first_store
                .comment_drafts(&repository_root, Some("head"))
                .unwrap()[0]
                .draft
                .body,
            "Newer draft"
        );
    }
}
