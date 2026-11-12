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
//! home of its own, `custom-home`, where it builds an image from an image
//! file and the scripts it calls, and one whose image file calls a script
//! that fails. Provisioning reuses the Debian
//! image in the real `~/.cache/sendit/downloads` if there is one, and
//! downloads it otherwise.
//!
//! Most tests run `sendit run` with plain pipes and work in the VM over
//! `sendit ssh`; the console tests run it on a pseudo-terminal and type into
//! it. Output of `sendit run` goes to `target/tmp/vm-tests/logs/`.
//!
//! The scratch homes, projects and running VMs are in `harness`, the
//! pseudo-terminal of the console tests in `terminal`.

#[path = "../common/mod.rs"]
mod common;
mod harness;
mod terminal;

use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use common::{assert_fails_with, text};
use harness::{
    BOOT_TIMEOUT, COMMAND_TIMEOUT, Project, STOP_TIMEOUT, custom_env, env, output, root, vm_slot,
    wait_timeout,
};
use terminal::{CONSOLE_TIMEOUT, LEAVE_ALTERNATE_SCREEN, TERMINAL_RESET};

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
    assert_eq!(vm.ok("getconf PAGESIZE"), "4096");
    assert_eq!(vm.ok("id -un"), "dev");
    assert_eq!(vm.root_ok("id -u"), "0");
    // Through a login shell, which starts in the project.
    assert_eq!(vm.ok("pwd"), "/home/dev/lifecycle");
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
    // With a command, the way to run it there.
    let second = project.sendit(&["run", "echo", "a b"]);
    assert_fails_with(&second, "ssh echo 'a b'` runs the command there");

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
fn runs_a_command_instead_of_the_console() {
    let _slot = vm_slot();
    let project = Project::new("command");
    fs::write(project.dir.join("hello.txt"), "from the host\n").unwrap();
    let run = project.command(&[
        "run",
        "sh",
        "-c",
        "cat hello.txt; echo to stderr >&2; exit 3",
    ]);
    let output = output(run, BOOT_TIMEOUT + STOP_TIMEOUT).unwrap();
    let stderr = text(&output.stderr);
    // Only the command's output: the console isn't shown.
    assert_eq!(text(&output.stdout), "from the host\n", "{stderr}");
    assert!(stderr.contains("to stderr"), "{stderr}");
    assert_eq!(output.status.code(), Some(3), "{stderr}");
    // The VM shut down when the command ended.
    assert!(project.ok(&["status"]).contains("stopped"));

    // Stopped meanwhile with `sendit stop` (SIGTERM) or Ctrl-C (SIGINT),
    // the guest shuts down cleanly: its shutdown ends the command, which
    // doesn't count as asking twice. sendit exits as a shell does for a
    // command killed by the signal.
    for (signal, code) in [(libc::SIGTERM, 143), (libc::SIGINT, 130)] {
        let started = format!("/home/dev/started-{signal}");
        let mut vm = project.run(&["sh", "-c", &format!("touch {started}; sleep 600")]);
        // `sendit ssh` may answer before `run` has started the command.
        let start = Instant::now();
        while !project
            .sendit(&["ssh", "test", "-e", &started])
            .status
            .success()
        {
            assert!(
                start.elapsed() < COMMAND_TIMEOUT,
                "the command didn't start"
            );
            sleep(Duration::from_secs(1));
        }
        if signal == libc::SIGTERM {
            project.ok(&["stop"]);
        } else {
            // SAFETY: plain syscall; the child hasn't been waited for, so
            // its PID is still its own.
            unsafe { libc::kill(vm.child.id() as libc::pid_t, signal) };
        }
        let status = wait_timeout(&mut vm.child, STOP_TIMEOUT).unwrap();
        let log = fs::read_to_string(&vm.log).unwrap();
        assert!(!log.contains("Forcing") && !log.contains("Error"), "{log}");
        assert_eq!(status.code(), Some(code), "{log}");
    }
}

#[test]
#[ignore = "boots VMs; run with `cargo test --test vm -- --ignored`"]
fn runs_in_the_background() {
    let _slot = vm_slot();
    let project = Project::new("detach");
    let started = output(project.command(&["run", "--detach"]), BOOT_TIMEOUT).unwrap();
    let stderr = text(&started.stderr);
    assert!(started.status.success(), "{stderr}");
    assert!(stderr.contains("runs in the background"), "{stderr}");
    // Not run in the project directory, sendit names the VM to reach it,
    // which works from anywhere.
    assert!(
        stderr.contains("`sendit -n detach ssh` opens a shell in it"),
        "{stderr}"
    );
    let by_name = |args: &[&str]| {
        let mut cmd = project.env.sendit();
        cmd.args(["-n", "detach"]).args(args);
        output(cmd, COMMAND_TIMEOUT).unwrap()
    };
    // sendit returned once the VM answered.
    assert_eq!(text(&by_name(&["ssh", "echo", "up"]).stdout), "up\n");
    assert!(project.ok(&["status"]).contains("running"));
    // Its process leads a session of its own, without a terminal to hang
    // it up.
    let vm = project.vm_dir();
    let lock = vm.with_file_name(format!(
        ".{}.lock",
        vm.file_name().unwrap().to_str().unwrap()
    ));
    let pid: libc::pid_t = fs::read_to_string(&lock).unwrap().trim().parse().unwrap();
    // SAFETY: plain syscall.
    assert_eq!(unsafe { libc::getsid(pid) }, pid);
    assert_fails_with(&project.sendit(&["run", "-d"]), "already running");

    let stopped = by_name(&["stop"]);
    assert!(stopped.status.success(), "{}", text(&stopped.stderr));
    assert!(project.ok(&["status"]).contains("stopped"));
    let log = text(&fs::read(project.vm_dir().join("run/console.log")).unwrap());
    // The login console's output, and a clean shutdown.
    assert!(log.contains("dev@sendit"), "{log}");
    assert!(log.contains("Shutting down."), "{log}");
    assert!(!log.contains("Forcing") && !log.contains("Error"), "{log}");

    // Ctrl-C before the VM answers shuts it down again, and sendit exits
    // as a shell reports a command killed by it.
    let log = project.next_log();
    let file = File::create(&log).unwrap();
    let mut child = project
        .command(&["run", "-d"])
        .stdin(Stdio::null())
        .stdout(file.try_clone().unwrap())
        .stderr(file)
        .spawn()
        .unwrap();
    // Once the process in the background has the lock, it is about to start
    // the VM. It may not catch signals yet, and end at once on the SIGTERM
    // it is passed.
    let last_pid = pid.to_string();
    let start = Instant::now();
    while fs::read_to_string(&lock).is_ok_and(|pid| pid.is_empty() || pid == last_pid) {
        assert!(
            start.elapsed() < COMMAND_TIMEOUT,
            "the VM didn't start; see {}",
            log.display()
        );
        sleep(Duration::from_millis(100));
    }
    // SAFETY: plain syscall; the child hasn't been waited for, so its PID
    // is still its own.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
    let status = wait_timeout(&mut child, STOP_TIMEOUT).unwrap();
    let output = fs::read_to_string(&log).unwrap();
    assert_eq!(status.code(), Some(130), "{output}");
    assert!(output.contains("Shutting down the VM"), "{output}");
    assert!(project.ok(&["status"]).contains("stopped"));
}

#[test]
#[ignore = "boots VMs; run with `cargo test --test vm -- --ignored`"]
fn starts_worktree_vms_as_copies() {
    let _slot = vm_slot();
    let project = Project::new("trunk");
    let worktrees = project.dir.with_extension("worktrees");
    let worktree = worktrees.join("feature-x");
    let in_worktree = |args: &[&str]| {
        let mut cmd = project.env.sendit();
        cmd.arg("-C").arg(&worktree).args(args);
        output(cmd, COMMAND_TIMEOUT).unwrap()
    };
    // The worktree of an earlier run, and its VM.
    fs::create_dir_all(&worktree).unwrap();
    in_worktree(&["stop"]);
    in_worktree(&["reset", "--yes"]);
    fs::remove_dir_all(&worktrees).unwrap();

    let git = |args: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(&project.dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
    };
    git(&["init", "-q"]);
    git(&["commit", "-q", "--allow-empty", "-m", "first"]);

    // Something installed in the main worktree's VM.
    let install = project.command(&["run", "sh", "-c", "echo installed > ~/tool"]);
    let installed = output(install, BOOT_TIMEOUT + STOP_TIMEOUT).unwrap();
    assert!(installed.status.success(), "{}", text(&installed.stderr));

    // Not copied while the main worktree's VM runs, unless confirmed.
    let vm = project.run(&[]);
    let refused = output(
        project.command(&["run", "--worktree", "feature/x", "true"]),
        COMMAND_TIMEOUT,
    )
    .unwrap();
    assert_fails_with(&refused, "is running");
    vm.stop();
    let status = text(&in_worktree(&["status"]).stdout);
    assert!(status.contains(" run` copies the VM of"), "{status}");

    let run = project.command(&[
        "run",
        "--worktree",
        "feature/x",
        "sh",
        "-c",
        "cat ~/tool; pwd; wc -c < .git; test -e ~/trunk || echo no trunk",
    ]);
    let output = output(run, BOOT_TIMEOUT + STOP_TIMEOUT).unwrap();
    let stderr = text(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    // Only the command's output, none of git's. The worktree's .git file
    // is hidden like a .git directory, and the main worktree's mount point
    // is gone.
    assert_eq!(
        text(&output.stdout),
        "installed\n/home/dev/feature-x\n0\nno trunk\n",
        "{stderr}"
    );
    let status = text(&in_worktree(&["status"]).stdout);
    assert!(status.contains("copy of  the VM of"), "{status}");
    // Named after the main worktree's VM and the worktree's folder.
    assert!(status.contains("name     trunk-feature-x\n"), "{status}");
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
    // The mount points of the dropped mounts are gone.
    vm.fails("test -e /mnt/ro-data || test -e ~/rw");
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

#[test]
#[ignore = "boots VMs; run with `cargo test --test vm -- --ignored`"]
fn restricts_the_network() {
    let _slot = vm_slot();
    let project = Project::new("network");
    let port = serve_on_host();
    let host = format!("curl -sS --max-time 10 http://host.sendit.internal:{port}/");
    // curl's exit code when it can't connect, e.g. because the connection
    // was reset: it fails at once instead of timing out.
    let refused = |what: &str| format!("{what}; test $? -eq 7");

    // By default, the internet, with DNS from the Mac, but not the Mac.
    let vm = project.run(&[]);
    vm.ok("getent hosts deb.debian.org");
    vm.ok("curl -sS -o /dev/null --max-time 20 https://deb.debian.org/");
    vm.ok(&refused(&host));
    vm.ok(&refused("curl -sS --max-time 10 http://192.168.1.1/"));
    assert!(
        vm.root_ok("journalctl -k -b")
            .contains("sendit-network: blocked")
    );
    assert!(project.ok(&["status"]).contains("network  internet ("));
    vm.stop();

    // Nothing but what --network-allow lets through, which lacks DNS.
    let vm = project.run(&[
        "--network",
        "none",
        "--network-allow",
        &format!("host:{port}"),
    ]);
    assert_eq!(vm.ok(&host), "hello from the host");
    vm.fails("getent hosts deb.debian.org");
    vm.ok(&refused("curl -sS --max-time 10 http://1.1.1.1/"));
    // status shows the rules the VM runs with, and the configured ones.
    let status = project.ok(&["status"]);
    assert!(
        status.contains("network  none (nothing; also allows host:"),
        "{status}"
    );
    assert!(status.contains("configured: internet ("), "{status}");
    vm.stop();
    assert!(!project.ok(&["status"]).contains("configured:"));

    // Everything.
    let vm = project.run(&["--network", "open"]);
    assert_eq!(vm.ok(&host), "hello from the host");
    vm.stop();
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
        "the old image is outdated; rebuild it with `sendit provision old`",
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
    // As the guest user from its home, in the order the image file calls
    // them, each script once, and only those it calls.
    assert_eq!(
        vm.ok("cat ~/provisioned"),
        "shared as dev in /home/dev\ninline as dev in /home/dev, profile seen\ntool"
    );
    // The helper is gone with the sudo rights.
    vm.fails("command -v sendit-provision");
    // They could use sudo, which is gone afterwards.
    assert_eq!(vm.ok("cat /etc/sendit-test"), "made by root");
    vm.fails("sudo -n true");
    // The mount the image file asks for, read-only without :rw; the one
    // whose host directory doesn't exist, from a script it calls, is left
    // out.
    assert_eq!(
        vm.ok("cat ~/script-share/shared.txt"),
        "asked for by a script"
    );
    vm.fails("touch ~/script-share/shared.txt");
    vm.fails("test -e /mnt/script-missing");
    vm.stop();
    let status = project.sendit(&["status"]);
    assert!(text(&status.stdout).contains("~/script-share -> /home/dev/script-share (ro)"));
    assert!(
        text(&status.stderr)
            .contains("Not sharing ~/script-missing, which the custom script tool.sh asks for")
    );

    // The build that failed left nothing behind.
    assert!(project.image_line("broken").contains("missing"));

    // Changing a script that runs marks the image until the script is
    // changed back, unless the change is in the comments at its top. A
    // mount there counts: it takes effect with the next build.
    let scripts = env.home.join(".config/sendit/provision-scripts");
    let script = scripts.join("shared.sh");
    let original = fs::read_to_string(&script).unwrap();
    assert!(project.image_line("custom").contains("provisioned"));
    fs::write(&script, format!("# Shared.\n{original}")).unwrap();
    assert!(project.image_line("custom").contains("provisioned"));
    fs::write(&script, format!("# sendit-mount: /tmp\n{original}")).unwrap();
    assert!(project.image_line("custom").contains("scripts changed"));
    // Until then, the VMs share what the build asked for.
    let status = project.ok(&["status"]);
    assert!(
        status.contains("~/script-share -> /home/dev/script-share (ro)")
            && !status.contains("-> /mnt/tmp"),
        "{status}"
    );
    fs::write(&script, &original).unwrap();
    fs::write(scripts.join("unused.sh"), "echo changed\n").unwrap();
    assert!(project.image_line("custom").contains("provisioned"));
    fs::write(&script, format!("{original}# changed\n")).unwrap();
    assert!(project.image_line("custom").contains("scripts changed"));
    assert!(project.ok(&["status"]).contains("custom scripts changed"));
    fs::write(&script, original).unwrap();
    assert!(project.image_line("custom").contains("provisioned"));
}

/// Serves "hello from the host" over HTTP on a port of the Mac that listens
/// on more than 127.0.0.1, and returns the port. With the macOS firewall on,
/// it may need allowing.
fn serve_on_host() -> u16 {
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
    port
}

#[test]
#[ignore = "boots VMs; run with `cargo test --test vm -- --ignored`"]
fn reaches_the_host() {
    let _slot = vm_slot();
    let project = Project::new("host");
    let port = serve_on_host();

    // By default, the Mac is out of reach; this lets its port through.
    let vm = project.run(&["--network-allow", &format!("host:{port}")]);
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
    // Half of it is swap on zram, compressed with zstd, which may keep at
    // most a quarter of it. The generator truncates MemTotal to whole MiB
    // before taking the fractions, and the swap area's header takes a page.
    assert_eq!(vm.ok("awk 'NR > 1 { print $1 }' /proc/swaps"), "/dev/zram0");
    assert!(
        vm.ok("cat /sys/block/zram0/comp_algorithm")
            .contains("[zstd]")
    );
    let near = |actual: u64, wanted: u64| (wanted - (1 << 20)..=wanted).contains(&actual);
    let swap: u64 = vm
        .ok("awk '/^SwapTotal:/ { print $2 * 1024 }' /proc/meminfo")
        .parse()
        .unwrap();
    assert!(near(swap, memory / 2), "the guest has {swap} bytes of swap");
    let limit: u64 = vm
        .ok("awk '{ print $4 }' /sys/block/zram0/mm_stat")
        .parse()
        .unwrap();
    assert!(
        near(limit, memory / 4),
        "zram may keep {limit} bytes of memory"
    );
    // Swapping out as readily as dropping the page cache, one page at a time.
    assert_eq!(
        vm.ok("cat /proc/sys/vm/swappiness /proc/sys/vm/page-cluster"),
        "100\n0"
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
