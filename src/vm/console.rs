//! Connecting the host terminal to the guest's virtio console.
//!
//! Stdin is read on a helper thread and forwarded to the guest through a pipe,
//! so the escape key (Ctrl-]) can be intercepted instead of reaching the guest.
//! Guest output also goes through a pipe and a helper thread. VZ must never
//! get the terminal itself: it makes the descriptors it is given
//! non-blocking, and on a terminal stdin and stdout share that flag, so
//! reading keystrokes would fail.
//!
//! The guest's console can't tell what kind of terminal it is connected to
//! or how large it is, so a third port carries both: when the guest asks,
//! sendit sends the terminal's size and type ($TERM and $COLORTERM), and it
//! sends the size again whenever the terminal is resized (SIGWINCH). A
//! service in the guest applies them to the console. The guest also says
//! there when it starts shutting down.
//!
//! Guest output reaches the terminal unchanged, including Program Status
//! Protocol reports (OSC 7501) from programs in the guest. The terminal's
//! reply to their query comes back as input. The records they leave would
//! outlive the VM, so sendit clears them all when the VM stops. While
//! provisioning, nothing in the guest reads the console, so such queries
//! go unanswered; reports still get through.

use std::fs::File;
use std::io::{self, IsTerminal, Write};
use std::os::fd::{AsRawFd, IntoRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use objc2::AllocAnyThread;
use objc2::rc::Retained;
use objc2_foundation::NSFileHandle;
use objc2_virtualization::{VZFileHandleSerialPortAttachment, VZSerialPortAttachment};

use super::ConsoleMode;

/// Ctrl-]
const ESCAPE_KEY: u8 = 0x1d;

/// When the console is closed, how long guest output may keep arriving
/// before the rest is dropped, in milliseconds. VZ may still be passing on
/// what the guest wrote just before it stopped.
const OUTPUT_GRACE_MS: libc::c_int = 100;

pub struct Console {
    /// The login console, /dev/hvc0 in the guest.
    main: Retained<VZFileHandleSerialPortAttachment>,
    /// Output-only port for provisioning, /dev/hvc1 in the guest. Its
    /// output is discarded outside of provisioning.
    log: Retained<VZFileHandleSerialPortAttachment>,
    /// Carries the terminal's type and size, /dev/hvc2 in the guest.
    terminal: Retained<VZFileHandleSerialPortAttachment>,
    escapes: Arc<AtomicUsize>,
    guest_stopping: Arc<AtomicBool>,
    /// Closing it stops forwarding stdin and sending the terminal, so the
    /// threads don't outlive the console, e.g. swallowing what is typed or
    /// the resizes meant for the next VM.
    stop: Option<OwnedFd>,
    /// Finished on drop, before the terminal is restored.
    output: Option<OutputCopy>,
    raw_mode: Option<RawMode>,
}

impl Console {
    /// Connects the terminal to the guest as `mode` says. For the login
    /// console, stdin goes to it and its output to stdout. While
    /// provisioning, its output is discarded instead, and a second port's
    /// output is shown and appended to the log: nothing runs a getty on that
    /// port, so provisioning output can't be cut off by one hanging up the
    /// terminal. Nothing in the guest reads keystrokes then, so Ctrl-C keeps
    /// raising SIGINT instead of reaching the guest. Hidden, the terminal is
    /// left alone, apart from telling the guest its type and size.
    pub fn attach(mode: &ConsoleMode) -> Result<Self> {
        let log = match mode {
            ConsoleMode::Provision(log) => Some(log),
            ConsoleMode::Login | ConsoleMode::Hidden => None,
        };
        let provisioning = log.is_some();
        let hidden = matches!(mode, ConsoleMode::Hidden);
        let escapes = Arc::new(AtomicUsize::new(0));
        let (stop_read, stop) = pipe()?;
        let guest_stopping = Arc::new(AtomicBool::new(false));
        let terminal = terminal_port(guest_stopping.clone(), stop_read.try_clone()?)?;
        // Hidden, stdin belongs to the program that has the terminal.
        let input = if hidden {
            None
        } else {
            let (read_end, write_end) = pipe()?;
            let counter = escapes.clone();
            std::thread::spawn(move || {
                forward_input(libc::STDIN_FILENO, write_end, &counter, stop_read)
            });
            Some(file_handle(read_end))
        };

        let (main_output, log_output, output) = if hidden {
            (null_output()?, null_output()?, None)
        } else {
            let shown = ShownOutput::new(log.map(|log| log.last_line.as_bytes()));
            let log = log
                .map(|log| {
                    File::options()
                        .create(true)
                        .append(true)
                        .open(&log.path)
                        .with_context(|| format!("opening {}", log.path.display()))
                })
                .transpose()?;
            let (output_read, output_write) = pipe()?;
            let output = OutputCopy::start(output_read, shown, log)?;
            if provisioning {
                (null_output()?, file_handle(output_write), Some(output))
            } else {
                (file_handle(output_write), null_output()?, Some(output))
            }
        };

        Ok(Self {
            main: serial_port(input.as_deref(), &main_output),
            log: serial_port(None, &log_output),
            terminal,
            escapes,
            guest_stopping,
            stop: Some(stop),
            output,
            raw_mode: if hidden {
                None
            } else {
                RawMode::enable(provisioning)?
            },
        })
    }

    /// The attachments for the guest's serial ports, in order.
    pub fn ports(&self) -> Vec<&VZSerialPortAttachment> {
        vec![&self.main, &self.log, &self.terminal]
    }

    /// How often the escape key has been pressed so far.
    pub fn escapes(&self) -> usize {
        self.escapes.load(Ordering::Relaxed)
    }

    /// Whether the guest has said that it is shutting down.
    pub fn guest_stopping(&self) -> bool {
        self.guest_stopping.load(Ordering::Relaxed)
    }
}

impl Drop for Console {
    /// Stops forwarding stdin and sending the terminal, and waits until the
    /// guest's last output has been shown and logged, so none of it lands
    /// after the terminal is restored or the log is read.
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(output) = self.output.take() {
            let left = output.finish();
            if left.status_records && io::stdout().is_terminal() {
                let _ = write_all(libc::STDOUT_FILENO, CLEAR_STATUS);
            }
            if let Some(raw_mode) = &mut self.raw_mode {
                raw_mode.alternate_screen = left.alternate_screen;
            }
        }
    }
}

fn serial_port(
    input: Option<&NSFileHandle>,
    output: &NSFileHandle,
) -> Retained<VZFileHandleSerialPortAttachment> {
    unsafe {
        VZFileHandleSerialPortAttachment::initWithFileHandleForReading_fileHandleForWriting(
            VZFileHandleSerialPortAttachment::alloc(),
            input,
            Some(output),
        )
    }
}

/// A handle that discards what is written to it. VZ needs handles backed by
/// real file descriptors, which `fileHandleWithNullDevice` isn't.
fn null_output() -> Result<Retained<NSFileHandle>> {
    let null = File::options().write(true).open("/dev/null")?;
    Ok(file_handle(null.into()))
}

/// Wraps `fd` in an `NSFileHandle` that closes it when released.
fn file_handle(fd: OwnedFd) -> Retained<NSFileHandle> {
    NSFileHandle::initWithFileDescriptor_closeOnDealloc(
        NSFileHandle::alloc(),
        fd.into_raw_fd(),
        true,
    )
}

/// The thread copying guest output to stdout and the log.
struct OutputCopy {
    /// Closing it tells the thread to finish.
    finish: OwnedFd,
    thread: JoinHandle<LeftBehind>,
}

/// What guest output left in the terminal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct LeftBehind {
    /// The terminal is on the alternate screen.
    alternate_screen: bool,
    /// Program Status Protocol records.
    status_records: bool,
}

impl OutputCopy {
    fn start(from_guest: OwnedFd, shown: ShownOutput, log: Option<File>) -> Result<Self> {
        let (finish_read, finish) = pipe()?;
        let thread = std::thread::spawn(move || {
            copy_output(from_guest, libc::STDOUT_FILENO, shown, log, finish_read)
        });
        Ok(Self { finish, thread })
    }

    /// Copies what is left, then waits for the thread to end. Returns what
    /// the guest left in the terminal.
    fn finish(self) -> LeftBehind {
        drop(self.finish);
        self.thread.join().unwrap_or_default()
    }
}

/// Copies guest output to `to` (stdout) as far as `shown` allows, and all
/// of it to `log` if given, until the guest side closes, or `finish` closes
/// and no more output has come for `OUTPUT_GRACE_MS`. Returns what the shown
/// output left in the terminal.
fn copy_output(
    from_guest: OwnedFd,
    to: libc::c_int,
    mut shown: ShownOutput,
    mut log: Option<File>,
    finish: OwnedFd,
) -> LeftBehind {
    let mut fds = [pollfd(from_guest.as_raw_fd()), pollfd(finish.as_raw_fd())];
    let mut timeout = -1;
    let mut buf = [0u8; 4096];
    let mut screen = ScreenTracker::default();
    let mut status = StatusTracker::default();
    let left = |screen: &ScreenTracker, status: &StatusTracker| LeftBehind {
        alternate_screen: screen.alternate,
        status_records: status.reported,
    };
    loop {
        if matches!(poll(&mut fds, timeout), Ok(0) | Err(_)) {
            return left(&screen, &status);
        }
        // Output first: finishing waits until none is pending.
        if fds[0].revents != 0 {
            let Ok(n @ 1..) = read(from_guest.as_raw_fd(), &mut buf) else {
                return left(&screen, &status);
            };
            let show = &buf[..shown.len(&buf[..n])];
            screen.feed(show);
            status.feed(show);
            let _ = write_all(to, show);
            if let Some(log) = &mut log {
                let _ = log.write_all(&buf[..n]);
            }
        } else if fds[1].revents != 0 {
            // poll() skips negative descriptors.
            fds[1].fd = -1;
            timeout = OUTPUT_GRACE_MS;
        }
    }
}

/// Decides how much guest output is shown: all of it, or with a last line,
/// everything up to the end of the line containing it.
struct ShownOutput {
    last_line: Option<Vec<u8>>,
    /// The end of the output so far, for a `last_line` split across reads.
    tail: Vec<u8>,
    /// `last_line` has come; the rest of its line is still shown.
    ending: bool,
    /// The last line has ended; nothing more is shown.
    done: bool,
}

impl ShownOutput {
    fn new(last_line: Option<&[u8]>) -> Self {
        Self {
            last_line: last_line.map(<[u8]>::to_vec),
            tail: Vec::new(),
            ending: false,
            done: false,
        }
    }

    /// How much of `data`, the output that comes next, to show.
    fn len(&mut self, data: &[u8]) -> usize {
        if self.done {
            return 0;
        }
        let mut from = 0;
        if !self.ending {
            let Some(last_line) = &self.last_line else {
                return data.len();
            };
            let kept = self.tail.len();
            self.tail.extend_from_slice(data);
            let Some(at) = self
                .tail
                .windows(last_line.len())
                .position(|window| window == last_line.as_slice())
            else {
                let keep = self.tail.len().min(last_line.len() - 1);
                self.tail.drain(..self.tail.len() - keep);
                return data.len();
            };
            // The kept tail is shorter than `last_line`, so it ends in `data`.
            from = at + last_line.len() - kept;
            self.ending = true;
        }
        match data[from..].iter().position(|&b| b == b'\n') {
            Some(newline) => {
                self.done = true;
                from + newline + 1
            }
            None => data.len(),
        }
    }
}

/// The sequences that switch the terminal to the alternate screen and back.
const ALTERNATE_SCREEN_ON: [&[u8]; 3] = [b"\x1b[?1049h", b"\x1b[?1047h", b"\x1b[?47h"];
const ALTERNATE_SCREEN_OFF: [&[u8]; 3] = [b"\x1b[?1049l", b"\x1b[?1047l", b"\x1b[?47l"];

/// Follows whether output has switched the terminal to the alternate screen.
/// Leaving it when it isn't on is not harmless: `ESC [ ? 1049 l` also moves
/// the cursor back to where it was last saved, and later output overwrites
/// what is shown there.
#[derive(Default)]
struct ScreenTracker {
    alternate: bool,
    /// The end of the output so far, for sequences split across reads.
    tail: Vec<u8>,
}

impl ScreenTracker {
    const LONGEST: usize = 8;

    fn feed(&mut self, data: &[u8]) {
        self.tail.extend_from_slice(data);
        // Sequences already seen in the kept tail are seen again, which
        // changes nothing: the last one wins either way.
        for (i, _) in self.tail.iter().enumerate().filter(|(_, b)| **b == 0x1b) {
            let rest = &self.tail[i..];
            if ALTERNATE_SCREEN_ON.iter().any(|seq| rest.starts_with(seq)) {
                self.alternate = true;
            } else if ALTERNATE_SCREEN_OFF.iter().any(|seq| rest.starts_with(seq)) {
                self.alternate = false;
            }
        }
        let keep = self.tail.len().min(Self::LONGEST - 1);
        self.tail.drain(..self.tail.len() - keep);
    }
}

/// The start of a Program Status Protocol sequence (OSC 7501).
const STATUS_REPORT: &[u8] = b"\x1b]7501;";

/// Removes all Program Status Protocol records.
const CLEAR_STATUS: &[u8] = b"\x1b]7501;state=clear\x1b\\";

/// Follows whether output has sent a Program Status Protocol report, which
/// leaves a record in the terminal. Asking whether the terminal supports
/// them (`ESC ] 7501 ; ?`) doesn't.
#[derive(Default)]
struct StatusTracker {
    reported: bool,
    /// The end of the output so far, for a report split across reads.
    tail: Vec<u8>,
}

impl StatusTracker {
    /// The start of a report and the first byte after it, which tells it
    /// from a query.
    const LONGEST: usize = STATUS_REPORT.len() + 1;

    fn feed(&mut self, data: &[u8]) {
        if self.reported {
            return;
        }
        self.tail.extend_from_slice(data);
        self.reported = self
            .tail
            .windows(Self::LONGEST)
            .any(|window| window.starts_with(STATUS_REPORT) && window[Self::LONGEST - 1] != b'?');
        let keep = self.tail.len().min(Self::LONGEST - 1);
        self.tail.drain(..self.tail.len() - keep);
    }
}

/// Write end of the pipe that wakes up `send_terminal` on SIGWINCH, or -1.
static WINCH_PIPE: AtomicI32 = AtomicI32::new(-1);

/// Read end of that pipe. Both ends stay open for good: the signal handler
/// may use the write end at any time.
static WINCH_READ: OnceLock<OwnedFd> = OnceLock::new();

extern "C" fn on_winch(_: libc::c_int) {
    // SAFETY: write() is async-signal-safe, and errno is restored for the
    // code the signal interrupted.
    unsafe {
        let errno = *libc::__error();
        libc::write(WINCH_PIPE.load(Ordering::Relaxed), [0u8].as_ptr().cast(), 1);
        *libc::__error() = errno;
    }
}

/// Creates the port that carries the terminal's type and size to the guest,
/// and starts sending them there until `stop` closes. `guest_stopping` is
/// set once the guest says it is shutting down.
fn terminal_port(
    guest_stopping: Arc<AtomicBool>,
    stop: OwnedFd,
) -> Result<Retained<VZFileHandleSerialPortAttachment>> {
    let winch = winch_pipe()?;
    let (guest_reads, to_guest) = pipe()?;
    let (from_guest, guest_writes) = pipe()?;
    std::thread::spawn(move || send_terminal(to_guest, from_guest, winch, &guest_stopping, stop));
    Ok(serial_port(
        Some(&file_handle(guest_reads)),
        &file_handle(guest_writes),
    ))
}

/// The read end of the pipe `on_winch` writes to, created on first use.
/// Only called on the main thread.
fn winch_pipe() -> io::Result<libc::c_int> {
    if let Some(winch) = WINCH_READ.get() {
        return Ok(winch.as_raw_fd());
    }
    let (winch, winch_write) = pipe()?;
    // Never block the signal handler: one pending wakeup is as good as many.
    // SAFETY: plain fcntl calls on a descriptor we own.
    unsafe {
        let flags = libc::fcntl(winch_write.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(
            winch_write.as_raw_fd(),
            libc::F_SETFL,
            flags | libc::O_NONBLOCK,
        );
    }
    WINCH_PIPE.store(winch_write.into_raw_fd(), Ordering::Relaxed);
    // SAFETY: the handler only makes async-signal-safe calls.
    unsafe { libc::signal(libc::SIGWINCH, on_winch as *const () as libc::sighandler_t) };
    Ok(WINCH_READ.get_or_init(|| winch).as_raw_fd())
}

/// Sends the terminal to the guest. When the guest writes a "?" (which it
/// does once it is ready), the size goes as a "<rows> <cols>" line, followed
/// by a "term <TERM> [<COLORTERM>]" line: the guest starts the login once it
/// has the type, so the size must be there by then. The size alone goes
/// again whenever `winch` is signalled. A "!" from the guest says that it has
/// started shutting down, which sets `guest_stopping`. Ends when `stop`
/// closes.
fn send_terminal(
    to_guest: OwnedFd,
    from_guest: OwnedFd,
    winch: libc::c_int,
    guest_stopping: &AtomicBool,
    stop: OwnedFd,
) {
    let mut fds = [
        pollfd(stop.as_raw_fd()),
        pollfd(winch),
        pollfd(from_guest.as_raw_fd()),
    ];
    let mut buf = [0u8; 64];
    loop {
        // Stopping first: resizes after it are for the next VM.
        if poll(&mut fds, -1).is_err() || fds[0].revents != 0 {
            return;
        }
        let (mut asked, mut resized) = (false, false);
        for pollfd in &mut fds[1..] {
            if pollfd.revents == 0 {
                continue;
            }
            match read(pollfd.fd, &mut buf) {
                // poll() skips negative descriptors.
                Ok(0) | Err(_) => pollfd.fd = -1,
                Ok(_) if pollfd.fd == winch => resized = true,
                Ok(n) => {
                    asked |= buf[..n].contains(&b'?');
                    if buf[..n].contains(&b'!') {
                        guest_stopping.store(true, Ordering::Relaxed);
                    }
                }
            }
        }
        let mut message = String::new();
        if (asked || resized)
            && let Some(size) = terminal_size()
        {
            message.push_str(&size);
        }
        if asked {
            message.push_str(&terminal_type());
        }
        if write_all(to_guest.as_raw_fd(), message.as_bytes()).is_err() {
            return;
        }
    }
}

/// The terminal's type as a "term <TERM> [<COLORTERM>]" line. Values that
/// are unset or don't look like a terminal name are left out, and the guest
/// falls back to a default.
fn terminal_type() -> String {
    let mut line = String::from("term");
    for name in ["TERM", "COLORTERM"] {
        match std::env::var(name) {
            Ok(value) if is_terminal_name(&value) => {
                line.push(' ');
                line.push_str(&value);
            }
            _ => break,
        }
    }
    line.push('\n');
    line
}

fn is_terminal_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
}

/// The terminal's size as a "<rows> <cols>" line, if stdout is a terminal.
fn terminal_size() -> Option<String> {
    let (rows, cols) = window_size()?;
    Some(format!("{rows} {cols}\n"))
}

/// The terminal's rows and columns, if stdout is a terminal.
pub(super) fn window_size() -> Option<(u16, u16)> {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ fills in the winsize it is given.
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } != 0
        || size.ws_row == 0
        || size.ws_col == 0
    {
        return None;
    }
    Some((size.ws_row, size.ws_col))
}

/// Forwards `from` (stdin) to the guest, counting escape keys, until `stop`
/// closes.
fn forward_input(from: libc::c_int, to_guest: OwnedFd, escapes: &AtomicUsize, stop: OwnedFd) {
    let mut fds = [pollfd(from), pollfd(stop.as_raw_fd())];
    let mut buf = [0u8; 4096];
    loop {
        // Stopping first: input that is already waiting is for whatever
        // reads stdin next.
        if poll(&mut fds, -1).is_err() || fds[1].revents != 0 {
            return;
        }
        let n = match read(from, &mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        for chunk in buf[..n].split_inclusive(|&b| b == ESCAPE_KEY) {
            let (data, escaped) = match chunk.split_last() {
                Some((&ESCAPE_KEY, data)) => (data, true),
                _ => (chunk, false),
            };
            if write_all(to_guest.as_raw_fd(), data).is_err() {
                return;
            }
            if escaped {
                escapes.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let (read, write) = io::pipe()?;
    Ok((read.into(), write.into()))
}

/// Runs a system call again while a signal interrupts it. Negative results
/// are errors.
fn syscall(mut call: impl FnMut() -> isize) -> io::Result<usize> {
    loop {
        let n = call();
        if n >= 0 {
            return Ok(n as usize);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

fn read(fd: i32, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: `buf` is valid for writes of its length.
    syscall(|| unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) })
}

fn write_all(fd: i32, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        // SAFETY: `data` is valid for reads of its length.
        let n = syscall(|| unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) })?;
        data = &data[n..];
    }
    Ok(())
}

/// Waits for input on `fds`; returns how many have events, 0 on timeout.
fn poll(fds: &mut [libc::pollfd], timeout: libc::c_int) -> io::Result<usize> {
    // SAFETY: `fds` is valid for its length.
    syscall(|| unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) } as isize)
}

fn pollfd(fd: libc::c_int) -> libc::pollfd {
    libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    }
}

/// Terminal modes a program in the guest may have switched on and not off
/// when the VM went away: attributes, hidden cursor, application cursor keys
/// and keypad, mouse reporting, bracketed paste.
const TERMINAL_RESET: &[u8] =
    b"\x1b[0m\x1b[?25h\x1b[?1l\x1b>\x1b[?1000l\x1b[?1002l\x1b[?1006l\x1b[?2004l";

/// Leaves the alternate screen, if the guest left the terminal on it.
const LEAVE_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049l";

/// Puts the terminal into raw mode, so keystrokes (including Ctrl-C) go to
/// the guest unmodified, unless `keep_signals` leaves Ctrl-C raising SIGINT.
/// On drop, resets what the guest may have changed and restores the previous
/// mode.
struct RawMode {
    original: libc::termios,
    /// Whether the guest left the terminal on the alternate screen.
    alternate_screen: bool,
}

impl RawMode {
    fn enable(keep_signals: bool) -> Result<Option<Self>> {
        if !io::stdin().is_terminal() {
            return Ok(None);
        }
        let fd = libc::STDIN_FILENO;
        // SAFETY: plain libc calls on a valid descriptor with owned out-params.
        unsafe {
            let mut original = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut original) != 0 {
                return Err(io::Error::last_os_error().into());
            }
            let mut raw = original;
            libc::cfmakeraw(&mut raw);
            if keep_signals {
                // Only Ctrl-C: suspending or quitting would skip the cleanup.
                raw.c_lflag |= libc::ISIG;
                raw.c_cc[libc::VQUIT] = libc::_POSIX_VDISABLE;
                raw.c_cc[libc::VSUSP] = libc::_POSIX_VDISABLE;
                raw.c_cc[libc::VDSUSP] = libc::_POSIX_VDISABLE;
            }
            if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
                return Err(io::Error::last_os_error().into());
            }
            Ok(Some(Self {
                original,
                alternate_screen: false,
            }))
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if io::stdout().is_terminal() {
            if self.alternate_screen {
                let _ = write_all(libc::STDOUT_FILENO, LEAVE_ALTERNATE_SCREEN);
            }
            let _ = write_all(libc::STDOUT_FILENO, TERMINAL_RESET);
        }
        // SAFETY: plain libc calls; restores the attributes read in `enable`.
        unsafe {
            // Drop input nobody will read now, such as the terminal's replies
            // to queries the guest sent, so it doesn't end up in the shell.
            libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.original);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::TempDir;
    use std::io::Read;
    use std::time::{Duration, Instant};

    #[test]
    fn forwards_input_until_stopped() {
        let (from, typed) = pipe().unwrap();
        let (guest_reads, to_guest) = pipe().unwrap();
        let (stop_read, stop) = pipe().unwrap();
        let escapes = Arc::new(AtomicUsize::new(0));
        let counter = escapes.clone();
        let from_fd = from.as_raw_fd();
        let thread = std::thread::spawn(move || {
            forward_input(from_fd, to_guest, &counter, stop_read);
            drop(from);
        });

        write_all(typed.as_raw_fd(), b"ls\x1d\n").unwrap();
        let mut forwarded = [0u8; 3];
        File::from(guest_reads).read_exact(&mut forwarded).unwrap();
        assert_eq!(&forwarded, b"ls\n");
        assert_eq!(escapes.load(Ordering::Relaxed), 1);

        // Stopping ends the thread while the input stays open.
        drop(stop);
        thread.join().unwrap();
        drop(typed);
    }

    #[test]
    fn shows_output_up_to_the_last_line() {
        let mut all = ShownOutput::new(None);
        assert_eq!(all.len(b"anything\r\n"), 10);

        let mut shown = ShownOutput::new(Some(b"TOKEN"));
        assert_eq!(shown.len(b"custom script failed\r\nSTATUS TO"), 31);
        // The line is shown to its end, even when that comes later.
        assert_eq!(shown.len(b"KEN"), 3);
        assert_eq!(shown.len(b" still\r\nshutting down\r\n"), 8);
        assert_eq!(shown.len(b"more\r\n"), 0);

        let mut shown = ShownOutput::new(Some(b"TOKEN"));
        assert_eq!(shown.len(b"STATUS TOKEN\r\nkeys\r\n"), 14);
        assert_eq!(shown.len(b"more"), 0);
    }

    #[test]
    fn tracks_the_alternate_screen() {
        let mut screen = ScreenTracker::default();
        screen.feed(b"plain output\r\n\x1b[1mbold\x1b[0m");
        assert!(!screen.alternate);
        screen.feed(b"vim starts \x1b[?1049h\x1b[H");
        assert!(screen.alternate);
        screen.feed(b"\x1b[?1049l back");
        assert!(!screen.alternate);

        // Split across reads.
        screen.feed(b"less \x1b[?10");
        assert!(!screen.alternate);
        screen.feed(b"49h");
        assert!(screen.alternate);
        screen.feed(b"\x1b[?47l then \x1b");
        assert!(!screen.alternate);
        screen.feed(b"[?1047h");
        assert!(screen.alternate);
    }

    #[test]
    fn tracks_status_reports() {
        let mut status = StatusTracker::default();
        status.feed(b"\x1b]0;title\x07\x1b]7501;?\x1b\\ asking is not reporting");
        assert!(!status.reported);

        // Split across reads, right before the byte that tells a report
        // from a query.
        status.feed(b"build \x1b]75");
        assert!(!status.reported);
        status.feed(b"01;");
        assert!(!status.reported);
        status.feed(b"state=working\x1b\\");
        assert!(status.reported);
    }

    #[test]
    fn reports_status_records_left_behind() {
        let copy = |output: &[u8]| {
            let (from_guest, guest_writes) = pipe().unwrap();
            let (shown, to) = pipe().unwrap();
            let (finish_read, _finish) = pipe().unwrap();
            write_all(guest_writes.as_raw_fd(), output).unwrap();
            drop(guest_writes);
            let left = copy_output(
                from_guest,
                to.as_raw_fd(),
                ShownOutput::new(None),
                None,
                finish_read,
            );
            drop(to);
            let mut out = Vec::new();
            File::from(shown).read_to_end(&mut out).unwrap();
            assert_eq!(out, output, "shown unchanged");
            left
        };
        assert!(copy(b"\x1b]7501;state=idle:app=test\x1b\\").status_records);
        assert!(!copy(b"\x1b]7501;?\x1b\\").status_records);
    }

    #[test]
    fn finishing_copies_pending_output() {
        let (from_guest, guest_writes) = pipe().unwrap();
        let (shown, to) = pipe().unwrap();
        let mut shown = File::from(shown);
        let temp = TempDir::new("log");
        let log_path = temp.path().join("console.log");
        let log = File::create(&log_path).unwrap();
        let (finish_read, finish) = pipe().unwrap();
        let to_fd = to.as_raw_fd();
        let thread = std::thread::spawn(move || {
            let left = copy_output(
                from_guest,
                to_fd,
                ShownOutput::new(None),
                Some(log),
                finish_read,
            );
            drop(to);
            left
        });
        let reader = std::thread::spawn(move || {
            let mut out = Vec::new();
            shown.read_to_end(&mut out).unwrap();
            out
        });

        // More than a pipe holds, so the copy is still busy when asked to
        // finish.
        let data = vec![b'x'; 200_000];
        write_all(guest_writes.as_raw_fd(), &data).unwrap();
        // The guest side stays open, as VZ may keep it: finishing must
        // not wait for it to close.
        let start = Instant::now();
        OutputCopy { finish, thread }.finish();
        assert!(start.elapsed() < Duration::from_secs(1));

        assert_eq!(reader.join().unwrap().len(), data.len(), "shown");
        let logged = std::fs::read(&log_path).unwrap();
        assert_eq!(logged.len(), data.len(), "logged");
        drop(guest_writes);
    }
}
