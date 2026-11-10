//! The lock on `.<dir>.lock` next to each VM's directory: only one sendit
//! process at a time makes, runs, copies or deletes a VM. `sendit run`
//! holds it from before it makes the VM until the VM has stopped, with its
//! PID in it once the VM starts. The other commands only look at it.

use std::fs::{self, File};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, ensure};

use super::sendit_for;
use super::ssh::shell_command;
use crate::paths::Project;
use crate::util;
use crate::vm::VmDir;

/// How often taking a VM's lock is retried, 20 ms apart.
const LOCK_RETRIES: u32 = 5;

/// Set to the number of the descriptor of the VM's lock for the `sendit
/// run` process that `run --detach` starts in the background, which the
/// starting process passes the lock on to.
const LOCK_FD_ENV: &str = "SENDIT_LOCK_FD";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Stopped,
    /// Another sendit process holds the lock and the VM isn't running yet:
    /// `sendit run` is making it, asking about it or starting up, or
    /// another command is copying or deleting it. `BUSY` says so.
    Busy,
    /// Running under this `sendit run` process.
    Running(u32),
}

/// What `State::Busy` means, for messages.
pub const BUSY: &str =
    "the VM is busy: another sendit process is making, starting, copying or deleting it";

/// A VM's lock, held until it is dropped, or the process ends.
#[derive(Debug)]
pub struct Lock(File);

impl Lock {
    /// Records this process's PID in the lock, as the one running the VM,
    /// which `stop` signals.
    pub(super) fn record_pid(&self) -> Result<()> {
        self.0
            .write_all_at(std::process::id().to_string().as_bytes(), 0)
            .context("recording the PID in the VM's lock")
    }

    /// Passes the lock on to the `sendit run` process that `command`
    /// starts, which takes it with `inherited`. Returns its descriptor,
    /// which a `pre_exec` hook must keep open across exec. The lock is then
    /// held until both processes have let go of it, so no other process
    /// can take it in between.
    pub(super) fn pass_to(&self, command: &mut Command) -> RawFd {
        let fd = self.0.as_raw_fd();
        command.env(LOCK_FD_ENV, fd.to_string());
        fd
    }

    /// The lock of the VM in `dir` that the process that started this one
    /// passed on with `pass_to`.
    pub(super) fn inherited(dir: &VmDir) -> Result<Lock> {
        let fd: RawFd = std::env::var(LOCK_FD_ENV)
            .ok()
            .and_then(|fd| fd.parse().ok())
            .context("the VM's lock was not passed on")?;
        // Not to be passed on further, e.g. to keep the VM looking like it
        // runs after this process has ended.
        // SAFETY: plain syscall; fails unless the descriptor is open.
        let set = unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        ensure!(set != -1, "the VM's lock was not passed on");
        // SAFETY: the descriptor is open, and was only passed on to be
        // taken here, once.
        let file = unsafe { File::from_raw_fd(fd) };
        let path = dir.lock();
        let (passed, there) = (file.metadata()?, fs::metadata(&path)?);
        ensure!(
            passed.dev() == there.dev() && passed.ino() == there.ino(),
            "the lock passed on is not {}",
            path.display()
        );
        Ok(Lock(file))
    }
}

/// Whether the VM is running, by whether its lock is taken. The lock is
/// taken shared for a moment to check, so that checks don't collide with
/// each other.
pub fn state(dir: &VmDir) -> Result<State> {
    let path = dir.lock();
    let Some(mut file) = util::if_exists(File::open(&path))
        .with_context(|| format!("opening {}", path.display()))?
    else {
        return Ok(State::Stopped);
    };
    match file.try_lock_shared() {
        Ok(()) => Ok(State::Stopped),
        Err(fs::TryLockError::WouldBlock) => {
            let mut pid = String::new();
            std::io::Read::read_to_string(&mut file, &mut pid)?;
            Ok(match pid.trim().parse() {
                Ok(pid) => State::Running(pid),
                Err(_) => State::Busy,
            })
        }
        Err(fs::TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("locking {}", path.display()))
        }
    }
}

/// Takes the lock of the project's VM, in `dir`, to make and run it; two
/// processes running it would corrupt its disk. If the VM is running, the
/// error says how to reach it, to run `command` there or else open a
/// shell.
pub(super) fn lock(dir: &VmDir, project: &Project, command: &[String]) -> Result<Lock> {
    if let Some(lock) = try_lock(dir)? {
        return Ok(lock);
    }
    match state(dir)? {
        State::Busy => anyhow::bail!("{BUSY}; try again in a moment"),
        _ => Err(already_running(dir, project, command)),
    }
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

/// Takes the VM's lock without recording a PID; `None` if another process
/// has it. The PID of its last run is cleared: once that process has
/// ended, the PID may belong to another process, which `stop` would
/// signal.
pub(super) fn try_lock(dir: &VmDir) -> Result<Option<Lock>> {
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
                return Ok(Some(Lock(file)));
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
    use super::super::ssh::quote;
    use super::super::test_util::*;
    use super::super::{dir, stop};
    use super::*;
    use crate::paths::Paths;
    use std::time::Instant;

    #[test]
    fn records_the_pid_only_while_running() {
        let temp = crate::util::TempDir::new("vm-pid");
        let paths = Paths::new(temp.path().join("home"));
        let project = Project::at(temp.path()).unwrap();
        let vm = dir(&paths, &project);
        fs::create_dir_all(vm.path()).unwrap();

        // Made and started, but not yet running.
        let running = lock(&vm, &project, &[]).unwrap();
        assert_eq!(state(&vm).unwrap(), State::Busy);
        running.record_pid().unwrap();
        assert_eq!(state(&vm).unwrap(), State::Running(std::process::id()));
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
        assert_eq!(state(&vm).unwrap(), State::Busy);
        assert!(stop(&paths, &project).is_err());
    }

    #[test]
    fn checks_dont_collide() {
        let temp = crate::util::TempDir::new("vm-checks");
        let paths = Paths::new(temp.path().join("home"));
        let project = Project::at(temp.path()).unwrap();
        let vm = dir(&paths, &project);
        fs::create_dir_all(vm.path()).unwrap();
        let last_run = lock(&vm, &project, &[]).unwrap();
        last_run.record_pid().unwrap();
        drop(last_run);
        assert_eq!(state_once_released(&vm), State::Stopped);
        // Another check, e.g. by `sendit list`, at the same instant: the
        // VM is still stopped, not running under the last run's PID.
        let other = File::open(vm.lock()).unwrap();
        other.try_lock_shared().unwrap();
        assert_eq!(state(&vm).unwrap(), State::Stopped);
    }

    #[test]
    fn covers_making_and_deleting_the_vm() {
        let temp = crate::util::TempDir::new("vm-lock-outside");
        let paths = Paths::new(temp.path().join("home"));
        let project = Project::at(temp.path()).unwrap();
        let vm = dir(&paths, &project);
        fs::create_dir_all(paths.vms_dir()).unwrap();
        // Taken before the VM exists, to make it.
        let making = lock(&vm, &project, &[]).unwrap();
        assert!(!vm.path().exists());
        assert!(try_lock(&vm).unwrap().is_none());
        drop(making);
        assert_eq!(state_once_released(&vm), State::Stopped);
        // Still held once its directory is gone, as while `reset` deletes
        // it.
        fs::create_dir_all(vm.path()).unwrap();
        let deleting = lock(&vm, &project, &[]).unwrap();
        fs::remove_dir_all(vm.path()).unwrap();
        fs::create_dir_all(vm.path()).unwrap();
        assert!(try_lock(&vm).unwrap().is_none());
        drop(deleting);
    }

    #[test]
    fn points_to_ssh_while_running() {
        let temp = crate::util::TempDir::new("vm-running");
        let paths = Paths::new(temp.path().join("home"));
        let project = Project::at(temp.path()).unwrap();
        let vm = dir(&paths, &project);
        fs::create_dir_all(vm.path()).unwrap();
        let running = lock(&vm, &project, &[]).unwrap();
        let error = |command: &[&str]| {
            let command: Vec<String> = command.iter().map(|s| s.to_string()).collect();
            lock(&vm, &project, &command).unwrap_err().to_string()
        };
        // Not yet running, e.g. while asking about shares: no shell to open.
        let busy = error(&[]);
        assert!(busy.starts_with(BUSY), "{busy}");
        running.record_pid().unwrap();
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
}
