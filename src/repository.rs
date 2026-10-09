use gix::bstr::ByteSlice;
use std::{
    fs,
    path::{Component, Path, PathBuf},
};

const MAX_LOCAL_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_LOCAL_INPUT_BYTES: u64 = 32 * 1024 * 1024;
const MAX_LOCAL_FILE_DIFF_BYTES: usize = 45_000;
const MAX_LOCAL_DIFF_BYTES: usize = 180_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositorySnapshot {
    pub root: PathBuf,
    pub branch: Option<String>,
    pub head_oid: Option<String>,
    pub changed_paths: Vec<String>,
    pub changed_files: Vec<cururu_core::ChangedFile>,
}

impl RepositorySnapshot {
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let repository = gix::discover(path.as_ref())?;
        let root = repository
            .workdir()
            .ok_or_else(|| {
                anyhow::anyhow!("bare repositories are not supported by the desktop client")
            })?
            .to_path_buf();
        let branch = repository
            .head_name()?
            .map(|name| name.shorten().to_string());
        let head_oid = match repository.head_id() {
            Ok(id) => Some(id.to_string()),
            Err(error) => {
                let is_unborn_branch = branch.is_some() && repository.head_ref()?.is_none();
                if is_unborn_branch {
                    None
                } else {
                    return Err(error.into());
                }
            }
        };
        let mut changed_paths = Vec::new();
        let status = repository
            .status(gix::progress::Discard)?
            .untracked_files(gix::status::UntrackedFiles::Files)
            .tree_index_track_renames(gix::status::tree_index::TrackRenames::Disabled)
            .into_iter(Vec::new())?;
        for item in status {
            let item = item?;
            let path = item
                .location()
                .to_str()
                .map_err(|_| anyhow::anyhow!("non-UTF-8 repository paths are not supported yet"))?;
            anyhow::ensure!(
                !path.chars().any(char::is_control),
                "repository paths containing control characters are not supported yet"
            );
            changed_paths.push(path.to_string());
        }
        changed_paths.sort();
        changed_paths.dedup();
        let changed_files = changed_files(&repository, &root, &changed_paths)?;

        Ok(Self {
            root,
            branch,
            head_oid,
            changed_paths,
            changed_files,
        })
    }
}

fn changed_files(
    repository: &gix::Repository,
    root: &Path,
    paths: &[String],
) -> anyhow::Result<Vec<cururu_core::ChangedFile>> {
    let head_tree = repository
        .head_tree_id_or_empty()?
        .object()?
        .try_into_tree()?;
    let mut files = Vec::with_capacity(paths.len());
    let mut total_input_bytes = 0_u64;

    for path in paths {
        let relative_path = Path::new(path);
        anyhow::ensure!(
            relative_path
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
            "Git reported an unsafe repository-relative path"
        );
        let worktree_path = root.join(relative_path);
        let new_content = match fs::symlink_metadata(&worktree_path) {
            Ok(metadata) if metadata.file_type().is_file() => {
                ensure_file_size(path, metadata.len(), &mut total_input_bytes)?;
                fs::read_to_string(&worktree_path).map_err(|error| {
                    anyhow::anyhow!(error).context(format!("failed to read text file {path}"))
                })?
            }
            Ok(_) => anyhow::bail!("non-regular file changes are not supported yet: {path}"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error.into()),
        };
        let old_content = match head_tree.lookup_entry_by_path(relative_path)? {
            Some(entry) => {
                let object_id = entry.object_id();
                let size = repository.find_header(object_id)?.size();
                ensure_file_size(path, size, &mut total_input_bytes)?;
                String::from_utf8(repository.find_blob(object_id)?.data.clone()).map_err(
                    |error| {
                        anyhow::anyhow!(error)
                            .context(format!("base file is not UTF-8 text: {path}"))
                    },
                )?
            }
            None => String::new(),
        };
        if old_content == new_content {
            continue;
        }
        let file_diff = similar::TextDiff::from_lines(&old_content, &new_content)
            .unified_diff()
            .header(&format!("a/{path}"), &format!("b/{path}"))
            .to_string();
        let patch = format!("diff --git a/{path} b/{path}\n{file_diff}");
        let parsed = cururu_core::parse_unified_diff(&patch);
        files.extend(parsed);
    }

    cururu_core::ensure_review_diff_limits(&files, MAX_LOCAL_FILE_DIFF_BYTES, MAX_LOCAL_DIFF_BYTES)
        .map_err(|error| {
            anyhow::anyhow!(error).context("local diff exceeds desktop review limits")
        })?;

    Ok(files)
}

fn ensure_file_size(path: &str, file_size: u64, total_input_bytes: &mut u64) -> anyhow::Result<()> {
    anyhow::ensure!(
        file_size <= MAX_LOCAL_FILE_BYTES,
        "changed file exceeds the desktop text input limit: {path}"
    );
    *total_input_bytes = total_input_bytes.saturating_add(file_size);
    anyhow::ensure!(
        *total_input_bytes <= MAX_LOCAL_INPUT_BYTES,
        "repository changes exceed the desktop text input limit"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::RepositorySnapshot;
    use gix::bstr::ByteSlice;
    use std::fs;
    #[test]
    fn opening_a_repository_reads_worktree_status_without_writing_the_index() {
        let directory = tempfile::tempdir().unwrap();
        gix::init(directory.path()).unwrap();
        fs::write(directory.path().join("new-file.txt"), "untracked content").unwrap();
        let index_path = directory.path().join(".git/index");
        assert!(!index_path.exists());

        let snapshot = RepositorySnapshot::open(directory.path()).unwrap();

        assert_eq!(snapshot.root, directory.path());
        assert_eq!(snapshot.branch.as_deref(), Some("main"));
        assert_eq!(snapshot.head_oid, None);
        assert_eq!(snapshot.changed_paths, ["new-file.txt"]);
        assert_eq!(snapshot.changed_files.len(), 1);
        assert_eq!(snapshot.changed_files[0].path, "new-file.txt");
        assert_eq!(snapshot.changed_files[0].right_lines, [1]);
        assert!(
            snapshot.changed_files[0]
                .patch
                .contains("+untracked content")
        );
        assert!(!index_path.exists());
    }

    #[test]
    fn opening_a_clean_repository_returns_no_changed_files() {
        let directory = tempfile::tempdir().unwrap();
        gix::init(directory.path()).unwrap();

        let snapshot = RepositorySnapshot::open(directory.path()).unwrap();

        assert_eq!(snapshot.changed_paths.len(), 0);
        assert_eq!(snapshot.changed_files.len(), 0);
    }

    #[test]
    fn opening_a_repository_renders_a_tracked_worktree_modification() {
        let directory = tempfile::tempdir().unwrap();
        let repository = gix::init(directory.path()).unwrap();
        let blob_id = repository.write_blob(b"before\n").unwrap().detach();
        let tree = gix::objs::Tree {
            entries: vec![gix::objs::tree::Entry {
                mode: (0o100_644).try_into().unwrap(),
                filename: "tracked.txt".into(),
                oid: blob_id,
            }],
        };
        let tree_id = repository.write_object(&tree).unwrap().detach();
        let actor = gix::actor::SignatureRef {
            name: b"Cururu Test".as_bstr(),
            email: b"cururu@example.test".as_bstr(),
            time: "1 +0000",
        };
        repository
            .commit_as(
                actor,
                actor,
                "refs/heads/main",
                "base commit",
                tree_id,
                std::iter::empty::<gix::ObjectId>(),
            )
            .unwrap();
        fs::write(directory.path().join("tracked.txt"), "after\n").unwrap();

        let snapshot = RepositorySnapshot::open(directory.path()).unwrap();

        assert_eq!(snapshot.changed_paths, ["tracked.txt"]);
        assert!(snapshot.head_oid.is_some());
        assert_eq!(snapshot.changed_files.len(), 1);
        assert!(snapshot.changed_files[0].patch.contains("-before"));
        assert!(snapshot.changed_files[0].patch.contains("+after"));
        assert_eq!(snapshot.changed_files[0].right_lines, [1]);

        let staged_blob = repository.write_blob(b"staged\n").unwrap().detach();
        let mut index = repository.index_from_tree(tree_id.as_ref()).unwrap();
        index.entries_mut()[0].id = staged_blob;
        index.remove_tree();
        index.write(gix::index::write::Options::default()).unwrap();
        fs::write(directory.path().join("tracked.txt"), "staged\n").unwrap();
        let index_path = directory.path().join(".git/index");
        let index_before_open = fs::read(&index_path).unwrap();

        let staged_snapshot = RepositorySnapshot::open(directory.path()).unwrap();

        assert_eq!(staged_snapshot.changed_paths, ["tracked.txt"]);
        assert_eq!(staged_snapshot.changed_files.len(), 1);
        assert!(staged_snapshot.changed_files[0].patch.contains("-before"));
        assert!(staged_snapshot.changed_files[0].patch.contains("+staged"));
        assert_eq!(staged_snapshot.changed_files[0].right_lines, [1]);
        assert_eq!(fs::read(index_path).unwrap(), index_before_open);
    }

    #[test]
    fn opening_a_repository_rejects_paths_with_control_characters() {
        let directory = tempfile::tempdir().unwrap();
        gix::init(directory.path()).unwrap();
        fs::write(directory.path().join("line\nbreak.txt"), "content").unwrap();

        let result = RepositorySnapshot::open(directory.path());

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("control characters")
        );
    }

    #[test]
    fn local_file_limits_reject_oversized_inputs_and_aggregate_bytes() {
        let mut aggregate = super::MAX_LOCAL_INPUT_BYTES;
        assert!(super::ensure_file_size("two", 1, &mut aggregate).is_err());

        let mut aggregate = 0;
        assert!(
            super::ensure_file_size("large", super::MAX_LOCAL_FILE_BYTES + 1, &mut aggregate)
                .is_err()
        );
    }

    #[test]
    fn opening_a_bare_repository_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        gix::init_bare(directory.path()).unwrap();

        let result = RepositorySnapshot::open(directory.path());

        assert!(result.is_err());
    }
}
