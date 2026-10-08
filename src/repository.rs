use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositorySnapshot {
    pub root: PathBuf,
    pub branch: Option<String>,
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
        Ok(Self { root, branch })
    }
}

#[cfg(test)]
mod tests {
    use super::RepositorySnapshot;
    #[test]
    fn opening_a_repository_reads_its_root_and_branch_without_writing_the_index() {
        let directory = tempfile::tempdir().unwrap();
        gix::init(directory.path()).unwrap();
        let index_path = directory.path().join(".git/index");
        assert!(!index_path.exists());

        let snapshot = RepositorySnapshot::open(directory.path()).unwrap();

        assert_eq!(snapshot.root, directory.path());
        assert_eq!(snapshot.branch.as_deref(), Some("main"));
        assert!(!index_path.exists());
    }

    #[test]
    fn opening_a_bare_repository_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        gix::init_bare(directory.path()).unwrap();

        let result = RepositorySnapshot::open(directory.path());

        assert!(result.is_err());
    }
}
