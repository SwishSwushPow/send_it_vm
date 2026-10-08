//! Tests that boot real VMs, driving the sendit binary like a user would.
//!
//! They are ignored by default: they need Virtualization.framework, which
//! GitHub's macOS runners lack, network access the first time, and a few
//! minutes. Run them with
//!
//! ```sh
//! cargo test --test vm -- --ignored
//! ```
//!
//! sendit runs with a scratch home in `target/tmp/vm-tests/home`, so the
//! real `~/.cache/sendit` and `~/.sendit` stay untouched. The first run
//! provisions the `default` image there, which is kept until the files it is
//! built from change. The custom provisioning scripts test has a scratch
//! home of its own, `custom-home`, where it builds an image with its
//! scripts, and one whose script fails. Provisioning reuses the Debian
//! image in the real `~/.cache/sendit/downloads` if there is one, and
//! downloads it otherwise.
//!
//! Most tests run `sendit run` with plain pipes and work in the VM over
//! `sendit ssh`; the console tests run it on a pseudo-terminal and type into
//! it. Output of `sendit run` goes to `target/tmp/vm-tests/logs/`.

mod common;

use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

use common::{assert_fails_with, text};

/// What the base images are built from, apart from custom scripts: when any
/// of it changes, the tests provision their images again.
const PROVISION_INPUTS: [&str; 3] = [
    include_str!("../src/assets/provision.sh"),
    include_str!("../src/assets/user-data.yaml"),
    include_str!("../src/provision.rs"),
];

/// Including downloading Debian and its packages.
const PROVISION_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Until the VM answers over SSH.
const BOOT_TIMEOUT: Duration = Duration::from_secs(180);
/// `sendit stop` itself gives up after 45 seconds.
const STOP_TIMEOUT: Duration = Duration::from_secs(90);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(90);
/// For each attempt to reach a booting VM.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Custom provisioning scripts for the `custom` and `broken` images, by path
/// below `~/.config/sendit/provision-scripts`. Both images run the shared
/// ones; `custom/20-replaced.sh` replaces the shared one of the same name
/// and asks for two mounts, one of which doesn't exist.
const CUSTOM_SCRIPTS: [(&str, &str); 4] = [
    (
        "10-shared.sh",
        "echo \"10-shared as $(id -un) in $PWD\" >> ~/provisioned\n",
    ),
    ("20-replaced.sh", "echo 20-shared >> ~/provisioned\n"),
    (
        "custom/20-replaced.sh",
        "# sendit-mount: ~/script-share:/home/dev/script-share\n\
         # sendit-mount: ~/script-missing\n\
         echo 20-own >> ~/provisioned\nsudo sh -c 'echo made by root > /etc/sendit-test'\n",
    ),
    ("broken/30-fail.sh", "exit 3\n"),
];

/// Where the tests keep their files.
fn root() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join("vm-tests")
}

/// A scratch home for sendit.
struct Env {
    home: PathBuf,
}

impl Env {
    fn sendit(&self) -> Command {
        common::sendit(&self.home)
    }

    fn cache(&self) -> PathBuf {
        self.home.join(".cache/sendit")
    }

    /// Runs `sendit provision <image> --force`.
    fn provision(&self, image: &str) -> Result<ExitStatus> {
        eprintln!(
            "Provisioning the {image} image for the VM tests in {}",
            self.home.display()
        );
        let mut child = self
            .sendit()
            .args(["provision", image, "--force"])
            .stdin(Stdio::piped())
            .spawn()
            .context("running sendit provision")?;
        // Stdin stays open: the guest's console gets no end of input.
        let _stdin = child.stdin.take();
        wait_timeout(&mut child, PROVISION_TIMEOUT).context("sendit provision")
    }

    /// Like `provision`, which must succeed.
    fn provision_ok(&self, image: &str) -> Result<()> {
        let status = self.provision(image)?;
        ensure!(
            status.success(),
            "`sendit provision {image}` failed with {status}; see its output above. \
             It needs network access to download Debian and its packages."
        );
        Ok(())
    }
}

/// The scratch home with a provisioned `default` image.
fn env() -> &'static Env {
    static ENV: OnceLock<Result<Env, String>> = OnceLock::new();
    fixture(&ENV, || {
        setup("home", &[], &["default"], |env| env.provision_ok("default"))
    })
}

/// A scratch home of its own with `CUSTOM_SCRIPTS`, whose shared scripts
/// would otherwise change the state of the `default` image, and the
/// `custom` image built with them. Building it also checks that a failing
/// script fails the build of the `broken` image.
fn custom_env() -> &'static Env {
    static ENV: OnceLock<Result<Env, String>> = OnceLock::new();
    fixture(&ENV, || {
        setup("custom-home", &CUSTOM_SCRIPTS, &["custom"], |env| {
            let status = env.provision("broken")?;
            ensure!(
                !status.success(),
                "a failing custom script didn't fail the build"
            );
            ensure!(
                !env.cache().join("images/broken").exists(),
                "the failed build left an image behind"
            );
            let log = env.cache().join("provision/broken/console.log");
            let log =
                fs::read_to_string(&log).with_context(|| format!("reading {}", log.display()))?;
            ensure!(
                log.contains("sendit: custom script broken/30-fail.sh failed"),
                "the console log doesn't say which custom script failed"
            );
            env.provision_ok("custom")
        })
    })
}

/// The `Env` in `cell`, set up with `init` by the first test that needs it.
/// If that fails, every test using it fails with the same error.
fn fixture(
    cell: &'static OnceLock<Result<Env, String>>,
    init: impl FnOnce() -> Result<Env>,
) -> &'static Env {
    match cell.get_or_init(|| init().map_err(|e| format!("{e:#}"))) {
        Ok(env) => env,
        Err(e) => panic!("setting up the VM tests failed: {e}"),
    }
}

/// Sets up the scratch home `name` with the custom provisioning `scripts`
/// and runs `build` in it to make `images`, unless an earlier run already
/// did so from the same inputs.
fn setup(
    name: &str,
    scripts: &[(&str, &str)],
    images: &[&str],
    build: impl FnOnce(&Env) -> Result<()>,
) -> Result<Env> {
    let env = Env {
        home: root().join(name),
    };
    fs::create_dir_all(&env.home)?;
    // Written every time, in case a test changed them and failed before
    // changing them back.
    let scripts_dir = env.home.join(".config/sendit/provision-scripts");
    let _ = fs::remove_dir_all(&scripts_dir);
    for (path, content) in scripts {
        let path = scripts_dir.join(path);
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(&path, content)?;
    }

    let mut hasher = Sha256::new();
    for input in PROVISION_INPUTS {
        hasher.update(input);
    }
    for (path, content) in scripts {
        hasher.update(path);
        hasher.update(content);
    }
    let key = hex::encode(hasher.finalize());
    let key_file = root().join(format!("{name}.key"));
    let built = images.iter().all(|image| {
        env.cache()
            .join(format!("images/{image}/provisioned.toml"))
            .exists()
    });
    if built && fs::read_to_string(&key_file).is_ok_and(|k| k == key) {
        return Ok(env);
    }

    let _ = fs::remove_file(&key_file);
    seed_downloads(&env.home)?;
    build(&env)?;
    fs::write(&key_file, key)?;
    Ok(env)
}

/// Clones the downloads of the real home into the scratch home, so that
/// provisioning doesn't download Debian again. Without any, sendit downloads
/// it.
fn seed_downloads(home: &Path) -> Result<()> {
    let Some(real) = std::env::home_dir().map(|h| h.join(".cache/sendit/downloads")) else {
        return Ok(());
    };
    let Ok(entries) = fs::read_dir(&real) else {
        eprintln!("No Debian image in {} to reuse.", real.display());
        return Ok(());
    };
    let downloads = home.join(".cache/sendit/downloads");
    fs::create_dir_all(&downloads)?;
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let to = downloads.join(entry.file_name());
        let _ = fs::remove_file(&to);
        // An APFS clone: instant, and takes no space.
        let cloned = Command::new("cp")
            .arg("-c")
            .arg(entry.path())
            .arg(&to)
            .status()
            .is_ok_and(|s| s.success());
        if !cloned {
            fs::copy(entry.path(), &to)
                .with_context(|| format!("copying {}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// How many tests may run VMs at the same time. Each VM gets the default 2
/// CPUs and 4 GiB of memory; more of them would compete for those and slow
/// each other down past the timeouts.
const MAX_VMS: usize = 2;

/// Waits until fewer than `MAX_VMS` tests are running VMs; the test may run
/// its VMs until it drops the returned slot.
fn vm_slot() -> VmSlot {
    let mut running = VM_SLOTS.0.lock().unwrap_or_else(|e| e.into_inner());
    while *running >= MAX_VMS {
        running = VM_SLOTS.1.wait(running).unwrap_or_else(|e| e.into_inner());
    }
    *running += 1;
    VmSlot
}

/// The number of tests holding a `VmSlot`, and the signal that one ended.
static VM_SLOTS: (Mutex<usize>, Condvar) = (Mutex::new(0), Condvar::new());

struct VmSlot;

impl Drop for VmSlot {
    fn drop(&mut self) {
        *VM_SLOTS.0.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
        VM_SLOTS.1.notify_one();
    }
}

/// A project directory, emptied and without a VM to start with.
struct Project {
    env: &'static Env,
    name: String,
    dir: PathBuf,
    runs: std::cell::Cell<usize>,
}

impl Project {
    /// A project in the scratch home with the `default` image.
    fn new(name: &str) -> Self {
        Self::in_env(env(), name)
    }

    fn in_env(env: &'static Env, name: &str) -> Self {
        let dir = root().join("projects").join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let project = Self {
            env,
            name: name.into(),
            dir,
            runs: Default::default(),
        };
        // A VM left over from an earlier run, maybe still running if that
        // run was killed.
        project.ok(&["stop"]);
        project.ok(&["reset", "--yes"]);
        project
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = self.env.sendit();
        cmd.arg("-C").arg(&self.dir).args(args);
        cmd
    }

    /// Runs `sendit <args>` for this project.
    fn sendit(&self, args: &[&str]) -> Output {
        output(self.command(args), COMMAND_TIMEOUT)
            .unwrap_or_else(|e| panic!("sendit {}: {e:#}", args.join(" ")))
    }

    /// Runs `sendit <args>`, which must succeed, and returns its stdout.
    fn ok(&self, args: &[&str]) -> String {
        let output = self.sendit(args);
        assert!(
            output.status.success(),
            "sendit {} failed with {}:\n{}",
            args.join(" "),
            output.status,
            text(&output.stderr)
        );
        text(&output.stdout)
    }

    /// A new log file for the next `sendit run`.
    fn next_log(&self) -> PathBuf {
        let logs = root().join("logs");
        fs::create_dir_all(&logs).unwrap();
        self.runs.set(self.runs.get() + 1);
        logs.join(format!("{}-{}.log", self.name, self.runs.get()))
    }

    /// Starts `sendit run <args>` and waits until the VM answers over SSH.
    fn run(&self, args: &[&str]) -> Vm<'_> {
        let log = self.next_log();
        let file = File::create(&log).unwrap();
        let mut child = self
            .command(&[&["run"], args].concat())
            .stdin(Stdio::piped())
            .stdout(file.try_clone().unwrap())
            .stderr(file)
            .spawn()
            .expect("running sendit run");
        let stdin = child.stdin.take();
        // From here on, a failure shuts the VM down.
        let mut vm = Vm {
            project: self,
            child,
            stdin,
            log,
        };

        let start = Instant::now();
        loop {
            if let Some(status) = vm.child.try_wait().unwrap() {
                panic!(
                    "`sendit run` exited with {status} before the VM was up; see {}",
                    vm.log.display()
                );
            }
            // Fails while the VM has no IP address or sshd isn't up yet.
            let probe = output(self.command(&["ssh", "--", "true"]), PROBE_TIMEOUT);
            if probe.is_ok_and(|o| o.status.success()) {
                return vm;
            }
            assert!(
                start.elapsed() < BOOT_TIMEOUT,
                "the VM did not answer over SSH within {} seconds; see {}",
                BOOT_TIMEOUT.as_secs(),
                vm.log.display()
            );
            sleep(Duration::from_secs(1));
        }
    }
}

/// A running `sendit run`. Dropping it shuts the VM down.
struct Vm<'a> {
    project: &'a Project,
    child: Child,
    /// Kept open for typing into the console.
    stdin: Option<ChildStdin>,
    log: PathBuf,
}

impl Vm<'_> {
    /// Runs `script` in the guest with `sh -c`, as root if `root`.
    fn ssh(&self, root: bool, script: &str) -> Output {
        let mut args = vec!["ssh"];
        if root {
            args.push("--root");
        }
        args.extend(["--", "sh", "-c", script]);
        self.project.sendit(&args)
    }

    /// Runs `script` as the guest user, which must succeed, and returns its
    /// trimmed stdout.
    fn ok(&self, script: &str) -> String {
        self.expect_ok(false, script)
    }

    /// Like `ok`, as root.
    fn root_ok(&self, script: &str) -> String {
        self.expect_ok(true, script)
    }

    fn expect_ok(&self, root: bool, script: &str) -> String {
        let output = self.ssh(root, script);
        assert!(
            output.status.success(),
            "`{script}` failed with {}:\n{}{}",
            output.status,
            text(&output.stdout),
            text(&output.stderr)
        );
        text(&output.stdout).trim().to_string()
    }

    /// Runs `script` as the guest user, which must fail.
    fn fails(&self, script: &str) {
        let output = self.ssh(false, script);
        assert!(
            !output.status.success(),
            "`{script}` succeeded:\n{}",
            text(&output.stdout)
        );
    }

    /// Shuts the VM down with `sendit stop`.
    fn stop(mut self) {
        self.project.ok(&["stop"]);
        self.wait_for_exit();
    }

    /// Shuts the VM down by logging out of its console.
    fn exit_console(mut self) {
        let start = Instant::now();
        // The console's shell may not be up yet, and drop what comes before.
        while self.child.try_wait().unwrap().is_none() {
            assert!(
                start.elapsed() < STOP_TIMEOUT,
                "the VM did not shut down after `exit` on its console; see {}",
                self.log.display()
            );
            if let Some(stdin) = &mut self.stdin {
                // Fails once sendit has exited.
                let _ = stdin.write_all(b"exit\n");
            }
            sleep(Duration::from_secs(5));
        }
        self.wait_for_exit();
    }

    /// Waits until `sendit run` has exited, which it must do successfully.
    fn wait_for_exit(&mut self) {
        let status = wait_timeout(&mut self.child, STOP_TIMEOUT).unwrap();
        assert!(
            status.success(),
            "`sendit run` exited with {status}; see {}",
            self.log.display()
        );
    }
}

impl Drop for Vm<'_> {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            terminate(&mut self.child);
        }
    }
}

/// What sendit writes when it gives the terminal back, as in
/// src/vm/console.rs.
const TERMINAL_RESET: &[u8] =
    b"\x1b[0m\x1b[?25h\x1b[?1l\x1b>\x1b[?1000l\x1b[?1002l\x1b[?1006l\x1b[?2004l";
const LEAVE_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049l";

/// The time to wait for the console's response to a line typed into it.
const CONSOLE_TIMEOUT: Duration = Duration::from_secs(20);

impl Project {
    /// Starts `sendit run` on a pseudo-terminal of `rows` and `cols` with
    /// `TERM` and `COLORTERM` set as given, and waits until the console's
    /// shell is ready.
    fn run_in_terminal(
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
struct Terminal {
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
    fn send(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
    }

    /// Types `line` into the console, followed by Enter.
    fn type_line(&mut self, line: &str) {
        self.send(format!("{line}\r").as_bytes());
    }

    /// Waits until `wanted` appears in the output after what earlier
    /// expectations consumed.
    fn expect(&mut self, wanted: &str, timeout: Duration) {
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
    fn type_until(&mut self, line: &str, wanted: &str) {
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
    fn expect_end(&self, wanted: &[u8]) {
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
    fn resize(&mut self, rows: u16, cols: u16) {
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
    fn local_modes(&self) -> libc::tcflag_t {
        // SAFETY: tcgetattr fills in the termios it is given.
        unsafe {
            let mut termios = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(self.slave.as_raw_fd(), &mut termios), 0);
            termios.c_lflag
        }
    }

    /// Waits until `sendit run` has exited, which it must do successfully.
    fn wait_for_exit(&mut self) {
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

/// Runs `cmd` with no input and returns its output, killing it after
/// `timeout`.
fn output(mut cmd: Command, timeout: Duration) -> Result<Output> {
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || tx.send(child.wait_with_output()));
    match rx.recv_timeout(timeout) {
        Ok(output) => Ok(output?),
        Err(_) => {
            // SAFETY: plain syscall; the child is still running, or exited
            // only just now, too recently for its PID to be reused.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            bail!("timed out after {} seconds", timeout.as_secs())
        }
    }
}

/// Waits for `child` to exit, ending it with `terminate` after `timeout`.
fn wait_timeout(child: &mut Child, timeout: Duration) -> Result<ExitStatus> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if start.elapsed() > timeout {
            terminate(child);
            bail!("timed out after {} seconds", timeout.as_secs());
        }
        sleep(Duration::from_millis(250));
    }
}

/// Sends SIGTERM to `child`, which makes sendit shut its VM down, and kills
/// it if it hasn't exited after `STOP_TIMEOUT`.
fn terminate(child: &mut Child) {
    // SAFETY: plain syscall; the child hasn't been waited for, so its PID
    // is still its own.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    let start = Instant::now();
    while start.elapsed() < STOP_TIMEOUT {
        if !matches!(child.try_wait(), Ok(None)) {
            return;
        }
        sleep(Duration::from_millis(250));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// The host's time zone, such as `Europe/Berlin`, as sendit tells the guest.
fn host_timezone() -> Option<String> {
    let target = fs::read_link("/etc/localtime").ok()?;
    let (_, name) = target.to_str()?.rsplit_once("/zoneinfo/")?;
    Some(name.into())
}

#[test]
#[ignore = "boots VMs; run with `cargo test --test vm -- --ignored`"]
fn runs_commands_over_ssh() {
    let _slot = vm_slot();
    let project = Project::new("lifecycle");
    let vm = project.run(&[]);

    assert_eq!(vm.ok("uname -m"), "aarch64");
    assert_eq!(vm.ok("getconf PAGESIZE"), "16384");
    assert_eq!(vm.ok("id -un"), "dev");
    assert_eq!(vm.root_ok("id -u"), "0");
    vm.fails("sudo -n true");
    // The root filesystem grew from the base image's 16 GiB to the default
    // disk size of 64 GiB.
    let root_size: u64 = vm.ok("findmnt -bno SIZE /").parse().unwrap();
    assert!(
        root_size > 32 << 30,
        "the root filesystem has {root_size} bytes"
    );
    vm.fails("systemctl is-failed --quiet sendit-growfs");
    assert!(project.ok(&["status"]).contains("running at"));

    let second = project.sendit(&["run"]);
    assert!(!second.status.success());
    let stderr = text(&second.stderr);
    assert!(stderr.contains("already running"), "{stderr}");

    // ~/.ssh/config may forward the agent or ports to every host, so
    // `sendit ssh` leaves it out. ssh finds it through the user database,
    // not $HOME, so the test can't plant one; instead an ssh on the PATH
    // passes ssh a config that breaks every connection. sendit's own -F
    // comes later and must win.
    let bin = root().join("ssh-wrapper");
    let _ = fs::remove_dir_all(&bin);
    fs::create_dir_all(&bin).unwrap();
    let config = bin.join("config");
    fs::write(&config, "Host *\n  ProxyCommand /usr/bin/false\n").unwrap();
    let used = bin.join("used");
    let wrapper = bin.join("ssh");
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\ntouch '{}'\nexec /usr/bin/ssh -F '{}' \"$@\"\n",
            used.display(),
            config.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    let mut ssh = project.command(&["ssh", "--", "echo", "reached"]);
    let path = std::env::var("PATH").unwrap_or_default();
    ssh.env("PATH", format!("{}:{path}", bin.display()));
    let ssh = output(ssh, COMMAND_TIMEOUT).unwrap();
    assert!(used.exists(), "sendit ssh didn't run ssh from the PATH");
    assert!(ssh.status.success(), "{}", text(&ssh.stderr));
    assert_eq!(text(&ssh.stdout), "reached\n");

    vm.stop();
    assert!(project.ok(&["status"]).contains("stopped"));
    let ssh = project.sendit(&["ssh", "--", "true"]);
    assert!(!ssh.status.success());
    let stderr = text(&ssh.stderr);
    assert!(stderr.contains("not running"), "{stderr}");
}

#[test]
#[ignore = "boots VMs; run with `cargo test --test vm -- --ignored`"]
fn shares_the_project_and_mounts() {
    let _slot = vm_slot();
    let project = Project::new("shares");
    fs::write(project.dir.join("hello.txt"), "from the host\n").unwrap();
    fs::create_dir(project.dir.join(".git")).unwrap();
    fs::write(project.dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    let mounts = root().join("mounts");
    let (ro, rw) = (mounts.join("ro-data"), mounts.join("rw-data"));
    for dir in [&ro, &rw] {
        let _ = fs::remove_dir_all(dir);
        fs::create_dir_all(dir).unwrap();
    }
    fs::write(ro.join("data.txt"), "read only\n").unwrap();

    let vm = project.run(&[
        "--mount",
        ro.to_str().unwrap(),
        "--mount",
        &format!("{}:/home/dev/rw:rw", rw.display()),
    ]);

    // The project is shared read-write at ~/<name>.
    assert_eq!(vm.ok("cat ~/shares/hello.txt"), "from the host");
    vm.ok("echo from the guest > ~/shares/guest.txt");
    assert_eq!(
        fs::read_to_string(project.dir.join("guest.txt")).unwrap(),
        "from the guest\n"
    );
    // Its .git is hidden behind an empty read-only mount.
    assert_eq!(vm.ok("ls -A ~/shares/.git"), "");
    vm.fails("touch ~/shares/.git/HEAD");

    // Without a guest path, a mount goes to /mnt/<name>; without :rw, it is
    // read-only.
    assert_eq!(vm.ok("cat /mnt/ro-data/data.txt"), "read only");
    vm.fails("touch /mnt/ro-data/data.txt");
    vm.ok("echo written > ~/rw/new.txt");
    assert_eq!(fs::read_to_string(rw.join("new.txt")).unwrap(), "written\n");

    if let Some(zone) = host_timezone() {
        assert_eq!(
            vm.ok("readlink /etc/localtime"),
            format!("/usr/share/zoneinfo/{zone}")
        );
    }
    vm.stop();

    // With --expose-git, .git is part of the project share.
    let vm = project.run(&["--expose-git"]);
    assert_eq!(vm.ok("cat ~/shares/.git/HEAD"), "ref: refs/heads/main");
    vm.ok("echo from the guest > ~/shares/.git/guest");
    assert_eq!(
        fs::read_to_string(project.dir.join(".git/guest")).unwrap(),
        "from the guest\n"
    );
    vm.stop();

    // With --read-only, the project share is read-only, .git included.
    let vm = project.run(&["--read-only", "--expose-git"]);
    assert_eq!(vm.ok("cat ~/shares/hello.txt"), "from the host");
    assert_eq!(vm.ok("cat ~/shares/.git/HEAD"), "ref: refs/heads/main");
    vm.fails("touch ~/shares/hello.txt");
    vm.fails("touch ~/shares/new.txt");
    vm.fails("touch ~/shares/.git/HEAD");
    vm.stop();
}

#[test]
#[ignore = "boots VMs; run with `cargo test --test vm -- --ignored`"]
fn keeps_the_vm_until_reset() {
    let _slot = vm_slot();
    let project = Project::new("persistence");

    let vm = project.run(&[]);
    vm.ok("echo kept > ~/marker");
    vm.exit_console();

    let vm = project.run(&[]);
    assert_eq!(vm.ok("cat ~/marker"), "kept");
    // The disk has grown on the first boot; nothing to grow isn't a failure.
    vm.fails("systemctl is-failed --quiet sendit-growfs");
    vm.stop();

    project.ok(&["reset", "--yes"]);
    let vm = project.run(&[]);
    vm.fails("test -e ~/marker");
    vm.stop();
}

#[test]
#[ignore = "boots VMs; run with `cargo test --test vm -- --ignored`"]
fn console_follows_the_terminal() {
    let _slot = vm_slot();
    let project = Project::new("console");
    let raw = libc::ICANON | libc::ECHO | libc::ISIG;
    let mut term = project.run_in_terminal("xterm-256color", Some("truecolor"), 40, 120);

    assert_eq!(
        term.local_modes() & raw,
        0,
        "the terminal is not in raw mode"
    );
    term.type_until(
        r#"echo "TERM-$TERM-$COLORTERM-END""#,
        "TERM-xterm-256color-truecolor-END",
    );
    term.type_until(r#"echo "SIZE-$(stty size)-END""#, "SIZE-40 120-END");
    term.resize(30, 100);
    term.type_until(r#"echo "SIZE-$(stty size)-END""#, "SIZE-30 100-END");

    // Ctrl-C interrupts the command in the guest.
    term.type_line(r#"echo "SLEEP-$((5 + 5))"; sleep 300"#);
    term.expect("SLEEP-10", CONSOLE_TIMEOUT);
    sleep(Duration::from_secs(1));
    term.send(b"\x03");
    term.type_line(r#"echo "ALIVE-$((3 + 3))""#);
    term.expect("ALIVE-6", CONSOLE_TIMEOUT);

    // Logging out shuts the VM down and gives the terminal back as it was.
    term.type_line("exit");
    term.wait_for_exit();
    assert_eq!(term.local_modes() & raw, raw, "the terminal is still raw");
    term.expect_end(TERMINAL_RESET);
}

#[test]
#[ignore = "boots VMs; run with `cargo test --test vm -- --ignored`"]
fn escape_key_shuts_the_vm_down() {
    let _slot = vm_slot();
    let project = Project::new("escape");
    // Without terminfo in the guest, the console falls back.
    let mut term = project.run_in_terminal("sendit-unknown-terminal", None, 40, 120);
    term.type_until(r#"echo "TERM-$TERM-END""#, "TERM-xterm-256color-END");

    // The VM goes away while the guest has the terminal on the alternate
    // screen, as a full-screen program would.
    term.type_line(r#"printf '\033[?1049h'; echo "ALT-$((4 + 4))""#);
    term.expect("ALT-8", CONSOLE_TIMEOUT);
    term.send(b"\x1d");
    term.expect("[sendit] Shutting down.", CONSOLE_TIMEOUT);
    term.wait_for_exit();
    term.expect_end(&[LEAVE_ALTERNATE_SCREEN, TERMINAL_RESET].concat());
}

impl Project {
    /// The directory of the project's VM, which must exist.
    fn vm_dir(&self) -> PathBuf {
        let prefix = format!("{}_", self.name);
        fs::read_dir(self.env.home.join(".sendit"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .starts_with(&prefix)
            })
            .unwrap_or_else(|| panic!("{} has no VM", self.name))
    }

    /// The line `sendit images` prints for `image`.
    fn image_line(&self, image: &str) -> String {
        let images = self.ok(&["images"]);
        images
            .lines()
            .find(|line| line.split_whitespace().next() == Some(image))
            .unwrap_or_else(|| panic!("`sendit images` doesn't list {image}:\n{images}"))
            .to_string()
    }
}

/// Replaces the value of `key` in the TOML file at `path`.
fn set_toml_value(path: &Path, key: &str, value: &str) {
    let text = fs::read_to_string(path).unwrap();
    let prefix = format!("{key} =");
    assert!(text.lines().any(|line| line.starts_with(&prefix)), "{text}");
    let text: String = text
        .lines()
        .map(|line| match line.starts_with(&prefix) {
            true => format!("{key} = {value}\n"),
            false => format!("{line}\n"),
        })
        .collect();
    fs::write(path, text).unwrap();
}

#[test]
#[ignore = "boots VMs; run with `cargo test --test vm -- --ignored`"]
fn creates_vms_from_the_chosen_image() {
    let _slot = vm_slot();
    let images = env().cache().join("images");
    // Copies of the default image stand in for other images, which would
    // take minutes to provision. `old` is from an earlier base revision.
    for name in ["other", "old"] {
        let dir = images.join(name);
        let _ = fs::remove_dir_all(&dir);
        let status = Command::new("cp")
            .arg("-cR")
            .arg(images.join("default"))
            .arg(&dir)
            .status()
            .unwrap();
        assert!(status.success());
    }
    set_toml_value(&images.join("old/provisioned.toml"), "revision", "0");
    let project = Project::new("images");

    assert_fails_with(
        &project.sendit(&["run", "--image", "old"]),
        "the old image is outdated; rebuild it with `sendit provision old --force`",
    );
    assert!(project.image_line("old").contains("outdated"));

    let vm = project.run(&["--image", "other"]);
    vm.stop();
    // Later runs keep using the image the VM was made from.
    assert!(project.ok(&["status"]).contains("image    other"));
    let line = project.image_line("other");
    assert!(
        line.contains("provisioned") && line.ends_with(" 1"),
        "{line}"
    );
    assert_fails_with(
        &project.sendit(&["run", "--image", "default"]),
        "was created from the other image, not default",
    );

    // A VM from an earlier base revision can't run until it is reset, and
    // `prune --outdated` deletes it.
    let vm_dir = project.vm_dir();
    set_toml_value(&vm_dir.join("project.toml"), "base_revision", "0");
    assert_fails_with(
        &project.sendit(&["run"]),
        "was created from an older base image",
    );
    assert!(project.ok(&["status"]).contains("outdated base image"));
    project.ok(&["prune", "--outdated", "--yes"]);
    assert!(!vm_dir.exists());
    assert!(project.ok(&["status"]).contains("not created"));
}

#[test]
#[ignore = "boots VMs; run with `cargo test --test vm -- --ignored`"]
fn runs_custom_provisioning_scripts() {
    let _slot = vm_slot();
    let env = custom_env();
    let project = Project::in_env(env, "custom-scripts");
    let share = env.home.join("script-share");
    fs::create_dir_all(&share).unwrap();
    fs::write(share.join("shared.txt"), "asked for by a script\n").unwrap();

    let vm = project.run(&["--image", "custom"]);
    // As the guest user from its home, in name order, with the image's own
    // script in place of the shared one of the same name.
    assert_eq!(
        vm.ok("cat ~/provisioned"),
        "10-shared as dev in /home/dev\n20-own"
    );
    // They could use sudo, which is gone afterwards.
    assert_eq!(vm.ok("cat /etc/sendit-test"), "made by root");
    vm.fails("sudo -n true");
    // The mount the script asks for, read-only without :rw; the one whose
    // host directory doesn't exist is left out.
    assert_eq!(
        vm.ok("cat ~/script-share/shared.txt"),
        "asked for by a script"
    );
    vm.fails("touch ~/script-share/shared.txt");
    vm.fails("test -e /mnt/script-missing");
    vm.stop();
    let status = project.sendit(&["status"]);
    assert!(text(&status.stdout).contains("~/script-share -> /home/dev/script-share (ro)"));
    assert!(text(&status.stderr).contains(
        "Not sharing ~/script-missing, which the custom script custom/20-replaced.sh asks for"
    ));

    // The build that failed left nothing behind.
    assert!(project.image_line("broken").contains("missing"));

    // Changing a script marks the image until the script is changed back,
    // unless the change is in the comments at its top.
    let script = env
        .home
        .join(".config/sendit/provision-scripts/10-shared.sh");
    let original = fs::read_to_string(&script).unwrap();
    assert!(project.image_line("custom").contains("provisioned"));
    fs::write(&script, format!("# sendit-mount: /tmp\n{original}")).unwrap();
    assert!(project.image_line("custom").contains("provisioned"));
    fs::write(&script, format!("{original}# changed\n")).unwrap();
    assert!(project.image_line("custom").contains("scripts changed"));
    assert!(project.ok(&["status"]).contains("custom scripts changed"));
    fs::write(&script, original).unwrap();
    assert!(project.image_line("custom").contains("provisioned"));
}

#[test]
#[ignore = "boots VMs; run with `cargo test --test vm -- --ignored`"]
fn reaches_the_host() {
    let _slot = vm_slot();
    let project = Project::new("host");
    // A service on the Mac that listens on more than 127.0.0.1. With the
    // macOS firewall on, it may need allowing.
    let listener = TcpListener::bind("0.0.0.0:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut request = [0; 1024];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(b"HTTP/1.0 200 OK\r\n\r\nhello from the host");
        }
    });

    let vm = project.run(&[]);
    // host.sendit.internal is the default gateway, set once the network is
    // up, which may be after sshd is.
    let host = vm.ok("for _ in $(seq 30); do \
             getent hosts host.sendit.internal && exit; sleep 1; \
         done; exit 1");
    let gateway = vm.ok("ip -4 route show default | awk '{ print $3; exit }'");
    assert_eq!(host.split_whitespace().next(), Some(gateway.as_str()));
    assert_eq!(
        vm.ok(&format!(
            "curl -sSf --max-time 10 http://host.sendit.internal:{port}/"
        )),
        "hello from the host"
    );
    vm.stop();
}

#[test]
#[ignore = "boots VMs; run with `cargo test --test vm -- --ignored`"]
fn refuses_mounts_through_symlinks() {
    let _slot = vm_slot();
    let project = Project::new("symlinks");
    let data = root().join("mounts/symlink-data");
    let _ = fs::remove_dir_all(&data);
    fs::create_dir_all(&data).unwrap();

    // The guest user plants a symlink where a later mount goes, so that
    // mount.sh, which runs as root, would create the mount point and mount
    // the share elsewhere.
    let vm = project.run(&[]);
    vm.ok("ln -s /var/tmp ~/link");
    vm.stop();

    let vm = project.run(&[
        "--mount",
        &format!("{}:/home/dev/link/escaped", data.display()),
    ]);
    vm.root_ok("test ! -e /var/tmp/escaped");
    let journal = vm.root_ok("journalctl -b -u sendit-mounts --no-pager");
    assert!(
        journal.contains("not mounting m0 at /home/dev/link/escaped: it leads through a symlink"),
        "{journal}"
    );
    // The other shares are still there.
    vm.ok("test -d ~/symlinks");
    vm.stop();
}

#[test]
#[ignore = "boots VMs; run with `cargo test --test vm -- --ignored`"]
fn applies_resources_in_the_guest() {
    let _slot = vm_slot();
    let project = Project::new("resources");
    // The disk size from the project's table in the config file, which the
    // other tests sharing this home don't match. Renamed into place, since
    // they read the file meanwhile.
    let config = env().home.join(".config/sendit/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    let partial = config.with_extension("toml.partial");
    fs::write(
        &partial,
        format!(
            "[projects.\"{}\"]\ndisk-size = \"20G\"\n",
            project.dir.display()
        ),
    )
    .unwrap();
    fs::rename(&partial, &config).unwrap();

    // The CPUs and memory from flags, unlike the defaults of 2 and 4 GiB.
    let vm = project.run(&["--cpus", "1", "--memory", "2G"]);
    assert_eq!(vm.ok("nproc"), "1");
    // The kernel keeps some memory for itself.
    let memory: u64 = vm
        .ok("awk '/^MemTotal:/ { print $2 * 1024 }' /proc/meminfo")
        .parse()
        .unwrap();
    assert!(
        (3 << 29..=2 << 30).contains(&memory),
        "the guest has {memory} bytes of memory"
    );
    // The root filesystem fills the 20 GiB disk, apart from the EFI
    // partition and the filesystem's own overhead.
    let root_size: u64 = vm.ok("findmnt -bno SIZE /").parse().unwrap();
    assert!(
        (16 << 30..=20 << 30).contains(&root_size),
        "the root filesystem has {root_size} bytes"
    );
    vm.stop();
}
