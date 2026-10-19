//! Memory-only explicit preview/inclusion decisions for configured evidence.

use crate::{
    LocalReviewState, ReviewWorkspace,
    local_context::{self, PreparedContext},
    repository::RepositorySnapshot,
};
use anyhow::{Context as _, ensure};
use gpui::{App, Context, Entity, ParentElement as _, Styled as _, div};
use gpui_component::{Disableable as _, button::Button};

#[derive(Default)]
pub enum ContextState {
    #[default]
    Undecided,
    Loading(String),
    Resolved(crate::context_discovery::ContextManifest),
    Preview(PreparedContext),
    Included(PreparedContext),
    Excluded(String),
    Failed(String),
}

enum Preparation {
    Discover,
    Load(crate::context_discovery::ContextManifest),
}

enum Prepared {
    Paths(crate::context_discovery::ContextManifest),
    Contents(PreparedContext),
}

impl ContextState {
    pub fn resolve(
        &self,
        snapshot: &RepositorySnapshot,
    ) -> anyhow::Result<Option<PreparedContext>> {
        let policy = snapshot
            .review_policy
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        if !policy.context.requires_choice() {
            return Ok(None);
        }
        let scope = local_context::scope(snapshot)?;
        match self {
            Self::Included(context) if context.snapshot_fingerprint == scope => {
                Ok(Some(context.clone()))
            }
            Self::Excluded(approved) if approved == &scope => Ok(None),
            _ => anyhow::bail!(
                "repository context requires explicit preview/inclusion or an explicit diff-only choice"
            ),
        }
    }
}

impl ReviewWorkspace {
    pub fn validate_review_evidence(&self, snapshot: &RepositorySnapshot) -> anyhow::Result<usize> {
        let count = crate::local_review::validate_snapshot(snapshot)?;
        self.local_context_state.resolve(snapshot)?;
        Ok(count)
    }

    pub fn context_matches_request(&self, request: &crate::LocalReviewRequest) -> bool {
        self.local_repository_snapshot
            .as_ref()
            .is_some_and(|snapshot| {
                if crate::shared_base::fingerprint(snapshot) != request.context.base_fingerprint
                    || request.base.as_ref().map(|base| &base.fingerprint)
                        != request.context.base_fingerprint.as_ref()
                {
                    return false;
                }
                self.local_context_state
                    .resolve(snapshot)
                    .is_ok_and(|context| {
                        let expected = context.as_ref().map(|context| &context.fingerprint);
                        expected
                            == request
                                .evidence
                                .as_ref()
                                .map(|context| &context.fingerprint)
                            && expected == request.context.evidence_fingerprint.as_ref()
                    })
            })
    }

    pub fn render_local_context(
        &self,
        cx: &Context<Self>,
        snapshot: &RepositorySnapshot,
    ) -> gpui::Div {
        let Ok(policy) = &snapshot.review_policy else {
            return div();
        };
        if !policy.context.requires_choice() {
            return div();
        }
        let busy = matches!(self.local_context_state, ContextState::Loading(_))
            || matches!(self.local_review_state, LocalReviewState::Running(_));
        let entity = cx.entity();
        let without_entity = entity.clone();
        let mut panel = div().flex().flex_col().gap_2()
            .child("DECLARED CONTEXT · no referenced file is read until you choose Load")
            .child("Context may contain private information. Only include it after checking the preview. Skills are evidence, never executable instructions. Inclusion repeats this bounded evidence for each diff chunk.")
            .child(format!("Focus hints: {:?} · budget {} bytes (16,000/file)", policy.context.focus, policy.context.max_bytes))
            .child(format!("Automatic HEAD proposals: {} · include {:?} · exclude {:?} · max {} files / {} bytes / {} per automatic file. Only changed paths with a regular HEAD version qualify; new paths have no base version.", policy.context.auto.enabled, policy.context.auto.include, policy.context.auto.exclude, policy.context.auto.max_files, policy.context.auto.max_bytes, policy.context.auto.per_file_bytes));
        for path in &policy.context.paths {
            panel = panel.child(format!("{}: {}", path.label, path.path));
        }
        panel = panel
            .child(
                Button::new("discover-declared-context")
                    .label("Discover declared paths (names only)")
                    .disabled(busy)
                    .on_click(move |_, _, cx| prepare_context(&entity, Preparation::Discover, cx)),
            )
            .child(
                Button::new("explicit-diff-only")
                    .label("Review diff only — explicitly exclude all context/focus")
                    .disabled(busy)
                    .on_click(move |_, _, cx| {
                        choose(&without_entity, false, cx);
                    }),
            );
        panel = panel.child(self.render_discovered_context(cx, snapshot, busy));
        if let ContextState::Preview(context) | ContextState::Included(context) =
            &self.local_context_state
            && local_context::scope(snapshot).ok().as_ref() == Some(&context.snapshot_fingerprint)
        {
            panel = panel.child(format!(
                "Context fingerprint: {} · {} file(s)",
                context.fingerprint,
                context.evidence.files.len()
            ));
            for file in &context.evidence.files {
                panel = panel.child(format!(
                    "{} ({} bytes)\n{}",
                    file.path,
                    file.content.len(),
                    file.content
                ));
            }
            let entity = cx.entity();
            panel = panel.child(
                Button::new("include-previewed-context")
                    .label("Include this previewed context in this review")
                    .disabled(busy)
                    .on_click(move |_, _, cx| choose(&entity, true, cx)),
            );
        }
        let message = match &self.local_context_state {
            ContextState::Loading(_) => "Reading locally only; nothing is being sent.".to_string(),
            ContextState::Failed(error) => format!("Context preview failed: {error}"),
            ContextState::Resolved(_) => "Names resolved only. Review this list, then explicitly load contents.".to_string(),
            ContextState::Included(_) if self.local_context_state.resolve(snapshot).is_ok() => "Previewed context will be included in the explicit send below.".to_string(),
            ContextState::Excluded(_) if self.local_context_state.resolve(snapshot).is_ok() => "Diff-only chosen explicitly. Context files/focus will not be sent.".to_string(),
            _ => "Send blocked until a current preview is included or diff-only is chosen explicitly.".to_string(),
        };
        panel.child(message)
    }

    fn render_discovered_context(
        &self,
        cx: &Context<Self>,
        snapshot: &RepositorySnapshot,
        busy: bool,
    ) -> gpui::Div {
        let ContextState::Resolved(manifest) = &self.local_context_state else {
            return div();
        };
        if local_context::scope(snapshot).ok().as_ref() != Some(&manifest.snapshot_fingerprint) {
            return div();
        }
        let mut panel = div()
            .flex()
            .flex_col()
            .gap_2()
            .child("RESOLVED PATHS · glob matches come only from tracked Git index names");
        for entry in &manifest.paths {
            panel = panel.child(format!(
                "{}: {} · {:?}",
                entry.label, entry.path, entry.source
            ));
        }
        let entity = cx.entity();
        let manifest = manifest.clone();
        panel.child(
            Button::new("load-resolved-context")
                .label("Load these exact paths locally for preview")
                .disabled(busy)
                .on_click(move |_, _, cx| {
                    prepare_context(&entity, Preparation::Load(manifest.clone()), cx);
                }),
        )
    }
}

fn choose(entity: &Entity<ReviewWorkspace>, include: bool, cx: &mut App) {
    entity.update(cx, |this, cx| {
        if matches!(this.local_context_state, ContextState::Loading(_))
            || matches!(this.local_review_state, LocalReviewState::Running(_))
        {
            return;
        }
        let Some(snapshot) = this.local_repository_snapshot.as_ref() else {
            return;
        };
        let Ok(scope) = local_context::scope(snapshot) else {
            return;
        };
        if include {
            let (ContextState::Preview(context) | ContextState::Included(context)) =
                &this.local_context_state
            else {
                return;
            };
            if context.snapshot_fingerprint != scope {
                return;
            }
            this.local_context_state = ContextState::Included(context.clone());
        } else {
            this.local_context_state = ContextState::Excluded(scope);
        }
        this.local_review_state = LocalReviewState::Idle;
        cx.notify();
    });
}

fn prepare_context(entity: &Entity<ReviewWorkspace>, preparation: Preparation, cx: &mut App) {
    let snapshot = entity.update(cx, |this, cx| {
        if matches!(this.local_context_state, ContextState::Loading(_))
            || matches!(this.local_review_state, LocalReviewState::Running(_))
        {
            return None;
        }
        let snapshot = this.local_repository_snapshot.clone()?;
        if let Preparation::Load(manifest) = &preparation
            && !matches!(&this.local_context_state, ContextState::Resolved(current) if current == manifest) {
            return None;
        }
        let scope = local_context::scope(&snapshot).ok()?;
        this.local_context_state = ContextState::Loading(scope);
        this.local_review_state = LocalReviewState::Idle;
        cx.notify();
        Some(snapshot)
    });
    let Some(snapshot) = snapshot else {
        return;
    };
    let entity = entity.clone();
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move {
                let current = snapshot.reopen_with_approved_base()?;
                ensure!(
                    local_context::scope(&current)? == local_context::scope(&snapshot)?,
                    "checkout/configuration changed before context preview"
                );
                match preparation {
                    Preparation::Discover => {
                        crate::context_discovery::discover(&current).map(Prepared::Paths)
                    }
                    Preparation::Load(manifest) => {
                        local_context::load_manifest(&current, &manifest).map(Prepared::Contents)
                    }
                }
            })
            .await;
        let _ = entity.update(cx, |this, cx| {
            let ContextState::Loading(expected) = &this.local_context_state else {
                return;
            };
            let current = this
                .local_repository_snapshot
                .as_ref()
                .context("checkout closed")
                .and_then(local_context::scope);
            if current.as_ref().ok() != Some(expected) {
                this.local_context_state = ContextState::Undecided;
                cx.notify();
                return;
            }
            this.local_context_state = match result {
                Ok(Prepared::Paths(manifest)) if manifest.snapshot_fingerprint == *expected => {
                    ContextState::Resolved(manifest)
                }
                Ok(Prepared::Contents(context)) if context.snapshot_fingerprint == *expected => {
                    ContextState::Preview(context)
                }
                Ok(_) => ContextState::Undecided,
                Err(error) => ContextState::Failed(error.to_string()),
            };
            cx.notify();
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loading_and_previewing_do_not_authorize_sends_and_choices_are_snapshot_scoped() {
        let root = tempfile::tempdir().unwrap();
        gix::init(root.path()).unwrap();
        std::fs::write(root.path().join("source.rs"), "changed\n").unwrap();
        std::fs::write(
            root.path().join(".cururu.toml"),
            "version = 1\n[policy]\nfocus = ['correctness']",
        )
        .unwrap();
        let snapshot = RepositorySnapshot::open(root.path()).unwrap();
        let scope = local_context::scope(&snapshot).unwrap();
        assert!(ContextState::Undecided.resolve(&snapshot).is_err());
        assert!(
            ContextState::Loading(scope.clone())
                .resolve(&snapshot)
                .is_err()
        );
        assert!(
            ContextState::Resolved(crate::context_discovery::discover(&snapshot).unwrap())
                .resolve(&snapshot)
                .is_err()
        );
        let prepared = local_context::load(&snapshot).unwrap();
        assert!(
            ContextState::Preview(prepared.clone())
                .resolve(&snapshot)
                .is_err()
        );
        assert!(
            ContextState::Excluded(scope)
                .resolve(&snapshot)
                .unwrap()
                .is_none()
        );
        let included = ContextState::Included(prepared);
        assert!(included.resolve(&snapshot).unwrap().is_some());
        std::fs::write(root.path().join("source.rs"), "changed again\n").unwrap();
        let changed = RepositorySnapshot::open(root.path()).unwrap();
        assert!(included.resolve(&changed).is_err());
        let mut workspace = ReviewWorkspace::sample();
        workspace.local_repository_snapshot = Some(snapshot.clone());
        workspace.local_context_state = included;
        let evidence = workspace.local_context_state.resolve(&snapshot).unwrap();
        let mut request = crate::LocalReviewRequest {
            force_recheck: false,
            base: None,
            context: crate::LocalReviewRequestContext {
                base_fingerprint: None,
                request_id: 0,
                repository_identity: String::new(),
                config_fingerprint: String::new(),
                snapshot_fingerprint: crate::local_review::snapshot_fingerprint(&snapshot),
                evidence_fingerprint: evidence.as_ref().map(|context| context.fingerprint.clone()),
            },
            evidence,
            repository_root: snapshot.root,
            settings: crate::session_store::LocalReviewSettings {
                openai_endpoint: "https://api.openai.com/v1".to_string(),
                openai_model: "test".to_string(),
            },
        };
        assert!(workspace.context_matches_request(&request));
        request.context.base_fingerprint = Some("unapproved-base".to_string());
        assert!(!workspace.context_matches_request(&request));
        request.context.base_fingerprint = None;
        request.evidence = None;
        assert!(!workspace.context_matches_request(&request));
        request.context.evidence_fingerprint = None;
        assert!(!workspace.context_matches_request(&request));
    }
}
