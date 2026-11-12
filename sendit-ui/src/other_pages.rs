//! The maintenance, activity and preferences pages.

use std::time::SystemTime;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::Input;
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::{ActiveTheme, Disableable, IconName, Sizable, StyledExt, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use send_it_vm::{PruneScope, PruneVm};

use crate::app::SenditApp;
use crate::launch::{Editor, Terminal};
use crate::ops::{FollowUp, OpState};
use crate::prefs::Appearance;
use crate::ui::{self, Tone, card, choice, field, header, mono, row, section};

impl SenditApp {
    pub fn render_maintenance(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .id("maintenance-page")
            .size_full()
            .p_6()
            .gap_6()
            .overflow_y_scrollbar()
            .child(header("Maintenance", None, div(), cx))
            .child(self.render_prune(cx))
            .child(self.render_ssh(cx))
            .child(self.render_paths(cx))
    }

    fn render_prune(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let scope = self.prune_scope;
        let plan = self
            .prune_plan
            .as_ref()
            .filter(|(for_scope, _)| *for_scope == scope)
            .map(|(_, plan)| plan.clone());
        let busy = self.busy("prune");
        let preview = match &plan {
            None => div().text_sm().child("Looking…").into_any_element(),
            Some(Err(e)) => ui::banner(e.clone(), true, cx).into_any_element(),
            Some(Ok(plan)) => {
                let group = |title: &str, vms: &[PruneVm]| {
                    (!vms.is_empty()).then(|| {
                        v_flex()
                            .gap_1()
                            .child(div().text_sm().font_medium().child(title.to_string()))
                            .children(vms.iter().map(|vm| mono(format!("  {}", vm.listed()), cx)))
                    })
                };
                v_flex()
                    .gap_3()
                    .when(plan.vms().next().is_none(), |this| {
                        this.child(div().text_sm().text_color(cx.theme().muted_foreground).child(match scope {
                            PruneScope::Missing => "No VMs of missing projects.",
                            PruneScope::Outdated => "No VMs of missing projects or made from an outdated base image.",
                            PruneScope::All => "No stopped VMs.",
                        }))
                    })
                    .children(group("Their project folder no longer exists", &plan.orphans))
                    .children(group("Made from an outdated base image", &plan.outdated))
                    .children(group("Other stopped VMs", &plan.others))
                    .children(plan.skipped.iter().map(|skipped| {
                        div().text_xs().text_color(cx.theme().muted_foreground).child(skipped.message())
                    }))
                    .into_any_element()
            }
        };
        // The VMs shown, which are the ones to delete.
        let shown: Vec<std::path::PathBuf> = plan
            .as_ref()
            .and_then(|plan| plan.as_ref().ok())
            .map_or_else(Vec::new, |plan| {
                plan.vms().map(|vm| vm.dir.path().to_path_buf()).collect()
            });
        let count = shown.len();
        section("Clean up VMs", cx).child(
            card(cx)
                .child(
                    h_flex()
                        .gap_1()
                        .children(
                            [
                                (PruneScope::Missing, "Missing projects"),
                                (PruneScope::Outdated, "… and outdated images"),
                                (PruneScope::All, "Every stopped VM"),
                            ]
                            .into_iter()
                            .map(|(option, label)| {
                                choice(SharedString::from(format!("prune-{label}")), label, scope == option).on_click(
                                    cx.listener(move |this, _, _, cx| {
                                        this.prune_scope = option;
                                        this.load_prune_plan(cx);
                                        cx.notify();
                                    }),
                                )
                            }),
                        )
                        .child(div().flex_1())
                        .child(
                            Button::new("prune-reload")
                                .ghost()
                                .small()
                                .icon(IconName::RefreshCw)
                                .on_click(cx.listener(|this, _, _, cx| this.load_prune_plan(cx))),
                        ),
                )
                .child(preview)
                .child(
                    h_flex().child(
                        Button::new("prune-go")
                            .danger()
                            .small()
                            .icon(IconName::Delete)
                            .label(match count {
                                1 => "Delete 1 VM…".to_string(),
                                n => format!("Delete {n} VMs…"),
                            })
                            .loading(busy)
                            .disabled(count == 0 || busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let shown = shown.clone();
                                this.confirm(
                                    match count {
                                        1 => "Delete 1 VM?".to_string(),
                                        n => format!("Delete {n} VMs?"),
                                    },
                                    "The VMs listed are deleted, with their disks and everything \
                                     stored in them; project folders stay.",
                                    "Delete",
                                    window,
                                    cx,
                                    move |this, window, cx| {
                                        // sendit picks the VMs again: only if they are
                                        // still those shown, which the user agreed to.
                                        let Ok(paths) = this.paths.clone() else { return };
                                        match crate::snapshot::prune_plan(&paths, scope) {
                                            Ok(now) => {
                                                let same = now
                                                    .vms()
                                                    .map(|vm| vm.dir.path())
                                                    .eq(shown.iter().map(|dir| dir.as_path()));
                                                if !same {
                                                    this.prune_plan = Some((scope, Ok(now)));
                                                    this.notify_error(
                                                        "The VMs to delete have changed since; check the list again",
                                                        window,
                                                        cx,
                                                    );
                                                    cx.notify();
                                                    return;
                                                }
                                            }
                                            Err(e) => {
                                                this.notify_error(format!("{e:#}"), window, cx);
                                                return;
                                            }
                                        }
                                        let mut args = vec!["prune".to_string()];
                                        match scope {
                                            PruneScope::Missing => {}
                                            PruneScope::Outdated => args.push("--outdated".into()),
                                            PruneScope::All => args.push("--all".into()),
                                        }
                                        args.push("--yes".into());
                                        this.run_op("Clean up VMs", args, FollowUp::None, Some("prune".into()), window, cx);
                                        this.prune_plan = None;
                                    },
                                )
                            })),
                    ),
                ),
        )
    }

    fn render_ssh(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let set_up = self.snapshot.ssh_set_up;
        section("SSH host names", cx).child(
            card(cx)
                .child(div().text_sm().child(
                    "Lets ssh, and programs that use it such as editors' remote modes or Rex, reach \
                     each running VM as sendit-<name>, e.g. `ssh sendit-my-app`, through sendit, \
                     which checks the VM's host key.",
                ))
                .child(
                    h_flex()
                        .gap_3()
                        .child(if set_up {
                            ui::tag(Tone::Good, "set up", cx)
                        } else {
                            ui::tag(Tone::Neutral, "not set up", cx)
                        })
                        .child(
                            Button::new("ssh-config")
                                .outline()
                                .small()
                                .label(if set_up { "Write the Host block again" } else { "Set up" })
                                .tooltip("Writes ~/.cache/sendit/ssh/config and includes it at the top of ~/.ssh/config")
                                .disabled(self.busy("ssh-config"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.run_op(
                                        "Set up SSH host names",
                                        vec!["ssh-config".into(), "--yes".into()],
                                        FollowUp::None,
                                        Some("ssh-config".into()),
                                        window,
                                        cx,
                                    )
                                })),
                        ),
                ),
        )
    }

    fn render_paths(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut rows = Vec::new();
        if let Ok(paths) = &self.paths {
            rows.push(("VMs", paths.vms_dir()));
            rows.push(("Base images", paths.images_dir()));
            rows.push(("Configuration", paths.config_dir()));
            rows.push(("Image files", paths.image_files_dir()));
            rows.push(("Scripts", paths.provision_scripts_dir()));
        }
        let shown = |path: &std::path::Path| {
            self.paths
                .as_ref()
                .map_or_else(|_| path.display().to_string(), |paths| paths.display(path))
        };
        section("Where things live", cx).child(
            card(cx)
                .children(rows.into_iter().enumerate().map(|(ix, (label, path))| {
                    let exists = path.exists();
                    row(
                        label,
                        h_flex()
                            .gap_2()
                            .child(mono(shown(&path), cx))
                            .when(exists, |this| {
                                this.child(
                                    Button::new(("reveal-path", ix))
                                        .ghost()
                                        .xsmall()
                                        .icon(IconName::FolderOpen)
                                        .on_click(move |_, _, cx| cx.reveal_path(&path)),
                                )
                            }),
                        cx,
                    )
                }))
                .child(row(
                    "sendit",
                    mono(
                        match (&self.sendit, &self.sendit_version) {
                            (Some(path), Some(version)) => format!("{} ({version})", shown(path)),
                            (Some(path), None) => shown(path),
                            (None, _) => "not found".to_string(),
                        },
                        cx,
                    ),
                    cx,
                )),
        )
    }

    pub fn render_activity(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let ops: Vec<_> = self.ops.iter().rev().cloned().collect();
        let selected = self
            .selected_op
            .and_then(|id| ops.iter().find(|op| op.lock().unwrap().id == id).cloned());
        let finished = ops.iter().any(|op| !op.lock().unwrap().is_running());
        let actions = Button::new("clear-ops")
            .outline()
            .small()
            .label("Clear finished")
            .disabled(!finished)
            .on_click(cx.listener(|this, _, _, cx| {
                this.ops.retain(|op| op.lock().unwrap().is_running());
                cx.notify();
            }));
        let list = v_flex()
            .id("ops")
            .w(px(300.))
            .h_full()
            .flex_shrink_0()
            .gap_1()
            .overflow_y_scrollbar()
            .when(ops.is_empty(), |this| {
                this.child(
                    div()
                        .p_3()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("What this app runs with sendit shows up here, with its output."),
                )
            })
            .children(ops.iter().map(|op| {
                let op = op.lock().unwrap();
                let id = op.id;
                let is_selected = self.selected_op == Some(id);
                v_flex()
                    .id(("op", id as usize))
                    .gap_0p5()
                    .px_3()
                    .py_2()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(if is_selected {
                        cx.theme().ring
                    } else {
                        cx.theme().border
                    })
                    .cursor_pointer()
                    .hover(|this| this.bg(cx.theme().accent))
                    .when(is_selected, |this| this.bg(cx.theme().accent))
                    .child(
                        h_flex()
                            .justify_between()
                            .gap_2()
                            .child(
                                div()
                                    .text_sm()
                                    .font_medium()
                                    .truncate()
                                    .child(op.title.clone()),
                            )
                            .child(state_tag(op.state, cx)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(ago(op.started)),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected_op = Some(id);
                        cx.notify();
                    }))
            }));
        let detail = match selected {
            None => div().into_any_element(),
            Some(op) => {
                let op_ref = op.clone();
                let op = op.lock().unwrap();
                // Follows new output to the end, as a terminal does.
                if self.op_seen != (op.id, op.output.len()) {
                    self.op_seen = (op.id, op.output.len());
                    self.op_scroll.scroll_to_bottom();
                }
                let lines: Vec<&str> = op.output.lines().collect();
                let skipped = lines.len().saturating_sub(MAX_LINES);
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .gap_2()
                    .child(
                        h_flex()
                            .justify_between()
                            .gap_2()
                            .child(
                                v_flex()
                                    .min_w_0()
                                    .child(div().font_medium().child(op.title.clone()))
                                    .child(
                                        mono(op.command.clone(), cx)
                                            .text_color(cx.theme().muted_foreground),
                                    ),
                            )
                            .child({
                                let log = op.log.clone();
                                Button::new("op-log")
                                    .ghost()
                                    .small()
                                    .icon(IconName::FileText)
                                    .label("Log file")
                                    .tooltip("All of its output, as sendit wrote it")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.open_in_editor(&log, window, cx)
                                    }))
                            })
                            .when(op.is_running(), |this| {
                                this.child(
                                    Button::new("cancel-op")
                                        .danger()
                                        .small()
                                        .label("Cancel")
                                        .tooltip("As Ctrl-C would; press again to force")
                                        .on_click(move |_, _, _| {
                                            op_ref.lock().unwrap().interrupt()
                                        }),
                                )
                            }),
                    )
                    .child(
                        div()
                            .id("op-output")
                            .flex_1()
                            .min_h_0()
                            .p_3()
                            .rounded(cx.theme().radius)
                            .bg(cx.theme().secondary)
                            .overflow_y_scroll()
                            .track_scroll(&self.op_scroll)
                            .child(
                                div()
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .text_xs()
                                    .whitespace_normal()
                                    .when(skipped > 0, |this| {
                                        this.child(
                                            div()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(format!("… {skipped} earlier lines")),
                                        )
                                    })
                                    .children(
                                        lines[skipped..]
                                            .iter()
                                            .map(|line| div().child(line.to_string())),
                                    )
                                    .when(op.output.is_empty(), |this| {
                                        this.child("No output yet.")
                                    }),
                            ),
                    )
                    .into_any_element()
            }
        };
        v_flex()
            .size_full()
            .p_6()
            .gap_4()
            .child(header("Activity", None, actions, cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .gap_4()
                    .child(list)
                    .child(detail),
            )
    }

    pub fn render_prefs(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let draft = &self.prefs_draft;
        let appearance = draft.appearance;
        let terminal = draft.terminal;
        let editor = draft.editor;
        let actions = Button::new("prefs-save")
            .primary()
            .small()
            .label("Save")
            .on_click(cx.listener(|this, _, window, cx| this.save_prefs(window, cx)));
        v_flex()
            .id("prefs-page")
            .size_full()
            .p_6()
            .gap_6()
            .overflow_y_scrollbar()
            .child(header("Preferences", Some("This app's own settings, apart from sendit's configuration.".into()), actions, cx))
            .when_some(self.prefs_error.clone(), |this, e| this.child(ui::banner(e, true, cx)))
            .child(
                section("Appearance", cx).child(
                    card(cx)
                        .child(div().text_sm().child("This app is shown in:"))
                        .child(h_flex().gap_1().children(
                            [(Appearance::System, "As macOS"), (Appearance::Light, "Light"), (Appearance::Dark, "Dark")]
                                .into_iter()
                                .map(|(option, label)| {
                                    choice(SharedString::from(format!("appearance-{label}")), label, appearance == option).on_click(
                                        cx.listener(move |this, _, window, cx| this.set_appearance(option, window, cx)),
                                    )
                                }),
                        ))
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("Takes effect at once, without Save. With As macOS, the app switches when macOS does, e.g. at sunset."),
                        ),
                ),
            )
            .child(
                section("Terminal", cx).child(
                    card(cx)
                        .child(div().text_sm().child("Shells, consoles and commands open in a new window of:"))
                        .child(h_flex().gap_1().children(
                            [(Terminal::Ghostty, "Ghostty"), (Terminal::Apple, "Terminal"), (Terminal::Custom, "Other")]
                                .into_iter()
                                .map(|(option, label)| {
                                    choice(SharedString::from(format!("term-{label}")), label, terminal == option).on_click(
                                        cx.listener(move |this, _, _, cx| {
                                            this.prefs_draft.terminal = option;
                                            cx.notify();
                                        }),
                                    )
                                }),
                        ))
                        .when(terminal == Terminal::Custom, |this| {
                            this.child(field(
                                "Command",
                                Some("Run with /bin/sh; {} is the script to run, e.g. `wezterm start -- {}` or `kitty --single-instance {}`."),
                                Input::new(&self.pref_terminal_command),
                                cx,
                            ))
                        }),
                ),
            )
            .child(
                section("Editor", cx).child(
                    card(cx)
                        .child(div().text_sm().child("Image files, the config file and console logs open in:"))
                        .child(h_flex().gap_1().children(
                            [(Editor::Terminal, "$EDITOR in the terminal"), (Editor::TextApp, "Default text app"), (Editor::Custom, "Other")]
                                .into_iter()
                                .map(|(option, label)| {
                                    choice(SharedString::from(format!("editor-{label}")), label, editor == option).on_click(
                                        cx.listener(move |this, _, _, cx| {
                                            this.prefs_draft.editor = option;
                                            cx.notify();
                                        }),
                                    )
                                }),
                        ))
                        .when(editor == Editor::Custom, |this| {
                            this.child(field(
                                "Command",
                                Some("Run with /bin/sh; {} is the file, e.g. `zed {}` or `code {}`."),
                                Input::new(&self.pref_editor_command),
                                cx,
                            ))
                        }),
                ),
            )
            .child(
                section("sendit", cx).child(
                    card(cx)
                        .child(field(
                            "sendit binary",
                            Some("Empty: next to this app, in ~/.cargo/bin, on PATH or in Homebrew's folders."),
                            Input::new(&self.pref_sendit),
                            cx,
                        ))
                        .child(field(
                            "VM folder",
                            Some("SENDIT_VM_DIR for this app and what it starts. An app started from the Finder doesn't see what your shell's profile sets, so set it here if you set it there."),
                            Input::new(&self.pref_vm_dir),
                            cx,
                        )),
                ),
            )
    }
}

/// The most lines of an operation's output shown at once.
const MAX_LINES: usize = 400;

fn state_tag(state: OpState, cx: &App) -> gpui_kit::component::tag::Tag {
    match state {
        OpState::Running => ui::tag(Tone::Busy, "running", cx),
        OpState::Succeeded => ui::tag(Tone::Good, "done", cx),
        OpState::Failed(Some(code)) => ui::tag(Tone::Bad, format!("failed ({code})"), cx),
        OpState::Failed(None) => ui::tag(Tone::Bad, "failed", cx),
    }
}

/// How long ago `time` was, roughly.
fn ago(time: SystemTime) -> String {
    let secs = time.elapsed().map_or(0, |d| d.as_secs());
    match secs {
        0..60 => "just now".to_string(),
        60..3600 => format!("{} min ago", secs / 60),
        _ => format!("{} h ago", secs / 3600),
    }
}
