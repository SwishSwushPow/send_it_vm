//! The app's window: its state, the refresh loop, and the actions shared
//! by the pages.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui_kit::component::WindowExt;
use gpui_kit::component::input::{InputState, TextareaState};
use gpui_kit::component::notification::Notification;
use gpui_kit::*;
use send_it_vm::{Paths, VmName};
use send_it_vm::{PrunePlan, PruneScope, StatusReport};

use crate::config_edit::{ConfigFile, Section};
use crate::launch::{self, Launch};
use crate::ops::{self, FollowUp, OpState, SharedOp};
use crate::prefs::Prefs;
use crate::runopts::{self, RunOptions, Target};
use crate::snapshot::{self, Snapshot};

/// How often the VMs and images are read again.
const REFRESH_EVERY: Duration = Duration::from_secs(3);
/// How often running operations are looked at.
const TICK: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Vms,
    Images,
    Config,
    Maintenance,
    Activity,
    Preferences,
}

/// What the VMs page shows the details of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selection {
    Vm(VmName),
    /// A project directory without a VM yet, to start one for.
    New(PathBuf),
}

/// The panel open under a VM's details.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Panel {
    None,
    StartOptions,
    Worktree,
    RunCommand,
    Rename,
}

/// The inputs of the run-options form.
pub struct RunForm {
    pub cpus: Entity<InputState>,
    pub memory: Entity<InputState>,
    pub image: Entity<InputState>,
    pub network: Option<send_it_vm::Mode>,
    pub network_allow: Entity<TextareaState>,
    pub mounts: Entity<TextareaState>,
    pub read_only: bool,
    pub expose_git: bool,
    pub fresh: bool,
    pub branch: Entity<InputState>,
    pub from: Entity<InputState>,
    pub worktree_path: Entity<InputState>,
}

/// The inputs of the config editor's form.
pub struct ConfigForm {
    pub cpus: Entity<InputState>,
    pub memory: Entity<InputState>,
    pub disk_size: Entity<InputState>,
    pub image: Entity<InputState>,
    pub network: String,
    pub network_allow: Entity<TextareaState>,
    pub mounts: Entity<TextareaState>,
    pub read_only: Option<bool>,
    pub expose_git: Option<bool>,
    pub banner: Option<bool>,
}

pub struct SenditApp {
    pub page: Page,
    pub prefs: Prefs,
    pub paths: Result<Paths, String>,
    pub sendit: Option<PathBuf>,
    pub sendit_version: Option<String>,
    pub snapshot: Snapshot,
    pub loaded: bool,
    refreshing: bool,
    refresh_requested: bool,
    last_refresh: Option<Instant>,

    // The VMs page.
    pub selection: Option<Selection>,
    pub status: Option<(Selection, Result<StatusReport, String>)>,
    pub panel: Panel,
    pub run_form: RunForm,
    pub command: Entity<InputState>,
    pub command_root: bool,
    pub new_name: Entity<InputState>,

    // The images page.
    pub provision_cpus: Entity<InputState>,
    pub provision_memory: Entity<InputState>,
    pub provision_force: bool,
    pub new_image: Entity<InputState>,

    // The config page.
    pub config: Option<Result<ConfigFile, String>>,
    /// The text of the config file as last loaded or saved.
    pub config_saved: String,
    pub config_section: Section,
    pub config_form: ConfigForm,
    pub config_error: Option<String>,
    pub new_image_section: Entity<InputState>,

    // The maintenance page.
    pub prune_scope: PruneScope,
    pub prune_plan: Option<(PruneScope, Result<PrunePlan, String>)>,

    // The activity page.
    pub ops: Vec<SharedOp>,
    next_op: u64,
    pub selected_op: Option<u64>,
    /// The output view's scroll position, and which operation's output,
    /// how long, it was last scrolled to the end of.
    pub op_scroll: ScrollHandle,
    pub op_seen: (u64, usize),

    // The preferences page.
    pub prefs_draft: Prefs,
    pub pref_terminal_command: Entity<InputState>,
    pub pref_editor_command: Entity<InputState>,
    pub pref_sendit: Entity<InputState>,
    pub pref_vm_dir: Entity<InputState>,
    pub prefs_error: Option<String>,

    /// The folder of the operations' logs.
    logs: Result<PathBuf, String>,
    _refresh: Task<()>,
}

fn input(
    window: &mut Window,
    cx: &mut Context<SenditApp>,
    placeholder: &str,
) -> Entity<InputState> {
    let placeholder = placeholder.to_string();
    cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
}

fn textarea(
    window: &mut Window,
    cx: &mut Context<SenditApp>,
    placeholder: &str,
) -> Entity<TextareaState> {
    let placeholder = placeholder.to_string();
    cx.new(|cx| {
        TextareaState::new(window, cx)
            .rows(3)
            .placeholder(placeholder)
    })
}

/// The paths sendit uses, with the VM folder from the preferences if set.
fn paths_for(prefs: &Prefs) -> Result<Paths, String> {
    let paths = match &prefs.vm_dir {
        Some(dir) => std::env::home_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot determine the home directory"))
            .and_then(|home| Paths::new(home).with_vms_dir(dir)),
        None => Paths::from_env(),
    };
    paths.map_err(|e| format!("{e:#}"))
}

impl SenditApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (prefs, prefs_error) = Prefs::load();
        let paths = paths_for(&prefs);
        let sendit = launch::find_sendit(prefs.sendit.as_deref());

        let run_form = RunForm {
            cpus: input(window, cx, "CPUs"),
            memory: input(window, cx, "Memory, e.g. 8G"),
            image: input(window, cx, "Image"),
            network: None,
            network_allow: textarea(
                window,
                cx,
                "None besides the config file's, e.g. host:11434",
            ),
            mounts: textarea(window, cx, "None besides the config file's, e.g. ~/data:rw"),
            read_only: false,
            expose_git: false,
            fresh: false,
            branch: input(window, cx, "Branch, e.g. feature/x"),
            from: input(window, cx, "Start commit [default: HEAD]"),
            worktree_path: input(window, cx, "Folder [default: <repo>.worktrees/<branch>]"),
        };
        let config_form = ConfigForm {
            cpus: input(window, cx, "default 2"),
            memory: input(window, cx, "default 4G"),
            disk_size: input(window, cx, "default 64G"),
            image: input(window, cx, "default"),
            network: String::new(),
            network_allow: textarea(window, cx, "One destination per line, e.g. host:11434"),
            mounts: textarea(window, cx, "One HOST[:GUEST][:ro|rw] per line"),
            read_only: None,
            expose_git: None,
            banner: None,
        };
        let pref_terminal_command = input(window, cx, "e.g. wezterm start -- {}");
        let pref_editor_command = input(window, cx, "e.g. zed {}");
        let pref_sendit = input(window, cx, "Found on its own");
        let pref_vm_dir = input(window, cx, "~/.sendit, or $SENDIT_VM_DIR");
        for (state, value) in [
            (&pref_terminal_command, prefs.terminal_command.clone()),
            (&pref_editor_command, prefs.editor_command.clone()),
            (
                &pref_sendit,
                prefs
                    .sendit
                    .as_ref()
                    .map_or_else(String::new, |p| p.display().to_string()),
            ),
            (
                &pref_vm_dir,
                prefs
                    .vm_dir
                    .as_ref()
                    .map_or_else(String::new, |p| p.display().to_string()),
            ),
        ] {
            state.update(cx, |state, cx| state.set_value(value, window, cx));
        }

        let refresh = cx.spawn_in(window, async move |this, cx| {
            loop {
                let job = this.update_in(cx, |this, window, cx| {
                    this.handle_finished_ops(window, cx);
                    if this.ops.iter().any(|op| op.lock().unwrap().is_running()) {
                        cx.notify();
                    }
                    this.refresh_job()
                });
                let Ok(job) = job else { break };
                if let Some((paths, selection)) = job {
                    let result = cx
                        .background_executor()
                        .spawn(async move {
                            let snapshot = snapshot::take(&paths);
                            let status = selection.map(|selection| {
                                let status = match &selection {
                                    Selection::Vm(name) => snapshot::status(&paths, name),
                                    Selection::New(dir) => snapshot::status_of_dir(&paths, dir),
                                };
                                (selection, status.map_err(|e| format!("{e:#}")))
                            });
                            (snapshot, status)
                        })
                        .await;
                    if this
                        .update(cx, |this, cx| this.apply_refresh(result.0, result.1, cx))
                        .is_err()
                    {
                        break;
                    }
                }
                cx.background_executor().timer(TICK).await;
            }
        });

        let mut app = Self {
            page: Page::Vms,
            prefs_draft: prefs.clone(),
            prefs,
            paths,
            sendit,
            sendit_version: None,
            snapshot: Snapshot::default(),
            loaded: false,
            refreshing: false,
            refresh_requested: true,
            last_refresh: None,
            selection: None,
            status: None,
            panel: Panel::None,
            run_form,
            command: input(window, cx, "Command, e.g. cargo test"),
            command_root: false,
            new_name: input(window, cx, "New name"),
            provision_cpus: input(window, cx, "CPUs for the build"),
            provision_memory: input(window, cx, "Memory for the build"),
            provision_force: false,
            new_image: input(window, cx, "New image name"),
            config: None,
            config_saved: String::new(),
            config_section: Section::Global,
            config_form,
            config_error: None,
            new_image_section: input(window, cx, "Image name"),
            prune_scope: PruneScope::Missing,
            prune_plan: None,
            ops: Vec::new(),
            next_op: 1,
            selected_op: None,
            op_scroll: ScrollHandle::new(),
            op_seen: (0, 0),
            pref_terminal_command,
            pref_editor_command,
            pref_sendit,
            pref_vm_dir,
            prefs_error,
            logs: ops::log_dir().map_err(|e| format!("{e:#}")),
            _refresh: refresh,
        };
        app.load_sendit_version(cx);
        if let Ok(logs) = &app.logs {
            ops::delete_old_logs(logs);
        }
        let this = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            this.update(cx, |this, cx| this.request_quit(window, cx))
                .unwrap_or(true)
        });
        app
    }

    /// What to read again now, if it is time.
    fn refresh_job(&mut self) -> Option<(Paths, Option<Selection>)> {
        let due = self.refresh_requested
            || self
                .last_refresh
                .is_none_or(|last| last.elapsed() >= REFRESH_EVERY);
        if !due || self.refreshing {
            return None;
        }
        let paths = self.paths.as_ref().ok()?.clone();
        self.refreshing = true;
        self.refresh_requested = false;
        Some((paths, self.selection.clone()))
    }

    fn apply_refresh(
        &mut self,
        snapshot: Snapshot,
        status: Option<(Selection, Result<StatusReport, String>)>,
        cx: &mut Context<Self>,
    ) {
        self.refreshing = false;
        self.loaded = true;
        self.last_refresh = Some(Instant::now());
        // A new VM's project gets its VM selected once it has one.
        if let Some(Selection::New(dir)) = &self.selection {
            let dir = send_it_vm::canonical(dir);
            if let Some(name) = snapshot.vms.iter().find_map(|row| {
                let info = row.info.as_ref()?;
                (info.project_path == dir)
                    .then(|| row.name.clone())
                    .flatten()
            }) {
                self.selection = Some(Selection::Vm(name));
                self.panel = Panel::None;
                self.refresh_requested = true;
            }
        }
        // Only for what is still selected.
        self.status = status.filter(|(selection, _)| Some(selection) == self.selection.as_ref());
        self.snapshot = snapshot;
        cx.notify();
    }

    /// Reads everything again at the next tick.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.refresh_requested = true;
        cx.notify();
    }

    pub fn select(&mut self, selection: Selection, cx: &mut Context<Self>) {
        if self.selection.as_ref() != Some(&selection) {
            self.selection = Some(selection);
            self.status = None;
            self.panel = Panel::None;
        }
        self.refresh(cx);
    }

    fn load_sendit_version(&mut self, cx: &mut Context<Self>) {
        self.sendit_version = None;
        let Some(sendit) = self.sendit.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let version = cx
                .background_executor()
                .spawn(async move {
                    std::process::Command::new(sendit)
                        .arg("--version")
                        .output()
                        .ok()
                        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.sendit_version = version;
                cx.notify();
            });
        })
        .detach();
    }

    /// What `SENDIT_VM_DIR` is to be for sendit: the VM folder this app
    /// shows, whether the preferences or the environment the app started
    /// with chose it, or `None` to unset it for the default. Always set or
    /// unset, so that sendit acts on the VMs the app shows.
    fn vm_dir_env(&self) -> Option<PathBuf> {
        let paths = self.paths.as_ref().ok()?;
        let dir = paths.vms_dir();
        (dir != Paths::new(paths.home().to_path_buf()).vms_dir()).then_some(dir)
    }

    /// Whether an operation with `key` runs.
    pub fn busy(&self, key: &str) -> bool {
        self.ops.iter().any(|op| {
            let op = op.lock().unwrap();
            op.is_running() && op.key.as_deref() == Some(key)
        })
    }

    /// Whether any operation runs.
    pub fn running_ops(&self) -> usize {
        self.ops
            .iter()
            .filter(|op| op.lock().unwrap().is_running())
            .count()
    }

    /// Runs `sendit args` as an operation, shown in the activity list.
    pub fn run_op(
        &mut self,
        title: impl Into<String>,
        args: Vec<String>,
        follow_up: FollowUp,
        key: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = title.into();
        let Some(sendit) = self.sendit.clone() else {
            self.notify_error("sendit not found; set its path in Preferences", window, cx);
            return;
        };
        // sendit would act on other VMs than the app can show.
        if let Err(e) = &self.paths {
            self.notify_error(format!("{title}: {e}"), window, cx);
            return;
        }
        if let Some(key) = &key
            && self.busy(key)
        {
            self.notify_error(format!("{title}: already in progress"), window, cx);
            return;
        }
        let logs = match &self.logs {
            Ok(logs) => logs.clone(),
            Err(e) => {
                self.notify_error(format!("{title}: {e}"), window, cx);
                return;
            }
        };
        let id = self.next_op;
        self.next_op += 1;
        match ops::start(
            id,
            &sendit,
            self.vm_dir_env().as_deref(),
            args,
            title.clone(),
            follow_up,
            key,
            &logs,
        ) {
            Ok(op) => {
                self.ops.push(op);
                self.selected_op = Some(id);
                window.push_notification(Notification::info(format!("{title}…")), cx);
            }
            Err(e) => self.notify_error(format!("{title}: {e:#}"), window, cx),
        }
        self.refresh(cx);
    }

    pub fn notify_error(
        &self,
        message: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.push_notification(Notification::error(message), cx);
    }

    /// Deals with operations that have ended: says how they went, and for
    /// a start that sendit refused without asking, asks and starts again.
    fn handle_finished_ops(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut ended = Vec::new();
        for op in &self.ops {
            let mut op = op.lock().unwrap();
            if !op.is_running() && !op.handled {
                op.handled = true;
                ended.push((
                    op.title.clone(),
                    op.state,
                    op.output.clone(),
                    op.follow_up.clone(),
                ));
            }
        }
        if !ended.is_empty() && (self.page == Page::Maintenance || self.prune_plan.is_none()) {
            self.load_prune_plan(cx);
        }
        for (title, state, output, follow_up) in ended {
            self.refresh(cx);
            if state == OpState::Succeeded {
                if let FollowUp::Select(name) = follow_up {
                    self.select(Selection::Vm(name), cx);
                }
                window.push_notification(Notification::success(format!("{title}: done")), cx);
                continue;
            }
            if let FollowUp::Start { args } = &follow_up {
                if let Some(shares) = ops::refused_shares(&output) {
                    self.ask_to_share(title, args.clone(), shares, window, cx);
                    continue;
                }
                if let Some(message) = ops::refused_copy(&output) {
                    self.ask_to_start_fresh(title, args.clone(), message, window, cx);
                    continue;
                }
            }
            let last = output
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("")
                .trim_start_matches("Error: ")
                .to_string();
            window.push_notification(
                Notification::error(last).title(format!("{title} failed")),
                cx,
            );
        }
    }

    fn ask_to_share(
        &mut self,
        title: String,
        args: Vec<String>,
        shares: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity();
        let list = shares.join("\n");
        window.open_alert_dialog(cx, move |alert, _, _| {
            let this = this.clone();
            let args = args.clone();
            let title = title.clone();
            alert
                .title("Share these anyway?")
                .description(format!(
                    "These shares let the VM get around its limits, e.g. become root or run \
                     commands on this Mac:\n\n{list}"
                ))
                .show_cancel(true)
                .ok_text("Share and start")
                .on_ok(move |_, window, cx| {
                    let args = runopts::retry_with(&args, "--yes");
                    let key = start_key(&args);
                    this.update(cx, |this, cx| {
                        this.run_op(
                            title.clone(),
                            args.clone(),
                            FollowUp::Start { args },
                            key,
                            window,
                            cx,
                        )
                    });
                    true
                })
        });
    }

    fn ask_to_start_fresh(
        &mut self,
        title: String,
        args: Vec<String>,
        message: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let this = this.clone();
            let args = args.clone();
            let title = title.clone();
            alert
                .title("The main worktree's VM is running")
                .description(format!(
                    "{message}.\n\nA copy of a running VM could miss what it hasn't written to \
                     its disk yet. Start this worktree's VM from the base image instead, or \
                     cancel, stop the main worktree's VM and start again."
                ))
                .show_cancel(true)
                .ok_text("Start from the image")
                .on_ok(move |_, window, cx| {
                    let args = runopts::retry_with(&args, "--fresh");
                    let key = start_key(&args);
                    this.update(cx, |this, cx| {
                        this.run_op(
                            title.clone(),
                            args.clone(),
                            FollowUp::Start { args },
                            key,
                            window,
                            cx,
                        )
                    });
                    true
                })
        });
    }

    /// Starts `target`'s VM in the background with `options`.
    pub fn start_vm(
        &mut self,
        title: String,
        target: &Target,
        options: &RunOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match options.args(target) {
            Ok(args) => {
                let key = start_key(&args);
                self.run_op(
                    title,
                    args.clone(),
                    FollowUp::Start { args },
                    key,
                    window,
                    cx,
                );
            }
            Err(e) => self.notify_error(format!("{e:#}"), window, cx),
        }
    }

    /// Opens `args` of sendit in a new terminal window.
    pub fn open_terminal(
        &self,
        title: String,
        args: Vec<String>,
        cwd: Option<PathBuf>,
        hold: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(sendit) = &self.sendit else {
            self.notify_error("sendit not found; set its path in Preferences", window, cx);
            return;
        };
        if let Err(e) = &self.paths {
            self.notify_error(e.clone(), window, cx);
            return;
        }
        // The terminal starts with its own environment, not the app's, in
        // which a shell's profile may set another.
        let mut argv = vec!["/usr/bin/env".to_string()];
        match self.vm_dir_env() {
            Some(dir) => argv.push(format!("SENDIT_VM_DIR={}", dir.display())),
            None => argv.extend(["-u".to_string(), "SENDIT_VM_DIR".to_string()]),
        }
        argv.push(sendit.display().to_string());
        argv.extend(args);
        let launch = Launch {
            argv,
            cwd,
            title,
            hold,
        };
        if let Err(e) =
            launch::open_in_terminal(self.prefs.terminal, &self.prefs.terminal_command, &launch)
        {
            self.notify_error(format!("{e:#}"), window, cx);
        }
    }

    pub fn open_in_editor(&self, file: &Path, window: &mut Window, cx: &mut App) {
        if let Err(e) = launch::open_in_editor(
            self.prefs.editor,
            &self.prefs.editor_command,
            self.prefs.terminal,
            &self.prefs.terminal_command,
            file,
        ) {
            self.notify_error(format!("{e:#}"), window, cx);
        }
    }

    /// Asks before doing something that can't be undone.
    pub fn confirm(
        &self,
        title: impl Into<SharedString>,
        description: impl Into<SharedString>,
        ok: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let this = cx.entity();
        let (title, description, ok) = (title.into(), description.into(), ok.into());
        let then = std::rc::Rc::new(then);
        window.open_alert_dialog(cx, move |alert, _, _| {
            let this = this.clone();
            let then = then.clone();
            alert
                .title(title.clone())
                .description(description.clone())
                .show_cancel(true)
                .ok_text(ok.clone())
                .on_ok(move |_, window, cx| {
                    this.update(cx, |this, cx| then(this, window, cx));
                    true
                })
        });
    }

    pub fn set_page(&mut self, page: Page, window: &mut Window, cx: &mut Context<Self>) {
        self.page = page;
        match page {
            Page::Config if !self.config_dirty(cx) => self.load_config(window, cx),
            Page::Maintenance => self.load_prune_plan(cx),
            _ => {}
        }
        cx.notify();
    }

    /// Quits, or asks first while operations run, which go on without the
    /// app. Returns whether the app may quit right away.
    pub fn request_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let running = self.running_ops();
        if running == 0 {
            cx.quit();
            return true;
        }
        let what = match running {
            1 => "1 operation runs".to_string(),
            n => format!("{n} operations run"),
        };
        let logs = self
            .logs
            .as_ref()
            .map_or_else(|_| "their logs".to_string(), |dir| self.show_path(dir));
        self.confirm(
            format!("Quit while {what}?"),
            format!(
                "They go on without the app, which can't show how they end then. Their output \
                 goes on in {logs}. To stop them instead, cancel them in Activity first."
            ),
            "Quit",
            window,
            cx,
            |_, _, cx| cx.quit(),
        );
        false
    }

    /// Reads what `prune` would delete for the chosen scope.
    pub fn load_prune_plan(&mut self, cx: &mut Context<Self>) {
        let Ok(paths) = self.paths.clone() else {
            return;
        };
        let scope = self.prune_scope;
        cx.spawn(async move |this, cx| {
            let plan =
                cx.background_executor()
                    .spawn(async move {
                        snapshot::prune_plan(&paths, scope).map_err(|e| format!("{e:#}"))
                    })
                    .await;
            let _ = this.update(cx, |this, cx| {
                this.prune_plan = Some((scope, plan));
                cx.notify();
            });
        })
        .detach();
    }

    /// Applies the preferences' draft, and saves it.
    pub fn save_prefs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = |state: &Entity<InputState>, cx: &App| state.read(cx).value().trim().to_string();
        let home = std::env::home_dir().unwrap_or_default();
        let path = |text: String| {
            (!text.is_empty()).then(|| Paths::new(home.clone()).expand_tilde(Path::new(&text)))
        };
        let mut prefs = self.prefs_draft.clone();
        prefs.terminal_command = text(&self.pref_terminal_command, cx);
        prefs.editor_command = text(&self.pref_editor_command, cx);
        prefs.sendit = path(text(&self.pref_sendit, cx));
        prefs.vm_dir = path(text(&self.pref_vm_dir, cx));
        if let Some(sendit) = &prefs.sendit
            && launch::find_sendit(Some(sendit)).is_none()
        {
            self.prefs_error = Some(format!("{} is not an executable file", sendit.display()));
            cx.notify();
            return;
        }
        let paths = paths_for(&prefs);
        if let Err(e) = &paths {
            self.prefs_error = Some(format!("VM folder: {e}"));
            cx.notify();
            return;
        }
        if let Err(e) = prefs.save() {
            self.prefs_error = Some(format!("{e:#}"));
            cx.notify();
            return;
        }
        self.prefs_error = None;
        self.sendit = launch::find_sendit(prefs.sendit.as_deref());
        self.paths = paths;
        self.prefs_draft = prefs.clone();
        self.prefs = prefs;
        self.load_sendit_version(cx);
        window.push_notification(Notification::success("Preferences saved"), cx);
        self.refresh(cx);
    }

    /// `path` as sendit shows it, e.g. with `~` for the home folder.
    pub fn show_path(&self, path: &Path) -> String {
        match &self.paths {
            Ok(paths) => paths.display(path),
            Err(_) => path.display().to_string(),
        }
    }

    /// The config file, as loaded for the config page.
    pub fn config_path(&self) -> Option<PathBuf> {
        self.paths.as_ref().ok().map(Paths::config_file)
    }
}

/// The key of the operation that starts the VM `args` are for, so that it
/// isn't started twice at once.
pub fn start_key(args: &[String]) -> Option<String> {
    match args {
        [flag, value, ..] if flag == "-n" || flag == "-C" => {
            let worktree = args
                .iter()
                .position(|arg| arg == "--worktree")
                .and_then(|i| args.get(i + 1));
            Some(match worktree {
                Some(branch) => format!("vm:{value}:{branch}"),
                None => format!("vm:{value}"),
            })
        }
        _ => None,
    }
}
