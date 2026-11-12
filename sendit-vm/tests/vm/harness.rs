//! The scratch homes, projects and running VMs that the VM tests drive
//! sendit with, and helpers for the processes they start.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc;
use std::sync::{Condvar, Mutex, OnceLock};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

use crate::common::{self, text};

/// What the base images are built from, apart from custom scripts: when any
/// of it changes, the tests provision their images again.
const PROVISION_INPUTS: [&str; 5] = [
    include_str!("../../src/assets/provision.sh"),
    include_str!("../../src/assets/user-data.yaml"),
    include_str!("../../src/provision/mod.rs"),
    include_str!("../../src/provision/scripts.rs"),
    include_str!("../../src/provision/shell.rs"),
];

/// Including downloading Debian and its packages.
const PROVISION_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Until the VM answers over SSH.
pub const BOOT_TIMEOUT: Duration = Duration::from_secs(180);
/// `sendit stop` itself gives up after 45 seconds.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(90);
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(90);
/// For each attempt to reach a booting VM.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// The image files of the `custom` and `broken` images and the shared
/// scripts they call, by path below `~/.config/sendit`. The `custom` image
/// file runs a command of its own between calls, which sees what `shared.sh`
/// added to `~/.profile`, and both it and `tool.sh` call `shared.sh`, which
/// runs once. It asks for a mount, `tool.sh` for one that doesn't exist.
/// Nothing calls `unused.sh`. In the `broken` image, `fail.sh` stops at a
/// pipeline whose first command fails, `careless.sh` carries on after it
/// failed, and the image file after calling it again, then ends while
/// `slow.sh` still runs in the background.
const CUSTOM_SCRIPTS: [(&str, &str); 8] = [
    (
        "images/custom.sh",
        "# sendit-mount: ~/script-share:/home/dev/script-share\n\
         sendit-provision shared.sh\n\
         echo \"inline as $(id -un) in $PWD, profile ${SHARED_PROFILE-unseen}\" >> ~/provisioned\n\
         sendit-provision tool.sh\n\
         sudo sh -c 'echo made by root > /etc/sendit-test'\n",
    ),
    (
        "provision-scripts/shared.sh",
        "echo \"shared as $(id -un) in $PWD\" >> ~/provisioned\n\
         echo 'export SHARED_PROFILE=seen' >> ~/.profile\n\
         echo '[ -e ~/nothing ] && . ~/nothing' >> ~/.profile\n",
    ),
    (
        "provision-scripts/tool.sh",
        "# sendit-mount: ~/script-missing\n\
         sendit-provision shared.sh\n\
         echo tool >> ~/provisioned\n",
    ),
    (
        "provision-scripts/unused.sh",
        "echo unused >> ~/provisioned\n",
    ),
    (
        "images/broken.sh",
        "sendit-provision careless.sh\n\
         if sendit-provision fail.sh; then echo fail.sh passed; \
         else echo \"fail.sh failed again: $?\"; fi\n\
         sendit-provision slow.sh &\n\
         sleep 3\n",
    ),
    (
        "provision-scripts/careless.sh",
        "sendit-provision fail.sh || echo careless.sh carried on\n",
    ),
    (
        "provision-scripts/fail.sh",
        "sh -c 'exit 3' | cat\necho fail.sh \"went on\"\n",
    ),
    ("provision-scripts/slow.sh", "sleep 30\n"),
];

/// Where the tests keep their files.
pub fn root() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join("vm-tests")
}

/// A scratch home for sendit.
pub struct Env {
    pub home: PathBuf,
}

impl Env {
    pub fn sendit(&self) -> Command {
        common::sendit(&self.home)
    }

    pub fn cache(&self) -> PathBuf {
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
pub fn env() -> &'static Env {
    static ENV: OnceLock<Result<Env, String>> = OnceLock::new();
    fixture(&ENV, || {
        setup("home", &[], &["default"], |env| env.provision_ok("default"))
    })
}

/// A scratch home of its own with `CUSTOM_SCRIPTS`, and the `custom` image
/// built with them. Building it also checks that a failing script fails the
/// build of the `broken` image, although the scripts that call it carry on.
pub fn custom_env() -> &'static Env {
    static ENV: OnceLock<Result<Env, String>> = OnceLock::new();
    fixture(&ENV, || {
        setup("custom-home", &CUSTOM_SCRIPTS, &["custom"], |env| {
            let status = env.provision("broken")?;
            ensure!(
                !status.success(),
                "a failing custom script didn't fail the build"
            );
            ensure!(
                !env.cache().join("images/broken").exists()
                    && !env.cache().join("images/.broken.partial").exists(),
                "the failed build left an image behind"
            );
            let log = env.cache().join("provision/broken/console.log");
            let log =
                fs::read_to_string(&log).with_context(|| format!("reading {}", log.display()))?;
            ensure!(
                log.contains("sendit: custom script fail.sh failed with exit status 3")
                    && !log.contains("fail.sh went on")
                    && log.contains("careless.sh carried on")
                    && log.contains(
                        "sendit: custom script fail.sh already ran and failed with exit status 3"
                    )
                    && log.contains("fail.sh failed again: 3")
                    && log.contains(
                        "sendit: custom scripts failed: fail.sh slow.sh (never finished)"
                    ),
                "the console log doesn't say which custom script failed"
            );
            env.provision_ok("custom")?;
            let log = env.cache().join("provision/custom/console.log");
            let log =
                fs::read_to_string(&log).with_context(|| format!("reading {}", log.display()))?;
            ensure!(
                log.contains("sendit: custom script shared.sh already ran"),
                "the console log doesn't say that a script ran already"
            );
            Ok(())
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

/// Sets up the scratch home `name` with the image files and custom
/// provisioning `scripts` (by path below `~/.config/sendit`) and runs
/// `build` in it to make `images`, unless an earlier run already did so from
/// the same inputs.
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
    let config_dir = env.home.join(".config/sendit");
    for dir in ["images", "provision-scripts"] {
        let _ = fs::remove_dir_all(config_dir.join(dir));
    }
    for (path, content) in scripts {
        let path = config_dir.join(path);
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
pub fn vm_slot() -> VmSlot {
    let mut running = VM_SLOTS.0.lock().unwrap_or_else(|e| e.into_inner());
    while *running >= MAX_VMS {
        running = VM_SLOTS.1.wait(running).unwrap_or_else(|e| e.into_inner());
    }
    *running += 1;
    VmSlot
}

/// The number of tests holding a `VmSlot`, and the signal that one ended.
static VM_SLOTS: (Mutex<usize>, Condvar) = (Mutex::new(0), Condvar::new());

pub struct VmSlot;

impl Drop for VmSlot {
    fn drop(&mut self) {
        *VM_SLOTS.0.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
        VM_SLOTS.1.notify_one();
    }
}

/// A project directory, emptied and without a VM to start with.
pub struct Project {
    pub env: &'static Env,
    name: String,
    pub dir: PathBuf,
    runs: std::cell::Cell<usize>,
}

impl Project {
    /// A project in the scratch home with the `default` image.
    pub fn new(name: &str) -> Self {
        Self::in_env(env(), name)
    }

    pub fn in_env(env: &'static Env, name: &str) -> Self {
        let projects = root().join("projects");
        // An empty `.git` ends the search for a repository here, so that
        // the projects aren't in a linked worktree when the tests run in
        // one, e.g. in `send_it.worktrees/x`: that would change their VMs'
        // names.
        fs::create_dir_all(projects.join(".git")).unwrap();
        let dir = projects.join(name);
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

    pub fn command(&self, args: &[&str]) -> Command {
        let mut cmd = self.env.sendit();
        cmd.arg("-C").arg(&self.dir).args(args);
        cmd
    }

    /// Runs `sendit <args>` for this project.
    pub fn sendit(&self, args: &[&str]) -> Output {
        output(self.command(args), COMMAND_TIMEOUT)
            .unwrap_or_else(|e| panic!("sendit {}: {e:#}", args.join(" ")))
    }

    /// Runs `sendit <args>`, which must succeed, and returns its stdout.
    pub fn ok(&self, args: &[&str]) -> String {
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
    pub fn next_log(&self) -> PathBuf {
        let logs = root().join("logs");
        fs::create_dir_all(&logs).unwrap();
        self.runs.set(self.runs.get() + 1);
        logs.join(format!("{}-{}.log", self.name, self.runs.get()))
    }

    /// Starts `sendit run <args>` and waits until the VM answers over SSH.
    pub fn run(&self, args: &[&str]) -> Vm<'_> {
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

    /// The directory of the project's VM, which must exist.
    pub fn vm_dir(&self) -> PathBuf {
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
    pub fn image_line(&self, image: &str) -> String {
        let images = self.ok(&["images"]);
        images
            .lines()
            .find(|line| line.split_whitespace().next() == Some(image))
            .unwrap_or_else(|| panic!("`sendit images` doesn't list {image}:\n{images}"))
            .to_string()
    }
}

/// A running `sendit run`. Dropping it shuts the VM down.
pub struct Vm<'a> {
    project: &'a Project,
    pub child: Child,
    /// Kept open for typing into the console.
    stdin: Option<ChildStdin>,
    pub log: PathBuf,
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
    pub fn ok(&self, script: &str) -> String {
        self.expect_ok(false, script)
    }

    /// Like `ok`, as root.
    pub fn root_ok(&self, script: &str) -> String {
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
    pub fn fails(&self, script: &str) {
        let output = self.ssh(false, script);
        assert!(
            !output.status.success(),
            "`{script}` succeeded:\n{}",
            text(&output.stdout)
        );
    }

    /// Shuts the VM down with `sendit stop`.
    pub fn stop(mut self) {
        self.project.ok(&["stop"]);
        self.wait_for_exit();
    }

    /// Shuts the VM down by logging out of its console.
    pub fn exit_console(mut self) {
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

/// Runs `cmd` with no input and returns its output, killing it after
/// `timeout`.
pub fn output(mut cmd: Command, timeout: Duration) -> Result<Output> {
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
pub fn wait_timeout(child: &mut Child, timeout: Duration) -> Result<ExitStatus> {
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
pub fn terminate(child: &mut Child) {
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
