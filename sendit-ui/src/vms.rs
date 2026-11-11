//! The VMs page: the list of VMs, and the selected VM's details and
//! actions.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, Textarea};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::{
    ActiveTheme, Disableable, IconName, Sizable, StyledExt, WindowExt, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use send_it_vm::{
    Activity, HOST_PREFIX, Mode, ProjectDir, Stale, StatusReport, VmName, VmRow, VmState, VmStatus,
};

use crate::app::{Panel, Selection, SenditApp};
use crate::ops::FollowUp;
use crate::runopts::{RunOptions, Target};
use crate::ui::{self, card, choice, field, header, mono, row, section};

impl SenditApp {
    pub fn render_vms(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let actions = h_flex()
            .gap_2()
            .child(
                Button::new("new-vm")
                    .primary()
                    .small()
                    .icon(IconName::Plus)
                    .label("New VM…")
                    .on_click(cx.listener(|this, _, window, cx| this.pick_new_project(window, cx))),
            )
            .child(
                Button::new("refresh")
                    .outline()
                    .small()
                    .icon(IconName::RefreshCw)
                    .tooltip("Read the VMs again")
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            );
        let subtitle = match self.snapshot.vms.len() {
            0 if !self.loaded => "Loading…".to_string(),
            0 => "No VMs yet".to_string(),
            1 => "1 VM".to_string(),
            n => format!("{n} VMs"),
        };
        v_flex()
            .size_full()
            .p_6()
            .gap_4()
            .child(header("Virtual machines", Some(subtitle), actions, cx))
            .when_some(self.snapshot.vms_error.clone(), |this, error| {
                this.child(ui::banner(error, true, cx))
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .gap_4()
                    .child(self.render_vm_list(cx))
                    .child(self.render_vm_detail(window, cx)),
            )
    }

    fn render_vm_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<VmRow> = self.snapshot.vms.clone();
        let empty = rows.is_empty() && self.loaded;
        v_flex()
            .id("vm-list")
            .w(px(300.))
            .h_full()
            .flex_shrink_0()
            .gap_1()
            .overflow_y_scrollbar()
            .when(empty, |this| {
                this.child(
                    div()
                        .p_4()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(
                            "A project's VM is made the first time it starts. Pick a project \
                             folder with \"New VM…\".",
                        ),
                )
            })
            .children(rows.into_iter().enumerate().map(|(ix, vm)| {
                let selected = matches!(
                    (&self.selection, &vm.name),
                    (Some(Selection::Vm(a)), Some(b)) if a == b
                );
                let name = vm.name.clone();
                let project = match &vm.info {
                    Some(info) => {
                        let mut text = self.show_path(&info.project_path);
                        match &info.project_dir {
                            ProjectDir::Present => {}
                            ProjectDir::Missing => text += " (missing)",
                            ProjectDir::Unmounted => text += " (volume not mounted)",
                            ProjectDir::Inaccessible(_) => text += " (inaccessible)",
                        }
                        text
                    }
                    None => vm.dir.path().display().to_string(),
                };
                let image = vm.info.as_ref().map(|info| {
                    if info.earlier_build {
                        format!("{} (earlier build)", info.image)
                    } else {
                        info.image.to_string()
                    }
                });
                v_flex()
                    .id(("vm", ix))
                    .gap_1()
                    .px_3()
                    .py_2()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(if selected {
                        cx.theme().ring
                    } else {
                        cx.theme().border
                    })
                    .when(selected, |this| this.bg(cx.theme().accent))
                    .cursor_pointer()
                    .hover(|this| this.bg(cx.theme().accent))
                    .child(
                        h_flex()
                            .justify_between()
                            .gap_2()
                            .child(
                                div().font_medium().text_sm().truncate().child(
                                    name.as_ref().map_or("?".to_string(), VmName::to_string),
                                ),
                            )
                            .child(ui::vm_state_tag(vm.state)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .truncate()
                            .child(project),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .when_some(image, |this, image| this.child(image))
                            .child(format!("{} on disk", vm.disk.approx())),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(name) = &name {
                            this.select(Selection::Vm(name.clone()), cx);
                        }
                    }))
            }))
    }

    /// The row of the selected VM in the list.
    fn selected_row(&self) -> Option<&VmRow> {
        let Some(Selection::Vm(name)) = &self.selection else {
            return None;
        };
        self.snapshot
            .vms
            .iter()
            .find(|row| row.name.as_ref() == Some(name))
    }

    fn render_vm_detail(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let content = match self.selection.clone() {
            None => div()
                .p_6()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("Select a VM to see its settings and what you can do with it.")
                .into_any_element(),
            Some(Selection::Vm(name)) => match self.selected_row().cloned() {
                Some(row) => self.render_vm(name, row, window, cx).into_any_element(),
                None if self.loaded => div()
                    .p_6()
                    .text_sm()
                    .child(format!("There is no VM named {name} any more."))
                    .into_any_element(),
                None => div().into_any_element(),
            },
            Some(Selection::New(dir)) => self.render_new_vm(dir, window, cx).into_any_element(),
        };
        v_flex()
            .id("vm-detail")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scrollbar()
            .child(content)
    }

    fn render_vm(
        &mut self,
        name: VmName,
        row: VmRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let running = row.state == VmState::Running;
        let stopped = matches!(row.state, VmState::Stopped | VmState::Outdated);
        let can_start = row.state == VmState::Stopped
            && row
                .info
                .as_ref()
                .is_some_and(|info| info.project_dir == ProjectDir::Present);
        let busy = self.busy(&format!("vm:{name}"));
        let target = Target::Name(name.clone());
        let project = row.info.as_ref().map(|info| info.project_path.clone());

        let n = name.to_string();
        let primary =
            h_flex()
                .gap_2()
                .flex_wrap()
                .when(!running, |this| {
                    let target = target.clone();
                    this.child(
                        Button::new("start")
                            .primary()
                            .small()
                            .icon(IconName::Play)
                            .label("Start")
                            .tooltip("Boot the VM in the background")
                            .loading(busy)
                            .disabled(!can_start || busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let title = format!("Start {}", vm_of(&target));
                                this.start_vm(title, &target, &RunOptions::default(), window, cx)
                            })),
                    )
                    .child(
                        Button::new("start-options")
                            .outline()
                            .small()
                            .label("Start with options…")
                            .disabled(!can_start || busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.toggle_panel(Panel::StartOptions, cx)
                            })),
                    )
                })
                .when(running, |this| {
                    let stop_name = n.clone();
                    this.child(
                        Button::new("stop")
                            .danger()
                            .small()
                            .icon(IconName::Square)
                            .label("Stop")
                            .loading(busy)
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let n = &stop_name;
                                this.run_op(
                                    format!("Stop {n}"),
                                    vec!["-n".into(), n.clone(), "stop".into()],
                                    FollowUp::None,
                                    Some(format!("vm:{n}")),
                                    window,
                                    cx,
                                )
                            })),
                    )
                    .child(self.terminal_button(
                        "shell",
                        "Shell",
                        IconName::SquareTerminal,
                        &n,
                        vec!["ssh".into()],
                        project.clone(),
                        cx,
                    ))
                    .child(self.terminal_button(
                        "root-shell",
                        "Root shell",
                        IconName::SquareTerminal,
                        &n,
                        vec!["ssh".into(), "--root".into()],
                        project.clone(),
                        cx,
                    ))
                })
                .when(stopped, |this| {
                    this.child(
                        self.terminal_button(
                            "console",
                            "Console",
                            IconName::SquareTerminal,
                            &n,
                            vec!["run".into()],
                            project.clone(),
                            cx,
                        )
                        .tooltip(
                            "Boot the VM attached to its console in a terminal; exit shuts it down",
                        )
                        .disabled(!can_start || busy),
                    )
                })
                .child(
                    Button::new("run-command")
                        .outline()
                        .small()
                        .label("Run command…")
                        .disabled(busy || (!running && !can_start))
                        .on_click(
                            cx.listener(|this, _, _, cx| this.toggle_panel(Panel::RunCommand, cx)),
                        ),
                );

        let ssh_host = format!("{HOST_PREFIX}{name}");
        let secondary = h_flex()
            .gap_2()
            .flex_wrap()
            .child(
                Button::new("rename")
                    .ghost()
                    .small()
                    .label("Rename…")
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_panel(Panel::Rename, cx))),
            )
            .child(
                Button::new("worktree")
                    .ghost()
                    .small()
                    .label("Worktree…")
                    .tooltip("Check out a branch in a git worktree and start its own VM")
                    .disabled(row.info.is_none())
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_panel(Panel::Worktree, cx))),
            )
            .when_some(project.clone(), |this, project| {
                this.child(
                    Button::new("reveal")
                        .ghost()
                        .small()
                        .icon(IconName::FolderOpen)
                        .label("Show project")
                        .on_click(move |_, _, cx| cx.reveal_path(&project)),
                )
            })
            .child({
                let log = row.dir.console_log();
                Button::new("console-log")
                    .ghost()
                    .small()
                    .icon(IconName::FileText)
                    .label("Console log")
                    .tooltip("The console output of the VM's last run in the background")
                    .disabled(!log.exists())
                    .on_click(cx.listener(move |this, _, window, cx| this.open_in_editor(&log, window, cx)))
            })
            .child({
                let ssh_set_up = self.snapshot.ssh_set_up;
                Button::new("copy-ssh")
                    .ghost()
                    .small()
                    .icon(IconName::Copy)
                    .label(format!("ssh {ssh_host}"))
                    .tooltip("Copy the command that reaches the VM with ssh, scp or an editor's remote mode")
                    .on_click(move |_, window, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(format!("ssh {ssh_host}")));
                        let note = if ssh_set_up {
                            Notification::success("Copied")
                        } else {
                            Notification::warning(
                                "Copied. For ssh to know the name, set up SSH host names in Maintenance.",
                            )
                        };
                        window.push_notification(note, cx);
                    })
            })
            .child({
                let n = n.clone();
                Button::new("reset")
                    .ghost()
                    .small()
                    .icon(IconName::Delete)
                    .label("Reset…")
                    .disabled(!stopped || busy)
                    .tooltip("Delete the VM; the next start makes it again")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let n = n.clone();
                        this.confirm(
                            format!("Reset {n}?"),
                            format!(
                                "This deletes the VM {n} and everything stored in it, but not the \
                                 project folder. The next start makes it again from a fresh copy \
                                 of the base image, or for a git worktree, of its main worktree's VM."
                            ),
                            "Delete the VM",
                            window,
                            cx,
                            move |this, window, cx| {
                                this.run_op(
                                    format!("Reset {n}"),
                                    vec!["-n".into(), n.clone(), "reset".into(), "--yes".into()],
                                    FollowUp::None,
                                    Some(format!("vm:{n}")),
                                    window,
                                    cx,
                                )
                            },
                        )
                    }))
            });

        let panel = match self.panel {
            Panel::None => None,
            Panel::StartOptions => Some(
                self.render_run_form(target.clone(), false, "Start", window, cx)
                    .into_any_element(),
            ),
            Panel::Worktree => Some(
                self.render_run_form(target.clone(), true, "Start worktree VM", window, cx)
                    .into_any_element(),
            ),
            Panel::RunCommand => Some(
                self.render_run_command(&n, running, project.clone(), cx)
                    .into_any_element(),
            ),
            Panel::Rename => Some(self.render_rename(&n, cx).into_any_element()),
        };

        // Why it can't start, if it can't.
        let blocked = match (&row.state, &row.info) {
            (_, None) => Some(
                "Its metadata can't be read, so sendit doesn't know its project. Reset deletes it."
                    .to_string(),
            ),
            (VmState::Outdated, _) => Some(
                "It was made from an outdated base image and can't start any more. Reset deletes \
                 it, so that the next start makes it again from the current image, which may \
                 need building on the Images page first."
                    .to_string(),
            ),
            (_, Some(info)) => match &info.project_dir {
                ProjectDir::Present => None,
                ProjectDir::Missing => Some(
                    "Its project folder no longer exists. Reset it, or clean up such VMs in \
                     Maintenance."
                        .to_string(),
                ),
                ProjectDir::Unmounted => {
                    Some("Its project is on a volume that isn't mounted.".to_string())
                }
                ProjectDir::Inaccessible(e) => {
                    Some(format!("Its project folder can't be read: {e}"))
                }
            },
        };
        v_flex()
            .gap_4()
            .pr_3()
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(div().text_lg().font_semibold().child(n.clone()))
                    .child(ui::vm_state_tag(row.state)),
            )
            .when_some(blocked.filter(|_| !running), |this, why| {
                this.child(ui::banner(why, false, cx))
            })
            .child(primary)
            .child(secondary)
            .when_some(panel, |this, panel| this.child(panel))
            .child(self.render_status(cx))
    }

    #[allow(clippy::too_many_arguments)]
    fn terminal_button(
        &self,
        id: &'static str,
        label: &'static str,
        icon: IconName,
        name: &str,
        args: Vec<String>,
        cwd: Option<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) -> Button {
        let name = name.to_string();
        Button::new(id)
            .outline()
            .small()
            .icon(icon)
            .label(label)
            .on_click(cx.listener(move |this, _, window, cx| {
                let mut all = vec!["-n".to_string(), name.clone()];
                all.extend(args.iter().cloned());
                let hold = args.first().is_some_and(|arg| arg == "run");
                this.open_terminal(
                    format!("{name} {label}"),
                    all,
                    cwd.clone(),
                    hold,
                    window,
                    cx,
                )
            }))
    }

    fn toggle_panel(&mut self, panel: Panel, cx: &mut Context<Self>) {
        self.panel = if self.panel == panel {
            Panel::None
        } else {
            panel
        };
        cx.notify();
    }

    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match &self.status {
            None => div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("Loading…")
                .into_any_element(),
            Some((_, Err(e))) => ui::banner(e.clone(), true, cx).into_any_element(),
            Some((_, Ok(report))) => status_rows(report, cx).into_any_element(),
        };
        section("Status and settings", cx).child(card(cx).child(body))
    }

    fn render_run_command(
        &self,
        name: &str,
        running: bool,
        cwd: Option<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let boots = !running;
        let name = name.to_string();
        card(cx)
            .child(div().font_medium().child("Run a command"))
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(if boots {
                        "The VM boots, runs the command in the project directory in a terminal, and \
                         shuts down when it ends."
                    } else {
                        "The command runs in the running VM, in a login shell in the project \
                         directory, in a terminal."
                    }),
            )
            .child(Input::new(&self.command))
            .when(!boots, |this| {
                this.child(
                    Checkbox::new("command-root")
                        .label("As root, e.g. to install packages")
                        .checked(self.command_root)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.command_root = *checked;
                            cx.notify();
                        })),
                )
            })
            .child(
                h_flex().child(
                    Button::new("run-command-go")
                        .primary()
                        .small()
                        .icon(IconName::SquareTerminal)
                        .label("Run in terminal")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let command = this.command.read(cx).value().trim().to_string();
                            if command.is_empty() {
                                this.notify_error("Type a command first", window, cx);
                                return;
                            }
                            let mut args = vec!["-n".to_string(), name.clone()];
                            if boots {
                                args.push("run".into());
                            } else {
                                args.push("ssh".into());
                                if this.command_root {
                                    args.push("--root".into());
                                }
                            }
                            // Through sh, for pipes and the like, as typed.
                            args.extend(["--".into(), "sh".into(), "-c".into(), command.clone()]);
                            this.open_terminal(format!("{name}: {command}"), args, cwd.clone(), true, window, cx);
                        })),
                ),
            )
    }

    fn render_rename(&self, name: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let name = name.to_string();
        card(cx)
            .child(div().font_medium().child(format!("Rename {name}")))
            .child(field(
                "New name",
                Some("Up to 64 lowercase letters, digits and dashes, not starting with a dash. Also while it runs."),
                Input::new(&self.new_name),
                cx,
            ))
            .child(
                h_flex().child(
                    Button::new("rename-go")
                        .primary()
                        .small()
                        .label("Rename")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let text = this.new_name.read(cx).value().trim().to_string();
                            let new: VmName = match text.parse() {
                                Ok(new) => new,
                                Err(e) => {
                                    this.notify_error(format!("{e:#}"), window, cx);
                                    return;
                                }
                            };
                            this.panel = Panel::None;
                            this.run_op(
                                format!("Rename {name} to {new}"),
                                vec!["-n".into(), name.clone(), "rename".into(), new.to_string()],
                                FollowUp::Select(new),
                                Some(format!("vm:{name}")),
                                window,
                                cx,
                            )
                        })),
                ),
            )
    }

    /// The run-options form, with the worktree fields for `worktree`.
    fn render_run_form(
        &self,
        target: Target,
        worktree: bool,
        go: &'static str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let form = &self.run_form;
        let network = form.network;
        card(cx)
            .child(div().font_medium().child(if worktree {
                "Start a git worktree's VM"
            } else {
                "Start with options"
            }))
            .child(div().text_sm().text_color(cx.theme().muted_foreground).child(
                "Empty fields keep what the config file says, or sendit's defaults. These \
                 options apply to this run only, except the image, which a new VM keeps.",
            ))
            .when(worktree, |this| {
                this.child(field(
                    "Branch",
                    Some("Checked out in a worktree next to the main one, and made if it doesn't exist. Its VM starts as a copy of this one, if this one is stopped."),
                    Input::new(&form.branch),
                    cx,
                ))
                .child(h_flex().items_start().gap_3().child(div().flex_1().child(field("Start from", None, Input::new(&form.from), cx))).child(div().flex_1().child(field("Folder", None, Input::new(&form.worktree_path), cx))))
            })
            .child(
                h_flex()
                    .items_start()
                    .gap_3()
                    .child(div().flex_1().child(field("CPUs", None, Input::new(&form.cpus), cx)))
                    .child(div().flex_1().child(field("Memory", None, Input::new(&form.memory), cx)))
                    .child(div().flex_1().child(field(
                        "Image",
                        Some("For a new VM"),
                        Input::new(&form.image),
                        cx,
                    ))),
            )
            .child(field(
                "Network",
                None,
                h_flex()
                    .gap_1()
                    .child(choice("net-default", "As configured", network.is_none()).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.run_form.network = None;
                            cx.notify();
                        }),
                    ))
                    .children([
                        (Mode::Internet, "Internet only"),
                        (Mode::Offline, "None"),
                        (Mode::Open, "Open"),
                    ]
                    .into_iter()
                    .map(|(mode, label)| {
                        choice(SharedString::from(format!("net-{mode}")), label, network == Some(mode))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.run_form.network = Some(mode);
                                cx.notify();
                            }))
                    })),
                cx,
            ))
            .child(field(
                "Allow connections to",
                Some("Whatever the network says: host[:PORT] for the Mac, ADDR[/LEN][:PORT], or [IPV6[/LEN]]:PORT. One per line."),
                Textarea::new(&form.network_allow),
                cx,
            ))
            .child(field(
                "Extra shares",
                Some("HOST[:GUEST][:ro|rw], read-only unless :rw, at /mnt/<name> without GUEST. One per line."),
                Textarea::new(&form.mounts),
                cx,
            ))
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        Checkbox::new("read-only")
                            .label("Share the project read-only")
                            .checked(form.read_only)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.run_form.read_only = *checked;
                                cx.notify();
                            })),
                    )
                    .child(
                        Checkbox::new("expose-git")
                            .label("Let the VM see and change .git, including hooks and config git on the Mac runs")
                            .checked(form.expose_git)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.run_form.expose_git = *checked;
                                cx.notify();
                            })),
                    )
                    .child(
                        Checkbox::new("fresh")
                            .label("For a git worktree: make its VM from the base image, not as a copy of the main worktree's VM")
                            .checked(form.fresh)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.run_form.fresh = *checked;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                h_flex().gap_2().child(
                    Button::new("run-form-go")
                        .primary()
                        .small()
                        .icon(IconName::Play)
                        .label(go)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let options = this.run_options(worktree, cx);
                            if worktree && options.worktree.is_empty() {
                                this.notify_error("Type a branch first", window, cx);
                                return;
                            }
                            let title = match worktree {
                                true => format!("Start {} for {}", options.worktree, vm_of(&target)),
                                false => format!("Start {}", vm_of(&target)),
                            };
                            this.start_vm(title, &target, &options, window, cx);
                            this.panel = Panel::None;
                        })),
                )
                .child(
                    Button::new("run-form-clear")
                        .ghost()
                        .small()
                        .label("Clear")
                        .on_click(cx.listener(|this, _, window, cx| this.clear_run_form(window, cx))),
                ),
            )
    }

    fn run_options(&self, worktree: bool, cx: &App) -> RunOptions {
        let form = &self.run_form;
        let text = |state: &Entity<gpui_kit::component::input::InputState>| {
            state.read(cx).value().to_string()
        };
        let area = |state: &Entity<gpui_kit::component::input::TextareaState>| {
            state.read(cx).value().to_string()
        };
        RunOptions {
            cpus: text(&form.cpus),
            memory: text(&form.memory),
            image: text(&form.image),
            network: form.network,
            network_allow: area(&form.network_allow),
            mounts: area(&form.mounts),
            read_only: form.read_only,
            expose_git: form.expose_git,
            fresh: form.fresh,
            worktree: if worktree {
                text(&form.branch)
            } else {
                String::new()
            },
            from: if worktree {
                text(&form.from)
            } else {
                String::new()
            },
            worktree_path: if worktree {
                text(&form.worktree_path)
            } else {
                String::new()
            },
        }
    }

    fn clear_run_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let form = &mut self.run_form;
        for state in [
            &form.cpus,
            &form.memory,
            &form.image,
            &form.branch,
            &form.from,
            &form.worktree_path,
        ] {
            state.update(cx, |state, cx| state.set_value("", window, cx));
        }
        for state in [&form.network_allow, &form.mounts] {
            state.update(cx, |state, cx| state.set_value("", window, cx));
        }
        form.network = None;
        form.read_only = false;
        form.expose_git = false;
        form.fresh = false;
        cx.notify();
    }

    fn pick_new_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose project".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(dir) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                let canonical = send_it_vm::canonical(&dir);
                // A project that has a VM already: select that.
                let existing = this.snapshot.vms.iter().find_map(|row| {
                    let info = row.info.as_ref()?;
                    (info.project_path == canonical)
                        .then(|| row.name.clone())
                        .flatten()
                });
                match existing {
                    Some(name) => {
                        window.push_notification(
                            Notification::info(format!(
                                "{} has a VM already: {name}",
                                dir.display()
                            )),
                            cx,
                        );
                        this.select(Selection::Vm(name), cx);
                    }
                    None => {
                        this.select(Selection::New(canonical), cx);
                        this.panel = Panel::StartOptions;
                    }
                }
            });
        })
        .detach();
    }

    fn render_new_vm(
        &mut self,
        dir: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let target = Target::Dir(dir.clone());
        v_flex()
            .gap_4()
            .pr_3()
            .child(
                v_flex()
                    .child(div().text_lg().font_semibold().child("New VM"))
                    .child(mono(dir.display().to_string(), cx)),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        "The project's VM is made from its base image when it first starts, named \
                 after the folder. For a git worktree, it starts as a copy of the main \
                 worktree's VM if that one is stopped.",
                    ),
            )
            .child(self.render_run_form(target, false, "Make and start the VM", window, cx))
            .child(self.render_status(cx))
    }
}

/// How a target is named in titles.
fn vm_of(target: &Target) -> String {
    match target {
        Target::Name(name) => name.to_string(),
        Target::Dir(dir) => dir.file_name().map_or_else(
            || dir.display().to_string(),
            |name| name.display().to_string(),
        ),
    }
}

/// The VM's state in words, as `sendit status` tells it, with the app's
/// buttons in place of sendit's commands.
fn describe_vm(report: &StatusReport, show: &dyn Fn(&std::path::Path) -> String) -> String {
    let image = &report.image;
    let (activity, stale) = match &report.vm {
        VmStatus::NotCreated { copies: None } => return "not created".to_string(),
        VmStatus::NotCreated {
            copies: Some(parent),
        } => {
            let copy = format!(
                "not created; starting it copies the VM of {}",
                show(&parent.project)
            );
            return if parent.behind {
                format!(
                    "{copy}, made from an earlier build of the {image} image; Start with \
                     options…, ticking “make its VM from the base image”, makes it from the \
                     current one"
                )
            } else {
                copy
            };
        }
        VmStatus::Made { activity, stale } => (activity, stale),
    };
    let state = match activity {
        Activity::Stopped => "stopped".to_string(),
        Activity::Busy => {
            "busy, another sendit process is making, starting, copying or deleting it".to_string()
        }
        Activity::Running { ip: Some(ip) } => format!("running at {ip}"),
        Activity::Running { ip: None } => "running".to_string(),
    };
    match stale {
        None => state,
        Some(Stale::OtherImage(made_from)) => {
            format!("{state}; made from the {made_from} image, Reset… makes it again from {image}")
        }
        Some(Stale::Outdated) => {
            format!("{state}; made from an outdated base image, Reset… makes it again")
        }
        Some(Stale::EarlierBuild { shares, fresh_too }) => {
            let what = if *shares {
                format!("shares what an earlier build of the {image} image asked for")
            } else {
                format!("made from an earlier build of the {image} image")
            };
            let how = if *fresh_too {
                "Reset… and then Start with options…, ticking “make its VM from the base \
                 image”, make it again"
            } else {
                "Reset… makes it again"
            };
            format!("{state}; {what}, {how} from the current one")
        }
    }
}

fn status_rows(report: &StatusReport, cx: &App) -> Div {
    let settings = &report.settings;
    let image = &report.image;
    let home = std::env::home_dir().unwrap_or_default();
    let show = |path: &std::path::Path| match path.strip_prefix(&home) {
        Ok(rest) if !rest.as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => path.display().to_string(),
    };
    let network = settings.network.describe();
    let network_value = match &report.applied_network {
        Some(applied) if *applied != network => v_flex()
            .child(applied.clone())
            .child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("configured: {network}, from the VM's next start")),
            )
            .into_any_element(),
        _ => div().child(network).into_any_element(),
    };
    v_flex()
        .gap_2()
        .child(row(
            "State",
            div().child(describe_vm(report, &|path| path.display().to_string())),
            cx,
        ))
        .child(row(
            "Image",
            h_flex()
                .gap_2()
                .flex_wrap()
                .child(image.to_string())
                .child(ui::base_state_tag(report.base_state))
                .child(
                    div()
                        .text_color(cx.theme().muted_foreground)
                        .child(show(&report.base_dir)),
                ),
            cx,
        ))
        .child(row(
            "Project",
            mono(report.project.display().to_string(), cx),
            cx,
        ))
        .child(row("VM folder", mono(show(report.vm_dir.path()), cx), cx))
        .when_some(report.copy_of.as_ref(), |this, copy_of| {
            this.child(row(
                "Copy of",
                div().child(format!("the VM of {}", show(copy_of))),
                cx,
            ))
        })
        .child(row("CPUs", div().child(settings.cpus.to_string()), cx))
        .child(row("Memory", div().child(settings.memory.to_string()), cx))
        .child(row(
            "Disk",
            div().child(format!("{} max", settings.disk_size)),
            cx,
        ))
        .child(row(
            ".git",
            div().child(if settings.expose_git {
                "visible"
            } else {
                "hidden"
            }),
            cx,
        ))
        .child(row("Network", network_value, cx))
        .child(row(
            "Shares",
            v_flex()
                .gap_1()
                .children(settings.mounts.iter().map(|mount| {
                    mono(
                        format!(
                            "{} → {} ({})",
                            show(&mount.host),
                            mount.guest.display(),
                            mount.mode()
                        ),
                        cx,
                    )
                })),
            cx,
        ))
}
