//! Storage implementation is a child of `session_store`, sharing its connection.

use super::SessionStore;
use crate::evaluation_history::{EvaluationRecord, StoredEvaluation};
use anyhow::{Context as _, ensure};
use rusqlite::params;

pub(super) fn migrate(transaction: &rusqlite::Transaction<'_>) -> anyhow::Result<()> {
    transaction.execute_batch("CREATE TABLE evaluation_history (id TEXT PRIMARY KEY NOT NULL, repository_key TEXT NOT NULL, pull_number INTEGER NOT NULL, created_at INTEGER NOT NULL, payload TEXT NOT NULL); CREATE INDEX evaluation_history_by_pr ON evaluation_history(repository_key,pull_number,created_at DESC,id); PRAGMA user_version = 7;")?;
    Ok(())
}

impl SessionStore {
    pub fn save_evaluation(&self, record: &EvaluationRecord) -> anyhow::Result<String> {
        let id = record.id()?;
        let payload = serde_json::to_string(record)?;
        let mut connection = self.connect()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM evaluation_history WHERE id = ?1)",
            [&id],
            |row| row.get(0),
        )?;
        if !exists {
            let count: i64 =
                transaction.query_row("SELECT count(*) FROM evaluation_history", [], |row| {
                    row.get(0)
                })?;
            ensure!(
                count < 200,
                "evaluation archive is full (200 records); explicitly delete records before saving more"
            );
            transaction.execute("INSERT INTO evaluation_history(id,repository_key,pull_number,created_at,payload) VALUES(?1,?2,?3,?4,?5)",
                params![id, record.anchor.repository_key(), i64::try_from(record.anchor.reference.number)?, super::unix_timestamp_millis()?, payload])?;
        }
        transaction.commit()?;
        Ok(id)
    }

    pub fn evaluations(
        &self,
        reference: &crate::github_pr::PullRequestRef,
        page: usize,
    ) -> anyhow::Result<Vec<StoredEvaluation>> {
        ensure!(page <= 10, "archive page is out of range");
        let key = format!("{}/{}", reference.owner, reference.repository).to_ascii_lowercase();
        let connection = self.connect()?;
        let mut statement = connection.prepare("SELECT id,created_at,CASE WHEN length(CAST(payload AS BLOB)) <= 64000 THEN payload ELSE '' END FROM evaluation_history WHERE repository_key = ?1 AND pull_number = ?2 ORDER BY created_at DESC,id LIMIT 20 OFFSET ?3")?;
        let rows = statement.query_map(
            params![
                key,
                i64::try_from(reference.number)?,
                i64::try_from(page * 20)?
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?;
        let mut records = Vec::new();
        for row in rows {
            let (id, created_at, payload) = row?;
            let record: EvaluationRecord =
                serde_json::from_str(&payload).context("saved evaluation format is invalid")?;
            ensure!(
                record.id()? == id
                    && record.anchor.repository_key() == key
                    && record.anchor.reference.number == reference.number,
                "saved evaluation identity does not match its index"
            );
            records.push(StoredEvaluation {
                id,
                created_at,
                record,
            });
        }
        Ok(records)
    }

    pub fn delete_evaluation(
        &self,
        reference: &crate::github_pr::PullRequestRef,
        id: &str,
    ) -> anyhow::Result<bool> {
        ensure!(
            id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "archive id is invalid"
        );
        let connection = self.connect()?;
        let key = format!("{}/{}", reference.owner, reference.repository).to_ascii_lowercase();
        Ok(connection.execute("DELETE FROM evaluation_history WHERE id = ?1 AND repository_key = ?2 AND pull_number = ?3", params![id,key,i64::try_from(reference.number)?])? > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_six_migration_preserves_existing_queue_and_settings() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("migration.sqlite3");
        let store = SessionStore::open(&path).unwrap();
        let settings = crate::session_store::EvaluatorSettings {
            openai_endpoint: "https://api.openai.com/v1/chat/completions".to_string(),
            openai_model: "judge".to_string(),
            jev_model: "jev".to_string(),
        };
        store.save_evaluator_settings(&settings).unwrap();
        store
            .save_github_drafts(0, &crate::session_store::GitHubDraftQueue::default())
            .unwrap();
        let before = store.github_drafts().unwrap();
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch("DROP TABLE publication_preference; DROP TABLE evaluation_history; PRAGMA user_version = 6;")
            .unwrap();
        let migrated = SessionStore::open(&path).unwrap();
        assert_eq!(migrated.evaluator_settings().unwrap(), Some(settings));
        assert_eq!(migrated.github_drafts().unwrap(), before);
        let record = crate::evaluation_history::sample_record();
        migrated.save_evaluation(&record).unwrap();
        assert_eq!(
            migrated.evaluations(&record.anchor.reference, 0).unwrap()[0].record,
            record
        );
    }

    #[test]
    fn corrupt_archive_and_invalid_probabilities_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::open(root.path().join("invalid.sqlite3")).unwrap();
        let mut record = crate::evaluation_history::sample_record();
        record.analysis.jev.defect_probability = f32::NAN;
        assert!(store.save_evaluation(&record).is_err());
        record.analysis.jev.defect_probability = 1.1;
        assert!(store.save_evaluation(&record).is_err());
        record.analysis.jev.defect_probability = 0.5;
        store.save_evaluation(&record).unwrap();
        store
            .connect()
            .unwrap()
            .execute("UPDATE evaluation_history SET payload = '{}';", [])
            .unwrap();
        assert!(store.evaluations(&record.anchor.reference, 0).is_err());
    }

    #[test]
    fn real_sqlite_archive_is_immutable_idempotent_scoped_and_explicitly_deleted() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("history.sqlite3");
        let first = SessionStore::open(&path).unwrap();
        let record = crate::evaluation_history::sample_record();
        let id = first.save_evaluation(&record).unwrap();
        assert_eq!(first.save_evaluation(&record).unwrap(), id);
        let reopened = SessionStore::open(&path).unwrap();
        let rows = reopened.evaluations(&record.anchor.reference, 0).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].record, record);
        let mut other = record.anchor.reference.clone();
        other.number = 2;
        assert_eq!(reopened.evaluations(&other, 0).unwrap().len(), 0);
        assert!(!reopened.delete_evaluation(&other, &id).unwrap());
        assert!(
            reopened
                .delete_evaluation(&record.anchor.reference, &id)
                .unwrap()
        );
        assert_eq!(
            first
                .evaluations(&record.anchor.reference, 0)
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn archive_is_bounded_and_never_silently_prunes_or_truncates() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::open(root.path().join("history.sqlite3")).unwrap();
        let mut record = crate::evaluation_history::sample_record();
        record.analysis.explanation.explanation = "x".repeat(64_001);
        assert!(store.save_evaluation(&record).is_err());
        record.analysis.explanation.explanation = "bounded".to_string();
        for id in 1..=200 {
            record.anchor.comment_id = id;
            store.save_evaluation(&record).unwrap();
        }
        record.anchor.comment_id = 201;
        assert!(store.save_evaluation(&record).is_err());
        assert_eq!(
            store
                .evaluations(&record.anchor.reference, 0)
                .unwrap()
                .len(),
            20
        );
        assert_eq!(
            store
                .evaluations(&record.anchor.reference, 9)
                .unwrap()
                .len(),
            20
        );
        assert_eq!(
            store
                .evaluations(&record.anchor.reference, 10)
                .unwrap()
                .len(),
            0
        );
    }
}
