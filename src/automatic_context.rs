//! Host-owned HEAD metadata discovery and separately authorized blob reads.

use crate::{
    local_context::{ContextPath, ContextSource},
    repository::RepositorySnapshot,
};
use anyhow::{Context as _, ensure};
use cururu_engine::{
    auto_context::{AutoContextConfig, CandidateSelector},
    config::AutoContextToml,
};

pub fn settings(value: AutoContextToml) -> anyhow::Result<AutoContextConfig> {
    let defaults = AutoContextConfig::default();
    let settings = AutoContextConfig {
        enabled: value.enabled.unwrap_or(defaults.enabled),
        max_bytes: value
            .max_bytes
            .unwrap_or(defaults.max_bytes)
            .min(cururu_engine::review_context::MAX_CONTEXT_BYTES),
        max_files: value
            .max_files
            .unwrap_or(defaults.max_files)
            .min(cururu_engine::review_context::MAX_CONTEXT_FILES),
        per_file_bytes: value
            .per_file_bytes
            .unwrap_or(defaults.per_file_bytes)
            .min(cururu_engine::review_context::MAX_CONTEXT_FILE_BYTES),
        include: value.include.unwrap_or(defaults.include),
        exclude: value.exclude.unwrap_or(defaults.exclude),
    };
    ensure!(
        settings.max_bytes > 0 && settings.max_files > 0 && settings.per_file_bytes > 0,
        "automatic context budgets must be positive"
    );
    ensure!(
        settings.include.len() <= 32
            && settings.exclude.len() <= 32
            && settings
                .include
                .iter()
                .chain(&settings.exclude)
                .all(|pattern| pattern.len() <= 1_024),
        "automatic context patterns exceed the bounded declaration limit"
    );
    CandidateSelector::new(&settings)
        .map_err(|_| anyhow::anyhow!("automatic context patterns are invalid"))?;
    Ok(settings)
}

pub fn append_candidates(
    snapshot: &RepositorySnapshot,
    paths: &mut Vec<ContextPath>,
) -> anyhow::Result<()> {
    let policy = snapshot
        .review_policy
        .as_ref()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let auto = &policy.context.auto;
    if !auto.enabled {
        return Ok(());
    }
    ensure!(
        paths.len() <= auto.max_files,
        "declared context exceeds automatic context file budget"
    );
    let selector = CandidateSelector::new(auto)?;
    let Some(commit) = &snapshot.head_oid else {
        return Ok(());
    };
    let repository = gix::open(&snapshot.root)?;
    ensure!(
        repository.head_id()?.to_string() == *commit,
        "HEAD changed before automatic context discovery"
    );
    let tree = repository
        .head_tree_id_or_empty()?
        .object()?
        .try_into_tree()?;
    for changed in &snapshot.changed_files {
        // Evaluate matching/duplicate rules independently of capacity to reject
        // excess explicitly instead of silently dropping candidates.
        if !selector.accepts(
            &changed.path,
            paths.iter().map(|entry| entry.path.as_str()),
            0,
            0,
        ) {
            continue;
        }
        crate::context_discovery::validate_path(&changed.path)?;
        let Some(entry) = tree.lookup_entry_by_path(&changed.path)? else {
            continue;
        };
        ensure!(
            entry.mode().is_blob() && !entry.mode().is_link(),
            "automatic context candidate is not a regular HEAD file"
        );
        ensure!(
            selector.accepts(
                &changed.path,
                paths.iter().map(|entry| entry.path.as_str()),
                paths.len(),
                0
            ),
            "automatic context candidates exceed the file budget; narrow patterns or choose diff-only"
        );
        paths.push(ContextPath {
            label: format!("Automatic HEAD context @ {commit}"),
            path: changed.path.clone(),
            source: ContextSource::Head {
                commit: commit.clone(),
                blob: entry.object_id().to_string(),
            },
        });
    }
    Ok(())
}

pub fn read_head(
    snapshot: &RepositorySnapshot,
    path: &str,
    commit: &str,
    blob: &str,
    limit: usize,
) -> anyhow::Result<Vec<u8>> {
    crate::context_discovery::validate_path(path)?;
    ensure!(
        snapshot.head_oid.as_deref() == Some(commit),
        "automatic context revision no longer matches snapshot"
    );
    let repository = gix::open(&snapshot.root)?;
    ensure!(
        repository.head_id()?.to_string() == commit,
        "HEAD changed before automatic context read"
    );
    let tree = repository
        .head_tree_id_or_empty()?
        .object()?
        .try_into_tree()?;
    let entry = tree
        .lookup_entry_by_path(path)?
        .context("automatic context path is absent at HEAD")?;
    ensure!(
        entry.mode().is_blob() && !entry.mode().is_link() && entry.object_id().to_string() == blob,
        "automatic context object no longer matches the approved manifest"
    );
    let id = entry.object_id();
    let header = repository.find_header(id)?;
    ensure!(
        header.size() <= u64::try_from(limit)?,
        "automatic HEAD context exceeds its per-file or aggregate budget; nothing was truncated"
    );
    let object = repository.find_blob(id)?;
    ensure!(
        object.data.len() <= limit,
        "automatic HEAD context exceeds its approved budget"
    );
    Ok(object.data.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gix::bstr::ByteSlice as _;

    fn fixture(content: &[u8], auto: &str) -> (tempfile::TempDir, RepositorySnapshot) {
        let root = tempfile::tempdir().unwrap();
        let repo = gix::init(root.path()).unwrap();
        let blob = repo.write_blob(content).unwrap().detach();
        let tree = repo
            .write_object(&gix::objs::Tree {
                entries: vec![gix::objs::tree::Entry {
                    mode: (0o100_644).try_into().unwrap(),
                    filename: "tracked.rs".into(),
                    oid: blob,
                }],
            })
            .unwrap()
            .detach();
        let actor = gix::actor::SignatureRef {
            name: b"Test".as_bstr(),
            email: b"test@example.test".as_bstr(),
            time: "1 +0000",
        };
        repo.commit_as(
            actor,
            actor,
            "refs/heads/main",
            "base",
            tree,
            std::iter::empty::<gix::ObjectId>(),
        )
        .unwrap();
        let mut index = repo.index_from_tree(tree.as_ref()).unwrap();
        index.write(gix::index::write::Options::default()).unwrap();
        std::fs::write(root.path().join("tracked.rs"), "working tree version\n").unwrap();
        std::fs::write(root.path().join("new.rs"), "no HEAD version\n").unwrap();
        std::fs::write(
            root.path().join(".cururu.toml"),
            format!("version = 1\n[context.auto]\nenabled = true\ninclude = ['*.rs']\n{auto}"),
        )
        .unwrap();
        let snapshot = RepositorySnapshot::open(root.path()).unwrap();
        (root, snapshot)
    }

    #[test]
    fn automatic_context_proposes_names_then_loads_only_head_bytes_and_preserves_choice() {
        let (_root, snapshot) = fixture(b"committed version\n", "");
        let manifest = crate::context_discovery::discover(&snapshot).unwrap();
        assert_eq!(manifest.paths.len(), 1);
        assert_eq!(manifest.paths[0].path, "tracked.rs");
        assert!(
            matches!(&manifest.paths[0].source, ContextSource::Head { commit, .. } if Some(commit) == snapshot.head_oid.as_ref())
        );
        assert!(
            crate::local_context_ui::ContextState::Undecided
                .resolve(&snapshot)
                .is_err()
        );
        assert!(
            crate::local_context_ui::ContextState::Resolved(manifest.clone())
                .resolve(&snapshot)
                .is_err()
        );
        let context = crate::local_context::load_manifest(&snapshot, &manifest).unwrap();
        assert_eq!(context.evidence.files[0].content, "committed version\n");
        assert!(
            context.evidence.files[0]
                .label
                .contains(snapshot.head_oid.as_ref().unwrap())
        );
        crate::local_context::validate(&snapshot, Some(&context)).unwrap();
        assert!(
            crate::local_context_ui::ContextState::Preview(context.clone())
                .resolve(&snapshot)
                .is_err()
        );
        assert!(
            crate::local_context_ui::ContextState::Included(context)
                .resolve(&snapshot)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn automatic_limits_fail_only_when_explicitly_loading_not_discovering() {
        for (content, auto) in [(vec![b'x'; 13_000], ""), (vec![b'x'; 12], "max_bytes = 10")] {
            let (_root, snapshot) = fixture(&content, auto);
            let manifest = crate::context_discovery::discover(&snapshot).unwrap();
            assert_eq!(manifest.paths.len(), 1);
            assert!(crate::local_context::load_manifest(&snapshot, &manifest).is_err());
        }
    }

    #[test]
    fn declared_paths_win_over_automatic_candidates_and_count_toward_automatic_limits() {
        let (root, _) = fixture(b"committed\n", "max_files = 1");
        std::fs::write(root.path().join(".cururu.toml"), "version = 1\n[context]\nadditional = ['tracked.rs']\n[context.auto]\nenabled = true\ninclude = ['*.rs']\nmax_files = 1").unwrap();
        let snapshot = RepositorySnapshot::open(root.path()).unwrap();
        let manifest = crate::context_discovery::discover(&snapshot).unwrap();
        assert_eq!(manifest.paths.len(), 1);
        assert_eq!(manifest.paths[0].source, ContextSource::Worktree);
        assert_eq!(
            crate::local_context::load_manifest(&snapshot, &manifest)
                .unwrap()
                .evidence
                .files[0]
                .content,
            "working tree version\n"
        );
        std::fs::write(root.path().join(".cururu.toml"), "version = 1\n[context]\nadditional = ['new.rs']\n[context.auto]\nenabled = true\ninclude = ['*.rs']\nmax_files = 1").unwrap();
        assert!(
            crate::context_discovery::discover(&RepositorySnapshot::open(root.path()).unwrap())
                .is_err()
        );
    }

    #[test]
    fn wrong_revision_or_object_never_falls_back_to_the_working_file() {
        let (_root, snapshot) = fixture(b"committed\n", "");
        let mut manifest = crate::context_discovery::discover(&snapshot).unwrap();
        let ContextSource::Head { commit, blob } = &manifest.paths[0].source else {
            panic!("HEAD source expected");
        };
        assert!(read_head(&snapshot, "tracked.rs", &"a".repeat(40), blob, 100).is_err());
        assert!(read_head(&snapshot, "tracked.rs", commit, &"b".repeat(40), 100).is_err());
        manifest.paths[0].source = ContextSource::Worktree;
        assert!(crate::local_context::load_manifest(&snapshot, &manifest).is_err());
    }

    #[test]
    fn changed_head_invalidates_the_manifest_before_loading_any_blob() {
        let (root, snapshot) = fixture(b"committed\n", "");
        let manifest = crate::context_discovery::discover(&snapshot).unwrap();
        let repo = gix::open(root.path()).unwrap();
        let tree = repo.head_tree_id_or_empty().unwrap().detach();
        let parent = repo.head_id().unwrap().detach();
        let actor = gix::actor::SignatureRef {
            name: b"Test".as_bstr(),
            email: b"test@example.test".as_bstr(),
            time: "2 +0000",
        };
        repo.commit_as(actor, actor, "HEAD", "advance", tree, [parent])
            .unwrap();
        assert!(crate::local_context::load_manifest(&snapshot, &manifest).is_err());
    }

    #[test]
    fn configuration_uses_shared_defaults_but_enforces_local_budgets_and_pattern_limits() {
        for field in [
            "max_files = 0",
            "max_bytes = 0",
            "per_file_bytes = 0",
            "include = ['[']",
            "exclude = ['[']",
        ] {
            let raw = format!("version = 1\n[context.auto]\n{field}");
            assert!(crate::repository_policy::RepositoryPolicy::parse(&raw).is_err());
        }
        let raw = "version = 1\n[context.auto]\nenabled = true\nmax_files = 999\nmax_bytes = 9999999\nper_file_bytes = 9999999";
        let policy = crate::repository_policy::RepositoryPolicy::parse(raw).unwrap();
        assert_eq!(
            (
                policy.context.auto.max_files,
                policy.context.auto.max_bytes,
                policy.context.auto.per_file_bytes
            ),
            (32, 64_000, 16_000)
        );
        let raw = format!(
            "version = 1\n[context.auto]\ninclude = [{}]",
            vec!["'src/**'"; 33].join(",")
        );
        assert!(crate::repository_policy::RepositoryPolicy::parse(&raw).is_err());
    }

    #[test]
    fn disabled_and_unborn_configuration_never_loads_or_proposes_base_contents() {
        let root = tempfile::tempdir().unwrap();
        gix::init(root.path()).unwrap();
        std::fs::write(
            root.path().join(".cururu.toml"),
            "version = 1\n[context.auto]\nenabled = true",
        )
        .unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/lib.rs"), "new\n").unwrap();
        let snapshot = RepositorySnapshot::open(root.path()).unwrap();
        assert_eq!(
            crate::context_discovery::discover(&snapshot).unwrap().paths,
            Vec::<ContextPath>::new()
        );
        let settings = settings(
            cururu_engine::config::CururuToml::parse("version = 1\n[context.auto]")
                .unwrap()
                .context
                .unwrap()
                .auto
                .unwrap(),
        )
        .unwrap();
        assert_eq!(settings, AutoContextConfig::default());
    }
}
