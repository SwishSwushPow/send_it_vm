//! Reaching a running VM over SSH: `sendit ssh`, the command of `sendit
//! run COMMAND`, and waiting until the VM answers.

use std::io::IsTerminal;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

use super::lock::{BUSY, State, state};
use super::{dir, sendit_for};
use crate::config::GUEST_USER;
use crate::paths::{Paths, Project};
use crate::vm::{VmDir, net};

/// Replaces this process with an SSH session to the project's running VM,
/// as root if `root`, running `command` if given.
pub fn ssh(paths: &Paths, project: &Project, root: bool, command: &[String]) -> Result<()> {
    let dir = dir(paths, project);
    match state(&dir)? {
        State::Stopped => bail!(
            "the VM is not running; start it with `{} run`",
            sendit_for(&dir, project)
        ),
        State::Busy => bail!("{BUSY}; try again once it runs"),
        State::Running(_) => {}
    }
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
pub(super) fn login(paths: &Paths, root: bool) -> Result<(&'static str, PathBuf)> {
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
pub(super) fn shell_command(args: &[String]) -> String {
    args.iter()
        .map(|arg| quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `arg` as one word for a POSIX shell.
pub(super) fn quote(arg: &str) -> String {
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
pub(super) fn run_command(
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
pub(super) fn wait_until_answers(
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

#[cfg(test)]
mod tests {
    use super::*;

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
