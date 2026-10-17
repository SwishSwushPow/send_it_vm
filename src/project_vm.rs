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

use std::fs::{self, File};
use std::io::{IsTerminal, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
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
use crate::paths::{ImageName, Paths, Project, is_project_id};
use crate::provision::{self, BaseState, ScriptMount};
use crate::util;
use crate::vm::{self, ConsoleMode, VmDir, VmSpec, net};
use crate::worktree;

/// How long `stop` waits for the VM to go away. `run` forces the VM off if
/// the guest hasn't shut down after `vm::STOP_TIMEOUT`.
const STOP_WAIT: Duration = Duration::from_secs(45);

/// How often taking a VM's lock is retried, 20 ms apart.
const LOCK_RETRIES: u32 = 5;

/// Written into a project VM's directory when it is created.
#[derive(Debug, Serialize, Deserialize)]
pub struct Metadata {
    pub project_path: PathBuf,
    pub image: ImageName,
    pub base_revision: u32,
    /// The project whose VM this one was made as a copy of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_of: Option<PathBuf>,
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
    provision::marker_mounts(&copy.unwrap_or_else(|| provision::marker(paths, image)))
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
    Ok(provision::marker_built(&vm.image_marker())? != Some(latest))
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
        return Ok(None);
    }
    let image = image(paths, project, settings.image.as_ref());
    let expected = new_vm_mounts(paths, project, &image, fresh)?;
    let note = create(paths, project, &dir, &image, fresh, ask)?;
    Ok(Some(Created {
        note,
        other_shares: script_mounts(paths, project, &image, fresh)? != expected,
    }))
}

/// Boots the project's VM, which `create_if_missing` has created, and runs
/// it until it stops, showing `note` once it starts. Without a
/// `command`, the console is attached. With one, the
/// command runs over SSH once the VM answers, as with `sendit ssh`, and the
/// VM shuts down when it has ended. Returns its exit code, or if a signal
/// stopped the VM first, 128 plus its number; else 0.
pub fn run(
    paths: &Paths,
    project: &Project,
    settings: &VmSettings,
    fresh: bool,
    note: Option<String>,
    command: &[String],
) -> Result<i32> {
    let dir = dir(paths, project);
    let metadata = metadata(&dir)?;
    if fresh && let Some(copy_of) = &metadata.copy_of {
        bail!(
            "{} is a copy of the VM of {} already; `sendit reset` deletes it so \
             that `run --fresh` starts it over from the {} image",
            paths.display(dir.path()),
            paths.display(copy_of),
            metadata.image,
        );
    }
    if let Some(image) = &settings.image {
        ensure!(
            metadata.image == *image,
            "{} was created from the {} image, not {image}; `sendit reset` deletes \
             it so the next run starts over from the {image} image",
            paths.display(dir.path()),
            metadata.image,
        );
    }
    ensure!(
        !metadata.outdated(),
        "{} was created from an older base image that this version of sendit \
         can't run; `sendit reset` deletes it so the next run starts over from \
         the current base image",
        paths.display(dir.path())
    );
    if !command.is_empty() {
        // Rather now than after booting.
        login(paths, false)?;
    }
    let _lock = lock(&dir, project, command)?;
    // The VM starts now: its boot and banner come first on the screen. A
    // command's output follows on from what is there, like any command's.
    if command.is_empty() {
        vm::clear_screen();
    }
    resize_disk(paths, &dir, settings)?;

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
    let shutdown = Arc::new(AtomicBool::new(false));
    let spec = VmSpec {
        cpus: settings.cpus,
        memory: settings.memory,
        shares: mounts::prepare(settings, &dir.meta())?,
        seed: None,
        console: if command.is_empty() {
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
        "the VM is not running; start it with `sendit run`"
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
    let result = (|| {
        let start = Instant::now();
        loop {
            ensure!(
                !stopped.load(Ordering::Relaxed),
                "the VM stopped before the command could run"
            );
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
                break;
            }
            ensure!(
                start.elapsed() < SSH_WAIT,
                "the VM didn't answer over SSH within {} seconds",
                SSH_WAIT.as_secs()
            );
            std::thread::sleep(Duration::from_millis(500));
        }
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

/// Waits until the ssh process `child` exits and returns its status. Once
/// `stopped` is set, it gets `SSH_GRACE` to exit, then is terminated, and
/// the result is `None`.
fn wait_for_ssh(mut child: Child, stopped: &AtomicBool) -> Result<Option<ExitStatus>> {
    let mut deadline = None;
    loop {
        if let Some(status) = child.try_wait().context("waiting for ssh")? {
            return Ok(Some(status));
        }
        if stopped.load(Ordering::Relaxed)
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
                eprintln!(
                    "Stop it (`sendit stop` in {main}), then run again to start from a copy."
                );
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
        let metadata = toml::to_string(&Metadata {
            project_path: project.root.clone(),
            image: image.clone(),
            base_revision,
            copy_of,
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
fn resize_disk(paths: &Paths, dir: &VmDir, settings: &VmSettings) -> Result<()> {
    let current = fs::metadata(dir.disk())
        .with_context(|| format!("reading {}", dir.disk().display()))?
        .len();
    if current > settings.disk_size.0 {
        eprintln!(
            "warning: the disk of {} is already {} and can't shrink to {}; `sendit reset` recreates it",
            paths.display(dir.path()),
            ByteSize(current),
            settings.disk_size
        );
    }
    image::grow_disk(&dir.disk(), settings.disk_size)
}

/// Makes sure only one process runs a VM at a time; two would corrupt its
/// disk. The lock is released when the returned file is closed, even if the
/// process dies. If the VM is running, the error says how to reach it, to
/// run `command` there or else open a shell.
fn lock(dir: &VmDir, project: &Project, command: &[String]) -> Result<File> {
    let Some(mut file) = try_lock(dir)? else {
        // Run elsewhere, e.g. with --worktree, `sendit ssh` needs the
        // project too.
        let here = std::env::current_dir().and_then(|cwd| cwd.canonicalize());
        let mut ssh = match here {
            Ok(cwd) if cwd == project.root => "sendit ssh".to_string(),
            _ => format!("sendit -C {} ssh", quote(&project.root.to_string_lossy())),
        };
        let does = if command.is_empty() {
            "opens another shell in it"
        } else {
            ssh = format!("{ssh} {}", shell_command(command));
            "runs the command there"
        };
        bail!("this project's VM is already running; `{ssh}` {does}");
    };
    write!(file, "{}", std::process::id())?;
    Ok(file)
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
        };
        let yes = |_: &str, _: &str| Ok(true);
        let vm = dir(&paths, &wt);
        let _running = lock(&parent, &wt, &[]).unwrap();
        let other_shares = |parent_marker: &str| {
            write_marker(&parent.image_marker(), parent_marker);
            let created = create_if_missing(&paths, &wt, &settings, false, &yes).unwrap();
            assert_eq!(fs::read_to_string(vm.disk()).unwrap(), "base disk");
            fs::remove_dir_all(vm.path()).unwrap();
            created.unwrap().other_shares
        };

        // Confirmed for the parent's build, made from the latest.
        assert!(other_shares("~/parent:ro"));
        assert!(!other_shares("~/latest:ro"));
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
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            format!(
                "revision = {}\ncustom_scripts = []\n\
                 [[mounts]]\nscript = \"images/rust.sh\"\nmount = \"{mount}\"\n",
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
        // process of the last run.
        let _busy = try_lock(&vm).unwrap().unwrap();
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
        // Tests don't run in the project, so it is named.
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
    }

    #[test]
    fn ends_ssh_once_the_vm_has_stopped() {
        let stopped = AtomicBool::new(false);
        let exits = Command::new("sh").args(["-c", "exit 3"]).spawn().unwrap();
        let status = wait_for_ssh(exits, &stopped).unwrap();
        assert_eq!(status.and_then(|s| s.code()), Some(3));

        // Like ssh to a VM that was forced off.
        stopped.store(true, Ordering::Relaxed);
        let stuck = Command::new("sleep").arg("60").spawn().unwrap();
        let start = Instant::now();
        assert_eq!(wait_for_ssh(stuck, &stopped).unwrap(), None);
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
