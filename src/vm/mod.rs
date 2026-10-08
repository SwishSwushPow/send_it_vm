//! Running a VM with Virtualization.framework.
//!
//! The VM lives on the main dispatch queue: `run` creates it on the main
//! thread and then drives the main run loop until the VM has stopped. This
//! keeps all (non-`Send`) Objective-C objects on a single thread.
//!
//! objc2-virtualization marks every method `unsafe`. Unless a `SAFETY`
//! comment says otherwise, the `unsafe` blocks here only call them with
//! valid, retained objects on the main thread.

mod config;
mod console;
pub mod net;

use std::cell::RefCell;
use std::io::{IsTerminal, Write};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSError, NSRunLoop};
use objc2_virtualization::{VZVirtualMachine, VZVirtualMachineDelegate};

use crate::config::ByteSize;
use crate::mounts::Share;
use console::Console;

/// How long to wait for the guest to shut down after asking it to, or after
/// it said that it is shutting down on its own.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the guest is asked again while it hasn't reacted. Early in
/// boot, nothing in the guest handles the request yet (systemd-logind
/// hasn't started), and the request is lost.
const STOP_REQUEST_INTERVAL: Duration = Duration::from_secs(2);

/// SIGTERM (e.g. from `sendit stop`), SIGHUP (the terminal went away) and
/// SIGINT received so far while a VM ran. Each one counts like a press of
/// the escape key.
static SIGNALS: AtomicUsize = AtomicUsize::new(0);

const CAUGHT_SIGNALS: [libc::c_int; 3] = [libc::SIGTERM, libc::SIGHUP, libc::SIGINT];

extern "C" fn on_signal(_: libc::c_int) {
    SIGNALS.fetch_add(1, Ordering::Relaxed);
}

/// Counts `CAUGHT_SIGNALS` in `SIGNALS` instead of letting them end the
/// process, until dropped. Then they act as before again, so that between
/// VMs, e.g. while `provision --all` prepares the next image, they don't
/// pile up for the next VM.
struct CatchSignals {
    previous: [libc::sighandler_t; CAUGHT_SIGNALS.len()],
}

impl CatchSignals {
    fn new() -> Self {
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

/// Prints a status line between guest output. Errors are ignored: after a
/// SIGHUP the terminal is gone, and the VM must still be shut down cleanly.
fn notice(message: &str) {
    let _ = write!(std::io::stderr(), "\r\n[sendit] {message}\r\n");
}

/// Clears the terminal's screen, if stdout is one, by scrolling everything on
/// it up into the scrollback: a newline per row, then the cursor goes home.
pub fn clear_screen() {
    let mut stdout = std::io::stdout();
    if !stdout.is_terminal() {
        return;
    }
    let Some((rows, _)) = console::window_size() else {
        return;
    };
    let mut clear = "\n".repeat(rows.into());
    clear.push_str("\x1b[H");
    let _ = stdout
        .write_all(clear.as_bytes())
        .and_then(|()| stdout.flush());
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

    /// The SSH host keys `sendit ssh` has seen for this VM.
    pub fn known_hosts(&self) -> PathBuf {
        self.0.join("known_hosts")
    }

    /// State of the current run: the lock and the meta share.
    pub fn run_dir(&self) -> PathBuf {
        self.0.join("run")
    }

    /// Held by the `sendit run` process of a running VM, and holds its PID.
    pub fn lock(&self) -> PathBuf {
        self.run_dir().join("lock")
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
    /// Provisioning mode: show and log the guest's /dev/hvc1 instead of
    /// showing the login console.
    pub provision_log: Option<ProvisionLog>,
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

#[derive(Clone, Copy)]
enum Stopping {
    No,
    /// A stop was asked for, but the VM can't be stopped yet (e.g. while it
    /// is starting); retried until it can.
    Pending,
    /// The guest was first asked to shut down at `since`, and last at `last`.
    Requested {
        since: Instant,
        last: Instant,
    },
    Forced,
}

/// State shared between the run loop and VZ callbacks (all on the main thread).
#[derive(Default)]
struct VmState {
    start_error: RefCell<Option<String>>,
    /// Set once the VM has stopped: `Ok` for a clean stop, `Err` otherwise.
    stopped: RefCell<Option<Result<(), String>>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "SenditVmDelegate"]
    #[ivars = Rc<VmState>]
    struct VmDelegate;

    unsafe impl NSObjectProtocol for VmDelegate {}

    unsafe impl VZVirtualMachineDelegate for VmDelegate {
        #[unsafe(method(guestDidStopVirtualMachine:))]
        fn guest_did_stop(&self, _vm: &VZVirtualMachine) {
            self.ivars().stopped.replace(Some(Ok(())));
        }

        #[unsafe(method(virtualMachine:didStopWithError:))]
        fn did_stop_with_error(&self, _vm: &VZVirtualMachine, error: &NSError) {
            self.ivars().stopped.replace(Some(Err(ns_message(error))));
        }
    }
);

impl VmDelegate {
    fn new(state: Rc<VmState>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(state);
        // SAFETY: NSObject's `init` takes no arguments and returns the object.
        unsafe { msg_send![super(this), init] }
    }
}

/// Boots the VM with the console attached to this terminal and blocks until
/// it has stopped. Must be called on the main thread, the only one that
/// drains the main dispatch queue the VM runs on. The guest powers off when
/// its console user logs out. Pressing Ctrl-] (or sending SIGTERM, SIGHUP
/// or SIGINT) asks the guest to shut down; doing it again, or the guest not
/// reacting in time, stops the VM forcibly. So does a shutdown the guest
/// started on its own that takes too long.
pub fn run(dir: &VmDir, spec: &VmSpec) -> Result<()> {
    ensure!(
        unsafe { VZVirtualMachine::isSupported() },
        "virtualization is not supported on this Mac"
    );

    // Signals from before this VM, which ended an earlier one, don't count.
    let mut seen_escapes = SIGNALS.load(Ordering::Relaxed);
    let _signals = CatchSignals::new();
    let console = Console::attach(spec.provision_log.as_ref())?;
    let configuration = catch_objc(|| config::build(dir, spec, &console.ports()))??;
    let state = Rc::new(VmState::default());
    let delegate = VmDelegate::new(state.clone());
    let vm = unsafe {
        VZVirtualMachine::initWithConfiguration(VZVirtualMachine::alloc(), &configuration)
    };
    unsafe { vm.setDelegate(Some(ProtocolObject::from_ref(&*delegate))) };

    let start_state = state.clone();
    let on_start = RcBlock::new(move |error: *mut NSError| {
        // SAFETY: VZ passes a valid NSError or null.
        if let Some(error) = unsafe { error.as_ref() } {
            start_state.start_error.replace(Some(ns_message(error)));
        }
    });
    catch_objc(|| unsafe { vm.startWithCompletionHandler(&on_start) })?;

    // While provisioning, nobody is logged in on the console to log out,
    // and Ctrl-C isn't forwarded to the guest.
    let (started, stopping_notice) = if spec.provision_log.is_some() {
        (
            "VM starting. Press Ctrl-C to abort.",
            "Shutting down. Press Ctrl-C again to force.",
        )
    } else {
        ("VM starting. Type exit to shut it down.", "Shutting down.")
    };
    notice(started);

    let run_loop = NSRunLoop::currentRunLoop();
    let mut stopping = Stopping::No;
    let result = loop {
        let deadline = NSDate::dateWithTimeIntervalSinceNow(0.2);
        run_loop.runMode_beforeDate(unsafe { NSDefaultRunLoopMode }, &deadline);

        if let Some(error) = state.start_error.take() {
            break Err(anyhow::anyhow!(error)).context("starting the VM");
        }
        if let Some(stopped) = state.stopped.take() {
            break stopped
                .map_err(anyhow::Error::msg)
                .context("the VM stopped with an error");
        }

        let escapes = console.escapes() + SIGNALS.load(Ordering::Relaxed);
        let escaped = escapes > seen_escapes;
        seen_escapes = escapes;
        // Provisioning ends with the guest shutting down, which must not be
        // cut short.
        let guest_stopping = spec.provision_log.is_none() && console.guest_stopping();
        stopping = match stopping {
            Stopping::No if escaped => {
                let next = stop(&vm, &state, stopping_notice);
                if matches!(next, Stopping::Pending) {
                    notice("The VM is still starting; it shuts down once it can.");
                }
                next
            }
            // The guest is already shutting down, e.g. after a logout: it
            // gets as long as if it had been asked to.
            Stopping::No | Stopping::Pending if guest_stopping => {
                let now = Instant::now();
                Stopping::Requested {
                    since: now,
                    last: now,
                }
            }
            Stopping::Pending => stop(&vm, &state, stopping_notice),
            Stopping::Requested { since, last } if escaped || since.elapsed() > STOP_TIMEOUT => {
                if force_stop(&vm, &state) {
                    notice("Forcing the VM off.");
                    Stopping::Forced
                } else {
                    Stopping::Requested { since, last }
                }
            }
            Stopping::Requested { since, last } if last.elapsed() >= STOP_REQUEST_INTERVAL => {
                // Once the guest shuts down, it ignores further requests.
                if unsafe { vm.canRequestStop() } {
                    let _ = unsafe { vm.requestStopWithError() };
                }
                Stopping::Requested {
                    since,
                    last: Instant::now(),
                }
            }
            other => other,
        };
    };
    drop(console);
    let _ = writeln!(std::io::stderr());
    result
}

fn ns_message(error: &NSError) -> String {
    error.localizedDescription().to_string()
}

/// Runs `f`, turning an Objective-C exception (which Rust can't unwind
/// through) into an error.
fn catch_objc<R>(f: impl FnOnce() -> R) -> Result<R> {
    objc2::exception::catch(AssertUnwindSafe(f)).map_err(|exception| match exception {
        Some(exception) => anyhow::anyhow!("Objective-C exception: {exception}"),
        None => anyhow::anyhow!("unknown Objective-C exception"),
    })
}

/// Asks the guest to shut down, or stops the VM forcibly if the guest can't
/// be asked. `Pending` if neither is possible yet.
fn stop(vm: &VZVirtualMachine, state: &Rc<VmState>, stopping_notice: &str) -> Stopping {
    if unsafe { vm.canRequestStop() && vm.requestStopWithError().is_ok() } {
        notice(stopping_notice);
        let now = Instant::now();
        Stopping::Requested {
            since: now,
            last: now,
        }
    } else if force_stop(vm, state) {
        Stopping::Forced
    } else {
        Stopping::Pending
    }
}

/// Stops the VM without asking the guest; false if it can't be stopped now.
fn force_stop(vm: &VZVirtualMachine, state: &Rc<VmState>) -> bool {
    if !unsafe { vm.canStop() } {
        return false;
    }
    let state = state.clone();
    let on_stop = RcBlock::new(move |error: *mut NSError| {
        // SAFETY: VZ passes a valid NSError or null.
        let result = match unsafe { error.as_ref() } {
            Some(error) => Err(ns_message(error)),
            None => Ok(()),
        };
        state.stopped.replace(Some(result));
    });
    unsafe { vm.stopWithCompletionHandler(&on_stop) };
    true
}
