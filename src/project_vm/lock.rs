//! The lock on `run/lock` that a VM's `sendit run` process holds while the
//! VM runs, with its PID in it: only one process may run a VM at a time.

use std::fs::{self, File};
use std::io::Write;
use std::time::Duration;

use anyhow::{Context, Result};

use super::sendit_for;
use super::ssh::shell_command;
use crate::paths::Project;
use crate::util;
use crate::vm::VmDir;

/// How often taking a VM's lock is retried, 20 ms apart.
const LOCK_RETRIES: u32 = 5;

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

/// Makes sure only one process runs a VM at a time; two would corrupt its
/// disk. The lock is released when the returned file is closed, even if the
/// process dies. If the VM is running, the error says how to reach it, to
/// run `command` there or else open a shell.
pub(super) fn lock(dir: &VmDir, project: &Project, command: &[String]) -> Result<File> {
    let Some(mut file) = try_lock(dir)? else {
        return Err(already_running(dir, project, command));
    };
    write!(file, "{}", std::process::id())?;
    Ok(file)
}

/// The error for starting the project's VM, in `dir`, while it runs, which
/// says how to reach it, to run `command` there or else open a shell.
pub(super) fn already_running(dir: &VmDir, project: &Project, command: &[String]) -> anyhow::Error {
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

/// Takes the VM's lock without recording a PID; `None` if the VM is running.
/// The PID of its last run is cleared: once that process has ended, the PID
/// may belong to another process, which `stop` would signal.
pub(super) fn try_lock(dir: &VmDir) -> Result<Option<File>> {
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
}
