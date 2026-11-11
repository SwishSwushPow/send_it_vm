//! Operations: sendit commands the app runs, such as `run -d` or
//! `provision`, with their output collected as it comes.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::{Context, Result};

use crate::launch::command_line;

/// The most output kept per operation; older output is dropped.
const MAX_OUTPUT: usize = 1 << 20;

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
    pub follow_up: FollowUp,
    /// What it works on, e.g. `vm:app` or `provision`: the app starts no
    /// other operation with the same key while it runs.
    pub key: Option<String>,
    /// Whether the app has dealt with its end.
    pub handled: bool,
    pid: u32,
}

impl Op {
    pub fn is_running(&self) -> bool {
        self.state == OpState::Running
    }

    /// Asks sendit to stop, as Ctrl-C would: a VM that is starting is
    /// shut down again, a build is cancelled. Pressed again, a VM being
    /// shut down is forced off.
    pub fn interrupt(&self) {
        if self.is_running() {
            // SAFETY: plain syscall. The child hasn't been waited for while
            // it runs, so its PID is still its own.
            unsafe { libc::kill(self.pid as libc::pid_t, libc::SIGINT) };
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

/// Starts `sendit args`, with `env` set, collecting its output in the
/// returned operation until it exits.
pub fn start(
    id: u64,
    sendit: &Path,
    env: &[(String, PathBuf)],
    args: Vec<String>,
    title: String,
    follow_up: FollowUp,
    key: Option<String>,
) -> Result<SharedOp> {
    let mut command = Command::new(sendit);
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(home) = std::env::home_dir() {
        command.current_dir(home);
    }
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command
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
        follow_up,
        key,
        handled: false,
        pid: child.id(),
    }));
    let readers: Vec<_> = [
        child
            .stdout
            .take()
            .map(|out| Box::new(out) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|err| Box::new(err) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    .map(|mut stream| {
        let op = op.clone();
        std::thread::spawn(move || {
            let mut buf = [0; 8192];
            // Bytes of a character split between two reads wait for the rest.
            let mut pending = Vec::new();
            while let Ok(n) = stream.read(&mut buf) {
                if n == 0 {
                    break;
                }
                pending.extend_from_slice(&buf[..n]);
                let valid = match std::str::from_utf8(&pending) {
                    Ok(_) => pending.len(),
                    Err(e) if e.error_len().is_none() => e.valid_up_to(),
                    Err(_) => pending.len(),
                };
                let text = String::from_utf8_lossy(&pending[..valid]).into_owned();
                pending.drain(..valid);
                op.lock().unwrap().append(&text);
            }
            if !pending.is_empty() {
                op.lock()
                    .unwrap()
                    .append(&String::from_utf8_lossy(&pending));
            }
        })
    })
    .collect();
    let waiter = op.clone();
    std::thread::spawn(move || {
        let status = child.wait();
        // All output first, so the state says the output is complete.
        for reader in readers {
            let _ = reader.join();
        }
        let mut op = waiter.lock().unwrap();
        op.state = match status {
            Ok(status) if status.success() => OpState::Succeeded,
            Ok(status) => OpState::Failed(status.code()),
            Err(e) => {
                op.append(&format!("\nwaiting for sendit: {e}\n"));
                OpState::Failed(None)
            }
        };
    });
    Ok(op)
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
        let op = start(
            1,
            Path::new("/bin/sh"),
            &[("SENDIT_TEST".into(), PathBuf::from("v"))],
            vec![
                "-c".into(),
                "echo out; echo \"$SENDIT_TEST\" >&2; exit 3".into(),
            ],
            "test".into(),
            FollowUp::None,
            None,
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
        assert!(op.output.contains("out\n") && op.output.contains("v\n"));
        assert!(op.command.starts_with("sendit -c"));
    }
}
