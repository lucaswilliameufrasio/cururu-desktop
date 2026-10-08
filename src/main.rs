use gpui::{
    App, Application, Bounds, Context, Render, SharedString, Window, WindowBounds, WindowOptions,
    div, prelude::*, px, rgb, size, uniform_list,
};
use std::fmt::Write as _;
use std::ops::Range;

pub mod repository;

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
    selected_file: usize,
    selected_line: usize,
}

impl ReviewWorkspace {
    fn new() -> Self {
        let diff = sample_unified_diff();
        let files = cururu_core::parse_unified_diff(&diff)
            .into_iter()
            .map(parse_diff_file)
            .collect();

        Self {
            files,
            selected_file: 0,
            selected_line: 42,
        }
    }

    const fn select_file(&mut self, index: usize) {
        if index >= self.files.len() {
            return;
        }
        self.selected_file = index;
        self.selected_line = 0;
    }
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
            || raw_line.starts_with("@@ ")
        {
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x0011_1820))
            .text_color(rgb(0x00d5_dee8))
            .child(Self::render_header())
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_file_rail(cx))
                    .child(self.render_diff(cx))
                    .child(self.render_finding_panel()),
            )
    }
}

impl ReviewWorkspace {
    fn render_header() -> impl IntoElement {
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
            .child(
                div()
                    .text_color(rgb(0x009b_aaba))
                    .child("feat/checked-calculation → working tree"),
            )
    }

    fn render_file_rail(&self, cx: &Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let mut file_rows = Vec::with_capacity(self.files.len());
        for (index, file) in self.files.iter().enumerate() {
            let selected = index == self.selected_file;
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
        let entity = cx.entity();
        let file = &self.files[self.selected_file];
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
                        let lines = &this.files[this.selected_file].lines;
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

    fn render_finding_panel(&self) -> impl IntoElement {
        let file = &self.files[self.selected_file];
        let line = &file.lines[self.selected_line];
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
            .child(div().text_color(rgb(0x00e5_a84b)).child("FINDING · HIGH"))
            .child(
                div()
                    .text_lg()
                    .text_color(rgb(0x00f0_f3f6))
                    .child("Unchecked result can escape"),
            )
            .child(
                div()
                    .text_color(rgb(0x00b5_c0cb))
                    .child("The new calculation may return an invalid value without preserving the parser error."),
            )
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
            .child(
                div()
                    .mt_2()
                    .text_color(rgb(0x0078_8a9b))
                    .child("10,000 diff rows · virtualized list"),
            )
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1440.0), px(900.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| ReviewWorkspace::new()),
        )
        .unwrap();
        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use super::{DIFF_LINE_COUNT, DiffKind, ReviewWorkspace, SAMPLE_FILE_PATHS};

    #[test]
    fn review_workspace_generates_large_diff_and_initial_selection() {
        let workspace = ReviewWorkspace::new();

        assert_eq!(workspace.files.len(), SAMPLE_FILE_PATHS.len());
        assert_eq!(
            workspace
                .files
                .iter()
                .map(|file| file.lines.len())
                .sum::<usize>(),
            DIFF_LINE_COUNT
        );
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
        let mut workspace = ReviewWorkspace::new();

        workspace.select_file(1);

        assert_eq!(workspace.selected_file, 1);
        assert_eq!(workspace.selected_line, 0);
        assert_eq!(
            workspace.files[workspace.selected_file].path.as_ref(),
            SAMPLE_FILE_PATHS[1]
        );

        workspace.select_file(usize::MAX);

        assert_eq!(workspace.selected_file, 1);
        assert_eq!(workspace.selected_line, 0);
    }
}
