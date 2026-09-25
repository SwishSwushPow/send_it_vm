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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::config::{ByteSize, VmSettings};
use crate::image;
use crate::mounts;
use crate::paths::{Paths, Project};
use crate::provision;
use crate::vm::{self, VmDir, VmSpec, net};

/// How long `stop` waits for the VM to go away. `run` forces the VM off if
/// the guest hasn't shut down after `vm::STOP_TIMEOUT`.
const STOP_WAIT: Duration = Duration::from_secs(45);

/// Written into a project VM's directory when it is created.
#[derive(Debug, Serialize, Deserialize)]
pub struct Metadata {
    pub project_path: PathBuf,
    #[serde(default = "provision::first_revision")]
    pub base_revision: u32,
    #[serde(default)]
    pub sendit_version: String,
    #[serde(default)]
    pub created_at_unix: u64,
}

fn metadata_file(dir: &VmDir) -> PathBuf {
    dir.path().join("project.toml")
}

pub fn metadata(dir: &VmDir) -> Result<Metadata> {
    let path = metadata_file(dir);
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("in {}", path.display()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Stopped,
    /// Running under this `sendit run` process. The PID is missing for a
    /// moment while the process starts up.
    Running(Option<u32>),
}

pub fn state(dir: &VmDir) -> Result<State> {
    let path = lock_file(dir);
    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(State::Stopped),
        Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
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
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", root.display())),
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
    let dir = VmDir::new(paths.vm_dir(project));
    if !dir.path().exists() {
        create(paths, project, &dir)?;
    }
    ensure!(
        metadata(&dir)?.base_revision >= provision::BASE_REVISION,
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
        shares: mounts::prepare(settings, &dir.path().join("run/meta"))?,
        seed: None,
        provision_log: None,
    };
    vm::run(&dir, &spec)
}

/// Asks the project's running VM to shut down and waits until it has.
pub fn stop(paths: &Paths, project: &Project) -> Result<()> {
    let dir = VmDir::new(paths.vm_dir(project));
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
/// running `command` if given.
pub fn ssh(paths: &Paths, project: &Project, command: &[String]) -> Result<()> {
    let dir = VmDir::new(paths.vm_dir(project));
    ensure!(
        state(&dir)? != State::Stopped,
        "the VM is not running; start it with `sendit run`"
    );
    let mac = fs::read_to_string(dir.mac())
        .with_context(|| format!("reading {}", dir.mac().display()))?;
    let ip = net::lease_for(mac.trim())?.with_context(|| {
        format!(
            "the VM has no IP address in {} yet; it may still be booting",
            net::LEASES_FILE
        )
    })?;

    let key = paths.ssh_dir().join("id_ed25519");
    // Each VM gets its own known_hosts: IP addresses are reused across VMs,
    // but a VM's host keys stay the same for its lifetime.
    let known_hosts = dir.path().join("known_hosts");
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
        .arg(format!("dev@{ip}"))
        .args(command)
        .exec();
    Err(error).context("running ssh")
}

/// Deletes a stopped VM.
pub fn delete(dir: &VmDir) -> Result<()> {
    ensure!(
        state(dir)? == State::Stopped,
        "{} is running; stop it first",
        dir.path().display()
    );
    fs::remove_dir_all(dir.path()).with_context(|| format!("deleting {}", dir.path().display()))
}

fn create(paths: &Paths, project: &Project, dir: &VmDir) -> Result<()> {
    ensure!(
        provision::marker(paths).exists(),
        "there is no base image yet; run `sendit provision` first"
    );
    ensure!(
        provision::base_revision(paths)? >= provision::BASE_REVISION,
        "the base image is outdated; rebuild it with `sendit provision --force`"
    );
    let base = VmDir::new(paths.base_dir());
    let partial = VmDir::new(paths.vms_dir().join(format!(".{}.partial", project.id)));
    let _ = fs::remove_dir_all(partial.path());
    fs::create_dir_all(partial.path())
        .with_context(|| format!("creating {}", partial.path().display()))?;

    eprintln!("Creating {} from the base image", paths.display(dir.path()));
    image::clone_file(&base.disk(), &partial.disk())?;
    image::clone_file(&base.efi_vars(), &partial.efi_vars())?;
    let metadata = toml::to_string(&Metadata {
        project_path: project.root.clone(),
        base_revision: provision::BASE_REVISION,
        sendit_version: env!("CARGO_PKG_VERSION").into(),
        created_at_unix: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
    })?;
    fs::write(metadata_file(&partial), metadata)?;

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

fn lock_file(dir: &VmDir) -> PathBuf {
    dir.path().join("run/lock")
}

/// Makes sure only one process runs a VM at a time; two would corrupt its
/// disk. The lock is released when the returned file is closed, even if the
/// process dies.
fn lock(dir: &VmDir) -> Result<File> {
    let path = lock_file(dir);
    fs::create_dir_all(dir.path().join("run"))?;
    let mut file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(fs::TryLockError::WouldBlock) => bail!("this project's VM is already running"),
        Err(fs::TryLockError::Error(e)) => {
            return Err(e).with_context(|| format!("locking {}", path.display()));
        }
    }
    file.set_len(0)?;
    write!(file, "{}", std::process::id())?;
    Ok(file)
}
