use gpui::{
    App, Application, Bounds, Context, Render, SharedString, Window, WindowBounds, WindowOptions,
    div, prelude::*, px, rgb, size, uniform_list,
};
use std::ops::Range;

const DIFF_LINE_COUNT: usize = 10_000;
const LINE_HEIGHT: f32 = 24.0;

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
    lines: Vec<DiffLine>,
    selected_line: usize,
}

impl ReviewWorkspace {
    fn new() -> Self {
        let mut lines = Vec::with_capacity(DIFF_LINE_COUNT);
        let mut old_number = 1;
        let mut new_number = 1;

        for index in 0..DIFF_LINE_COUNT {
            let kind = match index % 11 {
                3 | 4 => DiffKind::Removed,
                5 | 6 => DiffKind::Added,
                _ => DiffKind::Context,
            };
            let (old, new, marker, source) = match kind {
                DiffKind::Context => {
                    let values = (
                        Some(old_number),
                        Some(new_number),
                        ' ',
                        "let result = calculate(input);",
                    );
                    old_number += 1;
                    new_number += 1;
                    values
                }
                DiffKind::Removed => {
                    let values = (
                        Some(old_number),
                        None,
                        '-',
                        "let result = calculate_legacy(input);",
                    );
                    old_number += 1;
                    values
                }
                DiffKind::Added => {
                    let values = (
                        None,
                        Some(new_number),
                        '+',
                        "let result = calculate_checked(input)?;",
                    );
                    new_number += 1;
                    values
                }
            };
            lines.push(DiffLine {
                old_number: old,
                new_number: new,
                kind,
                text: format!("{marker}{source} // review row {index}").into(),
            });
        }

        Self {
            lines,
            selected_line: 42,
        }
    }
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
                    .child(Self::render_file_rail())
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

    fn render_file_rail() -> impl IntoElement {
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
                    .child("CHANGED FILES · 8"),
            )
            .child(Self::file_row("src/engine.rs", "+82  −14", true))
            .child(Self::file_row("src/parser.rs", "+31  −8", false))
            .child(Self::file_row("src/model.rs", "+19  −2", false))
            .child(Self::file_row("tests/engine.rs", "+54  −0", false))
    }

    fn file_row(path: &'static str, summary: &'static str, selected: bool) -> impl IntoElement {
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
                    .child("src/engine.rs"),
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
    use super::{DIFF_LINE_COUNT, DiffKind, ReviewWorkspace};

    #[test]
    fn review_workspace_generates_large_diff_and_initial_selection() {
        let workspace = ReviewWorkspace::new();

        assert_eq!(workspace.lines.len(), DIFF_LINE_COUNT);
        assert_eq!(workspace.selected_line, 42);
        assert_eq!(workspace.lines[0].kind, DiffKind::Context);
        assert_eq!(workspace.lines[3].kind, DiffKind::Removed);
        assert_eq!(workspace.lines[5].kind, DiffKind::Added);
    }
}
