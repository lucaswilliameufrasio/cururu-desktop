//! Bounded metadata receipts, never executable commands or publication retries.

use anyhow::ensure;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_OBSERVATION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ActionKind {
    LocalSuggestion,
    GitHubSuggestionImport,
    GitHubReview,
    GitHubReply,
    LocalCommand,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ActionOutcome {
    Confirmed,
    Unconfirmed,
    CommandExited(i32),
    CommandStopped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionRecord {
    pub observation_id: String,
    pub version: u32,
    pub scope_key: String,
    pub kind: ActionKind,
    pub outcome: ActionOutcome,
    pub occurred_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    pub input_fingerprint: String,
    pub result_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_id: Option<u64>,
    pub targets: usize,
}

pub fn hash(value: &impl Serialize) -> anyhow::Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}

pub fn github_key(reference: &crate::github_pr::PullRequestRef) -> anyhow::Result<String> {
    hash(&(
        "github.com",
        reference.owner.to_ascii_lowercase(),
        reference.repository.to_ascii_lowercase(),
        reference.number,
    ))
}

impl ActionRecord {
    pub fn new(
        scope_key: String,
        kind: ActionKind,
        head: Option<String>,
        input_fingerprint: String,
        result_fingerprint: String,
    ) -> anyhow::Result<Self> {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?;
        let record = Self {
            observation_id: hash(&(
                now.as_nanos().to_string(),
                std::process::id(),
                NEXT_OBSERVATION.fetch_add(1, Ordering::Relaxed),
            ))?,
            version: 1,
            scope_key,
            kind,
            outcome: ActionOutcome::Unconfirmed,
            occurred_at: u64::try_from(now.as_millis())?,
            head,
            input_fingerprint,
            result_fingerprint,
            remote_id: None,
            targets: 1,
        };
        record.validate()?;
        Ok(record)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.version == 1 && self.occurred_at > 0 && (1..=100).contains(&self.targets),
            "action receipt version/time/count is invalid"
        );
        for fingerprint in [
            &self.observation_id,
            &self.scope_key,
            &self.input_fingerprint,
            &self.result_fingerprint,
        ] {
            crate::local_review_history::validate_hash(fingerprint)?;
        }
        if let Some(head) = &self.head {
            ensure!(
                head.len() == 40 && head.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "action receipt revision is invalid"
            );
        }
        ensure!(
            self.remote_id != Some(0),
            "action receipt remote identity is invalid"
        );
        ensure!(
            serde_json::to_vec(self)?.len() <= 4_096,
            "action receipt exceeds 4096 bytes"
        );
        Ok(())
    }

    pub fn id(&self) -> anyhow::Result<String> {
        self.validate()?;
        hash(self)
    }
}

pub fn suggestion(
    preview: &crate::suggested_change::Preview,
    remote_import: bool,
) -> anyhow::Result<ActionRecord> {
    ActionRecord::new(
        format!(
            "{:x}",
            Sha256::digest(preview.repository_identity().as_bytes())
        ),
        if remote_import {
            ActionKind::GitHubSuggestionImport
        } else {
            ActionKind::LocalSuggestion
        },
        preview.head_oid.clone(),
        hash(&(
            preview.reviewed_diff_fingerprint(),
            &preview.path,
            preview.line,
            &preview.original_line,
        ))?,
        hash(&(&preview.path, preview.line, &preview.replacement))?,
    )
}

pub fn command(
    plan: &crate::local_command::CommandPlan,
    output: &crate::local_command::CommandOutput,
) -> anyhow::Result<ActionRecord> {
    // The scope uses the same raw canonical checkout hash as local review history.
    let scope_key = format!(
        "{:x}",
        Sha256::digest(plan.context.repository_identity.as_bytes())
    );
    let mut record = ActionRecord::new(
        scope_key,
        ActionKind::LocalCommand,
        plan.context.head_oid.clone(),
        hash(&(&plan.context.diff_fingerprint, plan.command.display()))?,
        hash(&(
            output.exit_code,
            output.was_cancelled(),
            output.timed_out(),
            &output.stdout,
            &output.stderr,
        ))?,
    )?;
    record.outcome = if output.termination.is_some() {
        ActionOutcome::CommandStopped
    } else if let Some(code) = output.exit_code {
        ActionOutcome::CommandExited(code)
    } else {
        ActionOutcome::Unconfirmed
    };
    Ok(record)
}

pub fn review(
    pending: &crate::github_pr::PendingReview,
    result: &anyhow::Result<crate::github_pr::PullRequestReview>,
) -> anyhow::Result<ActionRecord> {
    let remote_id = result.as_ref().ok().map(|review| review.id);
    let mut record = ActionRecord::new(
        github_key(&pending.reference)?,
        ActionKind::GitHubReview,
        Some(pending.head_sha.clone()),
        hash(pending)?,
        hash(&remote_id)?,
    )?;
    record.remote_id = remote_id;
    record.targets = pending.comments.len().max(1);
    if result.is_ok() {
        record.outcome = ActionOutcome::Confirmed;
    }
    Ok(record)
}

pub fn reply(
    pending: &crate::PendingThreadReply,
    result: &anyhow::Result<crate::github_pr::PullRequestComment>,
) -> anyhow::Result<ActionRecord> {
    let remote_id = result.as_ref().ok().map(|comment| comment.id);
    let mut record = ActionRecord::new(
        github_key(&pending.reference)?,
        ActionKind::GitHubReply,
        Some(pending.expected_head_sha.clone()),
        hash(&(pending.target_comment.id, &pending.body))?,
        hash(&remote_id)?,
    )?;
    record.remote_id = remote_id;
    if result.is_ok() {
        record.outcome = ActionOutcome::Confirmed;
    }
    Ok(record)
}

#[cfg(test)]
pub fn sample() -> ActionRecord {
    ActionRecord::new(
        "a".repeat(64),
        ActionKind::LocalSuggestion,
        Some("b".repeat(40)),
        "c".repeat(64),
        "d".repeat(64),
    )
    .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipts_are_metadata_only_and_uncertain_writes_never_claim_success() {
        let pending = crate::github_pr::PendingReview {
            reference: crate::github_pr::PullRequestRef::parse("Example/Repository#1").unwrap(),
            base_sha: "a".repeat(40),
            head_sha: "b".repeat(40),
            event: crate::github_pr::ReviewEvent::Comment,
            body: "synthetic-private-body-marker".to_string(),
            comments: Vec::new(),
        };
        let record = review(
            &pending,
            &Err(anyhow::anyhow!("synthetic-response-secret-marker")),
        )
        .unwrap();
        assert!(matches!(record.outcome, ActionOutcome::Unconfirmed));
        assert!(record.remote_id.is_none());
        let json = serde_json::to_string(&record).unwrap();
        assert!(!json.contains("synthetic-private-body-marker"));
        assert!(!json.contains("synthetic-response-secret-marker"));
        assert!(!json.contains("Repository"));
        assert_eq!(
            record.scope_key,
            github_key(&crate::github_pr::PullRequestRef::parse("example/repository#1").unwrap())
                .unwrap()
        );
        let mut other = sample();
        let same = other.clone();
        assert_eq!(other.id().unwrap(), same.id().unwrap());
        other.observation_id = sample().observation_id;
        assert_ne!(other.id().unwrap(), same.id().unwrap());
    }

    #[test]
    fn command_receipts_never_store_arguments_outputs_or_checkout_paths() {
        let plan = crate::local_command::CommandPlan {
            context: crate::local_command::CommandContext {
                repository_root: "/synthetic-private-checkout".into(),
                repository_identity: "/synthetic-private-checkout".to_string(),
                head_oid: Some("b".repeat(40)),
                diff_fingerprint: "c".repeat(64),
            },
            command: crate::local_command::ParsedCommand {
                executable: "/synthetic-tool".into(),
                canonical_executable: "/synthetic-tool".into(),
                arguments: vec!["synthetic-argument-secret".to_string()],
            },
            sandbox: crate::local_command::SandboxAvailability::Unavailable("fixture".to_string()),
        };
        let output = crate::local_command::CommandOutput {
            exit_code: Some(7),
            stdout: "synthetic-stdout-secret".to_string(),
            stderr: "synthetic-stderr-secret".to_string(),
            ..crate::local_command::CommandOutput::default()
        };
        let receipt = command(&plan, &output).unwrap();
        assert!(matches!(receipt.outcome, ActionOutcome::CommandExited(7)));
        let json = serde_json::to_string(&receipt).unwrap();
        for secret in [
            "synthetic-private-checkout",
            "synthetic-tool",
            "synthetic-argument-secret",
            "synthetic-stdout-secret",
            "synthetic-stderr-secret",
        ] {
            assert!(!json.contains(secret));
        }
    }

    #[test]
    fn malformed_metadata_is_rejected_without_normalizing_it() {
        let mut record = sample();
        record.targets = 0;
        assert!(record.validate().is_err());
        record = sample();
        record.head = Some("main".to_string());
        assert!(record.validate().is_err());
        record = sample();
        record.remote_id = Some(0);
        assert!(record.validate().is_err());
    }
}
