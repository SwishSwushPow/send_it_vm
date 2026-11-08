//! `sendit run` on a pseudo-terminal, for the console tests: typing into it
//! and watching what it writes.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::harness::{BOOT_TIMEOUT, Project, STOP_TIMEOUT, terminate, wait_timeout};

/// What sendit writes when it gives the terminal back, as in
/// src/vm/console.rs.
pub const TERMINAL_RESET: &[u8] =
    b"\x1b[0m\x1b[?25h\x1b[?1l\x1b>\x1b[?1000l\x1b[?1002l\x1b[?1006l\x1b[?2004l";
pub const LEAVE_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049l";

/// The time to wait for the console's response to a line typed into it.
pub const CONSOLE_TIMEOUT: Duration = Duration::from_secs(20);

impl Project {
    /// Starts `sendit run` on a pseudo-terminal of `rows` and `cols` with
    /// `TERM` and `COLORTERM` set as given, and waits until the console's
    /// shell is ready.
    pub fn run_in_terminal(
        &self,
        term: &str,
        colorterm: Option<&str>,
        rows: u16,
        cols: u16,
    ) -> Terminal {
        let (master, slave) = openpty(rows, cols);
        let log = self.next_log();

        let mut cmd = self.command(&["run"]);
        cmd.env("TERM", term)
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()));
        match colorterm {
            Some(colorterm) => cmd.env("COLORTERM", colorterm),
            None => cmd.env_remove("COLORTERM"),
        };
        let child = cmd.spawn().expect("running sendit run");
        // Closes the copies of the slave that went to sendit.
        drop(cmd);

        let output = Arc::new(Mutex::new(Vec::new()));
        let mut reader = File::from(master.try_clone().unwrap());
        let mut log_file = File::create(&log).unwrap();
        let copy = output.clone();
        // Ends once nothing has the slave open anymore.
        std::thread::spawn(move || {
            let mut buf = [0; 4096];
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                let _ = log_file.write_all(&buf[..n]);
                copy.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });

        // From here on, a failure shuts the VM down.
        let mut terminal = Terminal {
            child,
            master: File::from(master),
            slave,
            output,
            seen: 0,
            log,
        };
        terminal.expect("dev@sendit", BOOT_TIMEOUT);
        terminal
    }
}

/// Opens a pseudo-terminal of `rows` and `cols`, returning its master and
/// slave ends.
fn openpty(rows: u16, cols: u16) -> (OwnedFd, OwnedFd) {
    let (mut master, mut slave) = (-1, -1);
    let mut size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: plain libc calls with valid out-params; the descriptors are
    // owned from here on.
    unsafe {
        let result = libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        );
        assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
        // Neither end may leak into sendit; it gets copies of the slave.
        for fd in [master, slave] {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave))
    }
}

/// `sendit run` on a pseudo-terminal, as in a terminal window. Dropping it
/// shuts the VM down.
///
/// sendit doesn't get the terminal as its controlling terminal, which would
/// make macOS revoke it when sendit exits: the test keeps the slave open to
/// check the mode sendit leaves it in. So nothing raises signals for it,
/// and `resize` sends the SIGWINCH a terminal window would.
pub struct Terminal {
    child: Child,
    master: File,
    slave: OwnedFd,
    /// Everything sendit wrote to the terminal so far.
    output: Arc<Mutex<Vec<u8>>>,
    /// How much of `output` earlier expectations have consumed.
    seen: usize,
    log: PathBuf,
}

impl Terminal {
    /// Sends `bytes` as if typed.
    pub fn send(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
    }

    /// Types `line` into the console, followed by Enter.
    pub fn type_line(&mut self, line: &str) {
        self.send(format!("{line}\r").as_bytes());
    }

    /// Waits until `wanted` appears in the output after what earlier
    /// expectations consumed.
    pub fn expect(&mut self, wanted: &str, timeout: Duration) {
        let start = Instant::now();
        loop {
            if self.find(wanted) {
                return;
            }
            if start.elapsed() > timeout || !matches!(self.child.try_wait(), Ok(None)) {
                // sendit may have written it just before exiting.
                sleep(Duration::from_millis(500));
                if self.find(wanted) {
                    return;
                }
                panic!(
                    "{wanted:?} did not appear on the terminal; see {}",
                    self.log.display()
                );
            }
            sleep(Duration::from_millis(100));
        }
    }

    /// Types `line` until `wanted` appears, for changes the guest applies
    /// in the background. The lines should print `wanted` through an
    /// expansion, so that the echo of the typed line doesn't match.
    pub fn type_until(&mut self, line: &str, wanted: &str) {
        let start = Instant::now();
        loop {
            self.type_line(line);
            let attempt = Instant::now();
            while attempt.elapsed() < Duration::from_secs(3) {
                if self.find(wanted) {
                    return;
                }
                sleep(Duration::from_millis(100));
            }
            assert!(
                start.elapsed() < CONSOLE_TIMEOUT,
                "{wanted:?} did not appear on the terminal; see {}",
                self.log.display()
            );
        }
    }

    /// Whether `wanted` is in the new output; consumes it up to there.
    fn find(&mut self, wanted: &str) -> bool {
        let output = self.output.lock().unwrap();
        let found = output[self.seen..]
            .windows(wanted.len())
            .position(|w| w == wanted.as_bytes());
        if let Some(i) = found {
            self.seen += i + wanted.len();
        }
        found.is_some()
    }

    /// Checks that the output ends in `wanted`, apart from line breaks.
    pub fn expect_end(&self, wanted: &[u8]) {
        let start = Instant::now();
        loop {
            let output = self.output.lock().unwrap().clone();
            let end = output.trim_ascii_end();
            if end.ends_with(wanted) {
                return;
            }
            // The last output may still be on its way.
            if start.elapsed() > Duration::from_secs(2) {
                let tail = &end[end.len().saturating_sub(wanted.len() + 40)..];
                panic!(
                    "the output ends in {:?}, not {:?}; see {}",
                    tail.escape_ascii().to_string(),
                    wanted.escape_ascii().to_string(),
                    self.log.display()
                );
            }
            sleep(Duration::from_millis(100));
        }
    }

    /// Resizes the terminal as a terminal window would.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: plain syscalls; TIOCSWINSZ reads the winsize it is given,
        // and the child hasn't been waited for, so its PID is its own.
        unsafe {
            assert_eq!(
                libc::ioctl(self.slave.as_raw_fd(), libc::TIOCSWINSZ, &size),
                0
            );
            libc::kill(self.child.id() as libc::pid_t, libc::SIGWINCH);
        }
    }

    /// The terminal's local modes, such as `ICANON` and `ECHO`.
    pub fn local_modes(&self) -> libc::tcflag_t {
        // SAFETY: tcgetattr fills in the termios it is given.
        unsafe {
            let mut termios = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(self.slave.as_raw_fd(), &mut termios), 0);
            termios.c_lflag
        }
    }

    /// Waits until `sendit run` has exited, which it must do successfully.
    pub fn wait_for_exit(&mut self) {
        let status = wait_timeout(&mut self.child, STOP_TIMEOUT).unwrap();
        assert!(
            status.success(),
            "`sendit run` exited with {status}; see {}",
            self.log.display()
        );
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            terminate(&mut self.child);
        }
    }
}
