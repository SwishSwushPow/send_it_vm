//! Per-project VMs, created from the base image on first `run`.
//!
//! A project VM's disk and EFI variables are APFS clones of the base image's,
//! so creating one is instant and takes no space until the guest writes. The
//! machine identifier and MAC address are not copied: `vm::config` generates
//! new ones on first boot and saves them, so every VM has its own. The VM is
//! built in `.<id>.partial/` and only renamed into place once complete.
//!
//! The VM of a linked git worktree is instead a copy of its main worktree's
//! VM, if that is stopped: it starts with everything installed and cached
//! there. A running VM's disk changes under the copy and the guest's page
//! cache isn't on it yet, so a copy then would only be as good as after a
//! power cut; `run` offers to start from the base image instead.
//!
//! While a VM runs, its `sendit run` process holds a lock on `run/lock` and
//! has written its PID there; that is how the other commands find it.
//!
//! `run --detach` runs the VM in a second `sendit run` process, which it
//! starts in a session of its own, without a terminal, and leaves running
//! once the VM answers over SSH. It is sendit run again rather than a fork:
//! forking without exec isn't safe once Objective-C is loaded.
//!
//! Each VM has a name of its own, kept in its metadata, by which `-n` finds
//! it from anywhere: its folder's name, or for a linked git worktree, its
//! main worktree's VM's name and the worktree's folder name, with a number
//! added if another VM has that name already. VMs made before sendit named
//! them get one when a command next shows or looks up names, or makes or
//! runs a VM. Names are given out and changed while holding a lock, so no
//! two VMs get the same.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{IsTerminal, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::commands;
use crate::config::{ByteSize, GUEST_USER, VmSettings};
use crate::image;
use crate::mounts;
use crate::paths::{ImageName, Paths, Project, VmName, is_project_id};
use crate::provision::{self, BaseState, ScriptMount};
use crate::util;
use crate::vm::{self, ConsoleMode, VmDir, VmSpec, net};
use crate::worktree;

/// How long `stop` waits for the VM to go away. `run` forces the VM off if
/// the guest hasn't shut down after `vm::STOP_TIMEOUT`.
const STOP_WAIT: Duration = Duration::from_secs(45);

/// How often taking a VM's lock is retried, 20 ms apart.
const LOCK_RETRIES: u32 = 5;

/// Set to the project's root for the `sendit run` process that `run
/// --detach` starts in the background. That process has no terminal to
/// ask questions on, and takes the project from here, as found by the
/// process that started it: e.g. a new worktree exists by then, and opening
/// it again with --from would fail.
const DETACHED_ENV: &str = "SENDIT_DETACHED";

/// How many lines at the end of the console log `run --detach` shows when
/// the VM stops before it answers.
const LOG_TAIL: usize = 15;

/// Where `run` runs the VM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Detach {
    /// In this process, attached to the terminal.
    No,
    /// In a `sendit run` process in the background, which this one starts,
    /// returning once the VM answers over SSH.
    Start,
    /// This is that process.
    Background,
}

/// The project root for the `sendit run` process in the background, if
/// this is it.
pub fn background_project() -> Option<PathBuf> {
    std::env::var_os(DETACHED_ENV).map(PathBuf::from)
}

/// Written into a project VM's directory when it is created, and again
/// when it gets a name later or is renamed.
#[derive(Debug, Serialize, Deserialize)]
pub struct Metadata {
    pub project_path: PathBuf,
    pub image: ImageName,
    pub base_revision: u32,
    /// The project whose VM this one was made as a copy of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_of: Option<PathBuf>,
    /// The name `-n` finds the VM by. Missing only for VMs made before
    /// sendit named them, until `name_vms` gives them one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<VmName>,
}

impl Metadata {
    /// Whether the VM was created from a base image older than this version
    /// of sendit can run.
    pub fn outdated(&self) -> bool {
        self.base_revision < provision::BASE_REVISION
    }
}

/// The image `project` uses: the `chosen` one, else the one its VM was
/// created from, else, before it has a VM, the one its parent VM was, else
/// `default`.
pub fn image(paths: &Paths, project: &Project, chosen: Option<&ImageName>) -> ImageName {
    if let Some(image) = chosen {
        return image.clone();
    }
    let dir = dir(paths, project);
    let dir = match parent(paths, project) {
        Some(parent) if !dir.path().exists() => parent,
        _ => dir,
    };
    metadata(&dir)
        .map(|metadata| metadata.image)
        .unwrap_or_default()
}

/// What the custom scripts of `image` asked to share in the build of it
/// that the project's VM is made from: the VM's, or before it exists, what
/// `new_vm_mounts` says.
pub fn script_mounts(
    paths: &Paths,
    project: &Project,
    image: &ImageName,
    fresh: bool,
) -> Result<Vec<ScriptMount>> {
    let dir = dir(paths, project);
    if dir.path().exists() {
        build_mounts(paths, Some(dir), image)
    } else {
        new_vm_mounts(paths, project, image, fresh)
    }
}

/// What the custom scripts of `image` asked to share in the build of it
/// that a new VM of the project would be made from: its parent VM's if `run`
/// copies that, unless the VM is to be `fresh`, else the latest build's.
pub fn new_vm_mounts(
    paths: &Paths,
    project: &Project,
    image: &ImageName,
    fresh: bool,
) -> Result<Vec<ScriptMount>> {
    build_mounts(paths, parent(paths, project).filter(|_| !fresh), image)
}

/// What the custom scripts of `image` asked to share in the build of it
/// that `vm` was made from: the copy of that build's marker that it keeps.
/// Without a VM, or with one of another image or an outdated one, which
/// `run` won't copy, the latest build's; so too for a VM made before VMs
/// kept a copy.
fn build_mounts(paths: &Paths, vm: Option<VmDir>, image: &ImageName) -> Result<Vec<ScriptMount>> {
    let copy = vm
        .filter(|vm| made_from(vm, image))
        .map(|vm| vm.image_marker())
        .filter(|copy| copy.exists());
    match copy {
        Some(copy) => {
            provision::marker_mounts(&copy).with_context(|| broken_marker_copy(paths, &copy))
        }
        None => provision::marker_mounts(&provision::marker(paths, image)),
    }
}

/// Says how to get past a VM's `copy` of its build's marker that can't be
/// read, e.g. after it was edited by hand.
fn broken_marker_copy(paths: &Paths, copy: &std::path::Path) -> String {
    format!(
        "reading {}; `sendit reset` in the VM's project makes it again",
        paths.display(copy)
    )
}

/// Whether a new VM of the project would be made from an earlier build of
/// `image` than the latest one: whether its parent VM was, if `run` copies
/// that, unless the VM is to be `fresh`.
pub fn new_vm_from_earlier_build(
    paths: &Paths,
    project: &Project,
    image: &ImageName,
    fresh: bool,
) -> Result<bool> {
    match parent(paths, project).filter(|_| !fresh) {
        Some(parent) => earlier_build(paths, &parent, image),
        None => Ok(false),
    }
}

/// Whether `vm` was made from an earlier build of `image` than the latest
/// one, by the copy of that build's marker that it keeps. Not if it is a VM
/// of another image or an outdated one, which `run` won't copy: a new one
/// would be made from the latest build. Unknown, and so false, while the
/// latest build doesn't record when it was made; a VM whose copy doesn't,
/// or that has none, was made before it.
pub fn earlier_build(paths: &Paths, vm: &VmDir, image: &ImageName) -> Result<bool> {
    if !made_from(vm, image) {
        return Ok(false);
    }
    let Some(latest) = provision::marker_built(&provision::marker(paths, image))? else {
        return Ok(false);
    };
    let copy = vm.image_marker();
    let built = provision::marker_built(&copy).with_context(|| broken_marker_copy(paths, &copy))?;
    Ok(built != Some(latest))
}

/// Whether `vm` was made from a build of `image` that this version of
/// sendit can run.
fn made_from(vm: &VmDir, image: &ImageName) -> bool {
    metadata(vm).is_ok_and(|m| m.image == *image && !m.outdated())
}

/// The directory of the project's VM.
pub fn dir(paths: &Paths, project: &Project) -> VmDir {
    VmDir::new(paths.vm_dir(project))
}

/// The VM that the project's VM is made as a copy of: its main worktree's,
/// if the project is a linked git worktree and the main worktree has one.
pub fn parent(paths: &Paths, project: &Project) -> Option<VmDir> {
    let main = Project::at(&worktree::main_worktree(&project.root)?).ok()?;
    let dir = dir(paths, &main);
    dir.path().exists().then_some(dir)
}

/// The metadata of the project's parent VM, if it has one that can be read.
pub fn parent_metadata(paths: &Paths, project: &Project) -> Option<Metadata> {
    metadata(&parent(paths, project)?).ok()
}

pub fn metadata(dir: &VmDir) -> Result<Metadata> {
    let path = dir.metadata();
    util::read_toml(&path)?.with_context(|| format!("{} is missing", path.display()))
}

fn write_metadata(dir: &VmDir, metadata: &Metadata) -> Result<()> {
    util::write_atomic(&dir.metadata(), toml::to_string(metadata)?.as_bytes())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Stopped,
    /// Running under this `sendit run` process. The PID is missing for a
    /// moment while the process starts up, and while another command holds
    /// the lock to copy or delete the VM.
    Running(Option<u32>),
}

pub fn state(dir: &VmDir) -> Result<State> {
    let path = dir.lock();
    let Some(mut file) = util::if_exists(File::open(&path))
        .with_context(|| format!("opening {}", path.display()))?
    else {
        return Ok(State::Stopped);
    };
    match file.try_lock() {
        Ok(()) => Ok(State::Stopped),
        Err(fs::TryLockError::WouldBlock) => {
            let mut pid = String::new();
            std::io::Read::read_to_string(&mut file, &mut pid)?;
            Ok(State::Running(pid.trim().parse().ok()))
        }
        Err(fs::TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("locking {}", path.display()))
        }
    }
}

/// All project VM directories, sorted by name: those named like a project
/// ID. Unfinished `.partial` directories and anything else are skipped.
pub fn all(paths: &Paths) -> Result<Vec<VmDir>> {
    let mut dirs = Vec::new();
    for entry in util::read_dir(&paths.vms_dir())? {
        let entry = entry?;
        let is_vm = entry.file_name().to_str().is_some_and(is_project_id);
        if is_vm && entry.file_type()?.is_dir() {
            dirs.push(VmDir::new(entry.path()));
        }
    }
    dirs.sort_by(|a, b| a.path().cmp(b.path()));
    Ok(dirs)
}

/// The project of the VM in `dir`, whose metadata is `metadata`. Its
/// directory may be gone.
fn project_of(dir: &VmDir, metadata: &Metadata) -> Project {
    Project {
        root: metadata.project_path.clone(),
        id: dir
            .path()
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into(),
    }
}

/// The project whose VM is named `name`.
pub fn find(paths: &Paths, name: &VmName) -> Result<Project> {
    name_vms(paths)?;
    for dir in all(paths)? {
        if let Ok(metadata) = metadata(&dir)
            && metadata.name.as_ref() == Some(name)
        {
            return Ok(project_of(&dir, &metadata));
        }
    }
    bail!("no VM is named {name}; `sendit list` shows the names")
}

/// Renames the project's VM to `name`, unless another VM has that name.
pub fn rename(paths: &Paths, project: &Project, name: &VmName) -> Result<()> {
    let dir = dir(paths, project);
    ensure!(
        dir.path().exists(),
        "this project has no VM yet; `{} run` creates it",
        sendit_for(&dir, project)
    );
    let _names = lock_names(paths)?;
    let mut metadata = metadata(&dir)?;
    if metadata.name.as_ref() == Some(name) {
        eprintln!("The VM is named {name} already.");
        return Ok(());
    }
    for other in all(paths)? {
        if let Ok(other) = self::metadata(&other)
            && other.name.as_ref() == Some(name)
        {
            bail!(
                "the VM of {} is named {name} already",
                paths.display(&other.project_path)
            );
        }
    }
    let old = metadata.name.replace(name.clone());
    write_metadata(&dir, &metadata)?;
    match old {
        Some(old) => eprintln!("Renamed the VM {old} to {name}."),
        None => eprintln!("Named the VM {name}."),
    }
    Ok(())
}

/// Gives each VM without a name, made before sendit named VMs, the name it
/// would get if it were made now. Commands that show or look up names call
/// this first.
pub fn name_vms(paths: &Paths) -> Result<()> {
    // Without taking the lock, which would leave its file behind, unless
    // there is a VM to name.
    let unnamed = all(paths)?
        .iter()
        .any(|dir| metadata(dir).is_ok_and(|metadata| metadata.name.is_none()));
    if !unnamed {
        return Ok(());
    }
    let _names = lock_names(paths)?;
    name_unnamed(paths).map(drop)
}

/// What `name_vms` does, for a caller that holds the names lock. Returns
/// the names the VMs have then. A VM whose metadata can't be written stays
/// without a name, with a warning: that doesn't stop the command.
fn name_unnamed(paths: &Paths) -> Result<BTreeSet<VmName>> {
    let mut unnamed = Vec::new();
    for dir in all(paths)? {
        if let Ok(metadata) = metadata(&dir)
            && metadata.name.is_none()
        {
            unnamed.push((dir, metadata));
        }
    }
    let mut taken = names(paths)?;
    // Main worktrees first: the names of linked worktrees' VMs start with
    // theirs. The sort is stable, so the order is otherwise that of `all`.
    unnamed.sort_by_key(|(_, metadata)| worktree::main_worktree(&metadata.project_path).is_some());
    for (dir, mut metadata) in unnamed {
        let name = unique_name(wanted_name(paths, &project_of(&dir, &metadata)), &taken);
        metadata.name = Some(name.clone());
        match write_metadata(&dir, &metadata) {
            Ok(()) => {
                taken.insert(name);
            }
            Err(error) => eprintln!(
                "warning: could not name the VM in {}: {error:#}",
                paths.display(dir.path())
            ),
        }
    }
    Ok(taken)
}

/// The names the VMs have.
fn names(paths: &Paths) -> Result<BTreeSet<VmName>> {
    Ok(all(paths)?
        .iter()
        .filter_map(|dir| metadata(dir).ok()?.name)
        .collect())
}

/// Takes the lock that VM names are given out and changed under; it is
/// released when the returned file is closed.
fn lock_names(paths: &Paths) -> Result<File> {
    let path = paths.names_lock();
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.lock()
        .with_context(|| format!("locking {}", path.display()))?;
    Ok(file)
}

/// The name the VM of `project` gets, unless another VM has it: its
/// folder's name, or in a linked git worktree, the name of the VM of its
/// counterpart in the main worktree (see `parent`), or while that has none,
/// the counterpart's folder name, followed by the linked worktree's folder
/// name, e.g. `send-it-feature-x`, or for the subdirectory `apps/web` of
/// that worktree, `web-feature-x`. `vm` if those names have no letters or
/// digits.
fn wanted_name(paths: &Paths, project: &Project) -> VmName {
    let folder = |path: &Path| {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
    };
    let name = match worktree::linked_worktree(&project.root) {
        Some((main, top)) => {
            let main = parent_metadata(paths, project)
                .and_then(|metadata| metadata.name)
                .map(String::from)
                .or_else(|| folder(&main))
                .unwrap_or_default();
            VmName::from_text(&format!("{main}-{}", folder(top).unwrap_or_default()))
        }
        None => folder(&project.root).and_then(|name| VmName::from_text(&name)),
    };
    name.unwrap_or_else(|| "vm".parse().expect("valid name"))
}

/// `wanted`, or if it is `taken`, the first of `wanted-2`, `wanted-3`, …
/// that isn't.
fn unique_name(wanted: VmName, taken: &BTreeSet<VmName>) -> VmName {
    if !taken.contains(&wanted) {
        return wanted;
    }
    (2..)
        .map(|n| wanted.numbered(n))
        .find(|name| !taken.contains(name))
        .expect("a free number")
}

/// What `create_if_missing` did.
pub struct Created {
    /// Why the VM isn't a copy of its parent VM, if it has one and needn't
    /// be fresh.
    pub note: Option<String>,
    /// Whether the image's scripts share other directories with the new VM
    /// than `new_vm_mounts` said before: the user chose to make it from the
    /// image instead of its running parent VM, which came from another
    /// build.
    pub other_shares: bool,
}

/// Creates the project's VM unless it exists, from the image `settings`
/// choose. A `fresh` VM is created from the base image even for a git
/// worktree, whose VM is otherwise a copy of its parent VM; while that
/// runs, only if the user agrees when asked with `ask`.
pub fn create_if_missing(
    paths: &Paths,
    project: &Project,
    settings: &VmSettings,
    fresh: bool,
    ask: Ask,
) -> Result<Option<Created>> {
    let dir = dir(paths, project);
    if dir.path().exists() {
        // Made before sendit named VMs: `-n` finds it from now on.
        if metadata(&dir).is_ok_and(|metadata| metadata.name.is_none()) {
            name_vms(paths)?;
        }
        return Ok(None);
    }
    let image = image(paths, project, settings.image.as_ref());
    let expected = new_vm_mounts(paths, project, &image, fresh)?;
    let note = create(paths, project, &dir, &image, fresh, ask)?;
    Ok(Some(Created {
        note,
        other_shares: !provision::same_mounts(
            &script_mounts(paths, project, &image, fresh)?,
            &expected,
        ),
    }))
}

/// Boots the project's VM, which `create_if_missing` has created, and runs
/// it until it stops, showing `note` once it starts. Without a
/// `command`, the console is attached. With one, the
/// command runs over SSH once the VM answers, as with `sendit ssh`, and the
/// VM shuts down when it has ended. Returns its exit code, or if a signal
/// stopped the VM first, 128 plus its number; else 0. `detach` says where
/// the VM runs; in the background, it runs without a command.
pub fn run(
    paths: &Paths,
    project: &Project,
    settings: &VmSettings,
    fresh: bool,
    note: Option<String>,
    command: &[String],
    detach: Detach,
) -> Result<i32> {
    let dir = dir(paths, project);
    let metadata = metadata(&dir)?;
    let sendit = sendit_for(&dir, project);
    if fresh && let Some(copy_of) = &metadata.copy_of {
        bail!(
            "{} is a copy of the VM of {} already; `{sendit} reset` deletes it so \
             that `{} run --fresh` starts it over from the {} image",
            paths.display(dir.path()),
            paths.display(copy_of),
            sendit_in(project),
            metadata.image,
        );
    }
    if let Some(image) = &settings.image {
        ensure!(
            metadata.image == *image,
            "{} was created from the {} image, not {image}; `{sendit} reset` deletes \
             it so the next run starts over from the {image} image",
            paths.display(dir.path()),
            metadata.image,
        );
    }
    ensure!(
        !metadata.outdated(),
        "{} was created from an older base image that this version of sendit \
         can't run; `{sendit} reset` deletes it so the next run starts over from \
         the current base image",
        paths.display(dir.path())
    );
    if !command.is_empty() || detach == Detach::Start {
        // Rather now than after booting.
        login(paths, false)?;
    }
    let starting = |note: Option<String>| {
        eprintln!(
            "Starting {} ({} CPUs, {} memory, {} disk)",
            paths.display(dir.path()),
            settings.cpus,
            settings.memory,
            settings.disk_size
        );
        if let Some(note) = note {
            eprintln!("{note}");
        }
    };
    if detach == Detach::Start {
        // The lock is for the process in the background; this only checks
        // that the VM isn't running yet. Should another `sendit run` take
        // the lock first, the process in the background fails at once, long before
        // the other VM could answer over SSH, and this one reports it.
        if try_lock(&dir)?.is_none() {
            return Err(already_running(&dir, project, command));
        }
        // Here, where it is seen; the disk grows in the background.
        warn_if_shrinking(paths, &dir, project, settings)?;
        starting(note);
        return run_in_background(paths, project, &dir);
    }
    let _lock = lock(&dir, project, command)?;
    // The VM starts now: its boot and banner come first on the screen. A
    // command's output follows on from what is there, like any command's.
    if command.is_empty() && detach == Detach::No {
        vm::clear_screen();
    }
    resize_disk(paths, &dir, project, settings)?;

    starting(note);
    let shutdown = Arc::new(AtomicBool::new(false));
    let spec = VmSpec {
        cpus: settings.cpus,
        memory: settings.memory,
        shares: mounts::prepare(settings, &dir.meta())?,
        seed: None,
        console: if detach == Detach::Background {
            ConsoleMode::Detached
        } else if command.is_empty() {
            ConsoleMode::Login
        } else {
            ConsoleMode::Hidden
        },
        shutdown: Some(shutdown.clone()),
    };
    let stopped = Arc::new(AtomicBool::new(false));
    let runner = (!command.is_empty()).then(|| {
        let (paths, dir, command) = (paths.clone(), dir.clone(), command.to_vec());
        let stopped = stopped.clone();
        std::thread::spawn(move || run_command(&paths, &dir, &command, &stopped, &shutdown))
    });
    let result = vm::run(&dir, &spec);
    stopped.store(true, Ordering::Relaxed);
    // Even after an error: ssh mustn't outlive sendit, e.g. keeping the
    // terminal in raw mode.
    let code = match runner {
        Some(runner) => runner
            .join()
            .unwrap_or_else(|_| Err(anyhow::anyhow!("running the command panicked"))),
        None => Ok(0),
    };
    match result {
        Err(e) => Err(e),
        // Stopped by `sendit stop`, Ctrl-C or the terminal going away before
        // the command ended: like a shell reports a command killed by it.
        Ok(Some(signal)) if !command.is_empty() => Ok(128 + signal),
        Ok(_) => code,
    }
}

/// Starts the `sendit run` process that runs the VM in `dir` in the
/// background, with its output going to the VM's console log, and waits
/// until the VM answers over SSH. If a signal comes first, e.g. Ctrl-C, or
/// the VM doesn't answer in time, the VM is shut down again. Returns 0, or
/// 128 plus the number of that signal.
fn run_in_background(paths: &Paths, project: &Project, dir: &VmDir) -> Result<i32> {
    let log_path = dir.console_log();
    let log =
        File::create(&log_path).with_context(|| format!("creating {}", log_path.display()))?;
    let exe = std::env::current_exe().context("finding the sendit executable")?;
    let mut sendit = Command::new(exe);
    sendit
        .args(std::env::args_os().skip(1))
        .env(DETACHED_ENV, &project.root)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // In a session of its own, it has no terminal: none going away hangs
    // it up, and Ctrl-C in this one doesn't reach it. Descriptors that
    // were handed to this process to pass on, e.g. a jobserver's pipe,
    // would stay open while the VM runs, and whoever waits for them to
    // close would wait as long. Those of std are closed on exec anyway, and
    // it still needs them until then.
    // SAFETY: setsid, getdtablesize, fcntl and close are async-signal-safe,
    // so they may run between fork and exec.
    unsafe {
        sendit.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            for fd in 3..libc::getdtablesize() {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags != -1 && flags & libc::FD_CLOEXEC == 0 {
                    libc::close(fd);
                }
            }
            Ok(())
        });
    }
    let (first_signal, _) = vm::signals();
    let _signals = vm::CatchSignals::new();
    let mut child = sendit
        .spawn()
        .context("starting sendit in the background")?;

    let mut exited = None;
    let answers = wait_until_answers(paths, dir, &mut || {
        if exited.is_none() {
            exited = child.try_wait().ok().flatten();
        }
        exited.is_some() || vm::signals().0 > first_signal
    });
    if let Some(status) = exited {
        let tail = log_tail(&log_path);
        if !tail.is_empty() {
            say(&tail);
        }
        // E.g. after `sendit stop`, it exits successfully.
        let status = if status.success() {
            String::new()
        } else {
            format!(" ({status})")
        };
        bail!(
            "the VM stopped before it answered over SSH{status}; its console \
             output is in {}",
            paths.display(&log_path)
        );
    }
    match answers {
        Ok(true) => {
            let sendit = sendit_for(dir, project);
            say(&format!(
                "The VM runs in the background: `{sendit} ssh` opens a shell in it, and \
                 `{sendit} stop` shuts it down. Its console output goes to {}.",
                paths.display(&log_path)
            ));
            Ok(0)
        }
        // Like a shell reports a command killed by the signal.
        Ok(false) => {
            stop_in_background(&mut child)?;
            Ok(128 + vm::signals().1)
        }
        Err(e) => {
            stop_in_background(&mut child)?;
            Err(e)
        }
    }
}

/// Shuts down the VM that `child` runs in the background, as `sendit stop`
/// does, and waits until it has ended. Each signal here meanwhile is passed
/// on, so that a second one forces the VM off.
fn stop_in_background(child: &mut Child) -> Result<()> {
    let mut passed_on = None;
    loop {
        if child.try_wait().context("waiting for sendit")?.is_some() {
            return Ok(());
        }
        let (signals, _) = vm::signals();
        if passed_on != Some(signals) {
            // SAFETY: plain syscall; the child hasn't been waited for, so
            // its PID is still its own.
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
            if passed_on.is_none() {
                say("Shutting down the VM…");
            }
            passed_on = Some(signals);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Prints `message` on stderr, ignoring errors: after a SIGHUP the terminal
/// is gone, and the VM must still be shut down.
fn say(message: &str) {
    let _ = writeln!(std::io::stderr(), "{message}");
}

/// The last `LOG_TAIL` lines of the console log at `path`, as plain text.
fn log_tail(path: &Path) -> String {
    let log = plain_text(&String::from_utf8_lossy(
        &fs::read(path).unwrap_or_default(),
    ));
    let lines: Vec<&str> = log.lines().collect();
    lines[lines.len().saturating_sub(LOG_TAIL)..].join("\n")
}

/// `text` without the escape sequences and other control characters of
/// terminal output, apart from line breaks and tabs.
fn plain_text(text: &str) -> String {
    let mut plain = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                // CSI, e.g. colours: parameters up to a final byte.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC, e.g. the window title: up to BEL or ESC \.
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' {
                            chars.next_if_eq(&'\\');
                            break;
                        }
                    }
                }
                // Others, e.g. `ESC ( B` from `tput sgr0`: intermediate
                // bytes, then a final one.
                Some(mut c) => {
                    while (' '..='/').contains(&c) {
                        let Some(next) = chars.next() else { break };
                        c = next;
                    }
                }
                None => {}
            },
            '\n' | '\t' => plain.push(c),
            c if c.is_control() => {}
            c => plain.push(c),
        }
    }
    plain
}

/// Asks the project's running VM to shut down and waits until it has.
pub fn stop(paths: &Paths, project: &Project) -> Result<()> {
    let dir = dir(paths, project);
    let pid = match state(&dir)? {
        State::Stopped => {
            eprintln!("The VM is not running.");
            return Ok(());
        }
        State::Running(None) => bail!("the VM is starting or busy; try again in a moment"),
        State::Running(Some(pid)) => pid,
    };
    // `run` treats SIGTERM like Ctrl-]: it asks the guest to shut down and
    // forces it off if the guest doesn't react in time.
    // SAFETY: plain syscall; the PID comes from the running VM's lock file.
    if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("signalling sendit process {pid}"));
    }
    eprintln!("Shutting down the VM…");
    let start = Instant::now();
    while state(&dir)? != State::Stopped {
        ensure!(
            start.elapsed() < STOP_WAIT,
            "the VM did not stop within {} seconds",
            STOP_WAIT.as_secs()
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    eprintln!("Stopped.");
    Ok(())
}

/// Replaces this process with an SSH session to the project's running VM,
/// as root if `root`, running `command` if given.
pub fn ssh(paths: &Paths, project: &Project, root: bool, command: &[String]) -> Result<()> {
    let dir = dir(paths, project);
    ensure!(
        state(&dir)? != State::Stopped,
        "the VM is not running; start it with `{} run`",
        sendit_for(&dir, project)
    );
    let error = ssh_command(paths, &dir, root, Remote::Login(command))?.exec();
    Err(error).context("running ssh")
}

/// What an SSH connection to a VM is for.
#[derive(Clone, Copy)]
enum Remote<'a> {
    /// A login shell, running the command if there is one. It gets a
    /// terminal if it is interactive, or if ours is one.
    Login(&'a [String]),
    /// Only finding out whether the VM answers yet.
    Probe,
}

/// The ssh command that connects to the VM in `dir`, as root if `root`.
fn ssh_command(paths: &Paths, dir: &VmDir, root: bool, remote: Remote) -> Result<Command> {
    let ip = net::vm_ip(dir)?.with_context(|| {
        format!(
            "the VM has no IP address in {} yet; it may still be booting",
            net::LEASES_FILE
        )
    })?;
    let (user, key) = login(paths, root)?;
    // Each VM gets its own known_hosts: IP addresses are reused across VMs,
    // but a VM's host keys stay the same for its lifetime.
    let known_hosts = dir.known_hosts();
    // No config file: ~/.ssh/config may forward the SSH agent or ports to
    // every host, and thereby hand them to the VM.
    let mut ssh = Command::new("ssh");
    ssh.args(["-F", "/dev/null"])
        .arg("-i")
        .arg(&key)
        .args(["-o", "IdentitiesOnly=yes"])
        .args(["-o", "StrictHostKeyChecking=accept-new"])
        .arg("-o")
        .arg(format!("UserKnownHostsFile={}", known_hosts.display()))
        .args(["-o", "LogLevel=ERROR"])
        // The guest's sshd accepts it; ssh doesn't send it by default.
        .args(["-o", "SendEnv=COLORTERM"]);
    match remote {
        Remote::Login(command) if !command.is_empty() && interactive() => {
            ssh.arg("-t");
        }
        Remote::Login(_) => {}
        Remote::Probe => {
            ssh.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=2"]);
        }
    }
    ssh.arg(format!("{user}@{ip}"));
    match remote {
        Remote::Login([]) => {}
        Remote::Login(command) => {
            ssh.arg(login_shell_command(command));
        }
        Remote::Probe => {
            ssh.arg("true");
        }
    }
    Ok(ssh)
}

/// The user and SSH key to log in to a VM with, as root if `root`.
fn login(paths: &Paths, root: bool) -> Result<(&'static str, PathBuf)> {
    let (user, key) = if root {
        ("root", paths.root_ssh_key())
    } else {
        (GUEST_USER, paths.ssh_key())
    };
    // Its public key is baked into the base images, so a new one only
    // reaches VMs made from images built after it.
    ensure!(
        key.exists(),
        "{} is missing, so no VM made so far can be reached as {user}; \
         `sendit provision --all --force` makes a new key and rebuilds the images \
         with it, then `sendit reset` makes this project's VM again",
        paths.display(&key)
    );
    Ok((user, key))
}

/// Whether the user is at a terminal, so a command run over SSH gets one
/// too, e.g. for an editor or `htop`.
fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// The remote shell's command line for running `command` in a login shell,
/// as in an interactive session: with the PATH that ~/.profile sets up, and
/// in the project directory. ssh joins a command's arguments with spaces,
/// so they are quoted to arrive as they were given.
fn login_shell_command(command: &[String]) -> String {
    format!("exec bash -lc {}", quote(&shell_command(command)))
}

/// Joins `args` into a command line for a POSIX shell that runs them as given.
fn shell_command(args: &[String]) -> String {
    args.iter()
        .map(|arg| quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `arg` as one word for a POSIX shell.
fn quote(arg: &str) -> String {
    let safe = !arg.is_empty()
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"%+,-./:=@_".contains(&b));
    if safe {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

/// How long `run` waits for the VM to answer over SSH before giving up on
/// running its command.
const SSH_WAIT: Duration = Duration::from_secs(120);

/// How long an ssh process may run on once the VM has stopped. One whose
/// VM was forced off doesn't notice: it would wait for an answer forever.
const SSH_GRACE: Duration = Duration::from_secs(2);

/// Runs `command` in the VM in `dir` over SSH once it answers, as `sendit
/// ssh` would, then sets `shutdown`. Gives up once `stopped` is set.
/// Returns the command's exit code.
fn run_command(
    paths: &Paths,
    dir: &VmDir,
    command: &[String],
    stopped: &AtomicBool,
    shutdown: &AtomicBool,
) -> Result<i32> {
    let stopped = &mut || stopped.load(Ordering::Relaxed);
    let result = (|| {
        ensure!(
            wait_until_answers(paths, dir, stopped)?,
            "the VM stopped before the command could run"
        );
        let ssh = ssh_command(paths, dir, false, Remote::Login(command))?
            .spawn()
            .context("running ssh")?;
        let status = wait_for_ssh(ssh, stopped)?
            .context("the VM stopped while the command was still running")?;
        // Like a shell reports a command killed by a signal.
        Ok(status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)))
    })();
    shutdown.store(true, Ordering::Relaxed);
    result
}

/// Waits until the VM in `dir` answers over SSH, for up to `SSH_WAIT`.
/// False if `stopped` says first that the VM stopped, or that waiting
/// should end.
fn wait_until_answers(
    paths: &Paths,
    dir: &VmDir,
    stopped: &mut dyn FnMut() -> bool,
) -> Result<bool> {
    let start = Instant::now();
    loop {
        if stopped() {
            return Ok(false);
        }
        let answers = match ssh_command(paths, dir, false, Remote::Probe) {
            Ok(mut ssh) => {
                let probe = ssh
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .context("running ssh")?;
                wait_for_ssh(probe, stopped)?.is_some_and(|status| status.success())
            }
            // No IP address yet.
            Err(_) => false,
        };
        if answers {
            return Ok(true);
        }
        ensure!(
            start.elapsed() < SSH_WAIT,
            "the VM didn't answer over SSH within {} seconds",
            SSH_WAIT.as_secs()
        );
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Waits until the ssh process `child` exits and returns its status. Once
/// `stopped` says that the VM stopped, it gets `SSH_GRACE` to exit, then is
/// terminated, and the result is `None`.
fn wait_for_ssh(mut child: Child, stopped: &mut dyn FnMut() -> bool) -> Result<Option<ExitStatus>> {
    let mut deadline = None;
    loop {
        if let Some(status) = child.try_wait().context("waiting for ssh")? {
            return Ok(Some(status));
        }
        if stopped()
            && Instant::now() >= *deadline.get_or_insert_with(|| Instant::now() + SSH_GRACE)
        {
            // Not SIGKILL: ssh gives the terminal back on SIGTERM.
            // SAFETY: plain syscall; the child hasn't been waited for, so
            // its PID can't have been reused.
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
            child.wait().context("waiting for ssh")?;
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Deletes a stopped VM. It holds the VM's lock meanwhile, so the VM can't
/// be started while it is being deleted.
pub fn delete(dir: &VmDir) -> Result<()> {
    let Some(_lock) = try_lock(dir)? else {
        bail!("{} is running; stop it first", dir.path().display());
    };
    fs::remove_dir_all(dir.path()).with_context(|| format!("deleting {}", dir.path().display()))
}

/// What a new VM is a copy of.
enum Source {
    Base,
    Parent {
        dir: VmDir,
        metadata: Metadata,
        /// Keeps the parent from starting while it is copied.
        _lock: File,
    },
}

/// Asks a yes/no question, like `commands::confirm`.
pub type Ask<'a> = &'a dyn Fn(&str, &str) -> Result<bool>;

/// The parent VM, locked, if the project has one, it can be copied for
/// `image` and the VM isn't to be `fresh`; else the base image, with a note
/// why if there is a parent and the VM needn't be fresh.
/// While the parent runs, the base image only if the user agrees, else
/// `commands::Declined`.
fn source(
    paths: &Paths,
    project: &Project,
    image: &ImageName,
    fresh: bool,
    ask: Ask,
) -> Result<(Source, Option<String>)> {
    let Some(parent) = parent(paths, project).filter(|_| !fresh) else {
        return Ok((Source::Base, None));
    };
    let why_not = match metadata(&parent) {
        Err(e) => format!("{e:#}"),
        Ok(m) if m.image != *image => format!("it was made from the {} image", m.image),
        Ok(m) if m.outdated() => "it was made from an outdated base image".to_string(),
        Ok(metadata) => match try_lock(&parent)? {
            Some(lock) => {
                let source = Source::Parent {
                    dir: parent,
                    metadata,
                    _lock: lock,
                };
                return Ok((source, None));
            }
            None => {
                let main = paths.display(&metadata.project_path);
                let question = format!(
                    "The VM of {main} is running, and a copy of it would miss what it hasn't \
                     written to its disk yet. Start this worktree's VM from the {image} image \
                     instead?"
                );
                let no_terminal = format!(
                    "the VM of {main} is running; stop it to start this worktree's VM as a \
                     copy of it, or confirm in a terminal to start from the {image} image"
                );
                if ask(&question, &no_terminal)? {
                    return Ok((Source::Base, None));
                }
                let stop = match &metadata.name {
                    Some(name) => format!("`sendit -n {name} stop`"),
                    None => format!("`sendit stop` in {main}"),
                };
                eprintln!("Stop it ({stop}), then run again to start from a copy.");
                return Err(commands::Declined.into());
            }
        },
    };
    let note = format!(
        "This VM started from the {image} image, not as a copy of {}: {why_not}.",
        paths.display(parent.path())
    );
    Ok((Source::Base, Some(note)))
}

/// Creates the project's VM as a copy of its parent VM, or else, or if it
/// is to be `fresh`, of the base image, asking with `ask` first if the
/// parent is running. Returns a note on why it isn't a copy of its parent
/// VM if it has one and the VM needn't be fresh.
fn create(
    paths: &Paths,
    project: &Project,
    dir: &VmDir,
    image: &ImageName,
    fresh: bool,
    ask: Ask,
) -> Result<Option<String>> {
    let (source, note) = source(paths, project, image, fresh, ask)?;
    // The marker of the image's build that the VM is made from, which it
    // keeps a copy of. A parent made before VMs kept one shares what the
    // latest build asked for; so does a copy of it, which keeps none either.
    let (from, base_revision, copy_of, marker) = match &source {
        Source::Parent {
            dir: parent,
            metadata,
            ..
        } => {
            eprintln!(
                "Creating {} as a copy of the VM of {}",
                paths.display(dir.path()),
                paths.display(&metadata.project_path)
            );
            (
                parent.path().to_path_buf(),
                metadata.base_revision,
                Some(metadata.project_path.clone()),
                parent.image_marker(),
            )
        }
        Source::Base => {
            let state = provision::base_state(paths, image)?;
            if matches!(state, BaseState::Missing | BaseState::Outdated) {
                bail!("the {image} image is {}", state.describe(image));
            }
            eprintln!(
                "Creating {} from the {image} image",
                paths.display(dir.path())
            );
            (
                paths.image_dir(image),
                provision::BASE_REVISION,
                None,
                provision::marker(paths, image),
            )
        }
    };
    let from = VmDir::new(from);
    let read_marker = || {
        util::if_exists(fs::read(&marker)).with_context(|| format!("reading {}", marker.display()))
    };
    // `provision` swaps in a new build of the image, and `images --delete`
    // deletes it, without waiting for VMs being made from it, nor they for
    // a build, which takes minutes. Unless the image's marker is the same
    // after cloning as before, the disk may be from another build than the
    // marker, or gone.
    let rebuilt = || {
        anyhow::anyhow!(
            "the {image} image was rebuilt or deleted while {} was made from it; run again",
            paths.display(dir.path())
        )
    };
    let copy = read_marker()?;
    if matches!(source, Source::Base) && copy.is_none() {
        return Err(rebuilt());
    }
    let partial = VmDir::new(paths.vms_dir().join(format!(".{}.partial", project.id)));
    util::fresh_dir(partial.path())?;
    let made = (|| {
        let cloned = (|| {
            image::clone_file(&from.disk(), &partial.disk())?;
            image::clone_file(&from.efi_vars(), &partial.efi_vars())
        })();
        // Also after an error, which an image deleted meanwhile causes.
        if matches!(source, Source::Base) && read_marker()? != copy {
            return Err(rebuilt());
        }
        cloned?;
        if let Some(copy) = &copy {
            fs::write(partial.image_marker(), copy)
                .with_context(|| format!("copying {}", marker.display()))?;
        }
        // Until the VM is in place, where the next one made finds its name.
        let _names = lock_names(paths)?;
        // Older VMs first, so that the new one doesn't take the name one of
        // them would get.
        let taken = name_unnamed(paths)?;
        let metadata = toml::to_string(&Metadata {
            project_path: project.root.clone(),
            image: image.clone(),
            base_revision,
            copy_of,
            name: Some(unique_name(wanted_name(paths, project), &taken)),
        })?;
        fs::write(partial.metadata(), metadata)?;
        fs::rename(partial.path(), dir.path())
            .with_context(|| format!("moving the new VM to {}", dir.path().display()))
    })();
    if made.is_err() {
        // Nothing lists it; on another volume, its disk is a full copy.
        let _ = fs::remove_dir_all(partial.path());
    }
    made.map(|()| note)
}

/// Grows the disk to the configured size; the guest grows its root
/// filesystem to match on boot. Disks never shrink.
fn resize_disk(paths: &Paths, dir: &VmDir, project: &Project, settings: &VmSettings) -> Result<()> {
    warn_if_shrinking(paths, dir, project, settings)?;
    image::grow_disk(&dir.disk(), settings.disk_size)
}

/// Warns if the disk of the VM of `project`, in `dir`, is larger than
/// `settings` ask for: disks only grow.
fn warn_if_shrinking(
    paths: &Paths,
    dir: &VmDir,
    project: &Project,
    settings: &VmSettings,
) -> Result<()> {
    let current = fs::metadata(dir.disk())
        .with_context(|| format!("reading {}", dir.disk().display()))?
        .len();
    if current > settings.disk_size.0 {
        eprintln!(
            "warning: the disk of {} is already {} and can't shrink to {}; `{} reset` recreates it",
            paths.display(dir.path()),
            ByteSize(current),
            settings.disk_size,
            sendit_for(dir, project)
        );
    }
    Ok(())
}

/// Makes sure only one process runs a VM at a time; two would corrupt its
/// disk. The lock is released when the returned file is closed, even if the
/// process dies. If the VM is running, the error says how to reach it, to
/// run `command` there or else open a shell.
fn lock(dir: &VmDir, project: &Project, command: &[String]) -> Result<File> {
    let Some(mut file) = try_lock(dir)? else {
        return Err(already_running(dir, project, command));
    };
    write!(file, "{}", std::process::id())?;
    Ok(file)
}

/// The error for starting the project's VM, in `dir`, while it runs, which
/// says how to reach it, to run `command` there or else open a shell.
fn already_running(dir: &VmDir, project: &Project, command: &[String]) -> anyhow::Error {
    let ssh = format!("{} ssh", sendit_for(dir, project));
    let (ssh, does) = if command.is_empty() {
        (ssh, "opens another shell in it")
    } else {
        (
            format!("{ssh} {}", shell_command(command)),
            "runs the command there",
        )
    };
    anyhow::anyhow!("this project's VM is already running; `{ssh}` {does}")
}

/// The sendit command line for `project`, whose VM is in `dir`, from here,
/// for hints on what to run next. Run elsewhere, e.g. with -n or
/// --worktree, the VM must be given: by its name, or if it has none, e.g.
/// before it is made, by the project directory. A step after `reset`
/// takes `sendit_in` instead: the name goes with the VM.
pub fn sendit_for(dir: &VmDir, project: &Project) -> String {
    match metadata(dir).ok().and_then(|metadata| metadata.name) {
        Some(name) if !in_project(project) => format!("sendit -n {name}"),
        _ => sendit_in(project),
    }
}

/// The sendit command line for `project` from here, by its directory
/// unless that is here.
pub fn sendit_in(project: &Project) -> String {
    if in_project(project) {
        "sendit".to_string()
    } else {
        format!("sendit -C {}", quote(&project.root.to_string_lossy()))
    }
}

/// Whether sendit runs in the directory of `project`.
fn in_project(project: &Project) -> bool {
    std::env::current_dir()
        .and_then(|cwd| cwd.canonicalize())
        .is_ok_and(|cwd| cwd == project.root)
}

/// Takes the VM's lock without recording a PID; `None` if the VM is running.
/// The PID of its last run is cleared: once that process has ended, the PID
/// may belong to another process, which `stop` would signal.
fn try_lock(dir: &VmDir) -> Result<Option<File>> {
    let run = dir.run_dir();
    // Not `create_dir_all`: if the VM directory was deleted meanwhile, it
    // must not come back as an empty shell.
    match fs::create_dir(&run) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("creating {}", run.display())),
    }
    let path = dir.lock();
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    // `state` takes the lock for a moment to check it; don't mistake that
    // for a running VM.
    let mut retries = 0;
    loop {
        match file.try_lock() {
            Ok(()) => {
                file.set_len(0)
                    .with_context(|| format!("clearing {}", path.display()))?;
                return Ok(Some(file));
            }
            Err(fs::TryLockError::WouldBlock) if retries < LOCK_RETRIES => {
                retries += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(fs::TryLockError::WouldBlock) => return Ok(None),
            Err(fs::TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("locking {}", path.display()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::Network;
    use std::path::Path;

    #[test]
    fn keeps_the_image_a_vm_was_made_from() {
        let temp = crate::util::TempDir::new("vm-image");
        let paths = Paths::new(temp.path().join("home"));
        fs::create_dir_all(temp.path().join("proj")).unwrap();
        let project = Project::at(&temp.path().join("proj")).unwrap();
        let rust: ImageName = "rust".parse().unwrap();
        let go: ImageName = "go".parse().unwrap();

        assert_eq!(image(&paths, &project, None), ImageName::default());
        assert_eq!(image(&paths, &project, Some(&go)), go);

        let vm = dir(&paths, &project);
        fs::create_dir_all(vm.path()).unwrap();
        fs::write(
            vm.metadata(),
            "project_path = \"/p\"\nimage = \"rust\"\nbase_revision = 7\n",
        )
        .unwrap();
        assert_eq!(image(&paths, &project, None), rust);
        assert_eq!(image(&paths, &project, Some(&go)), go);
    }

    /// A worktree `wt` of `main` in `temp`, and the VM of `main`, made from
    /// the `rust` image.
    fn worktree_with_parent(temp: &Path, paths: &Paths) -> (Project, VmDir) {
        subdirectory_with_parent(temp, paths, "")
    }

    /// The subdirectory `sub` of a worktree `wt` of `main` in `temp`, and
    /// the VM of the same subdirectory of `main`, made from the `rust` image.
    fn subdirectory_with_parent(temp: &Path, paths: &Paths, sub: &str) -> (Project, VmDir) {
        let (main, wt) = worktree::layout(temp);
        let (main, wt) = (main.join(sub), wt.join(sub));
        fs::create_dir_all(&main).unwrap();
        fs::create_dir_all(&wt).unwrap();
        let main = Project::at(&main).unwrap();
        let parent = dir(paths, &main);
        fs::create_dir_all(parent.path()).unwrap();
        let metadata = Metadata {
            project_path: main.root.clone(),
            image: "rust".parse().unwrap(),
            base_revision: provision::BASE_REVISION,
            copy_of: None,
            // As made before sendit named VMs.
            name: None,
        };
        fs::write(parent.metadata(), toml::to_string(&metadata).unwrap()).unwrap();
        fs::write(parent.disk(), "disk").unwrap();
        fs::write(parent.efi_vars(), "efi").unwrap();
        (Project::at(&wt).unwrap(), parent)
    }

    #[test]
    fn worktrees_take_the_image_of_their_parent() {
        let temp = crate::util::TempDir::new("vm-parent-image");
        let paths = Paths::new(temp.path().join("home"));
        let (wt, _) = worktree_with_parent(temp.path(), &paths);
        let go: ImageName = "go".parse().unwrap();
        assert_eq!(image(&paths, &wt, None), "rust".parse().unwrap());
        assert_eq!(image(&paths, &wt, Some(&go)), go);
    }

    #[test]
    fn subdirectories_of_worktrees_take_the_image_of_their_parent() {
        let temp = crate::util::TempDir::new("vm-parent-image-sub");
        let paths = Paths::new(temp.path().join("home"));
        let (wt, vm) = subdirectory_with_parent(temp.path(), &paths, "apps/web");
        assert_eq!(
            parent(&paths, &wt).map(|dir| dir.path().to_path_buf()),
            Some(vm.path().to_path_buf())
        );
        assert_eq!(image(&paths, &wt, None), "rust".parse().unwrap());
        // Not for the directory above it, whose counterpart has no VM.
        let apps = Project::at(wt.root.parent().unwrap()).unwrap();
        assert!(parent(&paths, &apps).is_none());
        assert_eq!(image(&paths, &apps, None), ImageName::default());
    }

    /// The VM's state after this process has dropped its lock. A process
    /// another test spawns at that moment holds a copy of the lock until it
    /// has exec'd (macOS's `posix_spawn` copies even close-on-exec
    /// descriptors first), so the lock may stay taken for some milliseconds.
    fn state_once_released(dir: &VmDir) -> State {
        let start = Instant::now();
        loop {
            let state = state(dir).unwrap();
            if state == State::Stopped || start.elapsed() > Duration::from_secs(2) {
                return state;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn copies_the_parent_while_it_is_stopped() {
        let temp = crate::util::TempDir::new("vm-fork");
        let paths = Paths::new(temp.path().join("home"));
        let (wt, parent) = worktree_with_parent(temp.path(), &paths);
        let parent_project = temp.path().join("main");
        let rust: ImageName = "rust".parse().unwrap();
        let vm = dir(&paths, &wt);

        let never = |_: &str, _: &str| -> Result<bool> { panic!("asked") };
        let no = |_: &str, _: &str| Ok(false);
        let yes = |_: &str, _: &str| Ok(true);

        // Running: nothing unless the user agrees to the base image, which
        // isn't there.
        let running = lock(&parent, &wt, &[]).unwrap();
        let declined = create(&paths, &wt, &vm, &rust, false, &no).unwrap_err();
        assert!(declined.is::<commands::Declined>(), "{declined:#}");
        assert!(!vm.path().exists());
        let error = create(&paths, &wt, &vm, &rust, false, &yes).unwrap_err();
        assert!(error.to_string().contains("rust image"), "{error:#}");
        drop(running);
        assert_eq!(state_once_released(&parent), State::Stopped);

        // Made from another image: from the base image without asking.
        let go: ImageName = "go".parse().unwrap();
        assert!(create(&paths, &wt, &vm, &go, false, &never).is_err());

        assert_eq!(
            create(&paths, &wt, &vm, &rust, false, &never).unwrap(),
            None
        );
        assert_eq!(fs::read_to_string(vm.disk()).unwrap(), "disk");
        assert_eq!(fs::read_to_string(vm.efi_vars()).unwrap(), "efi");
        let metadata = metadata(&vm).unwrap();
        assert_eq!(metadata.project_path, wt.root);
        assert_eq!(metadata.image, rust);
        assert_eq!(metadata.copy_of.as_ref(), Some(&parent_project));
        assert!(!vm.machine_id().exists() && !vm.mac().exists());
        // Named after the parent, which got its name first.
        assert_eq!(metadata.name, Some("main-wt".parse().unwrap()));
        assert_eq!(
            self::metadata(&parent).unwrap().name,
            Some("main".parse().unwrap())
        );
        // The parent can start again.
        assert_eq!(state_once_released(&parent), State::Stopped);
    }

    #[test]
    fn asks_about_shares_again_if_the_image_asks_for_others() {
        let temp = crate::util::TempDir::new("vm-other-shares");
        let paths = Paths::new(temp.path().join("home"));
        let (wt, parent) = worktree_with_parent(temp.path(), &paths);
        let rust: ImageName = "rust".parse().unwrap();
        let base = VmDir::new(paths.image_dir(&rust));
        write_marker(&provision::marker(&paths, &rust), "~/latest:ro");
        fs::write(base.disk(), "base disk").unwrap();
        fs::write(base.efi_vars(), "base efi").unwrap();
        let settings = VmSettings {
            cpus: 1,
            memory: ByteSize::gib(1),
            disk_size: ByteSize::gib(8),
            mounts: Vec::new(),
            expose_git: false,
            banner: true,
            image: None,
            network: Network::default(),
        };
        let yes = |_: &str, _: &str| Ok(true);
        let vm = dir(&paths, &wt);
        let _running = lock(&parent, &wt, &[]).unwrap();
        let other_shares = |script: &str, mount: &str| {
            write_marker_of(&parent.image_marker(), script, mount);
            let created = create_if_missing(&paths, &wt, &settings, false, &yes).unwrap();
            assert_eq!(fs::read_to_string(vm.disk()).unwrap(), "base disk");
            fs::remove_dir_all(vm.path()).unwrap();
            created.unwrap().other_shares
        };

        // Confirmed for the parent's build, made from the latest. Only the
        // shares count, not the scripts asking for them.
        assert!(other_shares("images/rust.sh", "~/parent:ro"));
        assert!(!other_shares("images/rust.sh", "~/latest:ro"));
        assert!(!other_shares("renamed.sh", "~/latest:ro"));
    }

    #[test]
    fn fresh_vms_of_worktrees_start_from_the_base_image() {
        let temp = crate::util::TempDir::new("vm-fresh");
        let paths = Paths::new(temp.path().join("home"));
        let (wt, parent) = worktree_with_parent(temp.path(), &paths);
        let rust: ImageName = "rust".parse().unwrap();
        let base = VmDir::new(paths.image_dir(&rust));
        fs::create_dir_all(base.path()).unwrap();
        fs::write(
            base.path().join("provisioned.toml"),
            format!(
                "revision = {}\ncustom_scripts = []\n",
                provision::BASE_REVISION
            ),
        )
        .unwrap();
        fs::write(base.disk(), "base disk").unwrap();
        fs::write(base.efi_vars(), "base efi").unwrap();
        let vm = dir(&paths, &wt);

        // Without asking, even while the parent runs.
        let never = |_: &str, _: &str| -> Result<bool> { panic!("asked") };
        let _running = lock(&parent, &wt, &[]).unwrap();
        assert_eq!(create(&paths, &wt, &vm, &rust, true, &never).unwrap(), None);
        assert_eq!(fs::read_to_string(vm.disk()).unwrap(), "base disk");
        assert_eq!(fs::read_to_string(vm.efi_vars()).unwrap(), "base efi");
        let metadata = metadata(&vm).unwrap();
        assert_eq!(metadata.image, rust);
        assert_eq!(metadata.copy_of, None);
    }

    /// Writes the marker of a build made at `built`.
    fn write_built(path: &Path, built: u64) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            format!(
                "revision = {}\ncustom_scripts = []\nbuilt = {built}\n",
                provision::BASE_REVISION
            ),
        )
        .unwrap();
    }

    #[test]
    fn knows_whether_it_was_made_from_an_earlier_build() {
        let temp = crate::util::TempDir::new("vm-earlier-build");
        let paths = Paths::new(temp.path().join("home"));
        let (wt, parent) = worktree_with_parent(temp.path(), &paths);
        let rust: ImageName = "rust".parse().unwrap();
        let go: ImageName = "go".parse().unwrap();
        let vm = dir(&paths, &wt);
        let build = |built: u64| write_built(&provision::marker(&paths, &rust), built);
        let new_vm = |image: &ImageName, fresh: bool| {
            new_vm_from_earlier_build(&paths, &wt, image, fresh).unwrap()
        };
        let made = || earlier_build(&paths, &vm, &rust).unwrap();
        let never = |_: &str, _: &str| -> Result<bool> { panic!("asked") };

        // Unknown while the latest build doesn't say when it was made.
        assert!(!new_vm(&rust, false));
        write_marker(&provision::marker(&paths, &rust), "~/old:ro");
        assert!(!new_vm(&rust, false));

        // A parent made before VMs kept a copy, or before the builds were
        // recorded, is from an earlier one, and so would be a copy of it,
        // unless the VM is to be fresh, or of another image.
        build(2);
        assert!(new_vm(&rust, false));
        write_marker(&parent.image_marker(), "~/old:ro");
        assert!(new_vm(&rust, false));
        assert!(!new_vm(&rust, true));
        write_built(&provision::marker(&paths, &go), 1);
        assert!(!new_vm(&go, false));

        // A copy of a parent made from the latest build is too, until the
        // image is built again; made again, it would be a copy again.
        write_built(&parent.image_marker(), 2);
        assert!(!new_vm(&rust, false));
        create(&paths, &wt, &vm, &rust, false, &never).unwrap();
        assert!(!made());
        build(3);
        assert!(made());
        assert!(new_vm(&rust, false));
        assert!(!new_vm(&rust, true));
        // Not from an earlier build of an image it wasn't made from.
        assert!(!earlier_build(&paths, &vm, &go).unwrap());

        // Made from the image, it is from the latest build until the next.
        fs::remove_dir_all(vm.path()).unwrap();
        let base = VmDir::new(paths.image_dir(&rust));
        fs::write(base.disk(), "base disk").unwrap();
        fs::write(base.efi_vars(), "base efi").unwrap();
        create(&paths, &wt, &vm, &rust, true, &never).unwrap();
        assert!(!made());
        build(4);
        assert!(made());
    }

    #[test]
    fn keeps_the_marker_of_the_build_its_disk_is_from() {
        let temp = crate::util::TempDir::new("vm-rebuilt");
        let paths = Paths::new(temp.path().join("home"));
        let project = Project::at(temp.path()).unwrap();
        let rust: ImageName = "rust".parse().unwrap();
        let vm = dir(&paths, &project);
        // Each build's files say which build they are.
        let build = |paths: &Paths, n: u64| {
            let partial = VmDir::new(paths.image_partial_dir(&rust));
            fs::create_dir_all(partial.path()).unwrap();
            fs::write(partial.disk(), n.to_string()).unwrap();
            fs::write(partial.efi_vars(), n.to_string()).unwrap();
            write_built(&partial.path().join("provisioned.toml"), n);
            provision::swap_in(paths, &rust).unwrap();
        };
        build(&paths, 0);
        let done = AtomicBool::new(false);
        let never = |_: &str, _: &str| -> Result<bool> { panic!("asked") };
        let (mut made, mut wrong) = (0, Vec::new());
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for n in 1.. {
                    if done.load(Ordering::Relaxed) {
                        break;
                    }
                    build(&paths, n);
                    std::thread::sleep(Duration::from_micros(500));
                }
            });
            for _ in 0..300 {
                match create(&paths, &project, &vm, &rust, false, &never) {
                    Ok(_) => made += 1,
                    // Missing for a moment, or rebuilt meanwhile.
                    Err(e) if e.to_string().starts_with("the rust image") => continue,
                    Err(e) => {
                        wrong.push(format!("{e:#}"));
                        continue;
                    }
                }
                let disk = fs::read_to_string(vm.disk()).unwrap();
                let built = provision::marker_built(&vm.image_marker()).unwrap();
                if built.map(|n| n.to_string()).as_ref() != Some(&disk) {
                    wrong.push(format!("disk of build {disk}, marker of {built:?}"));
                }
                fs::remove_dir_all(vm.path()).unwrap();
            }
            done.store(true, Ordering::Relaxed);
        });
        assert!(wrong.is_empty(), "{wrong:#?}");
        assert!(made > 0, "never made");
    }

    /// Writes an image's marker, or a VM's copy of one, to `path`, saying
    /// that the scripts asked for `mount`.
    fn write_marker(path: &Path, mount: &str) {
        write_marker_of(path, "images/rust.sh", mount);
    }

    /// Like `write_marker`, with the mount asked for by `script`.
    fn write_marker_of(path: &Path, script: &str, mount: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            format!(
                "revision = {}\ncustom_scripts = []\n\
                 [[mounts]]\nscript = \"{script}\"\nmount = \"{mount}\"\n",
                provision::BASE_REVISION
            ),
        )
        .unwrap();
    }

    #[test]
    fn keeps_the_shares_of_the_build_it_was_made_from() {
        let temp = crate::util::TempDir::new("vm-build-shares");
        let paths = Paths::new(temp.path().join("home"));
        let (wt, parent) = worktree_with_parent(temp.path(), &paths);
        let rust: ImageName = "rust".parse().unwrap();
        let go: ImageName = "go".parse().unwrap();
        let vm = dir(&paths, &wt);
        let build = |marker: &str| write_marker(&provision::marker(&paths, &rust), marker);
        let described = |mounts: Result<Vec<ScriptMount>>| -> Vec<String> {
            mounts
                .unwrap()
                .iter()
                .map(|m| m.mount.to_string())
                .collect()
        };
        let shares =
            |image: &ImageName, fresh: bool| described(script_mounts(&paths, &wt, image, fresh));
        let never = |_: &str, _: &str| -> Result<bool> { panic!("asked") };

        assert!(shares(&rust, false).is_empty());
        // A parent made before VMs kept a copy of the image's marker shares
        // what the latest build asked for, and so does a copy of it.
        build("~/first:ro");
        assert_eq!(shares(&rust, false), ["~/first:ro"]);
        create(&paths, &wt, &vm, &rust, false, &never).unwrap();
        assert!(!vm.image_marker().exists());
        build("~/second:ro");
        assert_eq!(shares(&rust, false), ["~/second:ro"]);
        fs::remove_dir_all(vm.path()).unwrap();

        // A parent's copy is what a copy of it would share, unless the VM
        // is to be fresh, or of another image: then it is made from the
        // image.
        write_marker(&parent.image_marker(), "~/parent:ro");
        assert_eq!(shares(&rust, false), ["~/parent:ro"]);
        assert_eq!(shares(&rust, true), ["~/second:ro"]);
        write_marker(&provision::marker(&paths, &go), "~/go:ro");
        assert_eq!(shares(&go, false), ["~/go:ro"]);

        // A copy keeps the parent's, also after the image is built again,
        // and made again it would be a copy of the parent again.
        create(&paths, &wt, &vm, &rust, false, &never).unwrap();
        assert_eq!(
            fs::read_to_string(vm.image_marker()).unwrap(),
            fs::read_to_string(parent.image_marker()).unwrap()
        );
        build("~/third:ro");
        assert_eq!(shares(&rust, false), ["~/parent:ro"]);
        let new_vm = |fresh| described(new_vm_mounts(&paths, &wt, &rust, fresh));
        assert_eq!(new_vm(false), ["~/parent:ro"]);
        assert_eq!(new_vm(true), ["~/third:ro"]);

        // Made from the image, it keeps that build's.
        fs::remove_dir_all(vm.path()).unwrap();
        let base = VmDir::new(paths.image_dir(&rust));
        fs::write(base.disk(), "base disk").unwrap();
        fs::write(base.efi_vars(), "base efi").unwrap();
        create(&paths, &wt, &vm, &rust, true, &never).unwrap();
        build("~/fourth:ro");
        assert_eq!(shares(&rust, false), ["~/third:ro"]);

        // A copy that can't be read, e.g. after it was edited by hand, says
        // how to get past it.
        fs::write(vm.image_marker(), "mounts = 1\n").unwrap();
        // Only read once the latest build says when it was made.
        let marker = provision::marker(&paths, &rust);
        let latest = fs::read_to_string(&marker).unwrap();
        fs::write(&marker, format!("built = 1\n{latest}")).unwrap();
        for error in [
            script_mounts(&paths, &wt, &rust, false).unwrap_err(),
            earlier_build(&paths, &vm, &rust).unwrap_err(),
        ] {
            let error = format!("{error:#}");
            assert!(
                error.contains("image.toml; `sendit reset` in the VM's project makes it again"),
                "{error}"
            );
        }

        // A VM made before VMs kept a copy shares what the latest build
        // asked for.
        fs::remove_file(vm.image_marker()).unwrap();
        assert_eq!(shares(&rust, false), ["~/fourth:ro"]);
    }

    #[test]
    fn records_the_pid_only_while_running() {
        let temp = crate::util::TempDir::new("vm-pid");
        let paths = Paths::new(temp.path().join("home"));
        let project = Project::at(temp.path()).unwrap();
        let vm = dir(&paths, &project);
        fs::create_dir_all(vm.path()).unwrap();

        let running = lock(&vm, &project, &[]).unwrap();
        assert_eq!(
            state(&vm).unwrap(),
            State::Running(Some(std::process::id()))
        );
        drop(running);
        assert_eq!(state_once_released(&vm), State::Stopped);
        // Held to copy or delete the VM, the lock doesn't point at the
        // process of the last run. Taken once a spawned process has let go
        // of its copy, as in `state_once_released`.
        let start = Instant::now();
        let _busy = loop {
            match try_lock(&vm).unwrap() {
                Some(lock) => break lock,
                None if start.elapsed() < Duration::from_secs(2) => {}
                None => panic!("the lock stays taken"),
            }
        };
        assert_eq!(state(&vm).unwrap(), State::Running(None));
        assert!(stop(&paths, &project).is_err());
    }

    #[test]
    fn leaves_nothing_behind_when_creating_fails() {
        let temp = crate::util::TempDir::new("vm-create-fails");
        let paths = Paths::new(temp.path().join("home"));
        let (wt, parent) = worktree_with_parent(temp.path(), &paths);
        fs::remove_file(parent.efi_vars()).unwrap();
        let never = |_: &str, _: &str| -> Result<bool> { panic!("asked") };
        let vm = dir(&paths, &wt);
        assert!(create(&paths, &wt, &vm, &"rust".parse().unwrap(), false, &never).is_err());
        let left: Vec<_> = fs::read_dir(paths.vms_dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(left, [parent.path().file_name().unwrap()]);
    }

    /// The words a POSIX shell makes of `line`.
    fn words(line: &str) -> Vec<String> {
        let output = Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s\\0' {line}"))
            .output()
            .unwrap();
        let mut words: Vec<String> = String::from_utf8(output.stdout)
            .unwrap()
            .split('\0')
            .map(String::from)
            .collect();
        words.pop();
        words
    }

    #[test]
    fn points_to_ssh_while_running() {
        let temp = crate::util::TempDir::new("vm-running");
        let paths = Paths::new(temp.path().join("home"));
        let project = Project::at(temp.path()).unwrap();
        let vm = dir(&paths, &project);
        fs::create_dir_all(vm.path()).unwrap();
        let _running = lock(&vm, &project, &[]).unwrap();
        let error = |command: &[&str]| {
            let command: Vec<String> = command.iter().map(|s| s.to_string()).collect();
            lock(&vm, &project, &command).unwrap_err().to_string()
        };
        // Tests don't run in the project, so it is named, by its directory
        // while the VM has no name.
        let ssh = format!("sendit -C {} ssh", quote(&temp.path().to_string_lossy()));
        let shell = error(&[]);
        assert!(
            shell.ends_with(&format!("`{ssh}` opens another shell in it")),
            "{shell}"
        );
        let command = error(&["cargo", "test", "it's"]);
        assert!(
            command.ends_with(&format!(
                "`{ssh} cargo test 'it'\\''s'` runs the command there"
            )),
            "{command}"
        );
        write_vm(&vm, temp.path(), Some("app"));
        let shell = error(&[]);
        assert!(
            shell.ends_with("`sendit -n app ssh` opens another shell in it"),
            "{shell}"
        );
    }

    /// Writes the metadata of a VM in `vm` for the project at `root`, with
    /// `name` if given, else as made before sendit named VMs.
    fn write_vm(vm: &VmDir, root: &Path, name: Option<&str>) {
        fs::create_dir_all(vm.path()).unwrap();
        let metadata = Metadata {
            project_path: root.to_path_buf(),
            image: ImageName::default(),
            base_revision: provision::BASE_REVISION,
            copy_of: None,
            name: name.map(|name| name.parse().unwrap()),
        };
        fs::write(vm.metadata(), toml::to_string(&metadata).unwrap()).unwrap();
    }

    #[test]
    fn names_vms_and_finds_them_by_name() {
        let temp = crate::util::TempDir::new("vm-names");
        let paths = Paths::new(temp.path().join("home"));
        let project = |path: &str| {
            let root = temp.path().join(path);
            fs::create_dir_all(&root).unwrap();
            let project = Project::at(&root).unwrap();
            (dir(&paths, &project), project)
        };
        let (a, app) = project("a/My App");
        let (b, other_app) = project("b/my_app");
        let (c, named) = project("c/web");
        let (_, none) = project("d/none");
        write_vm(&a, &app.root, None);
        write_vm(&b, &other_app.root, None);
        write_vm(&c, &named.root, Some("my-app"));
        let name = |vm: &VmDir| metadata(vm).unwrap().name.unwrap().to_string();

        // The names taken already stay; those made from the folder names
        // get numbers, in the order of the VMs' directories.
        name_vms(&paths).unwrap();
        let (first, second) = if a.path() < b.path() {
            (&a, &b)
        } else {
            (&b, &a)
        };
        assert_eq!(name(first), "my-app-2");
        assert_eq!(name(second), "my-app-3");
        assert_eq!(name(&c), "my-app");

        let found = find(&paths, &name(&a).parse().unwrap()).unwrap();
        assert_eq!((found.root, found.id), (app.root.clone(), app.id.clone()));
        let error = find(&paths, &"web".parse().unwrap()).unwrap_err();
        assert_eq!(
            error.to_string(),
            "no VM is named web; `sendit list` shows the names"
        );
        // Also once the project directory is gone.
        fs::remove_dir(&app.root).unwrap();
        assert_eq!(find(&paths, &name(&a).parse().unwrap()).unwrap().id, app.id);

        let web: VmName = "web".parse().unwrap();
        rename(&paths, &named, &web).unwrap();
        assert_eq!(name(&c), "web");
        assert_eq!(find(&paths, &web).unwrap().id, named.id);
        rename(&paths, &named, &web).unwrap();
        let error = rename(&paths, &other_app, &web).unwrap_err();
        assert!(
            error.to_string().ends_with("is named web already"),
            "{error}"
        );
        assert_eq!(name(&c), "web");
        let error = rename(&paths, &none, &web).unwrap_err();
        assert!(error.to_string().contains("has no VM yet"), "{error}");
        // The old name is free again, for the next VM made.
        assert_eq!(
            unique_name("my-app".parse().unwrap(), &names(&paths).unwrap()).as_str(),
            "my-app"
        );
    }

    #[test]
    fn takes_the_names_lock_only_to_name_vms() {
        let temp = crate::util::TempDir::new("vm-names-lock");
        let paths = Paths::new(temp.path().join("home"));
        let project = Project::at(temp.path()).unwrap();
        write_vm(&dir(&paths, &project), temp.path(), Some("app"));
        name_vms(&paths).unwrap();
        assert!(!paths.names_lock().exists());
    }

    #[test]
    fn gives_steps_after_reset_by_directory() {
        let temp = crate::util::TempDir::new("vm-reset-hint");
        let paths = Paths::new(temp.path().join("home"));
        let project = Project::at(temp.path()).unwrap();
        let vm = dir(&paths, &project);
        write_vm(&vm, temp.path(), Some("app-wt"));
        let mut metadata = metadata(&vm).unwrap();
        metadata.copy_of = Some(temp.path().join("main"));
        write_metadata(&vm, &metadata).unwrap();
        let settings = VmSettings {
            cpus: 1,
            memory: ByteSize::gib(1),
            disk_size: ByteSize::gib(8),
            mounts: Vec::new(),
            expose_git: false,
            banner: true,
            image: None,
            network: Network::default(),
        };
        let error = run(&paths, &project, &settings, true, None, &[], Detach::No)
            .unwrap_err()
            .to_string();
        // Tests don't run in the project. The name is gone after the reset.
        let expected = format!(
            "`sendit -n app-wt reset` deletes it so that `sendit -C {} run --fresh` starts \
             it over",
            quote(&temp.path().to_string_lossy())
        );
        assert!(error.contains(&expected), "{error}");
    }

    #[test]
    fn names_what_it_can() {
        use std::os::unix::fs::PermissionsExt;
        // Root writes into a read-only directory all the same.
        // SAFETY: plain syscall.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let temp = crate::util::TempDir::new("vm-names-read-only");
        let paths = Paths::new(temp.path().join("home"));
        let project = Project::at(temp.path()).unwrap();
        let vm = dir(&paths, &project);
        write_vm(&vm, temp.path(), None);
        let mode = |mode| fs::set_permissions(vm.path(), fs::Permissions::from_mode(mode));

        // A VM whose metadata can't be written stays without a name, with
        // a warning, and the command carries on.
        mode(0o555).unwrap();
        let named = name_vms(&paths);
        mode(0o755).unwrap();
        named.unwrap();
        assert_eq!(metadata(&vm).unwrap().name, None);
        // Renaming it gives it a name.
        let app: VmName = "app".parse().unwrap();
        rename(&paths, &project, &app).unwrap();
        assert_eq!(metadata(&vm).unwrap().name, Some(app));
    }

    #[test]
    fn names_vms_of_worktrees_after_their_main_worktree() {
        let temp = crate::util::TempDir::new("vm-names-worktrees");
        let paths = Paths::new(temp.path().join("home"));
        let (wt, parent) = subdirectory_with_parent(temp.path(), &paths, "apps/web");
        let vm = dir(&paths, &wt);
        write_vm(&vm, &wt.root, None);
        let name = |vm: &VmDir| metadata(vm).unwrap().name.unwrap().to_string();

        name_vms(&paths).unwrap();
        assert_eq!(name(&parent), "web");
        assert_eq!(name(&vm), "web-wt");

        // After the name of the main worktree's VM, not its folder.
        fs::remove_file(vm.metadata()).unwrap();
        write_vm(&vm, &wt.root, None);
        rename(
            &paths,
            &project_of(&parent, &metadata(&parent).unwrap()),
            &"site".parse().unwrap(),
        )
        .unwrap();
        name_vms(&paths).unwrap();
        assert_eq!(name(&vm), "site-wt");
    }

    #[test]
    fn shows_the_end_of_the_console_log() {
        let temp = crate::util::TempDir::new("log-tail");
        let log = temp.path().join("console.log");
        let lines: Vec<String> = (1..=LOG_TAIL + 5)
            .map(|n| format!("\x1b[01;32mline\x1b[0m {n}\r"))
            .collect();
        fs::write(&log, lines.join("\n")).unwrap();
        let tail = log_tail(&log);
        assert_eq!(tail.lines().count(), LOG_TAIL);
        assert!(tail.starts_with("line 6\nline 7\n"), "{tail:?}");
        assert!(
            tail.ends_with(&format!("line {}", LOG_TAIL + 5)),
            "{tail:?}"
        );
        assert_eq!(log_tail(&temp.path().join("missing")), "");

        // A prompt that sets the window title, ended by BEL or by ST.
        let prompt = "\x1b]0;dev@sendit: ~\x07\x1b[?2004h\x1b]2;t\x1b\\$ \x1b>ok\x1b(B\tdone";
        assert_eq!(plain_text(prompt), "$ ok\tdone");
    }

    #[test]
    fn ends_ssh_once_the_vm_has_stopped() {
        let exits = Command::new("sh").args(["-c", "exit 3"]).spawn().unwrap();
        let status = wait_for_ssh(exits, &mut || false).unwrap();
        assert_eq!(status.and_then(|s| s.code()), Some(3));

        // Like ssh to a VM that was forced off.
        let stuck = Command::new("sleep").arg("60").spawn().unwrap();
        let start = Instant::now();
        assert_eq!(wait_for_ssh(stuck, &mut || true).unwrap(), None);
        assert!(start.elapsed() < SSH_GRACE + Duration::from_secs(5));
    }

    #[test]
    fn runs_commands_in_a_login_shell() {
        let command: Vec<String> = ["echo", "it's", "$HOME", "a b", ""]
            .map(String::from)
            .to_vec();
        let line = words(&login_shell_command(&command));
        assert_eq!(line[..3], ["exec", "bash", "-lc"]);
        assert_eq!(line.len(), 4);
        assert_eq!(words(&line[3]), command);
    }

    #[test]
    fn quotes_ssh_commands() {
        let command =
            |args: &[&str]| shell_command(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>());
        assert_eq!(command(&["ls", "-la", "/tmp"]), "ls -la /tmp");
        assert_eq!(command(&["ls", "my file"]), "ls 'my file'");
        assert_eq!(command(&["echo", "it's", ""]), r"echo 'it'\''s' ''");
        assert_eq!(command(&["echo", "$HOME;", "*"]), "echo '$HOME;' '*'");
    }
}
