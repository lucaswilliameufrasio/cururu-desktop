use super::SessionStore;
use crate::action_history::ActionRecord;
use anyhow::ensure;
use rusqlite::params;

pub(super) fn migrate(transaction: &rusqlite::Transaction<'_>) -> anyhow::Result<()> {
    transaction.execute_batch("CREATE TABLE action_history(id TEXT PRIMARY KEY NOT NULL, scope_key TEXT NOT NULL, occurred_at INTEGER NOT NULL, payload TEXT NOT NULL); CREATE INDEX action_history_by_scope ON action_history(scope_key,occurred_at DESC,id); PRAGMA user_version = 10;")?;
    Ok(())
}

impl SessionStore {
    pub fn save_action_receipt(&self, record: &ActionRecord) -> anyhow::Result<String> {
        let id = record.id()?;
        let mut connection = self.connect()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM action_history WHERE id = ?1)",
            [&id],
            |row| row.get(0),
        )?;
        if !exists {
            let count: i64 =
                transaction
                    .query_row("SELECT count(*) FROM action_history", [], |row| row.get(0))?;
            ensure!(
                count < 500,
                "action history is full (500 records); explicitly delete records before saving more"
            );
            transaction.execute(
                "INSERT INTO action_history(id,scope_key,occurred_at,payload) VALUES(?1,?2,?3,?4)",
                params![
                    id,
                    record.scope_key,
                    i64::try_from(record.occurred_at)?,
                    serde_json::to_string(record)?
                ],
            )?;
        }
        transaction.commit()?;
        Ok(id)
    }

    pub fn action_history(
        &self,
        key: &str,
        page: usize,
    ) -> anyhow::Result<Vec<(String, ActionRecord)>> {
        crate::local_review_history::validate_hash(key)?;
        ensure!(page <= 25, "action history page is out of range");
        let connection = self.connect()?;
        let mut statement = connection.prepare("SELECT CASE WHEN length(id) = 64 THEN id ELSE '' END, occurred_at, CASE WHEN length(CAST(payload AS BLOB)) <= 4096 THEN payload ELSE '' END FROM action_history WHERE scope_key = ?1 ORDER BY occurred_at DESC,id LIMIT 20 OFFSET ?2")?;
        let rows = statement.query_map(params![key, i64::try_from(page * 20)?], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut records = Vec::new();
        for row in rows {
            let (id, occurred_at, payload) = row?;
            let record: ActionRecord = serde_json::from_str(&payload)
                .map_err(|_| anyhow::anyhow!("action receipt format is invalid"))?;
            ensure!(
                record.id()? == id
                    && record.scope_key == key
                    && i64::try_from(record.occurred_at)? == occurred_at,
                "action receipt identity does not match its index"
            );
            records.push((id, record));
        }
        Ok(records)
    }

    pub fn delete_action_receipt(&self, key: &str, id: &str) -> anyhow::Result<bool> {
        crate::local_review_history::validate_hash(key)?;
        crate::local_review_history::validate_hash(id)?;
        Ok(self.connect()?.execute(
            "DELETE FROM action_history WHERE scope_key = ?1 AND id = ?2",
            params![key, id],
        )? > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_sqlite_receipts_are_scoped_immutable_idempotent_and_survive_reopening() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("actions.sqlite3");
        let store = SessionStore::open(&path).unwrap();
        let record = crate::action_history::sample();
        let id = store.save_action_receipt(&record).unwrap();
        assert_eq!(store.save_action_receipt(&record).unwrap(), id);
        let reopened = SessionStore::open(&path).unwrap();
        assert_eq!(
            reopened.action_history(&record.scope_key, 0).unwrap()[0].0,
            id
        );
        assert!(
            reopened
                .action_history(&"e".repeat(64), 0)
                .unwrap()
                .is_empty()
        );
        assert!(
            !reopened
                .delete_action_receipt(&"e".repeat(64), &id)
                .unwrap()
        );
        assert!(
            reopened
                .delete_action_receipt(&record.scope_key, &id)
                .unwrap()
        );
        assert!(
            store
                .action_history(&record.scope_key, 0)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn migration_nine_to_ten_preserves_review_annotations_and_drafts() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("migration.sqlite3");
        let store = SessionStore::open(&path).unwrap();
        let record = crate::local_review_history::sample_record();
        let parent = store.save_local_review(&record).unwrap();
        let annotation = store
            .annotate_local_finding(
                &record.repository_key,
                &parent,
                0,
                crate::evaluation_history::HumanAssessment::Agree,
            )
            .unwrap();
        store
            .save_github_drafts(0, &crate::session_store::GitHubDraftQueue::default())
            .unwrap();
        store
            .connect()
            .unwrap()
            .execute_batch("DROP TABLE action_history; PRAGMA user_version = 9;")
            .unwrap();
        let reopened = SessionStore::open(&path).unwrap();
        let rows = reopened
            .local_review_history(&record.repository_key, 0)
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|row| row.id == annotation));
        assert_eq!(reopened.github_drafts().unwrap().0, 1);
        reopened
            .save_action_receipt(&crate::action_history::sample())
            .unwrap();
    }

    #[test]
    fn receipts_are_bounded_and_detect_corruption_without_pruning() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::open(root.path().join("bounded.sqlite3")).unwrap();
        let record = crate::action_history::sample();
        let mut last = record.clone();
        for index in 0..500 {
            last.observation_id = format!("{index:064x}");
            store.save_action_receipt(&last).unwrap();
        }
        assert!(store.save_action_receipt(&last).is_ok());
        assert!(store.save_action_receipt(&record).is_err());
        assert_eq!(
            store.action_history(&record.scope_key, 24).unwrap().len(),
            20
        );
        assert!(
            store
                .action_history(&record.scope_key, 25)
                .unwrap()
                .is_empty()
        );
        store
            .connect()
            .unwrap()
            .execute("UPDATE action_history SET payload = '{}'", [])
            .unwrap();
        assert!(store.action_history(&record.scope_key, 0).is_err());
    }
}
