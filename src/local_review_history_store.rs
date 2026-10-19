//! Real local archive operations, sharing `SessionStore` transactions.

use super::SessionStore;
use crate::local_review_history::{LocalReviewRecord, StoredLocalReview, validate_hash};
use anyhow::ensure;
use rusqlite::params;

pub(super) fn migrate(transaction: &rusqlite::Transaction<'_>) -> anyhow::Result<()> {
    transaction.execute_batch("CREATE TABLE local_review_history(id TEXT PRIMARY KEY NOT NULL, repository_key TEXT NOT NULL, created_at INTEGER NOT NULL, payload TEXT NOT NULL); CREATE INDEX local_review_history_by_repo ON local_review_history(repository_key,created_at DESC,id); PRAGMA user_version = 9;")?;
    Ok(())
}

impl SessionStore {
    pub fn save_local_review(&self, record: &LocalReviewRecord) -> anyhow::Result<String> {
        let id = record.id()?;
        let payload = serde_json::to_string(record)?;
        let mut connection = self.connect()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM local_review_history WHERE id = ?1)",
            [&id],
            |row| row.get(0),
        )?;
        if !exists {
            let count: i64 =
                transaction.query_row("SELECT count(*) FROM local_review_history", [], |row| {
                    row.get(0)
                })?;
            ensure!(
                count < 200,
                "local review history is full (200 records); explicitly delete records before saving more"
            );
            transaction.execute("INSERT INTO local_review_history(id,repository_key,created_at,payload) VALUES(?1,?2,?3,?4)", params![id, record.repository_key, super::unix_timestamp_millis()?, payload])?;
        }
        transaction.commit()?;
        Ok(id)
    }

    pub fn local_review_history(
        &self,
        repository_key: &str,
        page: usize,
    ) -> anyhow::Result<Vec<StoredLocalReview>> {
        validate_hash(repository_key)?;
        ensure!(page <= 10, "local history page is out of range");
        let connection = self.connect()?;
        let mut statement = connection.prepare("SELECT CASE WHEN length(id) = 64 THEN id ELSE '' END,created_at,CASE WHEN length(CAST(payload AS BLOB)) <= 64000 THEN payload ELSE '' END FROM local_review_history WHERE repository_key = ?1 ORDER BY created_at DESC,id LIMIT 20 OFFSET ?2")?;
        let rows =
            statement.query_map(params![repository_key, i64::try_from(page * 20)?], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
        let mut records = Vec::new();
        for row in rows {
            let (id, created_at, payload) = row?;
            let record: LocalReviewRecord = serde_json::from_str(&payload)
                .map_err(|_| anyhow::anyhow!("saved local review format is invalid"))?;
            ensure!(
                record.id()? == id && record.repository_key == repository_key && created_at >= 0,
                "saved local review identity does not match its index"
            );
            records.push(StoredLocalReview {
                id,
                created_at,
                record,
            });
        }
        Ok(records)
    }

    pub fn delete_local_review(&self, repository_key: &str, id: &str) -> anyhow::Result<bool> {
        validate_hash(repository_key)?;
        validate_hash(id)?;
        Ok(self.connect()?.execute(
            "DELETE FROM local_review_history WHERE repository_key = ?1 AND id = ?2",
            params![repository_key, id],
        )? > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_sqlite_local_history_is_immutable_scoped_idempotent_and_survives_reopening() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("local.sqlite3");
        let store = SessionStore::open(&path).unwrap();
        let record = crate::local_review_history::sample_record();
        let id = store.save_local_review(&record).unwrap();
        assert_eq!(store.save_local_review(&record).unwrap(), id);
        let reopened = SessionStore::open(&path).unwrap();
        let rows = reopened
            .local_review_history(&record.repository_key, 0)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].record.id().unwrap(), id);
        assert_eq!(rows[0].record.suppressed.len(), 1);
        assert!(
            reopened
                .local_review_history(&"d".repeat(64), 0)
                .unwrap()
                .is_empty()
        );
        assert!(!reopened.delete_local_review(&"d".repeat(64), &id).unwrap());
        let mut assessed = record.clone();
        assessed.assessment = crate::evaluation_history::HumanAssessment::Agree;
        reopened.save_local_review(&assessed).unwrap();
        assert_eq!(
            store
                .local_review_history(&record.repository_key, 0)
                .unwrap()
                .len(),
            2
        );
        assert!(
            reopened
                .delete_local_review(&record.repository_key, &id)
                .unwrap()
        );
        assert_eq!(
            store
                .local_review_history(&record.repository_key, 0)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn migration_eight_to_nine_preserves_evaluations_preferences_and_queued_drafts() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("migration.sqlite3");
        let store = SessionStore::open(&path).unwrap();
        let evaluation = crate::evaluation_history::sample_record();
        let evaluation_id = store.save_evaluation(&evaluation).unwrap();
        store
            .save_publication_preference(
                crate::publication_preference::PublicationPreference::Individual,
            )
            .unwrap();
        store
            .save_github_drafts(0, &crate::session_store::GitHubDraftQueue::default())
            .unwrap();
        store
            .connect()
            .unwrap()
            .execute_batch("DROP TABLE local_review_history; PRAGMA user_version = 8;")
            .unwrap();
        let migrated = SessionStore::open(&path).unwrap();
        assert_eq!(
            migrated
                .evaluations(&evaluation.anchor.reference, 0)
                .unwrap()[0]
                .id,
            evaluation_id
        );
        assert_eq!(
            migrated.publication_preference().unwrap(),
            crate::publication_preference::PublicationPreference::Individual
        );
        assert_eq!(migrated.github_drafts().unwrap().0, 1);
        let local = crate::local_review_history::sample_record();
        migrated.save_local_review(&local).unwrap();
        assert_eq!(
            migrated
                .local_review_history(&local.repository_key, 0)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn concurrent_archive_writers_cannot_exceed_the_global_record_limit() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::open(root.path().join("concurrent.sqlite3")).unwrap();
        let mut record = crate::local_review_history::sample_record();
        for index in 0..199 {
            record.diff_fingerprint = format!("{index:064x}");
            store.save_local_review(&record).unwrap();
        }
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut writers = Vec::new();
        for index in [199, 200] {
            let store = store.clone();
            let barrier = barrier.clone();
            let mut record = record.clone();
            record.diff_fingerprint = format!("{index:064x}");
            writers.push(std::thread::spawn(move || {
                barrier.wait();
                store.save_local_review(&record).is_ok()
            }));
        }
        barrier.wait();
        let mut successes = 0;
        for writer in writers {
            if writer.join().unwrap() {
                successes += 1;
            }
        }
        assert_eq!(successes, 1);
        let mut count = 0;
        for page in 0..10 {
            count += store
                .local_review_history(&record.repository_key, page)
                .unwrap()
                .len();
        }
        assert_eq!(count, 200);
    }

    #[test]
    fn local_history_rejects_corruption_and_limits_without_silent_pruning() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::open(root.path().join("limits.sqlite3")).unwrap();
        let mut record = crate::local_review_history::sample_record();
        for index in 0..200 {
            record.diff_fingerprint = format!("{index:064x}");
            store.save_local_review(&record).unwrap();
        }
        assert!(store.save_local_review(&record).is_ok());
        record.diff_fingerprint = format!("{:064x}", 200);
        assert!(store.save_local_review(&record).is_err());
        for page in 0..10 {
            assert_eq!(
                store
                    .local_review_history(&record.repository_key, page)
                    .unwrap()
                    .len(),
                20
            );
        }
        assert!(
            store
                .local_review_history(&record.repository_key, 10)
                .unwrap()
                .is_empty()
        );
        store
            .connect()
            .unwrap()
            .execute("UPDATE local_review_history SET payload = '{}'", [])
            .unwrap();
        assert!(
            store
                .local_review_history(&record.repository_key, 0)
                .is_err()
        );
    }
}
