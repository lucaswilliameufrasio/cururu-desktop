//! Skip only an exact archived input identity, without restoring capabilities.

use crate::{
    LocalReviewRequest,
    repository::RepositorySnapshot,
    session_store::{LocalReviewSettings, SessionStore},
};

pub enum Outcome {
    Complete(Box<crate::local_review::LocalReviewResult>),
    Skipped(String),
}

pub fn enabled(snapshot: &RepositorySnapshot) -> bool {
    snapshot.review_policy.as_ref().is_ok_and(|policy| {
        policy.review_reuse == crate::repository_policy::ReviewReuse::ExactArchive
    })
}

pub fn preflight(
    store: &SessionStore,
    request: &LocalReviewRequest,
) -> anyhow::Result<(RepositorySnapshot, Option<String>)> {
    crate::validate_local_review_request(store, request)?;
    let snapshot = RepositorySnapshot::open(&request.repository_root)?
        .with_shared_base(request.base.as_ref())?;
    crate::validate_local_review_snapshot(request, &snapshot)?;
    let incremental = enabled(&snapshot);
    let matching = if incremental && !request.force_recheck {
        matching_record(
            store,
            &snapshot,
            &request.settings,
            request.evidence.as_ref(),
            request.analysis.as_ref(),
        )?
    } else {
        None
    };
    Ok((snapshot, matching))
}

fn matching_record(
    store: &SessionStore,
    snapshot: &RepositorySnapshot,
    settings: &LocalReviewSettings,
    evidence: Option<&crate::local_context::PreparedContext>,
    analysis: Option<&crate::local_analysis::PreparedAnalysis>,
) -> anyhow::Result<Option<String>> {
    let key = crate::local_review_history::repository_key(&snapshot.root)?;
    let config = crate::local_review::config_fingerprint(settings)?;
    let diff = crate::local_review::snapshot_fingerprint(snapshot);
    let context = evidence.map(|context| &context.fingerprint);
    let base = crate::shared_base::fingerprint(snapshot);
    let analysis = analysis.map(|analysis| &analysis.fingerprint);
    for page in 0..10 {
        let rows = store.local_review_history(&key, page)?;
        let exhausted = rows.len() < 20;
        for row in rows {
            let record = row.record;
            if record.artifacts.is_none()
                && record.head_oid == snapshot.head_oid
                && record.diff_fingerprint == diff
                && record.config_fingerprint == config
                && record.context_fingerprint.as_ref() == context
                && record.base_fingerprint == base
                && record.analysis_fingerprint.as_ref() == analysis
            {
                return Ok(Some(row.id));
            }
        }
        if exhausted {
            break;
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_archive_identity_skips_before_any_credential_or_provider_access_and_can_be_forced() {
        let root = tempfile::tempdir().unwrap();
        gix::init(root.path()).unwrap();
        std::fs::write(root.path().join("file.rs"), "changed\n").unwrap();
        std::fs::write(
            root.path().join(".cururu.toml"),
            "version = 1\n[policy]\nincremental = true",
        )
        .unwrap();
        let snapshot = RepositorySnapshot::open(root.path()).unwrap();
        let db = tempfile::tempdir().unwrap();
        let store = SessionStore::open(db.path().join("incremental.sqlite3")).unwrap();
        let settings = LocalReviewSettings {
            openai_endpoint: "https://example.test/v1".to_string(),
            openai_model: "model".to_string(),
        };
        store.save_local_review_settings(&settings).unwrap();
        let config = crate::local_review::config_fingerprint(&settings).unwrap();
        let identity = crate::local_review::repository_identity(root.path()).unwrap();
        store.authorize_local_review(&identity, &config).unwrap();
        let mut request = LocalReviewRequest {
            analysis: None,
            force_recheck: false,
            base: None,
            evidence: None,
            context: crate::LocalReviewRequestContext {
                analysis_fingerprint: None,
                base_fingerprint: None,
                evidence_fingerprint: None,
                request_id: 1,
                repository_identity: identity,
                config_fingerprint: config.clone(),
                snapshot_fingerprint: crate::local_review::snapshot_fingerprint(&snapshot),
            },
            repository_root: root.path().to_path_buf(),
            settings: settings.clone(),
        };
        assert!(preflight(&store, &request).unwrap().1.is_none());
        let mut record = crate::local_review_history::sample_record();
        record.repository_key = crate::local_review_history::repository_key(root.path()).unwrap();
        record
            .diff_fingerprint
            .clone_from(&request.context.snapshot_fingerprint);
        record.config_fingerprint = config;
        let id = store.save_local_review(&record).unwrap();
        assert_eq!(preflight(&store, &request).unwrap().1, Some(id.clone()));
        request.force_recheck = true;
        assert!(preflight(&store, &request).unwrap().1.is_none());
        request.force_recheck = false;
        let mut other_settings = settings;
        other_settings.openai_model = "other-model".to_string();
        assert!(
            matching_record(&store, &snapshot, &other_settings, None, None)
                .unwrap()
                .is_none()
        );
        store
            .delete_local_review(&record.repository_key, &id)
            .unwrap();
        record.context_fingerprint = Some("e".repeat(64));
        store.save_local_review(&record).unwrap();
        assert!(
            matching_record(&store, &snapshot, &request.settings, None, None)
                .unwrap()
                .is_none()
        );
        record.context_fingerprint = None;
        record.base_fingerprint = Some("f".repeat(64));
        store.save_local_review(&record).unwrap();
        assert!(
            matching_record(&store, &snapshot, &request.settings, None, None)
                .unwrap()
                .is_none()
        );
        record.base_fingerprint = None;
        record.analysis_fingerprint = Some("d".repeat(64));
        store.save_local_review(&record).unwrap();
        assert!(
            matching_record(&store, &snapshot, &request.settings, None, None)
                .unwrap()
                .is_none()
        );
        std::fs::write(root.path().join("file.rs"), "user edit\n").unwrap();
        assert!(preflight(&store, &request).is_err());
    }
}
