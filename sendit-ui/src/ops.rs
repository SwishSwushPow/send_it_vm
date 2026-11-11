//! Operations: sendit commands the app runs, such as `run -d` or
//! `provision`, with their output collected as it comes.
//!
//! Their output goes to a log file, not a pipe, which the app reads as it
//! grows: should the app end first, e.g. at logout, sendit goes on, where
//! a pipe would break and make it fail as it writes.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::launch::command_line;

/// The most output kept per operation; older output is dropped.
const MAX_OUTPUT: usize = 1 << 20;

/// How long logs of earlier operations are kept.
const KEEP_LOGS: Duration = Duration::from_secs(7 * 24 * 3600);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpState {
    Running,
    Succeeded,
    /// With the exit code, if it exited rather than being killed.
    Failed(Option<i32>),
}

/// What the app does once an operation ends, apart from refreshing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FollowUp {
    None,
    /// A `run -d` with these arguments: if sendit refused to share risky
    /// directories without asking, or to copy a running main worktree's
    /// VM, the app asks and starts it again.
    Start {
        args: Vec<String>,
    },
    /// Select this VM once it succeeded, e.g. after a rename.
    Select(send_it_vm::VmName),
}

pub struct Op {
    pub id: u64,
    pub title: String,
    /// The command line, for showing.
    pub command: String,
    pub started: SystemTime,
    pub state: OpState,
    pub output: String,
    /// The file the output goes to.
    pub log: PathBuf,
    pub follow_up: FollowUp,
    /// What it works on, e.g. `vm:app` or `provision`: the app starts no
    /// other operation with the same key while it runs.
    pub key: Option<String>,
    /// Whether the app has dealt with its end.
    pub handled: bool,
    /// Until it has been waited for; then its PID may be another's.
    child: Option<Child>,
}

impl Op {
    pub fn is_running(&self) -> bool {
        self.state == OpState::Running
    }

    /// Asks sendit to stop, as Ctrl-C would: a VM that is starting is
    /// shut down again, a build is cancelled. Pressed again, a VM being
    /// shut down is forced off.
    pub fn interrupt(&self) {
        if let Some(child) = &self.child {
            // SAFETY: plain syscall. The child hasn't been waited for (it
            // is taken out under the same lock once it has), so its PID is
            // still its own.
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
        }
    }

    fn append(&mut self, text: &str) {
        push_output(&mut self.output, text);
        if self.output.len() > MAX_OUTPUT {
            let cut = self.output.len() - MAX_OUTPUT;
            let cut = (cut..self.output.len())
                .find(|&i| self.output.is_char_boundary(i))
                .unwrap_or(self.output.len());
            self.output.drain(..cut);
        }
    }
}

pub type SharedOp = Arc<Mutex<Op>>;

/// The folder of the operations' logs, made private if needed.
pub fn log_dir() -> Result<PathBuf> {
    let home = std::env::home_dir().context("cannot determine the home directory")?;
    let dir = home.join("Library/Caches/sendit-ui/operations");
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// Deletes logs in `dir` older than a week.
pub fn delete_old_logs(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > KEEP_LOGS);
        if old {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Starts `sendit args`, with `SENDIT_VM_DIR` set to `vm_dir`, or unset
/// for `None`, writing its output to a new log in `logs`, and collecting it
/// in the returned operation until sendit exits.
#[allow(clippy::too_many_arguments)]
pub fn start(
    id: u64,
    sendit: &Path,
    vm_dir: Option<&Path>,
    args: Vec<String>,
    title: String,
    follow_up: FollowUp,
    key: Option<String>,
    logs: &Path,
) -> Result<SharedOp> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let log = logs.join(format!("{nanos}-{}-{id}.log", std::process::id()));
    let out = OpenOptions::new()
        .append(true)
        .create_new(true)
        .mode(0o600)
        .open(&log)
        .with_context(|| format!("creating {}", log.display()))?;
    let mut command = Command::new(sendit);
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out);
    if let Some(home) = std::env::home_dir() {
        command.current_dir(home);
    }
    match vm_dir {
        Some(dir) => command.env("SENDIT_VM_DIR", dir),
        None => command.env_remove("SENDIT_VM_DIR"),
    };
    let mut reader = File::open(&log).with_context(|| format!("opening {}", log.display()))?;
    let child = command
        .spawn()
        .with_context(|| format!("starting {}", sendit.display()))?;
    let shown: Vec<String> = std::iter::once("sendit".to_string())
        .chain(args.iter().cloned())
        .collect();
    let op = Arc::new(Mutex::new(Op {
        id,
        title,
        command: command_line(&shown),
        started: SystemTime::now(),
        state: OpState::Running,
        output: String::new(),
        log,
        follow_up,
        key,
        handled: false,
        child: Some(child),
    }));
    let watched = op.clone();
    std::thread::spawn(move || {
        // Bytes of a character split between two reads wait for the rest.
        let mut pending = Vec::new();
        loop {
            {
                let mut op = watched.lock().unwrap();
                read_new(&mut reader, &mut pending, &mut op);
                let status = match op.child.as_mut().map(Child::try_wait) {
                    Some(Ok(None)) => None,
                    Some(Ok(Some(status))) => Some(Ok(status)),
                    Some(Err(e)) => Some(Err(e)),
                    None => Some(Err(std::io::Error::other("no child"))),
                };
                if let Some(status) = status {
                    // Waited for: its PID isn't to be signalled any more.
                    op.child = None;
                    read_new(&mut reader, &mut pending, &mut op);
                    if !pending.is_empty() {
                        let rest = String::from_utf8_lossy(&pending).into_owned();
                        op.append(&rest);
                    }
                    op.state = match status {
                        Ok(status) if status.success() => OpState::Succeeded,
                        Ok(status) => OpState::Failed(status.code()),
                        Err(e) => {
                            op.append(&format!("\nwaiting for sendit: {e}\n"));
                            OpState::Failed(None)
                        }
                    };
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
    Ok(op)
}

/// Reads what was added to the log since the last read into `op`.
/// `pending` holds bytes of a character split between two reads.
fn read_new(reader: &mut File, pending: &mut Vec<u8>, op: &mut Op) {
    let mut buf = Vec::new();
    if reader.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
        pending.extend_from_slice(&buf);
        let valid = match std::str::from_utf8(pending) {
            Ok(_) => pending.len(),
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            Err(_) => pending.len(),
        };
        op.append(&String::from_utf8_lossy(&pending[..valid]));
        pending.drain(..valid);
    }
}

/// Appends `text` to `output` as a terminal would show it, roughly: escape
/// sequences are dropped, and a carriage return starts its line over, as
/// progress bars use it.
fn push_output(output: &mut String, text: &str) {
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                // CSI: parameters, then a final byte from @ to ~.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC: up to BEL or ST (ESC \).
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
                _ => {}
            },
            '\r' if chars.peek() == Some(&'\n') => {}
            '\r' => {
                let start = output.rfind('\n').map_or(0, |i| i + 1);
                output.truncate(start);
            }
            c if c.is_control() && c != '\n' && c != '\t' => {}
            c => output.push(c),
        }
    }
}

/// The lines of risky shares that sendit refused without asking, if that
/// is why `output` failed.
pub fn refused_shares(output: &str) -> Option<Vec<String>> {
    let start = output.find("not sharing these without a terminal to confirm it")?;
    let rest = &output[start..];
    let lines: Vec<String> = rest
        .lines()
        .skip(1)
        .take_while(|line| line.starts_with("  "))
        .map(|line| line.trim().to_string())
        .collect();
    Some(lines)
}

/// Whether sendit refused to start a worktree's VM because the main
/// worktree's VM runs, and a copy of it might miss data. The message,
/// e.g. "the VM of ~/app is running; …".
pub fn refused_copy(output: &str) -> Option<String> {
    let line = output
        .lines()
        .find(|line| line.contains("start from the") && line.contains("with --fresh"))?;
    Some(line.trim_start_matches("Error: ").trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(text: &str) -> String {
        let mut output = String::new();
        push_output(&mut output, text);
        output
    }

    #[test]
    fn shows_output_as_a_terminal_would() {
        assert_eq!(shown("a\r\nb\n"), "a\nb\n");
        assert_eq!(shown("x\n10%\r50%\r100%\n"), "x\n100%\n");
        assert_eq!(shown("\x1b[1;31mred\x1b[0m\n"), "red\n");
        assert_eq!(shown("\x1b]0;title\x07after"), "after");
        assert_eq!(shown("\x1b]8;;url\x1b\\link"), "link");
        assert_eq!(shown("tab\there\x08"), "tab\there");
    }

    #[test]
    fn finds_refusals() {
        let output = "Error: not sharing these without a terminal to confirm it, or --yes:\n  \
                      ~ (read-only) contains your home directory\n  \
                      /x (read-write) is inside sendit's configuration\n";
        assert_eq!(
            refused_shares(output).unwrap(),
            [
                "~ (read-only) contains your home directory",
                "/x (read-write) is inside sendit's configuration"
            ]
        );
        assert_eq!(refused_shares("Error: the VM is busy"), None);

        let output = "Error: the VM of ~/app is running; stop it to start this worktree's \
                      VM as a copy of it, or start from the rust image with --fresh\n";
        assert!(
            refused_copy(output)
                .unwrap()
                .starts_with("the VM of ~/app is running")
        );
        assert_eq!(refused_copy("Error: other"), None);
    }

    #[test]
    fn collects_output_and_exit_codes() {
        let logs = std::env::temp_dir().join(format!("sendit-ui-ops-{}", std::process::id()));
        fs::create_dir_all(&logs).unwrap();
        let op = start(
            1,
            Path::new("/bin/sh"),
            Some(Path::new("/v")),
            vec![
                "-c".into(),
                "echo out; echo \"$SENDIT_VM_DIR\" >&2; printf 'caf\\303'; sleep 0.3; printf '\\251\\n'; exit 3".into(),
            ],
            "test".into(),
            FollowUp::None,
            None,
            &logs,
        )
        .unwrap();
        for _ in 0..500 {
            if !op.lock().unwrap().is_running() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let op = op.lock().unwrap();
        assert_eq!(op.state, OpState::Failed(Some(3)));
        assert_eq!(op.output, "out\n/v\ncafé\n");
        assert!(op.command.starts_with("sendit -c"));
        op.interrupt();
        fs::remove_dir_all(&logs).unwrap();
    }
}
