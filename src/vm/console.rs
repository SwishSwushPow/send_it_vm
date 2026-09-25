//! Connecting the host terminal to the guest's virtio console.
//!
//! Stdin is read on a helper thread and forwarded to the guest through a pipe,
//! so the escape key (Ctrl-]) can be intercepted instead of reaching the guest.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use objc2::AllocAnyThread;
use objc2::rc::Retained;
use objc2_foundation::NSFileHandle;
use objc2_virtualization::{VZFileHandleSerialPortAttachment, VZSerialPortAttachment};

/// Ctrl-]
pub const ESCAPE_KEY: u8 = 0x1d;

pub struct Console {
    attachment: Retained<VZFileHandleSerialPortAttachment>,
    escapes: Arc<AtomicUsize>,
    _raw_mode: Option<RawMode>,
}

impl Console {
    /// Attaches the guest console to this process' stdin and stdout.
    pub fn attach() -> Result<Self> {
        let (read_end, write_end) = pipe()?;
        let escapes = Arc::new(AtomicUsize::new(0));
        let counter = escapes.clone();
        std::thread::spawn(move || forward_stdin(write_end, &counter));

        let attachment = unsafe {
            let to_guest = NSFileHandle::initWithFileDescriptor_closeOnDealloc(
                NSFileHandle::alloc(),
                read_end.into_raw_fd(),
                true,
            );
            VZFileHandleSerialPortAttachment::initWithFileHandleForReading_fileHandleForWriting(
                VZFileHandleSerialPortAttachment::alloc(),
                Some(&to_guest),
                Some(&NSFileHandle::fileHandleWithStandardOutput()),
            )
        };
        Ok(Self {
            attachment,
            escapes,
            _raw_mode: RawMode::enable()?,
        })
    }

    pub fn attachment(&self) -> &VZSerialPortAttachment {
        &self.attachment
    }

    /// How often the escape key has been pressed so far.
    pub fn escapes(&self) -> usize {
        self.escapes.load(Ordering::Relaxed)
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

/// Puts the terminal into raw mode, so keystrokes (including Ctrl-C) go to
/// the guest unmodified. Restores the previous mode on drop.
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
        // SAFETY: restores the attributes read in `enable`.
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.original) };
    }
}
