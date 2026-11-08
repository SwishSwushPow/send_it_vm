//! Running a VM: what makes one up on disk, and what the terminal and
//! signals do while it runs. Running it takes Virtualization.framework
//! (`machine`), so only macOS builds can; elsewhere, sendit builds for
//! developing it, and `run` fails.

// Only `machine` uses some of what's here; macOS builds check it isn't dead.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

#[cfg(target_os = "macos")]
mod machine;
pub mod net;

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};

use crate::config::ByteSize;
use crate::mounts::Share;
#[cfg(target_os = "macos")]
pub use machine::run;

/// Without Virtualization.framework, there is no VM to run.
#[cfg(not(target_os = "macos"))]
pub fn run(_dir: &VmDir, _spec: &VmSpec) -> anyhow::Result<Option<libc::c_int>> {
    anyhow::bail!("sendit runs VMs only on macOS")
}

/// SIGTERM (e.g. from `sendit stop`), SIGHUP (the terminal went away) and
/// SIGINT received so far while a VM ran. Each one counts like a press of
/// the escape key.
static SIGNALS: AtomicUsize = AtomicUsize::new(0);

/// The last of those signals.
static LAST_SIGNAL: AtomicI32 = AtomicI32::new(0);

const CAUGHT_SIGNALS: [libc::c_int; 3] = [libc::SIGTERM, libc::SIGHUP, libc::SIGINT];

extern "C" fn on_signal(signal: libc::c_int) {
    LAST_SIGNAL.store(signal, Ordering::Relaxed);
    SIGNALS.fetch_add(1, Ordering::Relaxed);
}

/// How many `CAUGHT_SIGNALS` have arrived so far, and the last of them.
pub fn signals() -> (usize, libc::c_int) {
    (
        SIGNALS.load(Ordering::Relaxed),
        LAST_SIGNAL.load(Ordering::Relaxed),
    )
}

/// Counts `CAUGHT_SIGNALS` in `SIGNALS` instead of letting them end the
/// process, until dropped. Then they act as before again, so that between
/// VMs, e.g. while `provision --all` prepares the next image, they don't
/// pile up for the next VM.
pub struct CatchSignals {
    previous: [libc::sighandler_t; CAUGHT_SIGNALS.len()],
}

impl CatchSignals {
    pub fn new() -> Self {
        // SAFETY: the handler only touches an atomic, which is signal-safe.
        let previous = CAUGHT_SIGNALS.map(|signal| unsafe {
            libc::signal(signal, on_signal as *const () as libc::sighandler_t)
        });
        Self { previous }
    }
}

impl Drop for CatchSignals {
    fn drop(&mut self) {
        for (signal, previous) in CAUGHT_SIGNALS.into_iter().zip(self.previous) {
            // SAFETY: restores the disposition `new` replaced.
            unsafe { libc::signal(signal, previous) };
        }
    }
}

/// Clears the terminal's screen, if stdout is one, by scrolling everything on
/// it up into the scrollback: a newline per row, then the cursor goes home.
pub fn clear_screen() {
    let mut stdout = std::io::stdout();
    if !stdout.is_terminal() {
        return;
    }
    let Some((rows, _)) = window_size() else {
        return;
    };
    let mut clear = "\n".repeat(rows.into());
    clear.push_str("\x1b[H");
    let _ = stdout
        .write_all(clear.as_bytes())
        .and_then(|()| stdout.flush());
}

/// The terminal's rows and columns, if stdout is a terminal.
fn window_size() -> Option<(u16, u16)> {
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

/// The files making up one VM (the base image or a project VM).
#[derive(Clone, Debug)]
pub struct VmDir(PathBuf);

impl VmDir {
    pub fn new(dir: PathBuf) -> Self {
        Self(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// A project VM's metadata, written when it is created.
    pub fn metadata(&self) -> PathBuf {
        self.0.join("project.toml")
    }

    /// A project VM's copy of the marker of the image build it was made
    /// from, which says what the image's scripts asked to share and when
    /// the build was made.
    pub fn image_marker(&self) -> PathBuf {
        self.0.join("image.toml")
    }

    /// The SSH host keys `sendit ssh` has seen for this VM.
    pub fn known_hosts(&self) -> PathBuf {
        self.0.join("known_hosts")
    }

    /// State of the current run: the lock, the meta share and, for a VM
    /// running in the background, its console log.
    pub fn run_dir(&self) -> PathBuf {
        self.0.join("run")
    }

    /// Held by the `sendit run` process of a running VM, and holds its PID.
    pub fn lock(&self) -> PathBuf {
        self.run_dir().join("lock")
    }

    /// What the `sendit run` process of a VM in the background and its
    /// login console wrote, since it last started.
    pub fn console_log(&self) -> PathBuf {
        self.run_dir().join("console.log")
    }

    /// The host side of the `sendit-meta` share.
    pub fn meta(&self) -> PathBuf {
        self.run_dir().join("meta")
    }

    pub fn disk(&self) -> PathBuf {
        self.0.join("disk.img")
    }

    pub fn efi_vars(&self) -> PathBuf {
        self.0.join("efi-vars")
    }

    pub fn machine_id(&self) -> PathBuf {
        self.0.join("machine-id")
    }

    pub fn mac(&self) -> PathBuf {
        self.0.join("mac")
    }
}

#[derive(Clone, Debug)]
pub struct VmSpec {
    pub cpus: u32,
    pub memory: ByteSize,
    /// Host directories shared with the guest over virtiofs.
    pub shares: Vec<Share>,
    /// Extra read-only disk, e.g. a cloud-init seed ISO.
    pub seed: Option<PathBuf>,
    pub console: ConsoleMode,
    /// Set from another thread to shut the VM down, as if Ctrl-] had been
    /// pressed once: setting it again doesn't force the VM off.
    pub shutdown: Option<Arc<AtomicBool>>,
}

/// What the terminal is used for while the VM runs.
#[derive(Clone, Debug)]
pub enum ConsoleMode {
    /// The guest's login console.
    Login,
    /// Nothing of the VM: another program has the terminal, e.g. `ssh`.
    /// Signals still shut the VM down.
    Hidden,
    /// In the background, without a terminal: the login console gets no
    /// input, which it would take an EOF from and log out, and its output
    /// goes to stdout, a log file.
    Detached,
    /// Provisioning: show and log the guest's /dev/hvc1.
    Provision(ProvisionLog),
}

#[derive(Clone, Debug)]
pub struct ProvisionLog {
    /// Everything the guest writes to /dev/hvc1 is appended to this file.
    pub path: PathBuf,
    /// Output is shown up to the end of the line containing this, which
    /// reports the result; what the guest writes while it shuts down after
    /// that is only logged, so the result stays on screen.
    pub last_line: String,
}
