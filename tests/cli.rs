//! Tests that drive the sendit binary without booting VMs, so they run in
//! CI: what it says in a home without images or VMs, how it lists them
//! (made up for the test), how it reports bad configuration, which shares
//! it refuses, and how it signs itself.
//!
//! Each test gets a scratch home and project directory of its own in
//! `target/tmp/cli-tests/<test>/`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use common::{SENDIT, assert_fails_with, text};

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
        common::command(program, &self.home)
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

#[test]
fn explains_what_to_do_in_a_fresh_home() {
    let env = Env::new("fresh-home");

    assert_fails_with(
        &env.sendit(&["run"]),
        "the default image is not provisioned yet; run `sendit provision`",
    );
    assert_fails_with(
        &env.sendit(&["run", "--image", "rust"]),
        "the rust image is not provisioned yet; run `sendit provision rust`",
    );
    // Failing to start left nothing behind.
    assert_eq!(fs::read_dir(env.home.join(".sendit")).unwrap().count(), 0);

    let (status, _) = env.ok(&["status"]);
    let lines: Vec<_> = status.lines().collect();
    assert_eq!(
        lines[0],
        "image    default in ~/.cache/sendit/images/default (not provisioned yet; run `sendit provision`)"
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
fn tells_when_the_vm_is_from_an_earlier_build() {
    let env = Env::new("earlier-build");
    let build = |built: u64| {
        let image = env.home.join(".cache/sendit/images/default");
        fs::create_dir_all(&image).unwrap();
        fs::write(
            image.join("provisioned.toml"),
            format!("revision = 1000\ncustom_scripts = []\nbuilt = {built}\n"),
        )
        .unwrap();
    };
    let vm_state = || {
        let (status, _) = env.ok(&["status"]);
        let line = status.lines().find(|l| l.starts_with("vm ")).unwrap();
        let state = line.split_once(" (").unwrap().1;
        state.strip_suffix(')').unwrap().to_string()
    };

    build(1);
    let (status, _) = env.ok(&["status"]);
    let vm = status
        .lines()
        .find_map(|line| line.strip_prefix("vm")?.trim().strip_prefix("~/"))
        .and_then(|line| line.split(' ').next())
        .map(|vm| env.home.join(vm))
        .unwrap();
    fs::create_dir_all(&vm).unwrap();
    fs::write(
        vm.join("project.toml"),
        format!(
            "project_path = \"{}\"\nimage = \"default\"\nbase_revision = 999\n",
            env.project.display()
        ),
    )
    .unwrap();
    fs::copy(
        env.home
            .join(".cache/sendit/images/default/provisioned.toml"),
        vm.join("image.toml"),
    )
    .unwrap();
    assert_eq!(vm_state(), "stopped");

    build(2);
    assert_eq!(
        vm_state(),
        "stopped; made from an earlier build of the default image, \
         `sendit reset` recreates it from the current one"
    );

    // A linked worktree of the project, as `git worktree add` leaves it,
    // whose VM would be a copy of the project's.
    let wt = Env {
        home: env.home.clone(),
        project: env.project.with_file_name("wt"),
    };
    let gitdir = env.project.join(".git/worktrees/wt");
    fs::create_dir_all(&gitdir).unwrap();
    fs::create_dir_all(&wt.project).unwrap();
    let dot_git = wt.project.join(".git");
    fs::write(&dot_git, format!("gitdir: {}\n", gitdir.display())).unwrap();
    fs::write(gitdir.join("gitdir"), format!("{}\n", dot_git.display())).unwrap();
    let (status, _) = wt.ok(&["status"]);
    assert!(
        status.contains(&format!(
            "(not created; `sendit run` copies the VM of {}, made from an earlier build \
             of the default image; `sendit run --fresh` makes it from the current one)",
            env.project.display()
        )),
        "{status}"
    );
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
fn lists_vms_and_images() {
    let env = Env::new("listing");
    let vms = env.home.join(".sendit");
    let images = env.home.join(".cache/sendit/images");
    // From a base revision no sendit has yet, so never outdated.
    fs::create_dir_all(images.join("rust")).unwrap();
    let marker = |built: u32| format!("revision = 999\ncustom_scripts = []\nbuilt = {built}\n");
    fs::write(images.join("rust/provisioned.toml"), marker(2)).unwrap();
    fs::write(images.join("rust/disk.img"), vec![1; 8192]).unwrap();
    // An image that only has an image file.
    let image_files = env.home.join(".config/sendit/images");
    fs::create_dir_all(&image_files).unwrap();
    fs::write(
        image_files.join("go.sh"),
        "sudo apt-get install -y golang\n",
    )
    .unwrap();
    let vm = |name: &str, uuid: &str, metadata: Option<(&Path, &str, u32)>| {
        let dir = vms.join(format!("{name}_6ba7b811-9dad-11d1-80b4-00c04fd430{uuid}"));
        fs::create_dir_all(&dir).unwrap();
        if let Some((project, image, revision)) = metadata {
            fs::write(
                dir.join("project.toml"),
                format!(
                    "project_path = \"{}\"\nimage = \"{image}\"\nbase_revision = {revision}\n",
                    project.display()
                ),
            )
            .unwrap();
        }
        dir
    };
    let gone = env.home.parent().unwrap().join("gone");
    // Made from the latest build of the rust image, and from an earlier one.
    let app = vm("app", "c1", Some((&env.project, "rust", 999)));
    fs::write(app.join("image.toml"), marker(2)).unwrap();
    let gone_vm = vm("gone", "c2", Some((&gone, "rust", 999)));
    fs::write(gone_vm.join("image.toml"), marker(1)).unwrap();
    vm("old", "c3", Some((&env.project, "default", 0)));
    vm("broken", "c4", None);

    let (list, _) = env.ok(&["list"]);
    let rows: Vec<Vec<_>> = list
        .lines()
        .map(|line| line.split_whitespace().collect())
        .collect();
    let project = env.project.display().to_string();
    let gone = gone.display().to_string();
    assert_eq!(
        rows,
        [
            vec!["STATE", "DISK", "IMAGE", "PROJECT"],
            vec!["stopped", "0", "B", "rust", &project],
            vec![
                "stopped",
                "0",
                "B",
                "?",
                "?",
                "(~/.sendit/broken_6ba7b811-9dad-11d1-80b4-00c04fd430c4)"
            ],
            vec![
                "stopped",
                "0",
                "B",
                "rust",
                "(earlier",
                "build)",
                &gone,
                "(missing)"
            ],
            vec!["outdated", "0", "B", "default", &project],
        ],
        "{list}"
    );
    // A VM's copy of the marker that can't be read leaves it unmarked.
    fs::write(gone_vm.join("image.toml"), "not toml").unwrap();
    let (list, _) = env.ok(&["list"]);
    assert!(!list.contains("earlier build"), "{list}");
    assert_eq!(list.lines().count(), 5, "{list}");

    let (images, _) = env.ok(&["images"]);
    let rows: Vec<Vec<_>> = images
        .lines()
        .map(|line| line.split_whitespace().collect())
        .collect();
    assert_eq!(
        rows,
        [
            vec!["IMAGE", "STATE", "DISK", "VMS"],
            vec!["default", "missing", "-", "1"],
            vec!["go", "missing", "-", "0"],
            vec!["rust", "provisioned", "8.0", "KiB", "2"],
        ],
        "{images}"
    );

    // Without a terminal, prune only lists what it would delete.
    let output = env.sendit(&["prune"]);
    assert_fails_with(&output, "pass --yes");
    assert_fails_with(
        &output,
        &format!("VMs whose project directory no longer exists:\n  {gone}  (~/.sendit/gone_"),
    );
    let (_, deleted) = env.ok(&["prune", "--yes"]);
    assert!(deleted.ends_with("Deleted 1 VM.\n"), "{deleted}");
    let (list, _) = env.ok(&["list"]);
    assert!(!list.contains(&gone), "{list}");
}

#[test]
fn deletes_images() {
    let env = Env::new("delete-images");
    let cache = env.home.join(".cache/sendit");
    for image in ["default", "go", "old", "rust"] {
        let dir = cache.join("images").join(image);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("provisioned.toml"),
            "revision = 999\ncustom_scripts = []\n",
        )
        .unwrap();
        fs::create_dir_all(cache.join("provision").join(image)).unwrap();
    }
    // Left behind by a build that was killed.
    fs::create_dir_all(cache.join("images/.rust.partial")).unwrap();
    // `provision --all` builds rust for its image file, go for the config
    // file, default always, and old not at all.
    let image_file = env.home.join(".config/sendit/images/rust.sh");
    fs::create_dir_all(image_file.parent().unwrap()).unwrap();
    fs::write(&image_file, "true\n").unwrap();
    env.config("[images.go]\ncpus = 1\n");
    for uuid in ["c1", "c2"] {
        let vm = env.home.join(format!(
            ".sendit/app_6ba7b811-9dad-11d1-80b4-00c04fd430{uuid}"
        ));
        fs::create_dir_all(&vm).unwrap();
        fs::write(
            vm.join("project.toml"),
            format!(
                "project_path = \"{}\"\nimage = \"rust\"\nbase_revision = 999\n",
                env.project.display()
            ),
        )
        .unwrap();
    }
    let vms = env.home.join(".sendit");

    assert_fails_with(
        &env.sendit(&["images", "--delete", "rust", "--delete", "python"]),
        "the python image isn't provisioned",
    );
    assert_eq!(env.sendit(&["images", "--yes"]).status.code(), Some(2));
    // Without a terminal, it only lists what it would delete.
    let output = env.sendit(&["images", "--delete", "rust", "--delete", "go"]);
    assert_fails_with(&output, "pass --yes");
    assert_fails_with(
        &output,
        "Base images to delete:\n  go  (~/.cache/sendit/images/go)\n  \
         rust  (~/.cache/sendit/images/rust; 2 VMs were made from it)\n",
    );
    assert!(cache.join("images/rust").exists());

    let (_, deleted) = env.ok(&["images", "--delete", "rust", "--delete", "rust", "--yes"]);
    assert!(
        deleted.ends_with(
            "Deleted the rust image.\n`sendit provision --all` builds the rust image again \
             while ~/.config/sendit/images/rust.sh exists.\n"
        ),
        "{deleted}"
    );
    for gone in ["images/rust", "images/.rust.partial", "provision/rust"] {
        assert!(!cache.join(gone).exists(), "{gone}");
    }
    assert!(image_file.exists());
    assert_eq!(fs::read_dir(&vms).unwrap().count(), 2);
    // Still listed for its VMs.
    let (images, _) = env.ok(&["images"]);
    assert!(
        images
            .lines()
            .any(|line| line.split_whitespace().eq(["rust", "missing", "-", "2"])),
        "{images}"
    );

    let (_, deleted) = env.ok(&[
        "images", "--delete", "old", "--delete", "go", "--delete", "default", "--yes",
    ]);
    assert!(
        deleted.ends_with(
            "Deleted 3 images.\n\
             `sendit provision --all` builds the default image again.\n\
             `sendit provision --all` builds the go image again while the config file names it.\n"
        ),
        "{deleted}"
    );
    assert_eq!(fs::read_dir(cache.join("images")).unwrap().count(), 0);
    assert_eq!(fs::read_dir(cache.join("provision")).unwrap().count(), 0);
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
        "disk-size must be at least 16 GiB, got 4 GiB",
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
    status_fails_with("[images.Bad]\n", "invalid image name \"Bad\"");
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
    let output = common::sendit(&env.home)
        .current_dir(&env.project)
        .args(["run", "--mount", "does-not-exist"])
        .output()
        .unwrap();
    assert_fails_with(
        &output,
        &format!("mount {}/does-not-exist", env.project_path()),
    );
    let output = common::sendit(&env.home)
        .args(["-C", "/does/not/exist", "status"])
        .output()
        .unwrap();
    assert_fails_with(&output, "project directory /does/not/exist");
}

#[test]
fn reports_bad_image_files() {
    let env = Env::new("image-files");
    let config = env.home.join(".config/sendit");
    let write = |path: &str, content: &str| {
        let path = config.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    };
    env.config("image = \"rust\"\n");

    // Scripts in the old layout aren't silently ignored, also not by `run`
    // of a VM that exists, which doesn't read the scripts.
    let (status, _) = env.ok(&["status"]);
    let vm = status
        .lines()
        .find_map(|line| line.strip_prefix("vm")?.trim().strip_prefix("~/"))
        .and_then(|line| line.split(' ').next())
        .map(|vm| env.home.join(vm))
        .unwrap();
    fs::create_dir_all(&vm).unwrap();
    fs::write(
        vm.join("project.toml"),
        format!(
            "project_path = \"{}\"\nimage = \"rust\"\nbase_revision = 999\n",
            env.project.display()
        ),
    )
    .unwrap();
    write("provision-scripts/rust/10-cargo.sh", "cargo\n");
    for args in [
        &["status"][..],
        &["run"],
        &["images"],
        &["provision", "--all"],
    ] {
        assert_fails_with(
            &env.sendit(args),
            "~/.config/sendit/provision-scripts/rust/: subfolders are no longer supported; \
             images are defined by ~/.config/sendit/images/NAME.sh",
        );
    }
    fs::remove_dir_all(config.join("provision-scripts/rust")).unwrap();
    fs::remove_dir_all(&vm).unwrap();

    // A call that can't be resolved stops `provision` before it downloads
    // Debian, even for an image that is built already. The image's VMs
    // don't read the scripts: they share what the build recorded.
    write("images/rust.sh", "sendit-provision rustup.sh\n");
    let missing = "the custom script images/rust.sh calls `sendit-provision rustup.sh`, \
                   but there is no ~/.config/sendit/provision-scripts/rustup.sh";
    assert_fails_with(&env.sendit(&["provision", "rust"]), missing);
    assert!(!env.home.join(".cache/sendit/downloads").exists());
    let built = env.home.join(".cache/sendit/images/rust");
    fs::create_dir_all(&built).unwrap();
    fs::write(
        built.join("provisioned.toml"),
        "revision = 1000\ncustom_scripts = []\n",
    )
    .unwrap();
    assert_fails_with(&env.sendit(&["provision", "rust"]), missing);
    let (status, stderr) = env.ok(&["status"]);
    assert!(status.contains("scripts changed"), "{status}");
    assert_eq!(stderr, "");
    let (images, _) = env.ok(&["images"]);
    assert!(images.contains("scripts changed"), "{images}");
    fs::remove_dir_all(&built).unwrap();

    write(
        "provision-scripts/rustup.sh",
        "curl https://sh.rustup.rs | sh\n",
    );
    env.ok(&["status"]);

    // An image file that is a symlink to nothing, e.g. into a dotfiles repo
    // that isn't cloned yet, stops a build, which would lack its scripts.
    let image_file = config.join("images/rust.sh");
    fs::remove_file(&image_file).unwrap();
    std::os::unix::fs::symlink(env.home.join("dotfiles/rust.sh"), &image_file).unwrap();
    let dangling = "the image file ~/.config/sendit/images/rust.sh points to \
                    ~/dotfiles/rust.sh, which doesn't exist";
    assert_fails_with(&env.sendit(&["provision", "rust"]), dangling);
    assert!(!env.home.join(".cache/sendit/downloads").exists());
    env.ok(&["status"]);
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
        "is not provisioned yet",
    );
}

#[test]
fn moves_vms_to_sendit_vm_dir() {
    let env = Env::new("vm-dir");
    let external = env.home.parent().unwrap().join("external");
    let vms = external.join("vms");
    let sendit = |vms: &Path, args: &[&str]| {
        common::sendit(&env.home)
            .env("SENDIT_VM_DIR", vms)
            .arg("-C")
            .arg(&env.project)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };

    // A drive that isn't connected is an error, not VMs made elsewhere.
    let output = sendit(&vms, &["list"]);
    assert_fails_with(&output, "invalid SENDIT_VM_DIR");
    assert_fails_with(
        &output,
        &format!(
            "{} is not a directory; create it, or connect the drive it is on",
            vms.display()
        ),
    );
    assert!(!vms.exists());
    assert_fails_with(
        &sendit(&env.home, &["list"]),
        "contains your home directory",
    );

    fs::create_dir_all(&vms).unwrap();
    let output = sendit(&vms, &["list"]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stderr),
        format!("No VMs in {}.\n", vms.display())
    );
    assert!(!env.home.join(".sendit").exists());

    // Shares of it are risky, ~/.sendit no longer is.
    let output = sendit(&vms, &["run", "--mount", external.to_str().unwrap()]);
    assert_fails_with(
        &output,
        &format!(
            "{} (read-only) contains sendit's VMs ({})",
            external.canonicalize().unwrap().display(),
            vms.canonicalize().unwrap().display()
        ),
    );
    fs::create_dir_all(env.home.join(".sendit")).unwrap();
    assert_fails_with(
        &sendit(&vms, &["run", "--mount", "~/.sendit"]),
        "is not provisioned yet",
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
    let output = common::command(&binary, &env.home)
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
