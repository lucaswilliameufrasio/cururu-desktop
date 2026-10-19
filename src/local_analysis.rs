//! Explicit, local-only artifact admission; never starts analyzers or providers.

use crate::repository::RepositorySnapshot;
use anyhow::{Context as _, ensure};
use cururu_engine::config::AnalysisToml;
use serde::Deserialize;

const MAX_ARTIFACT_BYTES: usize = 128_000;
const MAX_ARTIFACTS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AnalysisSpec {
    pub enabled: bool,
    pub manifest: Option<String>,
    pub paths: Vec<String>,
    pub max_findings: usize,
}

impl AnalysisSpec {
    pub fn parse(value: Option<AnalysisToml>) -> anyhow::Result<Self> {
        let Some(value) = value else {
            return Ok(Self::default());
        };
        ensure!(
            !value.check_runs.unwrap_or(false)
                && value.check_run_names.as_ref().is_none_or(Vec::is_empty),
            "desktop local analysis does not fetch check runs"
        );
        ensure!(
            value.require_current_head.unwrap_or(true),
            "desktop analysis requires the current HEAD and diff identity"
        );
        let spec = Self {
            enabled: value.enabled.unwrap_or(false),
            manifest: value.manifest,
            paths: value.sarif_paths.unwrap_or_default(),
            max_findings: value
                .max_findings
                .unwrap_or(crate::local_review::MAX_REVIEW_FINDINGS)
                .min(crate::local_review::MAX_REVIEW_FINDINGS),
        };
        ensure!(
            spec.max_findings > 0 && spec.paths.len() <= MAX_ARTIFACTS,
            "analysis limits must be positive and at most 8 SARIF artifacts"
        );
        if spec.enabled {
            ensure!(
                spec.manifest.is_some(),
                "desktop analysis requires an explicit revision/diff-bound manifest"
            );
        }
        for path in spec.manifest.iter().chain(&spec.paths) {
            validate_path(path)?;
        }
        Ok(spec)
    }
}

fn validate_path(path: &str) -> anyhow::Result<()> {
    crate::context_discovery::validate_path(path)?;
    ensure!(
        !crate::context_discovery::is_glob(path),
        "analysis artifacts require exact paths, not globs"
    );
    Ok(())
}

#[derive(Deserialize)]
struct DesktopManifest {
    #[serde(flatten)]
    manifest: cururu_core::AnalysisManifest,
    desktop_diff_fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalysisPreview {
    pub scope: String,
    pub manifest_hash: String,
    pub paths: Vec<String>,
    pub tools: Vec<cururu_core::AnalysisTool>,
}

#[derive(Debug, Clone)]
pub struct PreparedAnalysis {
    pub preview: AnalysisPreview,
    pub fingerprint: String,
    pub report: cururu_core::AnalysisReport,
    pub discarded: usize,
}

fn read_artifact(
    snapshot: &RepositorySnapshot,
    path: &str,
    limit: usize,
) -> anyhow::Result<Vec<u8>> {
    validate_path(path)?;
    ensure!(
        !snapshot.changed_files.iter().any(|file| file.path == path),
        "analysis artifacts must be outside the working-tree diff (use a Git-ignored artifact directory)"
    );
    crate::local_context::read_worktree(&snapshot.root, path, limit)
        .map_err(|_| anyhow::anyhow!("could not safely read the bounded regular analysis artifact"))
}

pub fn discover(snapshot: &RepositorySnapshot) -> anyhow::Result<AnalysisPreview> {
    let policy = snapshot
        .review_policy
        .as_ref()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    policy.require_ready()?;
    ensure!(
        policy.analysis.enabled,
        "local analysis is not enabled in the repository configuration"
    );
    let path = policy
        .analysis
        .manifest
        .as_ref()
        .context("analysis manifest path is missing")?;
    let bytes = read_artifact(snapshot, path, 64_000)?;
    let parsed: DesktopManifest = serde_json::from_slice(&bytes).map_err(|_| {
        anyhow::anyhow!("analysis manifest format is invalid; desktop_diff_fingerprint is required")
    })?;
    ensure!(
        parsed.manifest.has_supported_schema_version(),
        "analysis manifest version is unsupported"
    );
    let head = snapshot
        .head_oid
        .as_deref()
        .context("local analysis requires an existing HEAD commit")?;
    ensure!(
        parsed.manifest.commit_sha.as_deref() == Some(head)
            && parsed.desktop_diff_fingerprint
                == crate::local_review::snapshot_fingerprint(snapshot),
        "analysis manifest is stale for the current HEAD or desktop diff fingerprint"
    );
    ensure!(
        parsed.manifest.tools.len() <= 8,
        "analysis manifest exceeds 8 tools"
    );
    for tool in &parsed.manifest.tools {
        ensure!(
            !tool.name.is_empty()
                && tool.name.len() <= 128
                && !tool.name.chars().any(char::is_control)
                && matches!(
                    tool.status.as_str(),
                    "passed" | "succeeded" | "failed" | "not_run"
                )
                && tool
                    .message
                    .as_ref()
                    .is_none_or(|message| message.len() <= 2_048),
            "analysis tool metadata is invalid or oversized"
        );
    }
    let (tools, referenced) = parsed.manifest.into_analysis_data();
    let mut paths = policy.analysis.paths.clone();
    for path in referenced {
        validate_path(&path)?;
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    ensure!(
        paths.len() <= MAX_ARTIFACTS,
        "analysis manifest resolves more than 8 SARIF artifacts"
    );
    for path in &paths {
        validate_path(path)?;
        ensure!(
            !snapshot.changed_files.iter().any(|file| &file.path == path),
            "analysis artifacts must be outside the working-tree diff"
        );
    }
    Ok(AnalysisPreview {
        scope: crate::local_context::scope(snapshot)?,
        manifest_hash: crate::action_history::hash(&bytes)?,
        paths,
        tools,
    })
}

pub fn load(
    snapshot: &RepositorySnapshot,
    preview: &AnalysisPreview,
) -> anyhow::Result<PreparedAnalysis> {
    ensure!(
        crate::local_context::scope(snapshot)? == preview.scope,
        "analysis preview belongs to a different checkout or snapshot"
    );
    ensure!(
        discover(snapshot)? == *preview,
        "analysis manifest or scope changed; preview its paths again before reading artifacts"
    );
    let policy = snapshot
        .review_policy
        .as_ref()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let files = policy.review_files(&snapshot.changed_files)?;
    let mut candidates = Vec::new();
    let mut hashes = Vec::new();
    for path in &preview.paths {
        let bytes = read_artifact(snapshot, path, MAX_ARTIFACT_BYTES)?;
        hashes.push(crate::action_history::hash(&bytes)?);
        let raw = std::str::from_utf8(&bytes)
            .map_err(|_| anyhow::anyhow!("SARIF artifact must be UTF-8"))?;
        let findings = cururu_engine::sarif::parse_findings(
            raw,
            &files,
            policy
                .analysis
                .max_findings
                .saturating_sub(candidates.len())
                + 1,
        )
        .map_err(|_| anyhow::anyhow!("SARIF artifact format is invalid"))?;
        candidates.extend(findings);
        ensure!(
            candidates.len() <= policy.analysis.max_findings,
            "analysis findings exceed the configured/desktop limit; no findings were truncated"
        );
    }
    let prior = candidates.len();
    candidates.retain(|finding| {
        finding
            .line
            .is_some_and(|line| cururu_core::is_valid_anchor(&files, &finding.path, line))
    });
    let discarded = prior - candidates.len();
    let report = cururu_core::AnalysisReport::from_parts(preview.tools.clone(), candidates);
    ensure!(
        serde_json::to_vec(&report)?.len() <= 64_000,
        "normalized analysis report exceeds 64,000 bytes"
    );
    Ok(PreparedAnalysis {
        preview: preview.clone(),
        fingerprint: crate::action_history::hash(&(
            &preview.scope,
            &preview.manifest_hash,
            hashes,
            &report,
        ))?,
        report,
        discarded,
    })
}

pub fn validate(snapshot: &RepositorySnapshot, approved: &PreparedAnalysis) -> anyhow::Result<()> {
    let current = load(snapshot, &approved.preview)?;
    ensure!(
        current.fingerprint == approved.fingerprint,
        "analysis artifacts changed since the local preview"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gix::bstr::ByteSlice as _;

    fn fixture() -> (tempfile::TempDir, RepositorySnapshot) {
        let root = tempfile::tempdir().unwrap();
        let repo = gix::init(root.path()).unwrap();
        let blob = repo.write_blob(b"before\n").unwrap().detach();
        let tree = repo
            .write_object(&gix::objs::Tree {
                entries: vec![gix::objs::tree::Entry {
                    mode: (0o100_644).try_into().unwrap(),
                    filename: "file.rs".into(),
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
        std::fs::write(root.path().join("file.rs"), "after\n").unwrap();
        std::fs::write(root.path().join(".gitignore"), "artifacts/\n").unwrap();
        std::fs::write(
            root.path().join(".cururu.toml"),
            "version = 1\n[analysis]\nenabled = true\nmanifest = 'artifacts/manifest.json'",
        )
        .unwrap();
        std::fs::create_dir(root.path().join("artifacts")).unwrap();
        let snapshot = RepositorySnapshot::open(root.path()).unwrap();
        manifest(root.path(), &snapshot, "artifacts/check.sarif");
        (root, snapshot)
    }

    fn manifest(root: &std::path::Path, snapshot: &RepositorySnapshot, path: &str) {
        std::fs::write(
            root.join("artifacts/manifest.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1, "commit_sha": snapshot.head_oid,
                "desktop_diff_fingerprint": crate::local_review::snapshot_fingerprint(snapshot),
                "tools": [{"name":"check", "status":"failed", "exit_code":1, "sarif_path":path}]
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn sarif(line: u32) -> String {
        serde_json::json!({"runs":[{"tool":{"driver":{"name":"check"}},"results":[{
            "ruleId":"R1","level":"error","message":{"text":"Check boundary"},
            "locations":[{"physicalLocation":{"artifactLocation":{"uri":"file.rs"},"region":{"startLine":line}}}]
        }]}]}).to_string()
    }

    #[test]
    fn manifest_preview_does_not_read_referenced_sarif_and_loading_keeps_provenance() {
        let (root, snapshot) = fixture();
        let preview = discover(&snapshot).unwrap();
        assert_eq!(preview.paths, ["artifacts/check.sarif"]);
        assert!(!root.path().join("artifacts/check.sarif").exists());
        assert!(load(&snapshot, &preview).is_err());
        std::fs::write(root.path().join("artifacts/check.sarif"), sarif(1)).unwrap();
        let report = load(&snapshot, &preview).unwrap();
        assert_eq!(report.report.status, "failed");
        assert_eq!(report.report.findings.len(), 1);
        assert_eq!(report.report.findings[0].source.as_deref(), Some("check"));
        assert_eq!(report.report.findings[0].rule.as_deref(), Some("R1"));
        validate(&snapshot, &report).unwrap();
        std::fs::write(root.path().join("artifacts/check.sarif"), sarif(999)).unwrap();
        assert!(validate(&snapshot, &report).is_err());
        let invalid = load(&snapshot, &preview).unwrap();
        assert_eq!(invalid.discarded, 1);
        assert!(invalid.report.findings.is_empty());
    }

    #[test]
    fn changed_manifest_or_revision_cannot_open_new_paths_under_an_old_preview() {
        let (root, snapshot) = fixture();
        let preview = discover(&snapshot).unwrap();
        manifest(root.path(), &snapshot, "artifacts/unapproved.sarif");
        let error = load(&snapshot, &preview).unwrap_err();
        assert!(error.to_string().contains("preview its paths again"));
        assert!(!root.path().join("artifacts/unapproved.sarif").exists());
        std::fs::write(root.path().join("file.rs"), "new changes\n").unwrap();
        let fresh = RepositorySnapshot::open(root.path()).unwrap();
        assert!(discover(&fresh).is_err());
        std::fs::write(
            root.path().join("artifacts/manifest.json"),
            "{\"schema_version\":1,\"tools\":[]}",
        )
        .unwrap();
        assert!(discover(&snapshot).is_err());
    }

    #[test]
    fn excess_findings_and_cross_checkout_previews_are_rejected_without_new_reads() {
        let (root, snapshot) = fixture();
        let mut snapshot = snapshot;
        snapshot
            .review_policy
            .as_mut()
            .unwrap()
            .analysis
            .max_findings = 1;
        let preview = discover(&snapshot).unwrap();
        let finding: serde_json::Value = serde_json::from_str(&sarif(1)).unwrap();
        let mut artifact = finding;
        let result = artifact["runs"][0]["results"][0].clone();
        artifact["runs"][0]["results"]
            .as_array_mut()
            .unwrap()
            .push(result);
        std::fs::write(
            root.path().join("artifacts/check.sarif"),
            artifact.to_string(),
        )
        .unwrap();
        assert!(
            load(&snapshot, &preview)
                .unwrap_err()
                .to_string()
                .contains("no findings were truncated")
        );
        let (other_root, other) = fixture();
        std::fs::remove_file(other_root.path().join("artifacts/manifest.json")).unwrap();
        assert!(
            load(&other, &preview)
                .unwrap_err()
                .to_string()
                .contains("different checkout")
        );
    }

    #[test]
    fn invalid_config_limits_and_symlink_artifacts_fail_closed() {
        for config in [
            "enabled = true",
            "enabled = true\nmanifest = '../secret'",
            "require_current_head = false",
            "check_runs = true",
            "manifest = 'artifacts/*.json'",
            "max_findings = 0",
        ] {
            assert!(
                crate::repository_policy::RepositoryPolicy::parse(&format!(
                    "version = 1\n[analysis]\n{config}"
                ))
                .is_err()
            );
        }
        let (root, snapshot) = fixture();
        let preview = discover(&snapshot).unwrap();
        std::fs::write(
            root.path().join("artifacts/check.sarif"),
            "x".repeat(MAX_ARTIFACT_BYTES + 1),
        )
        .unwrap();
        assert!(load(&snapshot, &preview).is_err());
        #[cfg(unix)]
        {
            std::fs::remove_file(root.path().join("artifacts/check.sarif")).unwrap();
            std::os::unix::fs::symlink("../file.rs", root.path().join("artifacts/check.sarif"))
                .unwrap();
            assert!(load(&snapshot, &preview).is_err());
        }
    }
}
