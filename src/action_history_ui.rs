//! Explicit metadata persistence; no receipt can execute its former action.

use crate::{
    ReviewWorkspace,
    action_history::{ActionRecord, github_key},
};
use gpui::{App, Context, Entity, ParentElement as _, Styled as _, div};
use gpui_component::{Disableable as _, button::Button};

#[derive(Default)]
pub struct ActionHistory {
    pending: Vec<ActionRecord>,
    view: View,
}

#[derive(Default)]
enum View {
    #[default]
    Idle,
    Busy,
    Loaded {
        key: String,
        page: usize,
        rows: Vec<(String, ActionRecord)>,
    },
}

#[derive(Clone)]
enum Operation {
    Load { key: String, page: usize },
    Save(String),
    Delete { key: String, id: String },
    Forget(String),
}

impl ReviewWorkspace {
    pub fn capture_action(&mut self, receipt: anyhow::Result<ActionRecord>) {
        let result = receipt.and_then(|record| {
            record.validate()?;
            anyhow::ensure!(
                self.action_history.pending.len() < 20,
                "20 unsaved receipts are already pending; this observation was not retained"
            );
            self.action_history.pending.push(record);
            Ok(())
        });
        if let Err(error) = result {
            let prior = self.notice.as_ref().map_or("", |notice| notice.as_ref());
            self.notice = Some(format!("{prior} Receipt metadata unavailable: {error}. The performed action was not retried.").into());
        }
    }

    fn action_scope_key(&self) -> Option<String> {
        if let crate::RemoteReviewState::Loaded(snapshot) = &self.remote_review_state {
            github_key(&snapshot.reference).ok()
        } else {
            self.local_repository_snapshot
                .as_ref()
                .and_then(|snapshot| {
                    crate::local_review_history::repository_key(&snapshot.root).ok()
                })
        }
    }

    pub fn render_action_history(&self, cx: &Context<Self>) -> gpui::Div {
        let busy = matches!(self.action_history.view, View::Busy);
        let mut panel = div().flex().flex_col().gap_2()
            .child("ACTION RECEIPTS · explicit metadata saves, never replay")
            .child("Only action kind/outcome/time/revision and hashes are saved; no command text/output, comment bodies or code. Unsaved receipts are memory-only (20 max). This is not a complete crash-safe audit log. Unconfirmed means the effect is not known: do not blindly retry.");
        for receipt in &self.action_history.pending {
            let Ok(id) = receipt.id() else {
                continue;
            };
            panel = panel
                .child(receipt_text(receipt))
                .child(control(
                    cx.entity(),
                    Operation::Save(id.clone()),
                    format!("save-receipt-{id}"),
                    "Save this metadata receipt",
                    busy || self.session_store.is_none(),
                ))
                .child(control(
                    cx.entity(),
                    Operation::Forget(id.clone()),
                    format!("forget-receipt-{id}"),
                    "Discard this unsaved receipt only",
                    busy,
                ));
        }
        if let Some(key) = self.action_scope_key() {
            panel = panel.child(control(
                cx.entity(),
                Operation::Load { key, page: 0 },
                "load-action-history".to_string(),
                "Load receipts for current checkout / PR",
                busy || self.session_store.is_none(),
            ));
        }
        if busy {
            panel = panel.child("Updating local action receipts…");
        }
        if let View::Loaded { key, page, rows } = &self.action_history.view {
            panel = panel.child(format!(
                "Loaded scope {} · page {} · {} record(s)",
                crate::short_sha(key),
                page + 1,
                rows.len()
            ));
            if *page > 0 {
                panel = panel.child(control(
                    cx.entity(),
                    Operation::Load {
                        key: key.clone(),
                        page: page - 1,
                    },
                    "action-history-previous".to_string(),
                    "Previous receipt page",
                    busy,
                ));
            }
            if *page < 24 && rows.len() == 20 {
                panel = panel.child(control(
                    cx.entity(),
                    Operation::Load {
                        key: key.clone(),
                        page: page + 1,
                    },
                    "action-history-next".to_string(),
                    "Next receipt page",
                    busy,
                ));
            }
            for (id, record) in rows {
                panel = panel.child(receipt_text(record)).child(control(
                    cx.entity(),
                    Operation::Delete {
                        key: key.clone(),
                        id: id.clone(),
                    },
                    format!("delete-receipt-{id}"),
                    "Delete this saved receipt only",
                    busy,
                ));
            }
        }
        panel
    }
}

fn receipt_text(record: &ActionRecord) -> String {
    format!(
        "{:?} · {:?} · {} target(s) · time {}\nscope {} · head {:?} · remote ID {:?}\ninput {} · result {}",
        record.kind,
        record.outcome,
        record.targets,
        record.occurred_at,
        crate::short_sha(&record.scope_key),
        record.head,
        record.remote_id,
        record.input_fingerprint,
        record.result_fingerprint
    )
}

fn control(
    entity: Entity<ReviewWorkspace>,
    operation: Operation,
    id: String,
    label: &str,
    disabled: bool,
) -> Button {
    Button::new(gpui::SharedString::from(id))
        .label(label.to_string())
        .disabled(disabled)
        .on_click(move |_, _, cx| run(&entity, operation.clone(), cx))
}

fn run(entity: &Entity<ReviewWorkspace>, operation: Operation, cx: &mut App) {
    let request = entity.update(cx, |this, cx| {
        if matches!(this.action_history.view, View::Busy) {
            return None;
        }
        if let Operation::Forget(id) = &operation {
            this.action_history
                .pending
                .retain(|record| record.id().ok().as_ref() != Some(id));
            cx.notify();
            return None;
        }
        let store = this.session_store.clone()?;
        let record = if let Operation::Save(id) = &operation {
            Some(
                this.action_history
                    .pending
                    .iter()
                    .find(|record| record.id().ok().as_ref() == Some(id))?
                    .clone(),
            )
        } else {
            None
        };
        this.action_history.view = View::Busy;
        cx.notify();
        Some((store, record))
    });
    let Some((store, record)) = request else {
        return;
    };
    let entity = entity.clone();
    cx.spawn(async move |cx| {
        let result = cx.background_executor().spawn(async move {
            let (key, page, saved) = match operation {
                Operation::Save(id) => {
                    let record = record.ok_or_else(|| anyhow::anyhow!("unsaved receipt is no longer available"))?;
                    anyhow::ensure!(record.id()? == id, "unsaved receipt identity changed");
                    store.save_action_receipt(&record)?;
                    (record.scope_key, 0, Some(id))
                },
                Operation::Load { key, page } => (key, page, None),
                Operation::Delete { key, id } => {
                    store.delete_action_receipt(&key, &id)?;
                    (key, 0, None)
                },
                Operation::Forget(_) => anyhow::bail!("discard is an in-memory operation"),
            };
            let rows = store.action_history(&key, page)?;
            Ok::<_, anyhow::Error>((key, page, saved, rows))
        }).await;
        let _ = entity.update(cx, |this, cx| {
            match result {
                Ok((key, page, saved, rows)) => {
                    if let Some(id) = saved {
                        this.action_history.pending.retain(|record| record.id().ok().as_ref() != Some(&id));
                    }
                    this.action_history.view = View::Loaded { key, page, rows };
                    this.notice = Some("Action receipt history updated locally. No action was replayed.".into());
                },
                Err(error) => {
                    this.action_history.view = View::Idle;
                    this.notice = Some(format!("Receipt history unavailable: {error}. Unsaved receipts remain in memory; no action was retried.").into());
                },
            }
            cx.notify();
        });
    }).detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_is_bounded_memory_only_and_storage_errors_do_not_retry_actions() {
        let mut workspace = ReviewWorkspace::sample();
        for _ in 0..20 {
            workspace.capture_action(Ok(crate::action_history::sample()));
        }
        workspace.capture_action(Ok(crate::action_history::sample()));
        assert_eq!(workspace.action_history.pending.len(), 20);
        assert!(workspace.notice.as_ref().unwrap().contains("not retained"));
        workspace.capture_action(Err(anyhow::anyhow!("metadata failed")));
        assert_eq!(workspace.action_history.pending.len(), 20);
        assert!(matches!(workspace.action_history.view, View::Idle));
        assert!(workspace.notice.as_ref().unwrap().contains("not retried"));
    }
}
