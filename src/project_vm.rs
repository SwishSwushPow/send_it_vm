//! Per-project VMs, created from the base image on first `run`.
//!
//! A project VM's disk and EFI variables are APFS clones of the base image's,
//! so creating one is instant and takes no space until the guest writes. The
//! machine identifier and MAC address are not copied: `vm::config` generates
//! new ones on first boot and saves them, so every VM has its own. The VM is
//! built in `.<id>.partial/` and only renamed into place once complete.
//!
//! While a VM runs, its `sendit run` process holds a lock on `run/lock` and
//! has written its PID there; that is how the other commands find it.

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::config::{ByteSize, GUEST_USER, VmSettings};
use crate::image;
use crate::mounts;
use crate::paths::{ImageName, Paths, Project};
use crate::provision::{self, BaseState};
use crate::util;
use crate::vm::{self, VmDir, VmSpec, net};

/// How long `stop` waits for the VM to go away. `run` forces the VM off if
/// the guest hasn't shut down after `vm::STOP_TIMEOUT`.
const STOP_WAIT: Duration = Duration::from_secs(45);

/// How often taking a VM's lock is retried, 20 ms apart.
const LOCK_RETRIES: u32 = 5;

/// Written into a project VM's directory when it is created.
#[derive(Debug, Serialize, Deserialize)]
pub struct Metadata {
    pub project_path: PathBuf,
    /// VMs from before named images were made from the `default` image.
    #[serde(default)]
    pub image: ImageName,
    #[serde(default = "provision::first_revision")]
    pub base_revision: u32,
    #[serde(default)]
    pub sendit_version: String,
    #[serde(default)]
    pub created_at_unix: u64,
}

impl Metadata {
    /// Whether the VM was created from a base image older than this version
    /// of sendit can run.
    pub fn outdated(&self) -> bool {
        self.base_revision < provision::BASE_REVISION
    }
}

/// The directory of the project's VM.
pub fn dir(paths: &Paths, project: &Project) -> VmDir {
    VmDir::new(paths.vm_dir(project))
}

pub fn metadata(dir: &VmDir) -> Result<Metadata> {
    let path = dir.metadata();
    util::read_toml(&path)?.with_context(|| format!("{} is missing", path.display()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Stopped,
    /// Running under this `sendit run` process. The PID is missing for a
    /// moment while the process starts up.
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

/// All project VM directories, sorted by name. Unfinished `.partial`
/// directories are skipped.
pub fn all(paths: &Paths) -> Result<Vec<VmDir>> {
    let root = paths.vms_dir();
    let Some(entries) = util::if_exists(fs::read_dir(&root))
        .with_context(|| format!("reading {}", root.display()))?
    else {
        return Ok(Vec::new());
    };
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() && !entry.file_name().to_string_lossy().starts_with('.') {
            dirs.push(VmDir::new(entry.path()));
        }
    }
    dirs.sort_by(|a, b| a.path().cmp(b.path()));
    Ok(dirs)
}

/// Boots the project's VM, creating it first if needed, and attaches the
/// console until it stops.
pub fn run(paths: &Paths, project: &Project, settings: &VmSettings) -> Result<()> {
    let dir = dir(paths, project);
    if !dir.path().exists() {
        create(paths, project, &dir, &settings.image)?;
    }
    let metadata = metadata(&dir)?;
    ensure!(
        metadata.image == settings.image,
        "{} was created from the {} image, not {}; `sendit reset` deletes it so \
         the next run starts over from the {} image",
        paths.display(dir.path()),
        metadata.image,
        settings.image,
        settings.image
    );
    ensure!(
        !metadata.outdated(),
        "{} was created from an older base image that this version of sendit \
         can't run; `sendit reset` deletes it so the next run starts over from \
         the current base image",
        paths.display(dir.path())
    );
    let _lock = lock(&dir)?;
    resize_disk(paths, &dir, settings)?;

    eprintln!(
        "Starting {} ({} CPUs, {} memory, {} disk)",
        paths.display(dir.path()),
        settings.cpus,
        settings.memory,
        settings.disk_size
    );
    let spec = VmSpec {
        cpus: settings.cpus,
        memory: settings.memory,
        shares: mounts::prepare(settings, &dir.meta())?,
        seed: None,
        provision_log: None,
    };
    vm::run(&dir, &spec)
}

/// Asks the project's running VM to shut down and waits until it has.
pub fn stop(paths: &Paths, project: &Project) -> Result<()> {
    let dir = dir(paths, project);
    let pid = match state(&dir)? {
        State::Stopped => {
            eprintln!("The VM is not running.");
            return Ok(());
        }
        State::Running(None) => bail!("the VM is just starting; try again in a moment"),
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
    let ip = net::vm_ip(&dir)?.with_context(|| {
        format!(
            "the VM has no IP address in {} yet; it may still be booting",
            net::LEASES_FILE
        )
    })?;

    let (user, key) = if root {
        ("root", paths.root_ssh_key())
    } else {
        (GUEST_USER, paths.ssh_key())
    };
    ensure!(
        key.exists(),
        "{} is missing; `sendit provision --force` creates it",
        paths.display(&key)
    );
    // Each VM gets its own known_hosts: IP addresses are reused across VMs,
    // but a VM's host keys stay the same for its lifetime.
    let known_hosts = dir.known_hosts();
    let error = Command::new("ssh")
        .arg("-i")
        .arg(&key)
        .args(["-o", "IdentitiesOnly=yes"])
        .args(["-o", "StrictHostKeyChecking=accept-new"])
        .arg("-o")
        .arg(format!("UserKnownHostsFile={}", known_hosts.display()))
        .args(["-o", "LogLevel=ERROR"])
        // The guest's sshd accepts it; ssh doesn't send it by default.
        .args(["-o", "SendEnv=COLORTERM"])
        .arg(format!("{user}@{ip}"))
        // ssh joins the command's arguments with spaces for the remote shell;
        // quote them so they arrive as they were given.
        .args((!command.is_empty()).then(|| shell_command(command)))
        .exec();
    Err(error).context("running ssh")
}

/// Joins `args` into a command line for a POSIX shell that runs them as given.
fn shell_command(args: &[String]) -> String {
    let quote = |arg: &String| {
        let safe = !arg.is_empty()
            && arg
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"%+,-./:=@_".contains(&b));
        if safe {
            arg.clone()
        } else {
            format!("'{}'", arg.replace('\'', r"'\''"))
        }
    };
    args.iter().map(quote).collect::<Vec<_>>().join(" ")
}

/// Deletes a stopped VM. It holds the VM's lock meanwhile, so the VM can't
/// be started while it is being deleted.
pub fn delete(dir: &VmDir) -> Result<()> {
    let Some(_lock) = try_lock(dir)? else {
        bail!("{} is running; stop it first", dir.path().display());
    };
    fs::remove_dir_all(dir.path()).with_context(|| format!("deleting {}", dir.path().display()))
}

fn create(paths: &Paths, project: &Project, dir: &VmDir, image: &ImageName) -> Result<()> {
    match provision::base_state(paths, image)? {
        BaseState::Missing => bail!(
            "the {image} image isn't provisioned yet; run `{}` first",
            provision::command(image, false)
        ),
        BaseState::Outdated => bail!(
            "the {image} image is outdated; rebuild it with `{}`",
            provision::command(image, true)
        ),
        BaseState::ScriptsChanged | BaseState::Current => {}
    }
    let base = VmDir::new(paths.image_dir(image));
    let partial = VmDir::new(paths.vms_dir().join(format!(".{}.partial", project.id)));
    let _ = fs::remove_dir_all(partial.path());
    fs::create_dir_all(partial.path())
        .with_context(|| format!("creating {}", partial.path().display()))?;

    eprintln!(
        "Creating {} from the {image} image",
        paths.display(dir.path())
    );
    image::clone_file(&base.disk(), &partial.disk())?;
    image::clone_file(&base.efi_vars(), &partial.efi_vars())?;
    let metadata = toml::to_string(&Metadata {
        project_path: project.root.clone(),
        image: image.clone(),
        base_revision: provision::BASE_REVISION,
        sendit_version: env!("CARGO_PKG_VERSION").into(),
        created_at_unix: util::unix_now()?,
    })?;
    fs::write(partial.metadata(), metadata)?;

    fs::rename(partial.path(), dir.path())
        .with_context(|| format!("moving the new VM to {}", dir.path().display()))
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
/// process dies.
fn lock(dir: &VmDir) -> Result<File> {
    let Some(mut file) = try_lock(dir)? else {
        bail!("this project's VM is already running");
    };
    file.set_len(0)?;
    write!(file, "{}", std::process::id())?;
    Ok(file)
}

/// Takes the VM's lock without recording a PID; `None` if the VM is running.
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
            Ok(()) => return Ok(Some(file)),
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

    #[test]
    fn reads_metadata_from_before_named_images() {
        let metadata: Metadata = toml::from_str(
            "project_path = \"/p\"\nbase_revision = 7\nsendit_version = \"0.1.0\"\n",
        )
        .unwrap();
        assert_eq!(metadata.image, ImageName::default());
        assert!(!metadata.outdated());
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
