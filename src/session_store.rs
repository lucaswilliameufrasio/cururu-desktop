use std::{
    fs,
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Context as _;
use directories::ProjectDirs;
use rusqlite::{Connection, OptionalExtension, params};

const SCHEMA_VERSION: i64 = 3;
const MAX_DRAFT_BODY_BYTES: usize = 64_000;

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
    use super::SessionStore;

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
