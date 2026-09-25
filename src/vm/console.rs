//! Connecting the host terminal to the guest's virtio console.
//!
//! Stdin is read on a helper thread and forwarded to the guest through a pipe,
//! so the escape key (Ctrl-]) can be intercepted instead of reaching the guest.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

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
    /// Output-only port for provisioning, /dev/hvc1 in the guest.
    log: Option<Retained<VZFileHandleSerialPortAttachment>>,
    escapes: Arc<AtomicUsize>,
    _raw_mode: Option<RawMode>,
}

impl Console {
    /// Connects stdin to the guest's login console. Without `log`, the login
    /// console's output goes to stdout. With `log`, it is discarded instead,
    /// and a second port's output is shown and appended to `log`: nothing
    /// runs a getty on that port, so provisioning output can't be cut off by
    /// one hanging up the terminal.
    pub fn attach(log: Option<&Path>) -> Result<Self> {
        let (read_end, write_end) = pipe()?;
        let escapes = Arc::new(AtomicUsize::new(0));
        let counter = escapes.clone();
        std::thread::spawn(move || forward_stdin(write_end, &counter));

        let (main_output, log) = match log {
            None => (NSFileHandle::fileHandleWithStandardOutput(), None),
            Some(path) => {
                let log = File::options()
                    .create(true)
                    .append(true)
                    .open(path)
                    .with_context(|| format!("opening {}", path.display()))?;
                let (read_end, write_end) = pipe()?;
                std::thread::spawn(move || tee_output(read_end, log));
                let port = serial_port(None, &file_handle(write_end));
                // VZ needs handles backed by real file descriptors, which
                // `fileHandleWithNullDevice` isn't.
                let null = File::options().write(true).open("/dev/null")?;
                (file_handle(null.into()), Some(port))
            }
        };

        Ok(Self {
            main: serial_port(Some(&file_handle(read_end)), &main_output),
            log,
            escapes,
            _raw_mode: RawMode::enable()?,
        })
    }

    /// The attachments for the guest's serial ports, in order.
    pub fn ports(&self) -> Vec<&VZSerialPortAttachment> {
        let mut ports: Vec<&VZSerialPortAttachment> = vec![&self.main];
        ports.extend(self.log.as_deref().map(|port| &**port));
        ports
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

/// Wraps `fd` in an `NSFileHandle` that closes it when released.
fn file_handle(fd: OwnedFd) -> Retained<NSFileHandle> {
    NSFileHandle::initWithFileDescriptor_closeOnDealloc(
        NSFileHandle::alloc(),
        fd.into_raw_fd(),
        true,
    )
}

/// Copies guest output to stdout and `log` until the guest side closes.
fn tee_output(from_guest: OwnedFd, mut log: File) {
    let mut buf = [0u8; 4096];
    while let Ok(n @ 1..) = read(from_guest.as_raw_fd(), &mut buf) {
        let _ = write_all(libc::STDOUT_FILENO, &buf[..n]);
        let _ = log.write_all(&buf[..n]);
    }
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
/// the guest unmodified. On drop, resets what the guest may have changed and
/// restores the previous mode.
struct RawMode {
    original: libc::termios,
}

impl RawMode {
    fn enable() -> Result<Option<Self>> {
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
