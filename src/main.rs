use gpui::{
    App, Application, Bounds, Context, Entity, PathPromptOptions, Render, SharedString, Window,
    WindowBounds, WindowOptions, div, prelude::*, px, rgb, size, uniform_list,
};
use gpui_component::input::{InputState, TextInput};
use std::{fmt::Write as _, ops::Range, path::PathBuf};

pub mod github_auth;
pub mod repository;
pub mod session_store;

const DIFF_LINE_COUNT: usize = 10_000;
const LINE_HEIGHT: f32 = 24.0;
const SAMPLE_FILE_PATHS: [&str; 4] = [
    "src/engine.rs",
    "src/parser.rs",
    "src/model.rs",
    "tests/engine.rs",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffKind {
    Context,
    Added,
    Removed,
}

struct DiffLine {
    old_number: Option<usize>,
    new_number: Option<usize>,
    kind: DiffKind,
    text: SharedString,
}

struct DiffFile {
    path: SharedString,
    lines: Vec<DiffLine>,
    added: usize,
    removed: usize,
}

struct ReviewWorkspace {
    files: Vec<DiffFile>,
    repository_root: Option<SharedString>,
    branch: Option<SharedString>,
    load_error: Option<SharedString>,
    notice: Option<SharedString>,
    is_opening_repository: bool,
    is_saving_draft: bool,
    deleting_draft_id: Option<i64>,
    session_store: Option<session_store::SessionStore>,
    last_session: Option<session_store::StoredSession>,
    session_store_error: Option<SharedString>,
    head_oid: Option<String>,
    comment_drafts: Vec<session_store::StoredCommentDraft>,
    editing_draft: Option<session_store::StoredCommentDraft>,
    draft_input: Option<Entity<InputState>>,
    selected_file: Option<usize>,
    selected_line: usize,
}

impl ReviewWorkspace {
    fn sample() -> Self {
        let diff = sample_unified_diff();
        let files = cururu_core::parse_unified_diff(&diff)
            .into_iter()
            .map(parse_diff_file)
            .collect();

        Self {
            files,
            repository_root: None,
            branch: None,
            load_error: None,
            notice: None,
            is_opening_repository: false,
            is_saving_draft: false,
            deleting_draft_id: None,
            session_store: None,
            last_session: None,
            session_store_error: None,
            head_oid: None,
            comment_drafts: Vec::new(),
            editing_draft: None,
            draft_input: None,
            selected_file: Some(0),
            selected_line: 42,
        }
    }

    fn from_cli() -> Self {
        let mut workspace = Self::sample();
        match session_store::SessionStore::open_default() {
            Ok(store) => {
                match store.most_recent() {
                    Ok(session) => workspace.last_session = session,
                    Err(error) => {
                        workspace.session_store_error =
                            Some(format!("Could not read saved sessions: {error}").into());
                    }
                }
                workspace.session_store = Some(store);
            }
            Err(error) => {
                workspace.session_store_error =
                    Some(format!("Local session storage is unavailable: {error}").into());
            }
        }
        if let Some(path) = std::env::args_os().nth(1) {
            let result = load_repository_session(path.into(), workspace.session_store.clone());
            workspace.apply_repository_result(result);
        }
        workspace
    }

    fn apply_repository_result(&mut self, result: anyhow::Result<OpenedRepository>) {
        self.is_opening_repository = false;
        match result {
            Ok(opened) => {
                let snapshot = opened.snapshot;
                self.head_oid.clone_from(&snapshot.head_oid);
                let files = snapshot
                    .changed_files
                    .into_iter()
                    .map(parse_diff_file)
                    .collect::<Vec<_>>();
                let selected_file = (!files.is_empty()).then_some(0);
                self.files = files;
                self.repository_root = Some(snapshot.root.display().to_string().into());
                self.branch = snapshot.branch.map(Into::into);
                self.load_error = None;
                self.notice = Some("Repository opened · local access is read-only".into());
                if let Some(session_result) = opened.session_result {
                    match session_result {
                        Ok(session) => {
                            self.last_session = Some(session);
                            self.session_store_error = None;
                        }
                        Err(error) => {
                            self.session_store_error = Some(
                                format!("Repository opened, but session was not saved: {error}")
                                    .into(),
                            );
                        }
                    }
                }
                if let Some(drafts_result) = opened.drafts_result {
                    match drafts_result {
                        Ok(drafts) => {
                            self.comment_drafts = drafts;
                            self.editing_draft = None;
                        }
                        Err(error) => {
                            self.session_store_error = Some(
                                format!(
                                    "Repository opened, but drafts could not be loaded: {error}"
                                )
                                .into(),
                            );
                        }
                    }
                }
                self.selected_file = selected_file;
                self.selected_line = 0;
            }
            Err(error) => {
                let message = format!("Could not open repository: {error}");
                self.load_error = Some(message.clone().into());
                self.notice = Some(message.into());
            }
        }
    }

    fn select_file(&mut self, index: usize) {
        if index < self.files.len() {
            self.selected_file = Some(index);
            self.selected_line = 0;
            self.editing_draft = None;
        }
    }
}

struct OpenedRepository {
    snapshot: repository::RepositorySnapshot,
    session_result: Option<anyhow::Result<session_store::StoredSession>>,
    drafts_result: Option<anyhow::Result<Vec<session_store::StoredCommentDraft>>>,
}

struct DraftSave {
    store: session_store::SessionStore,
    repository_root: PathBuf,
    base_head_oid: Option<String>,
    current_head_oid: Option<String>,
    draft: cururu_core::ReviewCommentDraft,
    expected_revision: Option<i64>,
}

fn load_repository_session(
    path: PathBuf,
    store: Option<session_store::SessionStore>,
) -> anyhow::Result<OpenedRepository> {
    let snapshot = repository::RepositorySnapshot::open(path)?;
    let (session_result, drafts_result) = match store {
        Some(store) => {
            let session_result = store.record_opened(
                &snapshot.root,
                snapshot.branch.as_deref(),
                snapshot.head_oid.as_deref(),
            );
            let drafts_result = store.comment_drafts(&snapshot.root, snapshot.head_oid.as_deref());
            (Some(session_result), Some(drafts_result))
        }
        None => (None, None),
    };
    Ok(OpenedRepository {
        snapshot,
        session_result,
        drafts_result,
    })
}

fn start_repository_picker(
    cx: &App,
    entity: Entity<ReviewWorkspace>,
    store: Option<session_store::SessionStore>,
) {
    let receiver = cx.prompt_for_paths(PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some("Open repository".into()),
    });
    cx.spawn(async move |cx| {
        let paths = match receiver.await {
            Ok(Ok(Some(paths))) => paths,
            Ok(Ok(None)) => {
                finish_repository_picker(&entity, cx, None);
                return;
            }
            Ok(Err(error)) => {
                finish_repository_picker(
                    &entity,
                    cx,
                    Some(format!("Folder picker failed: {error}")),
                );
                return;
            }
            Err(error) => {
                finish_repository_picker(
                    &entity,
                    cx,
                    Some(format!("Folder picker did not complete: {error}")),
                );
                return;
            }
        };
        let Some(path) = paths.into_iter().next() else {
            finish_repository_picker(&entity, cx, None);
            return;
        };

        let _ = entity.update(cx, |this, cx| {
            this.notice = Some("Scanning repository changes…".into());
            cx.notify();
        });
        let result = cx
            .background_executor()
            .spawn(async move { load_repository_session(path, store) })
            .await;
        let _ = entity.update(cx, |this, cx| {
            this.apply_repository_result(result);
            cx.notify();
        });
    })
    .detach();
}

fn start_reopen_last_session(
    cx: &App,
    entity: Entity<ReviewWorkspace>,
    path: PathBuf,
    store: Option<session_store::SessionStore>,
) {
    cx.spawn(async move |cx| {
        let _ = entity.update(cx, |this, cx| {
            this.notice = Some("Scanning repository changes…".into());
            cx.notify();
        });
        let result = cx
            .background_executor()
            .spawn(async move { load_repository_session(path, store) })
            .await;
        let _ = entity.update(cx, |this, cx| {
            this.apply_repository_result(result);
            cx.notify();
        });
    })
    .detach();
}

fn finish_repository_picker(
    entity: &Entity<ReviewWorkspace>,
    cx: &mut gpui::AsyncApp,
    notice: Option<String>,
) {
    let _ = entity.update(cx, |this, cx| {
        this.is_opening_repository = false;
        this.notice = notice.map(Into::into);
        cx.notify();
    });
}

fn parse_diff_file(file: cururu_core::ChangedFile) -> DiffFile {
    let mut lines = Vec::with_capacity(file.right_lines.len());
    let mut old_number = 1;
    let mut right_lines = file.right_lines.into_iter();
    let mut added = 0;
    let mut removed = 0;

    for raw_line in file.patch.lines() {
        if raw_line.starts_with("diff --git ")
            || raw_line.starts_with("--- ")
            || raw_line.starts_with("+++ ")
        {
            continue;
        }
        if let Some(start) = hunk_old_start(raw_line) {
            old_number = start;
            continue;
        }
        let Some(marker) = raw_line.chars().next() else {
            continue;
        };
        let kind = match marker {
            ' ' => DiffKind::Context,
            '+' => DiffKind::Added,
            '-' => DiffKind::Removed,
            _ => continue,
        };
        let old = match kind {
            DiffKind::Context | DiffKind::Removed => {
                let number = Some(old_number);
                old_number += 1;
                number
            }
            DiffKind::Added => None,
        };
        let new = match kind {
            DiffKind::Context | DiffKind::Added => right_lines.next().map(|line| line as usize),
            DiffKind::Removed => None,
        };
        if kind == DiffKind::Added {
            added += 1;
        } else if kind == DiffKind::Removed {
            removed += 1;
        }
        lines.push(DiffLine {
            old_number: old,
            new_number: new,
            kind,
            text: raw_line.to_string().into(),
        });
    }

    DiffFile {
        path: file.path.into(),
        lines,
        added,
        removed,
    }
}

fn hunk_old_start(header: &str) -> Option<usize> {
    let range = header
        .strip_prefix("@@ ")?
        .split_whitespace()
        .find(|range| range.starts_with('-'))?
        .strip_prefix('-')?;
    range.split(',').next()?.parse().ok()
}

fn sample_unified_diff() -> String {
    let mut diff = String::new();
    let rows_per_file = DIFF_LINE_COUNT / SAMPLE_FILE_PATHS.len();
    let extra_rows = DIFF_LINE_COUNT % SAMPLE_FILE_PATHS.len();

    for (file_index, path) in SAMPLE_FILE_PATHS.iter().enumerate() {
        let row_count = rows_per_file + usize::from(file_index < extra_rows);
        let mut body = String::new();
        let mut old_count = 0;
        let mut new_count = 0;

        for index in 0..row_count {
            let (marker, source) = match (index + file_index) % 11 {
                3 | 4 => {
                    old_count += 1;
                    ('-', "let result = calculate_legacy(input);")
                }
                5 | 6 => {
                    new_count += 1;
                    ('+', "let result = calculate_checked(input)?;")
                }
                _ => {
                    old_count += 1;
                    new_count += 1;
                    (' ', "let result = calculate(input);")
                }
            };
            writeln!(body, "{marker}{source} // {path} row {index}")
                .expect("writing a line into a String cannot fail");
        }

        writeln!(
            diff,
            "diff --git a/{path} b/{path}\nindex 0000000..1111111 100644\n--- a/{path}\n+++ b/{path}\n@@ -1,{old_count} +1,{new_count} @@"
        )
        .expect("writing file metadata into a String cannot fail");
        diff.push_str(&body);
    }

    diff
}

impl Render for ReviewWorkspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.draft_input.is_none() {
            self.draft_input = Some(cx.new(|input_cx| {
                InputState::new(window, input_cx)
                    .multi_line()
                    .rows(5)
                    .placeholder("Write a local review comment…")
            }));
        }
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x0011_1820))
            .text_color(rgb(0x00d5_dee8))
            .child(self.render_header(cx))
            .children(self.notice.clone().map(|notice| {
                div()
                    .px_4()
                    .py_2()
                    .text_color(rgb(0x00b8_dfc4))
                    .child(notice)
            }))
            .children(self.session_store_error.clone().map(|error| {
                div()
                    .px_4()
                    .py_2()
                    .text_color(rgb(0x00f0_a090))
                    .child(error)
            }))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_file_rail(cx))
                    .child(self.render_diff(cx))
                    .child(self.render_finding_panel(cx)),
            )
    }
}

impl ReviewWorkspace {
    fn render_header(&self, cx: &Context<Self>) -> impl IntoElement {
        let subtitle = match (&self.repository_root, &self.branch) {
            (Some(root), Some(branch)) => format!("{root} · {branch} → working tree"),
            (Some(root), None) => format!("{root} · detached HEAD → working tree"),
            (None, None) => "synthetic review workspace".to_string(),
            (None, Some(branch)) => format!("{branch} → working tree"),
        };
        let entity = cx.entity();
        let store = self.session_store.clone();
        let reopen_button = self.last_session.clone().map(|session| {
            let entity = entity.clone();
            let path = session.repository_root;
            let store = store.clone();
            div()
                .id("reopen-last-session")
                .px_3()
                .py_2()
                .bg(rgb(0x0020_2b36))
                .cursor_pointer()
                .child("Reopen last session")
                .on_click(move |_event, _window, cx| {
                    let entity = entity.clone();
                    let can_open = entity.update(cx, |this, cx| {
                        if this.is_opening_repository {
                            return false;
                        }
                        this.is_opening_repository = true;
                        cx.notify();
                        true
                    });
                    if can_open {
                        start_reopen_last_session(cx, entity, path.clone(), store.clone());
                    }
                })
        });
        let open_button = div()
            .id("open-repository")
            .px_3()
            .py_2()
            .bg(rgb(if self.is_opening_repository {
                0x001b_2732
            } else {
                0x0026_3442
            }))
            .cursor_pointer()
            .child(if self.is_opening_repository {
                "Opening repository…"
            } else {
                "Open repository…"
            })
            .on_click(move |_event, _window, cx| {
                let entity = entity.clone();
                let can_open = entity.update(cx, |this, cx| {
                    if this.is_opening_repository {
                        return false;
                    }
                    this.is_opening_repository = true;
                    this.notice = Some("Choose a repository folder to open".into());
                    cx.notify();
                    true
                });
                if can_open {
                    start_repository_picker(cx, entity, store.clone());
                }
            });
        div()
            .flex()
            .items_center()
            .justify_between()
            .h(px(52.0))
            .px_4()
            .border_b_1()
            .border_color(rgb(0x002b_3946))
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(div().text_color(rgb(0x00e5_a84b)).child("CURURU"))
                    .child(div().text_color(rgb(0x009b_aaba)).child("review workspace")),
            )
            .child(div().text_color(rgb(0x009b_aaba)).child(subtitle))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .children(reopen_button)
                    .child(open_button),
            )
    }

    fn render_file_rail(&self, cx: &Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let mut file_rows = Vec::with_capacity(self.files.len());
        for (index, file) in self.files.iter().enumerate() {
            let selected = Some(index) == self.selected_file;
            let path = file.path.clone();
            let summary = format!("+{}  −{}", file.added, file.removed);
            let entity = entity.clone();
            file_rows.push(
                div()
                    .id(("changed-file", index))
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .bg(rgb(if selected { 0x0026_3442 } else { 0x0015_1e27 }))
                    .cursor_pointer()
                    .on_click(move |_event, _window, cx| {
                        entity.update(cx, |this, cx| {
                            this.select_file(index);
                            cx.notify();
                        });
                    })
                    .child(div().text_color(rgb(0x00d5_dee8)).child(path))
                    .child(div().text_color(rgb(0x008c_a392)).child(summary)),
            );
        }
        div()
            .flex()
            .flex_col()
            .w(px(224.0))
            .flex_shrink_0()
            .border_r_1()
            .border_color(rgb(0x002b_3946))
            .bg(rgb(0x0015_1e27))
            .child(
                div()
                    .px_3()
                    .py_3()
                    .text_color(rgb(0x009b_aaba))
                    .child(format!("CHANGED FILES · {}", self.files.len())),
            )
            .children(file_rows)
    }

    fn render_diff(&self, cx: &Context<Self>) -> impl IntoElement {
        let Some(selected_file) = self.selected_file else {
            let message = self
                .load_error
                .clone()
                .unwrap_or_else(|| "No changed files in this working tree.".to_string().into());
            return div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .justify_center()
                .items_center()
                .text_color(rgb(0x009b_aaba))
                .child(message);
        };
        let entity = cx.entity();
        let file = &self.files[selected_file];
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .child(
                div()
                    .h(px(38.0))
                    .px_3()
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(rgb(0x002b_3946))
                    .child(file.path.clone()),
            )
            .child(
                uniform_list(
                    "review-diff-lines",
                    file.lines.len(),
                    cx.processor(move |this, range: Range<usize>, _window, _cx| {
                        let mut elements = Vec::with_capacity(range.len());
                        let lines = &this.files[selected_file].lines;
                        for index in range {
                            let line = &lines[index];
                            let is_selected = index == this.selected_line;
                            let (background, foreground) = match line.kind {
                                DiffKind::Context => (0x0011_1820, 0x00c5_ced8),
                                DiffKind::Added => (0x0018_3126, 0x00b8_dfc4),
                                DiffKind::Removed => (0x003a_2227, 0x00e5_b6ba),
                            };
                            let background = if is_selected { 0x0034_4558 } else { background };
                            let entity = entity.clone();
                            elements.push(
                                div()
                                    .id(index)
                                    .flex()
                                    .items_center()
                                    .h(px(LINE_HEIGHT))
                                    .bg(rgb(background))
                                    .cursor_pointer()
                                    .on_click(move |_event, _window, cx| {
                                        entity.update(cx, |this, cx| {
                                            this.selected_line = index;
                                            this.editing_draft = None;
                                            cx.notify();
                                        });
                                    })
                                    .child(div().w(px(52.0)).text_color(rgb(0x0071_8191)).child(
                                        line.old_number.map_or_else(String::new, |n| n.to_string()),
                                    ))
                                    .child(div().w(px(52.0)).text_color(rgb(0x0071_8191)).child(
                                        line.new_number.map_or_else(String::new, |n| n.to_string()),
                                    ))
                                    .child(
                                        div()
                                            .flex_1()
                                            .whitespace_nowrap()
                                            .text_color(rgb(foreground))
                                            .child(line.text.clone()),
                                    ),
                            );
                        }
                        elements
                    }),
                )
                .flex_1(),
            )
    }

    fn render_finding_panel(&self, cx: &Context<Self>) -> impl IntoElement {
        let Some(selected_file) = self.selected_file else {
            return div()
                .w(px(300.0))
                .flex_shrink_0()
                .border_l_1()
                .border_color(rgb(0x002b_3946))
                .bg(rgb(0x0015_1e27));
        };
        let file = &self.files[selected_file];
        let line = &file.lines[self.selected_line];
        let (label, title, description) = if self.repository_root.is_some() {
            (
                "LOCAL CHANGE",
                "Working-tree diff",
                "No automated finding is attached to this line.",
            )
        } else {
            (
                "SYNTHETIC FINDING · HIGH",
                "Unchecked result can escape",
                "The new calculation may return an invalid value without preserving the parser error.",
            )
        };
        div()
            .flex()
            .flex_col()
            .w(px(300.0))
            .flex_shrink_0()
            .border_l_1()
            .border_color(rgb(0x002b_3946))
            .bg(rgb(0x0015_1e27))
            .p_4()
            .gap_3()
            .child(div().text_color(rgb(0x00e5_a84b)).child(label))
            .child(div().text_lg().text_color(rgb(0x00f0_f3f6)).child(title))
            .child(div().text_color(rgb(0x00b5_c0cb)).child(description))
            .child(
                div()
                    .mt_2()
                    .border_t_1()
                    .border_color(rgb(0x002b_3946))
                    .pt_3()
                    .text_color(rgb(0x009b_aaba))
                    .child(format!("Selected diff row {}", self.selected_line + 1)),
            )
            .child(
                div()
                    .bg(rgb(0x0020_2b36))
                    .p_2()
                    .text_color(rgb(0x00b8_dfc4))
                    .child(line.text.clone()),
            )
            .child(self.render_draft_controls(cx, file.path.as_ref(), line.new_number))
            .child(
                div()
                    .mt_2()
                    .text_color(rgb(0x0078_8a9b))
                    .child(format!("{} diff rows · virtualized list", file.lines.len())),
            )
    }

    fn render_draft_controls(
        &self,
        cx: &Context<Self>,
        path: &str,
        selected_line: Option<usize>,
    ) -> impl IntoElement {
        let can_comment = self.repository_root.is_some()
            && (selected_line.is_some() || self.editing_draft.is_some());
        let draft_input = can_comment
            .then(|| self.draft_input.as_ref().map(TextInput::new))
            .flatten();
        let is_saving_draft = self.is_saving_draft;
        let save_draft_button = if can_comment {
            let entity = cx.entity();
            let input = self.draft_input.clone();
            let store = self.session_store.clone();
            let root = self
                .repository_root
                .as_ref()
                .map(|root| PathBuf::from(root.as_ref()));
            let selected_path = path.to_string();
            let selected_line_number = selected_line.and_then(|number| u32::try_from(number).ok());
            let head_oid = self.head_oid.clone();
            let editing_draft = self.editing_draft.clone();
            let expected_revision = editing_draft.as_ref().map(|draft| draft.revision);
            let (path, line_number, base_head_oid) = editing_draft.as_ref().map_or_else(
                || (selected_path, selected_line_number, head_oid.clone()),
                |draft| {
                    (
                        draft.draft.path.clone(),
                        draft.draft.line,
                        draft.base_head_oid.clone(),
                    )
                },
            );
            Some(
                div()
                    .id("save-comment-draft")
                    .px_3()
                    .py_2()
                    .bg(rgb(if is_saving_draft {
                        0x001b_2732
                    } else {
                        0x0026_3442
                    }))
                    .cursor_pointer()
                    .child(if is_saving_draft {
                        "Saving draft…"
                    } else {
                        "Save comment draft"
                    })
                    .on_click(move |_event, _window, cx| {
                        if is_saving_draft {
                            return;
                        }
                        let (Some(input), Some(store), Some(root), Some(line)) =
                            (input.clone(), store.clone(), root.clone(), line_number)
                        else {
                            return;
                        };
                        let body = input.read(cx).value().to_string();
                        if body.trim().is_empty() {
                            entity.update(cx, |this, cx| {
                                this.notice =
                                    Some("Write a comment before saving the draft.".into());
                                cx.notify();
                            });
                            return;
                        }
                        let draft = cururu_core::ReviewCommentDraft {
                            path: path.clone(),
                            line: Some(line),
                            body,
                        };
                        persist_comment_draft(
                            entity.clone(),
                            DraftSave {
                                store,
                                repository_root: root,
                                base_head_oid: base_head_oid.clone(),
                                current_head_oid: head_oid.clone(),
                                draft,
                                expected_revision,
                            },
                            cx,
                        );
                    }),
            )
        } else {
            None
        };
        let draft_rows = self.render_saved_drafts(cx);
        div()
            .flex()
            .flex_col()
            .gap_2()
            .children(draft_input)
            .children(save_draft_button)
            .children(draft_rows)
    }

    fn render_saved_drafts(
        &self,
        cx: &Context<Self>,
    ) -> impl IntoIterator<Item = impl IntoElement> {
        self.comment_drafts.iter().map(|draft| {
            let status = if draft.is_stale {
                "STALE · saved against another HEAD"
            } else {
                "Saved locally · not published"
            };
            let entity = cx.entity();
            let edit_entity = entity.clone();
            let input = self.draft_input.clone();
            let selected_draft = draft.clone();
            let draft_id = draft.id;
            let store = self.session_store.clone();
            let root = self
                .repository_root
                .as_ref()
                .map(|root| PathBuf::from(root.as_ref()));
            let is_deleting = self.deleting_draft_id == Some(draft.id);
            div()
                .id(("comment-draft", draft_id.unsigned_abs()))
                .p_2()
                .bg(rgb(0x0020_2b36))
                .child(
                    div()
                        .id(("comment-draft-body", draft_id.unsigned_abs()))
                        .cursor_pointer()
                        .on_click(move |_event, window, cx| {
                            if let Some(input) = input.clone() {
                                let body = selected_draft.draft.body.clone();
                                input.update(cx, |state, cx| state.set_value(body, window, cx));
                            }
                            edit_entity.update(cx, |this, cx| {
                                let draft_path = selected_draft.draft.path.as_str();
                                let draft_line = selected_draft
                                    .draft
                                    .line
                                    .and_then(|line| usize::try_from(line).ok());
                                if let Some(file_index) = this
                                    .files
                                    .iter()
                                    .position(|file| file.path.as_ref() == draft_path)
                                    && let Some(line_index) = this.files[file_index]
                                        .lines
                                        .iter()
                                        .position(|line| line.new_number == draft_line)
                                {
                                    this.selected_file = Some(file_index);
                                    this.selected_line = line_index;
                                }
                                this.editing_draft = Some(selected_draft.clone());
                                this.notice = Some("Draft loaded into the editor.".into());
                                cx.notify();
                            });
                        })
                        .child(format!(
                            "{}:{} · {status}",
                            draft.draft.path,
                            draft.draft.line.unwrap_or_default()
                        ))
                        .child(
                            div()
                                .text_color(rgb(0x00b5_c0cb))
                                .child(draft.draft.body.clone()),
                        ),
                )
                .child(
                    div()
                        .id(("delete-comment-draft", draft_id.unsigned_abs()))
                        .px_2()
                        .py_1()
                        .text_color(rgb(0x00e5_a84b))
                        .cursor_pointer()
                        .child(if is_deleting {
                            "Deleting…"
                        } else {
                            "Delete draft"
                        })
                        .on_click(move |_event, _window, cx| {
                            if is_deleting {
                                return;
                            }
                            let (Some(store), Some(root)) = (store.clone(), root.clone()) else {
                                return;
                            };
                            delete_comment_draft(entity.clone(), store, root, draft_id, cx);
                        }),
                )
        })
    }
}

fn delete_comment_draft(
    entity: Entity<ReviewWorkspace>,
    store: session_store::SessionStore,
    root: PathBuf,
    draft_id: i64,
    cx: &mut App,
) {
    entity.update(cx, |this, cx| {
        this.deleting_draft_id = Some(draft_id);
        this.notice = Some("Deleting local draft…".into());
        cx.notify();
    });
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move { store.delete_comment_draft(&root, draft_id) })
            .await;
        let _ = entity.update(cx, |this, cx| {
            this.deleting_draft_id = None;
            match result {
                Ok(true) => {
                    this.comment_drafts.retain(|draft| draft.id != draft_id);
                    if this
                        .editing_draft
                        .as_ref()
                        .is_some_and(|draft| draft.id == draft_id)
                    {
                        this.editing_draft = None;
                    }
                    this.notice = Some("Local draft deleted.".into());
                }
                Ok(false) => {
                    this.comment_drafts.retain(|draft| draft.id != draft_id);
                    this.notice = Some("Draft was already deleted in another app instance.".into());
                }
                Err(error) => {
                    this.session_store_error =
                        Some(format!("Could not delete comment draft: {error}").into());
                }
            }
            cx.notify();
        });
    })
    .detach();
}

fn persist_comment_draft(entity: Entity<ReviewWorkspace>, save: DraftSave, cx: &mut App) {
    entity.update(cx, |this, cx| {
        this.is_saving_draft = true;
        this.notice = Some("Saving comment draft locally…".into());
        cx.notify();
    });
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move {
                save.store.save_comment_draft(
                    &save.repository_root,
                    save.base_head_oid.as_deref(),
                    save.current_head_oid.as_deref(),
                    &save.draft,
                    save.expected_revision,
                )
            })
            .await;
        let _ = entity.update(cx, |this, cx| {
            this.is_saving_draft = false;
            match result {
                Ok(saved) => {
                    this.comment_drafts.retain(|draft| draft.id != saved.id);
                    this.editing_draft = Some(saved.clone());
                    this.comment_drafts.push(saved);
                    this.session_store_error = None;
                    this.notice = Some("Draft saved locally · not published".into());
                }
                Err(error) => {
                    this.session_store_error =
                        Some(format!("Could not save comment draft: {error}").into());
                }
            }
            cx.notify();
        });
    })
    .detach();
}

fn main() {
    Application::new().run(|cx: &mut App| {
        gpui_component::init(cx);
        let bounds = Bounds::centered(None, size(px(1440.0), px(900.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| ReviewWorkspace::from_cli()),
        )
        .unwrap();
        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use super::{DIFF_LINE_COUNT, DiffKind, ReviewWorkspace, SAMPLE_FILE_PATHS};
    use std::fs;

    #[test]
    fn review_workspace_generates_large_diff_and_initial_selection() {
        let workspace = ReviewWorkspace::sample();

        assert_eq!(workspace.files.len(), SAMPLE_FILE_PATHS.len());
        assert_eq!(
            workspace
                .files
                .iter()
                .map(|file| file.lines.len())
                .sum::<usize>(),
            DIFF_LINE_COUNT
        );
        assert_eq!(workspace.selected_file, Some(0));
        assert_eq!(workspace.selected_line, 42);
        let file = &workspace.files[0];
        assert_eq!(file.path.as_ref(), SAMPLE_FILE_PATHS[0]);
        assert_eq!(file.lines[0].kind, DiffKind::Context);
        assert_eq!(file.lines[3].kind, DiffKind::Removed);
        assert_eq!(file.lines[5].kind, DiffKind::Added);
        assert_eq!(file.lines[0].old_number, Some(1));
        assert_eq!(file.lines[0].new_number, Some(1));
        assert_eq!(file.lines[5].old_number, None);
        assert_eq!(file.lines[5].new_number, Some(4));
        let mut anchored_lines = 0;
        for line in &file.lines {
            match line.kind {
                DiffKind::Context | DiffKind::Added => {
                    assert!(line.new_number.is_some());
                    anchored_lines += 1;
                }
                DiffKind::Removed => assert!(line.new_number.is_none()),
            }
        }
        assert_eq!(anchored_lines, file.lines.len() - file.removed);
    }

    #[test]
    fn selecting_a_file_resets_the_diff_selection_and_rejects_invalid_indices() {
        let mut workspace = ReviewWorkspace::sample();

        workspace.select_file(1);

        assert_eq!(workspace.selected_file, Some(1));
        assert_eq!(workspace.selected_line, 0);
        assert_eq!(
            workspace.files[workspace.selected_file.unwrap()]
                .path
                .as_ref(),
            SAMPLE_FILE_PATHS[1]
        );

        workspace.select_file(usize::MAX);

        assert_eq!(workspace.selected_file, Some(1));
        assert_eq!(workspace.selected_line, 0);
    }

    #[test]
    fn real_unified_hunk_offsets_are_used_for_old_side_line_numbers() {
        let diff = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -20,2 +40,2 @@\n-old\n+new\n context\n";
        let file = cururu_core::parse_unified_diff(diff).remove(0);
        let file = super::parse_diff_file(file);

        assert_eq!(file.lines[0].old_number, Some(20));
        assert_eq!(file.lines[0].new_number, None);
        assert_eq!(file.lines[1].old_number, None);
        assert_eq!(file.lines[1].new_number, Some(40));
        assert_eq!(file.lines[2].old_number, Some(21));
        assert_eq!(file.lines[2].new_number, Some(41));
    }

    #[test]
    fn opening_a_local_repository_replaces_the_sample_workspace() {
        let directory = tempfile::tempdir().unwrap();
        gix::init(directory.path()).unwrap();
        fs::write(directory.path().join("notes.txt"), "local change\n").unwrap();
        let opened = super::load_repository_session(directory.path().to_path_buf(), None).unwrap();
        let mut workspace = ReviewWorkspace::sample();

        workspace.apply_repository_result(Ok(opened));

        assert_eq!(workspace.files.len(), 1);
        assert_eq!(workspace.files[0].path.as_ref(), "notes.txt");
        assert_eq!(workspace.selected_file, Some(0));
        assert_eq!(workspace.selected_line, 0);
        assert!(workspace.repository_root.is_some());
        assert!(workspace.notice.as_ref().unwrap().contains("read-only"));
    }

    #[test]
    fn opening_a_repository_records_it_for_explicit_session_resume() {
        let directory = tempfile::tempdir().unwrap();
        let repository_root = directory.path().join("repo");
        gix::init(&repository_root).unwrap();
        fs::write(repository_root.join("notes.txt"), "local change\n").unwrap();
        let store = super::session_store::SessionStore::open(
            directory.path().join("app-data/sessions.sqlite3"),
        )
        .unwrap();

        let opened =
            super::load_repository_session(repository_root.clone(), Some(store.clone())).unwrap();
        let mut workspace = ReviewWorkspace::sample();
        workspace.apply_repository_result(Ok(opened));

        let saved = store.most_recent().unwrap().unwrap();
        assert_eq!(saved.repository_root, repository_root);
        assert_eq!(saved.branch.as_deref(), Some("main"));
        assert_eq!(workspace.last_session, Some(saved));
    }
}
