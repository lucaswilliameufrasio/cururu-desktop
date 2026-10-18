use std::{
    fs::{self, Metadata, OpenOptions},
    io::Write as _,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context as _, ensure};
use similar::TextDiff;

use crate::{local_review, repository::RepositorySnapshot};

const MAX_SUGGESTED_FILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_SUGGESTION_BYTES: usize = 10_000;
const TEMP_FILE_ATTEMPTS: usize = 8;

static NEXT_TEMP_FILE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct Preview {
    context: Option<crate::local_context::PreparedContext>,
    pub path: String,
    pub line: u32,
    pub title: String,
    pub original_line: String,
    pub replacement: String,
    pub diff: String,
    root: PathBuf,
    repository_identity: String,
    reviewed_diff_fingerprint: String,
    original_source: String,
    replacement_source: String,
}

impl Preview {
    #[must_use]
    pub fn repository_root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn repository_identity(&self) -> &str {
        &self.repository_identity
    }

    #[must_use]
    pub fn reviewed_diff_fingerprint(&self) -> &str {
        &self.reviewed_diff_fingerprint
    }
}

#[cfg(test)]
pub fn prepare(
    repository_root: &Path,
    repository_identity: &str,
    reviewed_diff_fingerprint: &str,
    finding: &cururu_core::ReviewFinding,
) -> anyhow::Result<Preview> {
    prepare_with_context(
        repository_root,
        repository_identity,
        reviewed_diff_fingerprint,
        finding,
        None,
    )
}

pub fn prepare_with_context(
    repository_root: &Path,
    repository_identity: &str,
    reviewed_diff_fingerprint: &str,
    finding: &cururu_core::ReviewFinding,
    context: Option<&crate::local_context::PreparedContext>,
) -> anyhow::Result<Preview> {
    let identity = local_review::repository_identity(repository_root)?;
    ensure!(
        identity == repository_identity,
        "the selected repository changed; reopen it before previewing this suggestion"
    );
    let snapshot = RepositorySnapshot::open(repository_root)?;
    crate::local_context::validate(&snapshot, context)?;
    ensure!(
        local_review::snapshot_fingerprint(&snapshot) == reviewed_diff_fingerprint,
        "working-tree changes differ from the reviewed preview; run local review again"
    );
    let line = finding
        .line
        .context("the finding does not have a safe changed-line anchor")?;
    ensure!(
        cururu_core::is_valid_anchor(&snapshot.changed_files, &finding.path, line),
        "the finding anchor is not present in the reviewed diff"
    );
    let suggested_change = finding
        .suggested_change
        .as_ref()
        .context("the finding does not contain an applicable code suggestion")?;
    validate_replacement(&suggested_change.replacement)?;

    prepare_file(
        snapshot,
        identity,
        FileSuggestion {
            path: &finding.path,
            line,
            title: &finding.title,
            replacement: &suggested_change.replacement,
        },
        context,
        None,
    )
}

#[derive(Clone, Copy)]
struct FileSuggestion<'a> {
    path: &'a str,
    line: u32,
    title: &'a str,
    replacement: &'a str,
}

pub fn prepare_remote(
    repository_root: &Path,
    source: &crate::github_pr::PullRequestSourceFile,
    line: u32,
    replacement: &str,
) -> anyhow::Result<Preview> {
    let identity = local_review::repository_identity(repository_root)?;
    let snapshot = RepositorySnapshot::open(repository_root)?;
    ensure!(
        snapshot.head_oid.as_deref() == Some(source.commit_sha.as_str()),
        "selected checkout HEAD must equal the reviewed GitHub commit"
    );
    prepare_file(
        snapshot,
        identity,
        FileSuggestion {
            path: &source.path,
            line,
            title: "GitHub comment suggestion",
            replacement,
        },
        None,
        Some(&source.content),
    )
}

fn prepare_file(
    snapshot: RepositorySnapshot,
    identity: String,
    suggestion: FileSuggestion<'_>,
    context: Option<&crate::local_context::PreparedContext>,
    expected_source: Option<&str>,
) -> anyhow::Result<Preview> {
    let FileSuggestion {
        path,
        line,
        title,
        replacement,
    } = suggestion;
    validate_replacement(replacement)?;
    let target = safe_worktree_target(&snapshot.root, path)?;
    let metadata = fs::symlink_metadata(&target)
        .context("could not inspect the suggested file before previewing")?;
    let original_source = read_bounded_source(&target, &metadata)?;
    ensure!(
        expected_source.is_none_or(|expected| expected == original_source),
        "local target differs from the reviewed GitHub source; no file was modified"
    );
    let original_line = source_line(&original_source, line)?;
    let replacement_source = replace_line(&original_source, line, replacement)?;
    ensure!(
        replacement_source != original_source,
        "the suggestion does not change the selected source line"
    );
    let diff = TextDiff::from_lines(&original_source, &replacement_source)
        .unified_diff()
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string();

    Ok(Preview {
        context: context.cloned(),
        path: path.to_string(),
        line,
        title: title.to_string(),
        original_line,
        replacement: replacement.to_string(),
        diff,
        reviewed_diff_fingerprint: local_review::snapshot_fingerprint(&snapshot),
        root: snapshot.root,
        repository_identity: identity,
        original_source,
        replacement_source,
    })
}

pub fn apply(preview: &Preview) -> anyhow::Result<()> {
    let identity = local_review::repository_identity(&preview.root)?;
    ensure!(
        identity == preview.repository_identity,
        "the selected repository changed after the preview; no file was modified"
    );
    let snapshot = RepositorySnapshot::open(&preview.root)?;
    crate::local_context::validate(&snapshot, preview.context.as_ref())?;
    ensure!(
        local_review::snapshot_fingerprint(&snapshot) == preview.reviewed_diff_fingerprint,
        "working-tree changes differ from the preview; no file was modified"
    );
    let target = safe_worktree_target(&preview.root, &preview.path)?;
    let metadata = fs::symlink_metadata(&target)
        .context("could not inspect the suggested file before applying")?;
    let current_source = read_bounded_source(&target, &metadata)?;
    ensure!(
        current_source == preview.original_source,
        "the suggested file changed after preview; no file was modified"
    );
    ensure!(
        replace_line(&current_source, preview.line, &preview.replacement)?
            == preview.replacement_source,
        "the suggestion no longer matches the preview; no file was modified"
    );

    atomic_replace(
        &preview.root,
        &preview.path,
        &preview.original_source,
        &preview.replacement_source,
    )
}

fn validate_replacement(replacement: &str) -> anyhow::Result<()> {
    ensure!(
        !replacement.trim().is_empty(),
        "the suggested replacement cannot be empty"
    );
    ensure!(
        replacement.len() <= MAX_SUGGESTION_BYTES,
        "the suggested replacement exceeds the size limit"
    );
    ensure!(
        !replacement.contains(['\n', '\r']) && !replacement.contains("```"),
        "only a single source line can be applied as a suggestion"
    );
    ensure!(
        !replacement.chars().any(|character| {
            character == '\0' || (character.is_control() && character != '\t')
        }),
        "the suggested replacement contains unsupported control characters"
    );
    Ok(())
}

fn replace_line(source: &str, line: u32, replacement: &str) -> anyhow::Result<String> {
    validate_replacement(replacement)?;
    ensure!(line > 0, "the suggested line number must be positive");
    let line_index = usize::try_from(line - 1).context("the suggested line number is too large")?;
    let lines = source.split_inclusive('\n').collect::<Vec<_>>();
    let selected = lines
        .get(line_index)
        .context("the suggested line is no longer present in the file")?;
    let (selected_text, line_ending) = selected.strip_suffix("\r\n").map_or_else(
        || {
            selected
                .strip_suffix('\n')
                .map_or((*selected, ""), |text| (text, "\n"))
        },
        |text| (text, "\r\n"),
    );
    ensure!(
        selected_text != replacement,
        "the suggestion already matches the selected source line"
    );

    let mut updated = String::with_capacity(
        source
            .len()
            .saturating_add(replacement.len())
            .saturating_sub(selected_text.len()),
    );
    for (index, source_line) in lines.iter().enumerate() {
        if index == line_index {
            updated.push_str(replacement);
            updated.push_str(line_ending);
        } else {
            updated.push_str(source_line);
        }
    }
    ensure!(
        updated.len() <= MAX_SUGGESTED_FILE_BYTES,
        "the suggested file would exceed the local text-file size limit"
    );
    Ok(updated)
}

fn source_line(source: &str, line: u32) -> anyhow::Result<String> {
    ensure!(line > 0, "the suggested line number must be positive");
    let line_index = usize::try_from(line - 1).context("the suggested line number is too large")?;
    let selected = source
        .split_inclusive('\n')
        .nth(line_index)
        .context("the suggested line is no longer present in the file")?;
    let selected = selected
        .strip_suffix("\r\n")
        .or_else(|| selected.strip_suffix('\n'))
        .unwrap_or(selected);
    Ok(selected.to_string())
}

fn safe_worktree_target(root: &Path, relative_path: &str) -> anyhow::Result<PathBuf> {
    let relative_path = Path::new(relative_path);
    ensure!(
        !relative_path.as_os_str().is_empty()
            && relative_path
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "the suggested repository path is unsafe"
    );
    let root = root
        .canonicalize()
        .context("could not resolve the suggested repository path")?;
    let mut target = root;
    let components = relative_path.components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(component) = component else {
            anyhow::bail!("the suggested repository path is unsafe");
        };
        target.push(component);
        let metadata = fs::symlink_metadata(&target).with_context(|| {
            format!(
                "could not inspect suggested path component {}",
                target.display()
            )
        })?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "suggestions cannot be applied through a symbolic link"
        );
        let is_last = index + 1 == components.len();
        if is_last {
            ensure!(
                metadata.is_file(),
                "suggestions can only be applied to a regular worktree file"
            );
        } else {
            ensure!(
                metadata.is_dir(),
                "a suggested path parent is not a regular directory"
            );
        }
    }
    Ok(target)
}

fn read_bounded_source(path: &Path, metadata: &Metadata) -> anyhow::Result<String> {
    ensure!(
        metadata.len() <= MAX_SUGGESTED_FILE_BYTES as u64,
        "the suggested file exceeds the local text-file size limit"
    );
    let bytes = fs::read(path).context("could not read the suggested worktree file")?;
    ensure!(
        bytes.len() <= MAX_SUGGESTED_FILE_BYTES,
        "the suggested file exceeds the local text-file size limit"
    );
    String::from_utf8(bytes).context("suggestions can only be applied to UTF-8 text files")
}

fn atomic_replace(
    root: &Path,
    relative_path: &str,
    expected_source: &str,
    replacement_source: &str,
) -> anyhow::Result<()> {
    let target = safe_worktree_target(root, relative_path)?;
    let parent = target
        .parent()
        .context("the suggested file has no parent directory")?;
    let filename = target
        .file_name()
        .and_then(|name| name.to_str())
        .context("the suggested file name is not valid UTF-8")?;
    let mut temporary = None;
    for _ in 0..TEMP_FILE_ATTEMPTS {
        let id = NEXT_TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
        let temporary_path = parent.join(format!(
            ".{filename}.cururu-suggestion-{}-{id}.tmp",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
        {
            Ok(file) => {
                temporary = Some((TemporaryPath(temporary_path), file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).context("could not create a temporary suggestion file");
            }
        }
    }
    let Some((temporary_path, mut file)) = temporary else {
        anyhow::bail!("could not allocate a unique temporary suggestion file");
    };
    file.write_all(replacement_source.as_bytes())
        .context("could not write the temporary suggestion file")?;
    file.sync_all()
        .context("could not flush the temporary suggestion file")?;

    let latest_target = safe_worktree_target(root, relative_path)?;
    let latest_metadata = fs::symlink_metadata(&latest_target)
        .context("could not recheck the suggested file before applying")?;
    ensure!(
        read_bounded_source(&latest_target, &latest_metadata)? == expected_source,
        "the suggested file changed during application; no file was modified"
    );
    file.set_permissions(latest_metadata.permissions())
        .context("could not preserve suggested file permissions")?;
    file.sync_all()
        .context("could not flush the temporary suggestion file")?;
    drop(file);
    fs::rename(&temporary_path.0, &latest_target)
        .context("could not atomically replace the suggested worktree file")?;
    Ok(())
}

struct TemporaryPath(PathBuf);

impl Drop for TemporaryPath {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::{Preview, apply, prepare, replace_line};
    use crate::repository::RepositorySnapshot;

    fn repository_with_file(source: &str) -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir_all(directory.path().join("src")).unwrap();
        fs::write(directory.path().join("src/lib.rs"), source).unwrap();
        gix::init(directory.path()).unwrap();
        directory
    }

    fn finding(path: &str, line: Option<u32>, replacement: &str) -> cururu_core::ReviewFinding {
        cururu_core::ReviewFinding {
            severity: "medium".to_string(),
            path: path.to_string(),
            line,
            title: "Use the checked helper".to_string(),
            message: "The current call does not validate the input.".to_string(),
            suggestion: "Replace this line with the checked helper.".to_string(),
            confidence: 0.9,
            suggested_change: Some(cururu_core::SuggestedChange {
                replacement: replacement.to_string(),
            }),
            source: None,
            rule: None,
        }
    }

    fn preview_for(directory: &Path) -> anyhow::Result<Preview> {
        let snapshot = RepositorySnapshot::open(directory)?;
        let identity = crate::local_review::repository_identity(directory)?;
        prepare(
            directory,
            &identity,
            &crate::local_review::snapshot_fingerprint(&snapshot),
            &finding("src/lib.rs", Some(2), "checked_call()"),
        )
    }

    #[test]
    fn replace_line_preserves_crlf_and_final_newline() {
        let updated =
            replace_line("first\r\nunsafe_call()\r\nlast\r\n", 2, "checked_call()").unwrap();

        assert_eq!(updated, "first\r\nchecked_call()\r\nlast\r\n");
    }

    #[test]
    fn preview_does_not_write_and_apply_changes_only_the_confirmed_line() {
        let directory = repository_with_file("first_line()\nunsafe_call()\nlast_line()\n");
        let path = directory.path().join("src/lib.rs");
        let index_path = directory.path().join(".git/index");
        let before = fs::read_to_string(&path).unwrap();
        let preview = preview_for(directory.path()).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), before);
        assert!(!index_path.exists());
        assert_eq!(preview.original_line, "unsafe_call()");
        assert!(preview.diff.contains("-unsafe_call()"));
        assert!(preview.diff.contains("+checked_call()"));

        apply(&preview).unwrap();

        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "first_line()\nchecked_call()\nlast_line()\n"
        );
        assert!(!index_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn apply_preserves_file_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = repository_with_file("first_line()\nunsafe_call()\nlast_line()\n");
        let path = directory.path().join("src/lib.rs");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let preview = preview_for(directory.path()).unwrap();

        apply(&preview).unwrap();

        let permissions = fs::metadata(path).unwrap().permissions();
        assert_eq!(permissions.mode() & 0o777, 0o640);
    }

    #[test]
    fn context_edits_block_both_suggestion_preview_and_application_without_writing() {
        let directory = repository_with_file("first_line()\nunsafe_call()\nlast_line()\n");
        fs::write(directory.path().join(".gitignore"), "CONTEXT.md\n").unwrap();
        fs::write(
            directory.path().join(".cururu.toml"),
            "version = 1\n[context]\nconventions = ['CONTEXT.md']",
        )
        .unwrap();
        fs::write(directory.path().join("CONTEXT.md"), "Check calls\n").unwrap();
        let snapshot = RepositorySnapshot::open(directory.path()).unwrap();
        let evidence = crate::local_context::load(&snapshot).unwrap();
        let identity = crate::local_review::repository_identity(directory.path()).unwrap();
        let fingerprint = crate::local_review::snapshot_fingerprint(&snapshot);
        let finding = finding("src/lib.rs", Some(2), "checked_call()");
        let preview = super::prepare_with_context(
            directory.path(),
            &identity,
            &fingerprint,
            &finding,
            Some(&evidence),
        )
        .unwrap();
        fs::write(directory.path().join("CONTEXT.md"), "Changed context\n").unwrap();
        assert!(
            super::prepare_with_context(
                directory.path(),
                &identity,
                &fingerprint,
                &finding,
                Some(&evidence)
            )
            .is_err()
        );
        assert!(apply(&preview).is_err());
        assert_eq!(
            fs::read_to_string(directory.path().join("src/lib.rs")).unwrap(),
            "first_line()\nunsafe_call()\nlast_line()\n"
        );
    }

    #[test]
    fn apply_fails_closed_when_the_repository_changes_after_preview() {
        let directory = repository_with_file("first_line()\nunsafe_call()\nlast_line()\n");
        let path = directory.path().join("src/lib.rs");
        let preview = preview_for(directory.path()).unwrap();
        let user_edit = "first_line()\nuser_changed_this()\nlast_line()\n";
        fs::write(&path, user_edit).unwrap();

        let result = apply(&preview);

        assert!(result.is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), user_edit);
    }

    #[test]
    fn prepare_rejects_missing_anchors_and_multiline_replacements() {
        let directory = repository_with_file("first_line()\nunsafe_call()\nlast_line()\n");
        let snapshot = RepositorySnapshot::open(directory.path()).unwrap();
        let identity = crate::local_review::repository_identity(directory.path()).unwrap();
        let fingerprint = crate::local_review::snapshot_fingerprint(&snapshot);

        assert!(
            prepare(
                directory.path(),
                &identity,
                &fingerprint,
                &finding("src/lib.rs", None, "checked_call()")
            )
            .is_err()
        );
        assert!(
            prepare(
                directory.path(),
                &identity,
                &fingerprint,
                &finding("src/lib.rs", Some(2), "first_line()\nsecond_line()")
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn safe_worktree_paths_reject_symlinked_directories() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("file.rs"), "keep me\n").unwrap();
        symlink(outside.path(), directory.path().join("linked-src")).unwrap();

        let result = super::safe_worktree_target(directory.path(), "linked-src/file.rs");

        assert!(result.is_err());
        assert_eq!(
            fs::read_to_string(outside.path().join("file.rs")).unwrap(),
            "keep me\n"
        );
    }
}
