//! Running a VM with Virtualization.framework.
//!
//! The VM lives on the main dispatch queue: `run` creates it on the main
//! thread and then drives the main run loop until the VM has stopped. This
//! keeps all (non-`Send`) Objective-C objects on a single thread.

mod config;
mod console;
pub mod net;

use std::cell::RefCell;
use std::io::Write;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
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

/// How long to wait for the guest to shut down after asking it to.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// SIGTERM (e.g. from `send_it stop`), SIGHUP (the terminal went away) and
/// SIGINT received so far. Each one counts like a press of the escape key.
static SIGNALS: AtomicUsize = AtomicUsize::new(0);

extern "C" fn on_signal(_: libc::c_int) {
    SIGNALS.fetch_add(1, Ordering::Relaxed);
}

fn handle_signals() {
    for signal in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
        // SAFETY: the handler only touches an atomic, which is signal-safe.
        unsafe { libc::signal(signal, on_signal as *const () as libc::sighandler_t) };
    }
}

/// Prints a status line between guest output. Errors are ignored: after a
/// SIGHUP the terminal is gone, and the VM must still be shut down cleanly.
fn notice(message: &str) {
    let _ = write!(std::io::stderr(), "\r\n[send_it] {message}\r\n");
}

/// The files making up one VM (the base image or a project VM).
#[derive(Clone, Debug)]
pub struct VmDir(PathBuf);

impl VmDir {
    pub fn new(dir: PathBuf) -> Self {
        Self(dir)
    }

    pub fn path(&self) -> &std::path::Path {
        &self.0
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
    /// Provisioning mode: show and log the guest's /dev/hvc1 to this file
    /// instead of showing the login console.
    pub provision_log: Option<PathBuf>,
}

#[derive(Clone, Copy)]
enum Stopping {
    No,
    /// The guest was asked to shut down at this time.
    Requested(Instant),
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
    #[name = "SendItVmDelegate"]
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
            let message = error.localizedDescription().to_string();
            self.ivars().stopped.replace(Some(Err(message)));
        }
    }
);

impl VmDelegate {
    fn new(state: Rc<VmState>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(state);
        unsafe { msg_send![super(this), init] }
    }
}

/// Boots the VM with the console attached to this terminal and blocks until
/// it has stopped. Pressing Ctrl-] (or sending SIGTERM, SIGHUP or SIGINT)
/// asks the guest to shut down; doing it again, or the guest not reacting in
/// time, stops the VM forcibly.
pub fn run(dir: &VmDir, spec: &VmSpec) -> Result<()> {
    // The VM runs on the main dispatch queue, which only the main thread drains.
    ensure!(
        unsafe { libc::pthread_main_np() } == 1,
        "vm::run must be called on the main thread"
    );
    ensure!(
        unsafe { VZVirtualMachine::isSupported() },
        "virtualization is not supported on this Mac"
    );
    ensure!(
        dir.disk().exists(),
        "{} does not exist",
        dir.disk().display()
    );

    handle_signals();
    let console = Console::attach(spec.provision_log.as_deref())?;
    let configuration = catch_objc(|| config::build(dir, spec, &console.ports()))??;
    let state = Rc::new(VmState::default());
    let delegate = VmDelegate::new(state.clone());
    let vm = unsafe {
        VZVirtualMachine::initWithConfiguration(VZVirtualMachine::alloc(), &configuration)
    };
    unsafe { vm.setDelegate(Some(ProtocolObject::from_ref(&*delegate))) };

    let start_state = state.clone();
    let on_start = RcBlock::new(move |error: *mut NSError| {
        if let Some(error) = unsafe { error.as_ref() } {
            let message = error.localizedDescription().to_string();
            start_state.start_error.replace(Some(message));
        }
    });
    catch_objc(|| unsafe { vm.startWithCompletionHandler(&on_start) })?;

    notice("VM starting. Press Ctrl-] to shut it down.");

    let run_loop = NSRunLoop::currentRunLoop();
    let mut seen_escapes = 0;
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
        stopping = match stopping {
            Stopping::No if escaped => {
                if unsafe { vm.canRequestStop() && vm.requestStopWithError().is_ok() } {
                    notice("Shutting down. Press Ctrl-] again to force.");
                    Stopping::Requested(Instant::now())
                } else {
                    force_stop(&vm, &state);
                    Stopping::Forced
                }
            }
            Stopping::Requested(at) if escaped || at.elapsed() > STOP_TIMEOUT => {
                notice("Forcing the VM off.");
                force_stop(&vm, &state);
                Stopping::Forced
            }
            other => other,
        };
    };
    drop(console);
    let _ = writeln!(std::io::stderr());
    result
}

/// Runs `f`, turning an Objective-C exception (which Rust can't unwind
/// through) into an error.
fn catch_objc<R>(f: impl FnOnce() -> R) -> Result<R> {
    objc2::exception::catch(AssertUnwindSafe(f)).map_err(|exception| match exception {
        Some(exception) => anyhow::anyhow!("Objective-C exception: {exception}"),
        None => anyhow::anyhow!("unknown Objective-C exception"),
    })
}

fn force_stop(vm: &VZVirtualMachine, state: &Rc<VmState>) {
    if !unsafe { vm.canStop() } {
        return;
    }
    let state = state.clone();
    let on_stop = RcBlock::new(move |error: *mut NSError| {
        let result = match unsafe { error.as_ref() } {
            Some(error) => Err(error.localizedDescription().to_string()),
            None => Ok(()),
        };
        state.stopped.replace(Some(result));
    });
    unsafe { vm.stopWithCompletionHandler(&on_stop) };
}
