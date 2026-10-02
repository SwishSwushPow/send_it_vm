//! Tests that drive the sendit binary without booting VMs, so they run in
//! CI: what it says in a home without images or VMs, how it reports bad
//! configuration, which shares it refuses, and how it signs itself.
//!
//! Each test gets a scratch home and project directory of its own in
//! `target/tmp/cli-tests/<test>/`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const SENDIT: &str = env!("CARGO_BIN_EXE_sendit");

/// A scratch home and an empty project directory.
struct Env {
    home: PathBuf,
    project: PathBuf,
}

impl Env {
    fn new(test: &str) -> Self {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("cli-tests")
            .join(test);
        let _ = fs::remove_dir_all(&root);
        let env = Self {
            home: root.join("home"),
            project: root.join("project"),
        };
        fs::create_dir_all(&env.home).unwrap();
        fs::create_dir_all(&env.project).unwrap();
        env
    }

    /// Runs `program <args>` for the project, with no input.
    fn run(&self, program: &Path, args: &[&str]) -> Output {
        Command::new(program)
            .env("HOME", &self.home)
            .arg("-C")
            .arg(&self.project)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// Runs `sendit <args>` for the project.
    fn sendit(&self, args: &[&str]) -> Output {
        self.run(Path::new(SENDIT), args)
    }

    /// Runs `sendit <args>`, which must succeed, and returns its stdout and
    /// stderr.
    fn ok(&self, args: &[&str]) -> (String, String) {
        let output = self.sendit(args);
        assert!(
            output.status.success(),
            "sendit {} failed with {}:\n{}",
            args.join(" "),
            output.status,
            text(&output.stderr)
        );
        (text(&output.stdout), text(&output.stderr))
    }

    /// Writes the config file.
    fn config(&self, toml: &str) {
        let dir = self.home.join(".config/sendit");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.toml"), toml).unwrap();
    }

    /// The project directory as sendit shows it, with symlinks resolved.
    fn project_path(&self) -> String {
        self.project.canonicalize().unwrap().display().to_string()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Fails unless `output` failed with `message` in its stderr.
fn assert_fails_with(output: &Output, message: &str) {
    let stderr = text(&output.stderr);
    assert!(
        !output.status.success() && stderr.contains(message),
        "expected an error saying {message:?}, got {}:\n{stderr}",
        output.status
    );
}

#[test]
fn explains_what_to_do_in_a_fresh_home() {
    let env = Env::new("fresh-home");

    assert_fails_with(
        &env.sendit(&["run"]),
        "the default image isn't provisioned yet; run `sendit provision` first",
    );
    assert_fails_with(
        &env.sendit(&["run", "--image", "rust"]),
        "the rust image isn't provisioned yet; run `sendit provision rust` first",
    );
    // Failing to start left nothing behind.
    assert_eq!(fs::read_dir(env.home.join(".sendit")).unwrap().count(), 0);

    let (status, _) = env.ok(&["status"]);
    let lines: Vec<_> = status.lines().collect();
    assert_eq!(
        lines[0],
        "image    default in ~/.cache/sendit/images/default (run `sendit provision`)"
    );
    assert_eq!(lines[1], format!("project  {}", env.project_path()));
    assert!(
        lines[2].starts_with("vm       ~/.sendit/project_"),
        "{status}"
    );
    assert!(lines[2].ends_with(" (not created)"), "{status}");
    assert_eq!(
        lines[3..7],
        [
            "cpus     2",
            "memory   4 GiB",
            "disk     64 GiB max",
            ".git     hidden"
        ]
    );
    assert_eq!(
        lines[7],
        format!("mounts   {} -> /home/dev/project (rw)", env.project_path())
    );
    assert_eq!(lines.len(), 8, "{status}");
}

#[test]
fn has_nothing_to_list_or_remove_without_vms() {
    let env = Env::new("empty-home");

    assert_eq!(
        env.ok(&["list"]),
        (String::new(), "No VMs in ~/.sendit.\n".into())
    );
    let (images, _) = env.ok(&["images"]);
    let rows: Vec<Vec<_>> = images
        .lines()
        .map(|line| line.split_whitespace().collect())
        .collect();
    assert_eq!(
        rows,
        [
            vec!["IMAGE", "STATE", "DISK", "VMS"],
            vec!["default", "missing", "-", "0"]
        ]
    );
    // Images named in the config file are listed too.
    env.config("image = \"rust\"\n");
    let (images, _) = env.ok(&["images"]);
    assert!(
        images.lines().any(|line| line.starts_with("rust ")),
        "{images}"
    );

    // Without a terminal and without --yes, these would fail if they had
    // anything to ask about.
    assert_eq!(env.ok(&["reset"]).1, "This project has no VM.\n");
    assert_eq!(env.ok(&["stop"]).1, "The VM is not running.\n");
    assert_eq!(env.ok(&["prune"]).1, "No VMs of missing projects.\n");
    assert_eq!(env.ok(&["prune", "--all"]).1, "No stopped VMs.\n");
    assert_fails_with(
        &env.sendit(&["ssh", "--", "true"]),
        "the VM is not running; start it with `sendit run`",
    );
}

#[test]
fn reports_bad_configuration() {
    let env = Env::new("config-errors");
    let status_fails_with = |config: &str, message: &str| {
        env.config(config);
        assert_fails_with(&env.sendit(&["status"]), message);
    };

    status_fails_with(
        "cpus = \"two\"\n",
        "invalid type: string \"two\", expected u32",
    );
    status_fails_with("colour = true\n", "unknown field `colour`");
    status_fails_with("memory = \"4\"\n", "invalid size \"4\"");
    // More than any Mac has.
    status_fails_with("cpus = 1000\n", "cpus must be between 1 and ");
    status_fails_with(
        "memory = \"100M\"\n",
        "memory must be at least 512 MiB, got 100 MiB",
    );
    status_fails_with(
        "disk-size = \"4G\"\n",
        "disk-size must be at least 8 GiB, got 4 GiB",
    );
    status_fails_with(
        "mounts = [\"relative/dir\"]\n",
        "mount relative/dir: host path in the config file must be absolute or start with ~",
    );
    status_fails_with(
        "mounts = [\"/tmp:/home/dev/project\"]\n",
        "both use guest path /home/dev/project",
    );
    status_fails_with(
        "mounts = [\"/tmp:relative\"]\n",
        "guest path relative must be an absolute path below /",
    );
    status_fails_with("[provision.Bad]\n", "invalid image name \"Bad\"");
    status_fails_with(
        &format!(
            "[projects.\"{}\"]\ncpus = 1\n[projects.\"{}/../project\"]\ncpus = 1\n",
            env.project.display(),
            env.project.display()
        ),
        "both configure",
    );

    // The project's table applies on top of the top-level keys.
    env.config(&format!(
        "cpus = 3\nmemory = \"1G\"\n[projects.\"{}\"]\ncpus = 1\n",
        env.project.display()
    ));
    let (status, _) = env.ok(&["status"]);
    assert!(status.contains("cpus     1\nmemory   1 GiB\n"), "{status}");
}

#[test]
fn rejects_bad_arguments() {
    let env = Env::new("arguments");
    for args in [
        &["run", "--memory", "8"][..],
        &["run", "--mount", "/a:/b:/c"],
        &["run", "--image", "Rust"],
        &["provision", "rust", "--all"],
        &["no-such-command"],
    ] {
        let output = env.sendit(args);
        // clap's usage errors.
        assert_eq!(output.status.code(), Some(2), "sendit {}", args.join(" "));
    }
    assert_fails_with(
        &env.sendit(&["run", "--cpus", "0"]),
        "cpus must be between 1 and ",
    );
    // Relative mounts on the command line are relative to where it runs.
    let output = Command::new(SENDIT)
        .env("HOME", &env.home)
        .current_dir(&env.project)
        .args(["run", "--mount", "does-not-exist"])
        .output()
        .unwrap();
    assert_fails_with(
        &output,
        &format!("mount {}/does-not-exist", env.project_path()),
    );
    let output = Command::new(SENDIT)
        .env("HOME", &env.home)
        .args(["-C", "/does/not/exist", "status"])
        .output()
        .unwrap();
    assert_fails_with(&output, "project directory /does/not/exist");
}

#[test]
fn asks_before_risky_shares() {
    let env = Env::new("risky-shares");
    let scripts = env.home.join(".config/sendit/provision-scripts");
    fs::create_dir_all(&scripts).unwrap();
    let home = env.home.canonicalize().unwrap();

    // Without a terminal to ask on, sendit refuses before it looks for the
    // image, let alone boots anything.
    for (mount, reason) in [
        (home.clone(), "(read-only) contains your home directory"),
        (
            home.join(".config"),
            "(read-only) contains sendit's configuration (~/.config/sendit)",
        ),
        (
            scripts.canonicalize().unwrap(),
            "(read-write) is inside sendit's configuration (~/.config/sendit)",
        ),
    ] {
        let spec = match reason.contains("read-write") {
            true => format!("{}:/mnt/x:rw", mount.display()),
            false => mount.display().to_string(),
        };
        let output = env.sendit(&["run", "--mount", &spec]);
        assert_fails_with(
            &output,
            "not sharing these without a terminal to confirm it:",
        );
        assert_fails_with(&output, reason);
    }
    // The same for shares from the config file.
    env.config("mounts = [\"~\"]\n");
    assert_fails_with(
        &env.sendit(&["run"]),
        "~ (read-only) contains your home directory",
    );

    // An ordinary share gets as far as the missing image.
    env.config("");
    let data = env.project.parent().unwrap().join("data");
    fs::create_dir_all(&data).unwrap();
    assert_fails_with(
        &env.sendit(&["run", "--mount", data.to_str().unwrap()]),
        "isn't provisioned yet",
    );
}

/// The entitlements `binary` is signed with, as XML.
fn entitlements(binary: &Path) -> String {
    let output = Command::new("codesign")
        .args(["--display", "--entitlements", "-", "--xml"])
        .arg(binary)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", text(&output.stderr));
    text(&output.stdout)
}

#[test]
fn signs_itself_with_the_virtualization_entitlement() {
    const ENTITLEMENT: &str = "com.apple.security.virtualization";
    let env = Env::new("self-signing");
    // As `cargo install` from crates.io leaves it: signed by the linker,
    // without entitlements. A fresh file, since the kernel keeps the
    // signature of an executable it has run.
    let dir = env.home.parent().unwrap().join("bin");
    fs::create_dir_all(&dir).unwrap();
    let binary = dir.join("sendit");
    fs::copy(SENDIT, &binary).unwrap();
    let status = Command::new("codesign")
        .args(["--sign", "-", "--force"])
        .arg(&binary)
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    assert!(!entitlements(&binary).contains(ENTITLEMENT));

    // A signature that didn't take is an error, not a loop.
    let output = Command::new(&binary)
        .env("HOME", &env.home)
        .env("SENDIT_SIGNED", "1")
        .arg("list")
        .output()
        .unwrap();
    assert_fails_with(
        &output,
        "is signed but still lacks the virtualization entitlement",
    );

    let output = env.run(&binary, &["list"]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let stderr = text(&output.stderr);
    assert!(
        stderr.starts_with("sendit: signing ")
            && stderr.contains("with the virtualization entitlement"),
        "{stderr}"
    );
    // The command ran after signing, with the same arguments.
    assert!(stderr.ends_with("No VMs in ~/.sendit.\n"), "{stderr}");
    assert!(entitlements(&binary).contains(ENTITLEMENT));
    // No copy left next to it.
    let files: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(files, ["sendit"]);

    // Signed once and for all.
    let output = env.run(&binary, &["list"]);
    assert!(output.status.success());
    assert_eq!(text(&output.stderr), "No VMs in ~/.sendit.\n");
}
