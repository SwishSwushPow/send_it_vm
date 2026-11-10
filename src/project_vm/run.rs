//! Running a project's VM, attached to the terminal or in the background,
//! and stopping it.

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

use super::lock::{self, BUSY, Lock, State, state};
use super::ssh::{login, run_command, wait_until_answers};
use super::{dir, metadata, sendit_for};
use crate::config::{ByteSize, VmSettings};
use crate::image;
use crate::mounts;
use crate::paths::{Paths, Project};
use crate::vm::{self, ConsoleMode, VmDir, VmSpec};

/// How long `stop` waits for the VM to go away. `run` forces the VM off if
/// the guest hasn't shut down after `vm::STOP_TIMEOUT`.
const STOP_WAIT: Duration = Duration::from_secs(45);

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

/// Takes the lock of the project's VM, to make it if need be and run it
/// with `run`; if the VM is running, the error says how to reach it, to run
/// `command` there or else open a shell. The process in the background
/// takes the lock that the one that started it passed on.
pub fn lock_to_run(
    paths: &Paths,
    project: &Project,
    command: &[String],
    detach: Detach,
) -> Result<Lock> {
    let dir = dir(paths, project);
    match detach {
        Detach::Background => Lock::inherited(&dir),
        Detach::No | Detach::Start => lock::lock(&dir, project, command),
    }
}

/// Boots the project's VM, which exists or `create` has made, under its
/// `lock`, and runs it until it stops, showing `note` once it starts.
/// Without a `command`, the console is attached. With one, the command runs
/// over SSH once the VM answers, as with `sendit ssh`, and the
/// VM shuts down when it has ended. Returns its exit code, or if a signal
/// stopped the VM first, 128 plus its number; else 0. `detach` says where
/// the VM runs; in the background, it runs without a command.
pub fn run(
    paths: &Paths,
    project: &Project,
    settings: &VmSettings,
    note: Option<String>,
    command: &[String],
    detach: Detach,
    lock: Lock,
) -> Result<i32> {
    let dir = dir(paths, project);
    let metadata = metadata(&dir)?;
    let sendit = sendit_for(&dir, project);
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
    let run_dir = dir.run_dir();
    fs::create_dir_all(&run_dir).with_context(|| format!("creating {}", run_dir.display()))?;
    if detach == Detach::Start {
        // Here, where it is seen; the disk grows in the background.
        warn_if_shrinking(paths, &dir, project, settings)?;
        starting(note);
        return run_in_background(paths, project, &dir, &lock);
    }
    lock.record_pid()?;
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
/// background, passing `lock` on to it, with its output going to the VM's
/// console log, and waits until the VM answers over SSH. If a signal comes
/// first, e.g. Ctrl-C, or the VM doesn't answer in time, the VM is shut
/// down again. Returns 0, or 128 plus the number of that signal.
fn run_in_background(paths: &Paths, project: &Project, dir: &VmDir, lock: &Lock) -> Result<i32> {
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
    let lock_fd = lock.pass_to(&mut sendit);
    // In a session of its own, it has no terminal: none going away hangs
    // it up, and Ctrl-C in this one doesn't reach it. Descriptors that
    // were handed to this process to pass on, e.g. a jobserver's pipe,
    // would stay open while the VM runs, and whoever waits for them to
    // close would wait as long. Those of std are closed on exec anyway, and
    // it still needs them until then.
    // SAFETY: setsid, getdtablesize, fcntl and close are async-signal-safe,
    // so they may run between fork and exec.
    unsafe {
        sendit.pre_exec(move || {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            for fd in 3..libc::getdtablesize() {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags != -1 && flags & libc::FD_CLOEXEC == 0 {
                    libc::close(fd);
                }
            }
            // Open after exec, for the process to take as its own.
            if libc::fcntl(lock_fd, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
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
        State::Busy => bail!("{BUSY}; try again in a moment"),
        State::Running(pid) => pid,
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
