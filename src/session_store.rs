use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Context as _;
use directories::ProjectDirs;
use rusqlite::{Connection, OptionalExtension, params};

const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSession {
    pub repository_root: PathBuf,
    pub branch: Option<String>,
    pub head_oid: Option<String>,
    pub created_at: i64,
    pub last_opened_at: i64,
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
        transaction
            .commit()
            .context("failed to commit the session database migration")
    }

    fn connect(&self) -> anyhow::Result<Connection> {
        Connection::open(&self.database_path).context("failed to open the local session database")
    }
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
}
