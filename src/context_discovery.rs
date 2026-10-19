//! Names-only expansion of declared context. Globs inspect Git index entries,
//! never recurse through the filesystem or open file contents.

use crate::{local_context::ContextPath, repository::RepositorySnapshot};
use anyhow::{Context as _, ensure};
use gix::bstr::ByteSlice as _;
use std::{collections::HashSet, path::Path};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextManifest {
    pub snapshot_fingerprint: String,
    pub paths: Vec<ContextPath>,
}

pub fn is_glob(path: &str) -> bool {
    path.contains(['*', '?', '['])
}

pub fn validate_path(path: &str) -> anyhow::Result<()> {
    cururu_engine::review_context::ReviewContext {
        focus: Vec::new(),
        files: vec![cururu_core::ContextFile {
            label: "Context".to_string(),
            path: path.to_string(),
            content: String::new(),
        }],
    }
    .validate()?;
    ensure!(
        !path.eq_ignore_ascii_case(".cururu.toml")
            && path.split('/').all(|part| {
                let lower = part.to_ascii_lowercase();
                !matches!(
                    lower.as_str(),
                    ".git"
                        | ".ssh"
                        | ".aws"
                        | ".gnupg"
                        | ".config"
                        | ".npmrc"
                        | ".netrc"
                        | "id_rsa"
                        | "id_ed25519"
                        | "credentials"
                        | "credentials.json"
                ) && lower != ".env"
                    && !lower.starts_with(".env.")
                    && !Path::new(part).extension().is_some_and(|extension| {
                        extension.eq_ignore_ascii_case("pem")
                            || extension.eq_ignore_ascii_case("key")
                    })
            }),
        "context references a credential/configuration location that cannot be sent"
    );
    if is_glob(path) {
        globset::Glob::new(path)
            .map_err(|_| anyhow::anyhow!("context contains an invalid glob"))?;
    }
    Ok(())
}

pub fn discover(snapshot: &RepositorySnapshot) -> anyhow::Result<ContextManifest> {
    let policy = snapshot
        .review_policy
        .as_ref()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    policy.require_ready()?;
    let has_globs = policy
        .context
        .paths
        .iter()
        .any(|entry| is_glob(&entry.path));
    let index = if has_globs {
        Some(gix::discover(&snapshot.root)?.index_or_empty()?)
    } else {
        None
    };
    if let Some(index) = &index {
        ensure!(
            index.entries().len() <= 200_000,
            "context discovery exceeds the bounded index-entry limit"
        );
    }
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    for declaration in &policy.context.paths {
        validate_path(&declaration.path)?;
        if !is_glob(&declaration.path) {
            push(&mut paths, &mut seen, declaration.clone())?;
            continue;
        }
        let mut builder = globset::GlobSetBuilder::new();
        builder.add(
            globset::Glob::new(&declaration.path)
                .map_err(|_| anyhow::anyhow!("context contains an invalid glob"))?,
        );
        let matcher = builder.build().context("could not compile context glob")?;
        let index = index.as_ref().context("context index was unavailable")?;
        let mut has_candidates = false;
        for entry in index.entries() {
            let path = entry.path(index).to_str().map_err(|_| {
                anyhow::anyhow!("context discovery requires UTF-8 repository paths")
            })?;
            if !matcher.is_match(path) {
                continue;
            }
            has_candidates = true;
            ensure!(
                entry.stage() == gix::index::entry::Stage::Unconflicted
                    && (entry.mode == gix::index::entry::Mode::FILE
                        || entry.mode == gix::index::entry::Mode::FILE_EXECUTABLE),
                "context glob matched a conflicted, symlink or non-file index entry"
            );
            push(
                &mut paths,
                &mut seen,
                ContextPath {
                    label: declaration.label.clone(),
                    path: path.to_string(),
                },
            )?;
        }
        ensure!(
            has_candidates,
            "a context glob matched no tracked index paths; choose diff-only or update the declaration"
        );
    }
    Ok(ContextManifest {
        snapshot_fingerprint: crate::local_context::scope(snapshot)?,
        paths,
    })
}

fn push(
    paths: &mut Vec<ContextPath>,
    seen: &mut HashSet<String>,
    entry: ContextPath,
) -> anyhow::Result<()> {
    validate_path(&entry.path)?;
    if seen.insert(entry.path.clone()) {
        ensure!(
            paths.len() < cururu_engine::review_context::MAX_CONTEXT_FILES,
            "resolved context exceeds 32 files; no paths were silently omitted"
        );
        paths.push(entry);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_index(root: &Path, names: &[&str], mode: u32) {
        write_index_with_stage(root, names, mode, gix::index::entry::Stage::Unconflicted);
    }

    fn write_index_with_stage(
        root: &Path,
        names: &[&str],
        mode: u32,
        stage: gix::index::entry::Stage,
    ) {
        let repository = gix::open(root).unwrap();
        let blob = repository.write_blob(b"fixture\n").unwrap().detach();
        let mut names = names.to_vec();
        names.sort_unstable();
        let tree = gix::objs::Tree {
            entries: names
                .into_iter()
                .map(|name| gix::objs::tree::Entry {
                    mode: mode.try_into().unwrap(),
                    filename: name.into(),
                    oid: blob,
                })
                .collect(),
        };
        let tree = repository.write_object(&tree).unwrap().detach();
        let mut index = repository.index_from_tree(tree.as_ref()).unwrap();
        if let Some(entry) = index.entries_mut().first_mut() {
            entry.flags |= gix::index::entry::Flags::from_stage(stage);
        }
        index.remove_tree();
        index.write(gix::index::write::Options::default()).unwrap();
    }

    fn fixture(declarations: &str) -> (tempfile::TempDir, RepositorySnapshot) {
        let root = tempfile::tempdir().unwrap();
        gix::init(root.path()).unwrap();
        for name in ["A.md", "B.md", "other.txt", "UNTRACKED.md"] {
            std::fs::write(root.path().join(name), "fixture\n").unwrap();
        }
        write_index(root.path(), &["A.md", "B.md", "other.txt"], 0o100_644);
        std::fs::write(
            root.path().join(".cururu.toml"),
            format!("version = 1\n[context]\n{declarations}"),
        )
        .unwrap();
        let snapshot = RepositorySnapshot::open(root.path()).unwrap();
        (root, snapshot)
    }

    #[test]
    fn glob_discovery_uses_only_index_names_and_matches_the_core_contract() {
        for pattern in ["*.md", "A.?d", "[AB].md"] {
            let (_root, snapshot) = fixture(&format!("conventions = ['{pattern}']"));
            let manifest = discover(&snapshot).unwrap();
            let expected: Vec<&str> = ["A.md", "B.md", "other.txt"]
                .into_iter()
                .filter(|path| cururu_core::context_path_matches(path, pattern))
                .collect();
            assert_eq!(
                manifest
                    .paths
                    .iter()
                    .map(|entry| entry.path.as_str())
                    .collect::<Vec<_>>(),
                expected
            );
            assert!(
                !manifest
                    .paths
                    .iter()
                    .any(|entry| entry.path == "UNTRACKED.md")
            );
        }
        let (_root, snapshot) =
            fixture("conventions = ['*.md']\nadditional = ['A.md', 'UNTRACKED.md']");
        let manifest = discover(&snapshot).unwrap();
        assert_eq!(
            manifest
                .paths
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            ["A.md", "B.md", "UNTRACKED.md"]
        );
        assert_eq!(manifest.paths[0].label, "Conventions");
    }

    #[test]
    fn discovering_never_opens_contents_and_index_expansion_requires_a_new_preview() {
        let (root, snapshot) = fixture("conventions = ['*.md']");
        // Non-text contents cannot affect names-only discovery.
        std::fs::write(root.path().join("A.md"), [0xff]).unwrap();
        let manifest = discover(&snapshot).unwrap();
        assert_eq!(manifest.paths.len(), 2);
        assert!(crate::local_context::load_manifest(&snapshot, &manifest).is_err());
        std::fs::write(root.path().join("A.md"), "fixture\n").unwrap();
        let approved = crate::local_context::load_manifest(&snapshot, &manifest).unwrap();
        write_index(
            root.path(),
            &["A.md", "B.md", "NEW.md", "other.txt"],
            0o100_644,
        );
        // NEW.md does not even exist. The changed manifest must be rejected
        // before any attempt to open unapproved paths.
        let error = crate::local_context::validate(&snapshot, Some(&approved)).unwrap_err();
        assert!(error.to_string().contains("resolved context paths changed"));
        assert!(!root.path().join("NEW.md").exists());
    }

    #[test]
    fn missing_excess_sensitive_and_nonregular_glob_matches_fail_closed() {
        let (root, snapshot) = fixture("conventions = ['*.md']");
        write_index(root.path(), &[], 0o100_644);
        assert!(discover(&snapshot).is_err());
        write_index(root.path(), &["linked.md"], 0o120_000);
        assert!(discover(&snapshot).is_err());
        write_index_with_stage(
            root.path(),
            &["conflict.md"],
            0o100_644,
            gix::index::entry::Stage::Ours,
        );
        assert!(discover(&snapshot).is_err());
        let names: Vec<String> = (0..33).map(|index| format!("file{index}.md")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        write_index(root.path(), &names, 0o100_644);
        assert!(discover(&snapshot).is_err());
        let (root, snapshot) = fixture("conventions = ['*']");
        write_index(root.path(), &[".env", "A.md"], 0o100_644);
        assert!(discover(&snapshot).is_err());
    }
}
