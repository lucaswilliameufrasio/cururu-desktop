//! Product destinations and shell composition, separate from review operations.

use crate::{GitHubAuthState, RemoteReviewState, ReviewWorkspace};
use gpui::{Context, SharedString, div, prelude::*, px, rgb};
use gpui_component::{
    Disableable as _, Selectable as _,
    button::{Button, ButtonVariants as _},
};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Destination {
    #[default]
    Start,
    Review,
    GitHub,
    ReviewSettings,
    OpinionSettings,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum InspectorSection {
    #[default]
    Discussion,
    Assistance,
    Checks,
    History,
}

#[derive(Debug, Clone, Copy)]
pub struct WorkspaceLayout {
    pub navigation: gpui::Pixels,
    pub files: gpui::Pixels,
    pub inspector: gpui::Pixels,
}

impl WorkspaceLayout {
    pub fn for_width(width: gpui::Pixels) -> Self {
        if width < px(1280.0) {
            Self {
                navigation: px(176.0),
                files: px(160.0),
                inspector: px(320.0),
            }
        } else {
            Self {
                navigation: px(184.0),
                files: px(224.0),
                inspector: px(380.0),
            }
        }
    }
}

pub fn initial_window_size(display: gpui::Size<gpui::Pixels>) -> gpui::Size<gpui::Pixels> {
    gpui::size(
        (display.width - px(48.0)).max(px(1.0)).min(px(1440.0)),
        (display.height - px(48.0)).max(px(1.0)).min(px(900.0)),
    )
}

impl ReviewWorkspace {
    pub fn ensure_connection_input(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        if self.github_client_id_input.is_none() {
            let value = self.github_app_client_id.clone().unwrap_or_default();
            self.github_client_id_input = Some(cx.new(|input_cx| {
                gpui_component::input::InputState::new(window, input_cx)
                    .placeholder("Public Client ID, not the numeric App ID")
                    .default_value(value)
            }));
        }
    }
    pub fn render_inspector_navigation(&self, cx: &Context<Self>) -> gpui::Div {
        let mut tabs = div().flex().flex_wrap().gap_1().mb_4();
        for (index, (section, label)) in [
            (InspectorSection::Discussion, "Discussion"),
            (InspectorSection::Assistance, "AI"),
            (InspectorSection::Checks, "Checks"),
            (InspectorSection::History, "History"),
        ]
        .into_iter()
        .enumerate()
        {
            let entity = cx.entity();
            tabs = tabs.child(
                Button::new(("inspector-section", index))
                    .label(label)
                    .selected(self.inspector_section == section)
                    .on_click(move |_, _, cx| {
                        entity.update(cx, |this, cx| {
                            this.inspector_section = section;
                            cx.notify();
                        });
                    }),
            );
        }
        tabs
    }

    pub const fn has_review(&self) -> bool {
        self.repository_root.is_some()
            || matches!(self.remote_review_state, RemoteReviewState::Loaded(_))
    }

    pub fn render_navigation(&self, width: gpui::Pixels, cx: &Context<Self>) -> gpui::Div {
        let destinations = [
            (Destination::Start, "Start"),
            (Destination::Review, "Review"),
            (Destination::GitHub, "GitHub"),
            (Destination::ReviewSettings, "Review provider"),
            (Destination::OpinionSettings, "Second opinion"),
        ];
        let mut navigation = div()
            .flex()
            .flex_col()
            .gap_2()
            .w(width)
            .flex_shrink_0()
            .p_4()
            .bg(rgb(0x0018_222d))
            .border_r_1()
            .border_color(rgb(0x002b_3946))
            .child(
                div()
                    .text_xl()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .mb_6()
                    .child("Cururu"),
            );
        for (index, (destination, label)) in destinations.into_iter().enumerate() {
            if index == 2 {
                navigation = navigation.child(
                    div()
                        .mt_6()
                        .mb_2()
                        .text_sm()
                        .text_color(rgb(0x00a6_b5c5))
                        .child("Connections & AI"),
                );
            }
            let entity = cx.entity();
            navigation = navigation.child(
                Button::new(("destination", index))
                    .label(label)
                    .ghost()
                    .selected(self.destination == destination)
                    .disabled(destination == Destination::Review && !self.has_review())
                    .on_click(move |_, _, cx| {
                        entity.update(cx, |this, cx| {
                            this.destination = destination;
                            cx.notify();
                        });
                    }),
            );
        }
        navigation.child(div().flex_1()).child(
            div()
                .text_sm()
                .text_color(rgb(0x00a6_b5c5))
                .child("Your code. Your review."),
        )
    }

    fn page(title: &'static str, description: &'static str) -> gpui::Div {
        div()
            .flex()
            .flex_col()
            .gap_4()
            .w_full()
            .max_w(px(680.0))
            .child(
                div()
                    .text_2xl()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(title),
            )
            .child(div().text_color(rgb(0x00a6_b5c5)).child(description))
    }

    pub fn render_start(&self, cx: &Context<Self>) -> gpui::Div {
        Self::page(
            "What would you like to review?",
            "Open a local project or inspect a GitHub pull request. AI is optional.",
        )
        .child(div().mt_6().text_lg().child("Local changes"))
        .child(div().text_color(rgb(0x00a6_b5c5)).child(
            "Inspect your working tree, compare changes and prepare comments. No sign-in required.",
        ))
        .child(self.render_header_actions(cx))
        .child(div().mt_6().text_lg().child("GitHub pull request"))
        .child(div().text_color(rgb(0x00a6_b5c5)).child(
            "Connect your account, then open a pull request. Nothing is published automatically.",
        ))
        .child(Self::destination_button(
            "start-github",
            "Connect or manage GitHub",
            Destination::GitHub,
            cx,
        ))
        .children(
            matches!(self.github_auth_state, GitHubAuthState::SignedIn)
                .then(|| self.render_pull_request_loader(cx)),
        )
    }

    pub fn destination_button(
        id: &'static str,
        label: &'static str,
        destination: Destination,
        cx: &Context<Self>,
    ) -> gpui::Div {
        let entity = cx.entity();
        let button = Button::new(id)
            .label(label)
            .outline()
            .on_click(move |_, _, cx| {
                entity.update(cx, |this, cx| {
                    this.destination = destination;
                    cx.notify();
                });
            });
        div().flex().child(button)
    }

    pub fn render_github_connection(&self, cx: &Context<Self>) -> gpui::Div {
        let mut page = Self::page("Connect GitHub", "Sign in in your browser. Cururu never asks for your GitHub password.")
            .child(self.render_github_auth_status())
            .child("GitHub App Client ID")
            .children(self.github_client_id_input.as_ref().map(|input| div().flex().w_full().h(px(36.0)).child(gpui_component::input::TextInput::new(input)
                .disabled(!matches!(self.github_auth_state, GitHubAuthState::SignedOut | GitHubAuthState::Failed(_))).w(px(640.0)))))
            .child(div().text_sm().text_color(rgb(0x00a6_b5c5)).child("Public identifier, not a secret. Used for this session; no client secret or private key is needed."))
            .child(div().flex().child(self.render_github_auth_button(cx)));
        if let GitHubAuthState::AwaitingDeviceAuthorization {
            user_code,
            verification_url,
            ..
        } = &self.github_auth_state
        {
            let url = verification_url.clone();
            let code = user_code.clone();
            page = page.child(
                div()
                    .p_6()
                    .rounded_lg()
                    .bg(rgb(0x0018_222d))
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child("Enter this code on GitHub")
                    .child(
                        div()
                            .text_2xl()
                            .child(SharedString::from(user_code.clone())),
                    )
                    .child(Button::new("copy-device-code").label("Copy code").on_click(
                        move |_, _, cx| {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(code.clone()));
                        },
                    ))
                    .child(
                        Button::new("reopen-device-page")
                            .label("Open GitHub in browser")
                            .primary()
                            .on_click(move |_, _, cx| cx.open_url(&url)),
                    ),
            );
        }
        page.child(div().mt_6().text_lg().child("Alternative: GitHub CLI"))
            .child(div().text_color(rgb(0x00a6_b5c5)).child("Use an existing gh login for this session only. Disconnecting Cururu will not sign gh out."))
            .child(div().flex().child(self.render_gh_fallback_button(cx)))
            .child(Self::destination_button("github-back-start", "Open a pull request", Destination::Start, cx))
    }

    pub fn render_destination(&self, cx: &Context<Self>) -> gpui::Div {
        match self.destination {
            Destination::Start => self.render_start(cx),
            Destination::GitHub => self.render_github_connection(cx),
            Destination::ReviewSettings => Self::page("Review provider", "Optional AI assistance for your local changes. Saving settings does not send code.")
                .child(self.render_local_review_settings(cx)),
            Destination::OpinionSettings => Self::page("Second opinion", "Configure independent evaluators. Each analysis requires a separate preview and confirmation.")
                .child(self.render_evaluator_settings(cx)),
            Destination::Review => Self::page("No review open", "Choose a project or pull request to begin."),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_window_fits_small_displays_and_caps_large_ones() {
        assert_eq!(
            initial_window_size(gpui::size(px(1100.0), px(800.0))),
            gpui::size(px(1052.0), px(752.0))
        );
        assert_eq!(
            initial_window_size(gpui::size(px(3840.0), px(2160.0))),
            gpui::size(px(1440.0), px(900.0))
        );
    }

    #[test]
    fn compact_columns_leave_room_for_the_diff() {
        let layout = WorkspaceLayout::for_width(px(1052.0));
        assert!(px(1052.0) - layout.navigation - layout.files - layout.inspector >= px(390.0));
        let wide = WorkspaceLayout::for_width(px(1440.0));
        assert_eq!(wide.navigation + wide.files + wide.inspector, px(788.0));
    }

    #[test]
    fn navigation_does_not_turn_sample_data_into_a_real_review() {
        let workspace = ReviewWorkspace::sample();
        assert_eq!(workspace.destination, Destination::Start);
        assert!(!workspace.has_review());
    }

    #[test]
    fn changing_destination_preserves_review_mode_and_does_not_connect() {
        let mut workspace = ReviewWorkspace::sample();
        let mode = workspace.review_mode;
        workspace.destination = Destination::ReviewSettings;
        workspace.destination = Destination::GitHub;
        assert_eq!(workspace.review_mode, mode);
        assert!(workspace.github_credentials.is_none());
        assert!(matches!(
            workspace.github_auth_state,
            GitHubAuthState::Uninitialized
        ));
    }

    #[test]
    fn app_id_is_not_accepted_as_an_oauth_client_id() {
        assert!(
            crate::github_auth::validate_client_id("5420832")
                .unwrap_err()
                .to_string()
                .contains("numeric App ID")
        );
        assert!(crate::github_auth::validate_client_id("").is_err());
        assert!(crate::github_auth::validate_client_id("Iv1.synthetic").is_ok());
        assert!(crate::github_auth::validate_client_id("Iv1.with space").is_err());
    }
}
