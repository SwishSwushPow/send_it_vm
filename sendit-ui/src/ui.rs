//! The window's layout, the sidebar, and small building blocks the pages
//! share.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{
    ActiveTheme, ColorName, IconName, Selectable, Sizable, StyledExt, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use send_it_vm::BaseState;
use send_it_vm::VmState;

use crate::app::{Page, SenditApp};

/// A page's heading, with its actions on the right.
pub fn header(title: &str, subtitle: Option<String>, actions: impl IntoElement, cx: &App) -> Div {
    h_flex()
        .w_full()
        .justify_between()
        .items_center()
        .gap_4()
        .pb_3()
        .border_b_1()
        .border_color(cx.theme().border)
        .child(
            v_flex()
                .child(div().text_xl().font_semibold().child(title.to_string()))
                .when_some(subtitle, |this, subtitle| {
                    this.child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(subtitle),
                    )
                }),
        )
        .child(h_flex().gap_2().flex_wrap().child(actions))
}

/// A titled group of content.
pub fn section(title: &str, cx: &App) -> Div {
    v_flex().gap_2().child(
        div()
            .text_xs()
            .font_semibold()
            .text_color(cx.theme().muted_foreground)
            .child(title.to_uppercase()),
    )
}

/// A row of a label and its value.
pub fn row(label: &str, value: impl IntoElement, cx: &App) -> Div {
    h_flex()
        .items_start()
        .gap_3()
        .text_sm()
        .child(
            div()
                .w(px(120.))
                .flex_shrink_0()
                .text_color(cx.theme().muted_foreground)
                .child(label.to_string()),
        )
        .child(div().flex_1().min_w_0().child(value))
}

/// Text in the monospace font.
pub fn mono(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .font_family(cx.theme().mono_font_family.clone())
        .text_sm()
        .child(text.into())
}

/// A field of a form: its label above it, and a hint below.
pub fn field(label: &str, hint: Option<&str>, input: impl IntoElement, cx: &App) -> Div {
    v_flex()
        .gap_1()
        .child(div().text_sm().font_medium().child(label.to_string()))
        .child(input)
        .when_some(hint, |this, hint| {
            this.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(hint.to_string()),
            )
        })
}

/// A box around a panel's content.
pub fn card(cx: &App) -> Div {
    v_flex()
        .gap_3()
        .p_4()
        .rounded(cx.theme().radius)
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().background)
}

/// A message across the page, e.g. an error.
pub fn banner(message: impl Into<SharedString>, danger: bool, cx: &App) -> Div {
    let color = if danger {
        cx.theme().danger
    } else {
        cx.theme().warning
    };
    h_flex()
        .w_full()
        .min_w_0()
        .items_start()
        .gap_2()
        .p_3()
        .rounded(cx.theme().radius)
        .border_1()
        .border_color(color)
        .text_sm()
        .child(
            Icon::new(IconName::TriangleAlert)
                .flex_shrink_0()
                .text_color(color),
        )
        .child(div().flex_1().min_w_0().child(message.into()))
}

/// What a tag says about the state it shows, which sets its color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Good,
    Busy,
    Warning,
    Bad,
    Neutral,
}

/// A small tag with `text`, colored for `tone`. Solid in the light theme;
/// tinted in the dark one, whose solid tags put dark text on a light color.
pub fn tag(tone: Tone, text: impl Into<SharedString>, cx: &App) -> Tag {
    let tag = if cx.theme().is_dark() {
        match tone {
            Tone::Good => Tag::color(ColorName::Green),
            Tone::Busy => Tag::color(ColorName::Cyan),
            Tone::Warning => Tag::color(ColorName::Yellow),
            Tone::Bad => Tag::color(ColorName::Red),
            Tone::Neutral => Tag::color(ColorName::Neutral),
        }
    } else {
        match tone {
            Tone::Good => Tag::success(),
            Tone::Busy => Tag::info(),
            Tone::Warning => Tag::warning(),
            Tone::Bad => Tag::danger(),
            Tone::Neutral => Tag::secondary(),
        }
    };
    tag.small().child(text.into())
}

pub fn vm_state_tag(state: VmState, cx: &App) -> Tag {
    let tone = match state {
        VmState::Running => Tone::Good,
        VmState::Busy => Tone::Busy,
        VmState::Outdated => Tone::Warning,
        VmState::Stopped => Tone::Neutral,
    };
    tag(tone, state.as_str(), cx)
}

pub fn base_state_tag(state: BaseState, cx: &App) -> Tag {
    let (tone, text) = match state {
        BaseState::Missing => (Tone::Neutral, "not provisioned"),
        BaseState::Outdated => (Tone::Bad, "outdated"),
        BaseState::ScriptsChanged => (Tone::Warning, "scripts changed"),
        BaseState::Current => (Tone::Good, "provisioned"),
    };
    tag(tone, text, cx)
}

/// One of a set of buttons that picks one option.
pub fn choice(id: impl Into<ElementId>, label: &str, selected: bool) -> Button {
    let button = Button::new(id)
        .small()
        .label(label.to_string())
        .selected(selected);
    if selected {
        button.primary()
    } else {
        button.outline()
    }
}

impl Render for SenditApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let page = match self.page {
            Page::Vms => self.render_vms(window, cx).into_any_element(),
            Page::Images => self.render_images(window, cx).into_any_element(),
            Page::Config => self.render_config(window, cx).into_any_element(),
            Page::Maintenance => self.render_maintenance(window, cx).into_any_element(),
            Page::Activity => self.render_activity(window, cx).into_any_element(),
            Page::Preferences => self.render_prefs(window, cx).into_any_element(),
        };
        let mut problems = Vec::new();
        if self.sendit.is_none() {
            problems.push(
                "sendit wasn't found next to this app, in ~/.cargo/bin or on PATH: set its \
                 path in Preferences to start, stop or build anything."
                    .to_string(),
            );
        }
        if let Err(e) = &self.paths {
            problems.push(format!("sendit's folders can't be used: {e}"));
        }
        h_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_sidebar(cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .children(
                        problems
                            .into_iter()
                            .map(|problem| div().px_6().pt_4().child(banner(problem, true, cx))),
                    )
                    .child(div().flex_1().min_h_0().child(page)),
            )
    }
}

impl SenditApp {
    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let running_vms = self
            .snapshot
            .vms
            .iter()
            .filter(|row| row.state == VmState::Running)
            .count();
        let running_ops = self.running_ops();
        let items = [
            (
                Page::Vms,
                IconName::LayoutDashboard,
                "VMs",
                match running_vms {
                    0 => format!("{}", self.snapshot.vms.len()),
                    n => format!("{n} running"),
                },
            ),
            (
                Page::Images,
                IconName::HardDrive,
                "Images",
                format!("{}", self.snapshot.images.len()),
            ),
            (
                Page::Config,
                IconName::Settings2,
                "Configuration",
                String::new(),
            ),
            (
                Page::Maintenance,
                IconName::Inbox,
                "Maintenance",
                String::new(),
            ),
            (
                Page::Activity,
                IconName::SquareTerminal,
                "Activity",
                match running_ops {
                    0 => String::new(),
                    n => format!("{n} running"),
                },
            ),
            (
                Page::Preferences,
                IconName::Settings,
                "Preferences",
                String::new(),
            ),
        ];
        v_flex()
            .w(px(220.))
            .h_full()
            .flex_shrink_0()
            .bg(cx.theme().sidebar)
            .text_color(cx.theme().sidebar_foreground)
            .border_r_1()
            .border_color(cx.theme().sidebar_border)
            .child(
                v_flex()
                    .px_4()
                    .pt_10()
                    .pb_4()
                    .child(div().text_lg().font_bold().child("Send it"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("Per-project Debian VMs"),
                    ),
            )
            .child(v_flex().px_2().gap_1().children(items.into_iter().map(
                |(page, icon, label, badge)| {
                    let selected = self.page == page;
                    h_flex()
                        .id(SharedString::from(format!("nav-{label}")))
                        .gap_2()
                        .px_3()
                        .py_1p5()
                        .rounded(cx.theme().radius)
                        .text_sm()
                        .cursor_pointer()
                        .when(selected, |this| {
                            this.bg(cx.theme().sidebar_accent)
                                .text_color(cx.theme().sidebar_accent_foreground)
                                .font_medium()
                        })
                        .hover(|this| this.bg(cx.theme().sidebar_accent))
                        .child(Icon::new(icon).small())
                        .child(div().flex_1().child(label))
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(badge),
                        )
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.set_page(page, window, cx)),
                        )
                },
            )))
            .child(div().flex_1())
            .child(
                div()
                    .p_4()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(match (&self.sendit_version, self.loaded) {
                        (Some(version), _) => version.clone(),
                        (None, true) => "sendit".to_string(),
                        (None, false) => "Loading…".to_string(),
                    }),
            )
    }
}

/// Lets `Icon` be used with the component library's names.
pub use gpui_kit::component::Icon;
