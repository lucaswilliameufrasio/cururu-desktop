use gpui::{
    App, Application, Bounds, Context, Render, SharedString, Window, WindowBounds, WindowOptions,
    div, prelude::*, px, rgb, size, uniform_list,
};
use std::fmt::Write as _;
use std::ops::Range;

const DIFF_LINE_COUNT: usize = 10_000;
const LINE_HEIGHT: f32 = 24.0;
const SAMPLE_FILE_PATH: &str = "src/engine.rs";

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

struct ReviewWorkspace {
    path: SharedString,
    lines: Vec<DiffLine>,
    selected_line: usize,
}

impl ReviewWorkspace {
    fn new() -> Self {
        let diff = sample_unified_diff();
        let file = cururu_core::parse_unified_diff(&diff)
            .into_iter()
            .next()
            .expect("the generated unified diff contains one file");
        let mut lines = Vec::with_capacity(DIFF_LINE_COUNT);
        let mut old_number = 1;
        let mut right_lines = file.right_lines.into_iter();

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
            lines.push(DiffLine {
                old_number: old,
                new_number: new,
                kind,
                text: raw_line.to_string().into(),
            });
        }

        Self {
            path: file.path.into(),
            lines,
            selected_line: 42,
        }
    }
}

fn sample_unified_diff() -> String {
    let mut body = String::new();
    let mut old_count = 0;
    let mut new_count = 0;

    for index in 0..DIFF_LINE_COUNT {
        let (marker, source) = match index % 11 {
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
        writeln!(body, "{marker}{source} // review row {index}")
            .expect("writing a line into a String cannot fail");
    }

    format!(
        "diff --git a/{SAMPLE_FILE_PATH} b/{SAMPLE_FILE_PATH}\nindex 0000000..1111111 100644\n--- a/{SAMPLE_FILE_PATH}\n+++ b/{SAMPLE_FILE_PATH}\n@@ -1,{old_count} +1,{new_count} @@\n{body}"
    )
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
                    .child(self.render_file_rail())
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

    fn render_file_rail(&self) -> impl IntoElement {
        let added = self
            .lines
            .iter()
            .filter(|line| line.kind == DiffKind::Added)
            .count();
        let removed = self
            .lines
            .iter()
            .filter(|line| line.kind == DiffKind::Removed)
            .count();
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
                    .child("CHANGED FILES · 1"),
            )
            .child(Self::file_row(
                self.path.clone(),
                format!("+{added}  −{removed}"),
                true,
            ))
    }

    fn file_row(path: SharedString, summary: String, selected: bool) -> impl IntoElement {
        let background = if selected { 0x0026_3442 } else { 0x0015_1e27 };
        div()
            .flex()
            .items_center()
            .justify_between()
            .px_3()
            .py_2()
            .bg(rgb(background))
            .child(div().text_color(rgb(0x00d5_dee8)).child(path))
            .child(div().text_color(rgb(0x008c_a392)).child(summary))
    }

    fn render_diff(&self, cx: &Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
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
                    .child(self.path.clone()),
            )
            .child(
                uniform_list(
                    "review-diff-lines",
                    self.lines.len(),
                    cx.processor(move |this, range: Range<usize>, _window, _cx| {
                        let mut elements = Vec::with_capacity(range.len());
                        for index in range {
                            let line = &this.lines[index];
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
        let line = &self.lines[self.selected_line];
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
    use super::{DIFF_LINE_COUNT, DiffKind, ReviewWorkspace, SAMPLE_FILE_PATH};

    #[test]
    fn review_workspace_generates_large_diff_and_initial_selection() {
        let workspace = ReviewWorkspace::new();

        assert_eq!(workspace.lines.len(), DIFF_LINE_COUNT);
        assert_eq!(workspace.selected_line, 42);
        assert_eq!(workspace.path.as_ref(), SAMPLE_FILE_PATH);
        assert_eq!(workspace.lines[0].kind, DiffKind::Context);
        assert_eq!(workspace.lines[3].kind, DiffKind::Removed);
        assert_eq!(workspace.lines[5].kind, DiffKind::Added);
        assert_eq!(workspace.lines[0].old_number, Some(1));
        assert_eq!(workspace.lines[0].new_number, Some(1));
        assert_eq!(workspace.lines[5].old_number, None);
        assert_eq!(workspace.lines[5].new_number, Some(4));
        let mut anchored_lines = 0;
        for line in &workspace.lines {
            match line.kind {
                DiffKind::Context | DiffKind::Added => {
                    assert!(line.new_number.is_some());
                    anchored_lines += 1;
                }
                DiffKind::Removed => assert!(line.new_number.is_none()),
            }
        }
        assert_eq!(anchored_lines, 8_182);
    }
}
