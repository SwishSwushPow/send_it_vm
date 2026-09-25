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
//! sendit sends the terminal's type ($TERM and $COLORTERM) and size, and it
//! sends the size again whenever the terminal is resized (SIGWINCH). A
//! service in the guest applies them to the console.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

use anyhow::{Context, Result};
use objc2::AllocAnyThread;
use objc2::rc::Retained;
use objc2_foundation::NSFileHandle;
use objc2_virtualization::{VZFileHandleSerialPortAttachment, VZSerialPortAttachment};

/// Ctrl-]
pub const ESCAPE_KEY: u8 = 0x1d;

pub struct Console {
    /// The login console, /dev/hvc0 in the guest.
    main: Retained<VZFileHandleSerialPortAttachment>,
    /// Output-only port for provisioning, /dev/hvc1 in the guest. Its
    /// output is discarded outside of provisioning.
    log: Retained<VZFileHandleSerialPortAttachment>,
    /// Carries the terminal's type and size, /dev/hvc2 in the guest.
    terminal: Retained<VZFileHandleSerialPortAttachment>,
    escapes: Arc<AtomicUsize>,
    _raw_mode: Option<RawMode>,
}

impl Console {
    /// Connects stdin to the guest's login console. Without `log`, the login
    /// console's output goes to stdout. With `log`, it is discarded instead,
    /// and a second port's output is shown and appended to `log`: nothing
    /// runs a getty on that port, so provisioning output can't be cut off by
    /// one hanging up the terminal. Nothing in the guest reads keystrokes
    /// then, so Ctrl-C keeps raising SIGINT instead of reaching the guest.
    pub fn attach(log: Option<&Path>) -> Result<Self> {
        let log_given = log.is_some();
        let (read_end, write_end) = pipe()?;
        let escapes = Arc::new(AtomicUsize::new(0));
        let counter = escapes.clone();
        std::thread::spawn(move || forward_stdin(write_end, &counter));

        let (main_output, log_output) = match log {
            None => {
                let (read_end, write_end) = pipe()?;
                std::thread::spawn(move || copy_output(read_end, None));
                (file_handle(write_end), null_output()?)
            }
            Some(path) => {
                let log = File::options()
                    .create(true)
                    .append(true)
                    .open(path)
                    .with_context(|| format!("opening {}", path.display()))?;
                let (read_end, write_end) = pipe()?;
                std::thread::spawn(move || copy_output(read_end, Some(log)));
                (null_output()?, file_handle(write_end))
            }
        };

        Ok(Self {
            main: serial_port(Some(&file_handle(read_end)), &main_output),
            log: serial_port(None, &log_output),
            terminal: terminal_port()?,
            escapes,
            _raw_mode: RawMode::enable(log_given)?,
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

/// Copies guest output to stdout, and to `log` if given, until the guest
/// side closes.
fn copy_output(from_guest: OwnedFd, mut log: Option<File>) {
    let mut buf = [0u8; 4096];
    while let Ok(n @ 1..) = read(from_guest.as_raw_fd(), &mut buf) {
        let _ = write_all(libc::STDOUT_FILENO, &buf[..n]);
        if let Some(log) = &mut log {
            let _ = log.write_all(&buf[..n]);
        }
    }
}

/// Write end of the pipe that wakes up `send_terminal` on SIGWINCH, or -1.
static WINCH_PIPE: AtomicI32 = AtomicI32::new(-1);

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
/// and starts sending them there.
fn terminal_port() -> Result<Retained<VZFileHandleSerialPortAttachment>> {
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
    // Left open for good: the signal handler may use it at any time.
    WINCH_PIPE.store(winch_write.into_raw_fd(), Ordering::Relaxed);
    // SAFETY: the handler only makes async-signal-safe calls.
    unsafe { libc::signal(libc::SIGWINCH, on_winch as *const () as libc::sighandler_t) };

    let (guest_reads, to_guest) = pipe()?;
    let (from_guest, guest_writes) = pipe()?;
    std::thread::spawn(move || send_terminal(to_guest, from_guest, winch));
    Ok(serial_port(
        Some(&file_handle(guest_reads)),
        &file_handle(guest_writes),
    ))
}

/// Sends the terminal to the guest. When the guest writes a "?" (which it
/// does once it is ready), a "term <TERM> [<COLORTERM>]" line goes first,
/// and the size follows as a "<rows> <cols>" line; the size alone goes again
/// whenever `winch` is signalled.
fn send_terminal(to_guest: OwnedFd, from_guest: OwnedFd, winch: OwnedFd) {
    let mut fds = [&winch, &from_guest].map(|fd| libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    });
    let mut buf = [0u8; 64];
    loop {
        // SAFETY: `fds` is valid for its length.
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) } < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        let (mut asked, mut resized) = (false, false);
        for pollfd in &mut fds {
            if pollfd.revents == 0 {
                continue;
            }
            match read(pollfd.fd, &mut buf) {
                // poll() skips negative descriptors.
                Ok(0) | Err(_) => pollfd.fd = -1,
                Ok(_) if pollfd.fd == winch.as_raw_fd() => resized = true,
                Ok(n) => asked |= buf[..n].contains(&b'?'),
            }
        }
        let mut message = String::new();
        if asked {
            message.push_str(&terminal_type());
        }
        if (asked || resized)
            && let Some(size) = terminal_size()
        {
            message.push_str(&size);
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
    Some(format!("{} {}\n", size.ws_row, size.ws_col))
}

fn forward_stdin(to_guest: OwnedFd, escapes: &AtomicUsize) {
    let mut buf = [0u8; 4096];
    loop {
        let n = match read(libc::STDIN_FILENO, &mut buf) {
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
    let mut fds = [0; 2];
    // SAFETY: `fds` has room for the two descriptors.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe() just returned these descriptors, and nothing else owns them.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn read(fd: i32, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        // SAFETY: `buf` is valid for writes of its length.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

fn write_all(fd: i32, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        // SAFETY: `data` is valid for reads of its length.
        let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        data = &data[n as usize..];
    }
    Ok(())
}

/// Terminal modes a program in the guest may have switched on and not off
/// when the VM went away: attributes, alternate screen, hidden cursor,
/// application cursor keys and keypad, mouse reporting, bracketed paste.
const TERMINAL_RESET: &[u8] =
    b"\x1b[0m\x1b[?1049l\x1b[?25h\x1b[?1l\x1b>\x1b[?1000l\x1b[?1002l\x1b[?1006l\x1b[?2004l";

/// Puts the terminal into raw mode, so keystrokes (including Ctrl-C) go to
/// the guest unmodified, unless `keep_signals` leaves Ctrl-C raising SIGINT.
/// On drop, resets what the guest may have changed and restores the previous
/// mode.
struct RawMode {
    original: libc::termios,
}

impl RawMode {
    fn enable(keep_signals: bool) -> Result<Option<Self>> {
        let fd = libc::STDIN_FILENO;
        // SAFETY: plain libc calls on a valid descriptor with owned out-params.
        unsafe {
            if libc::isatty(fd) == 0 {
                return Ok(None);
            }
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
            Ok(Some(Self { original }))
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: plain libc calls; restores the attributes read in `enable`.
        unsafe {
            if libc::isatty(libc::STDOUT_FILENO) == 1 {
                let _ = write_all(libc::STDOUT_FILENO, TERMINAL_RESET);
            }
            // Drop input nobody will read now, such as the terminal's replies
            // to queries the guest sent, so it doesn't end up in the shell.
            libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.original);
        }
    }
}
