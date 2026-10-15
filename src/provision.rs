//! Building the named base images that project VMs are cloned from.
//!
//! The Debian cloud image boots with a cloud-init seed ISO attached, runs
//! `assets/provision.sh` and powers itself off. An image is built in
//! `images/.<name>.partial/` and only replaces `images/<name>/` once it
//! succeeded. One sendit process at a time builds images; others wait.
//!
//! Custom scripts travel on the seed ISO too and run after the built-in
//! provisioning, e.g. to install more tools. An image with custom scripts
//! has an image file, `~/.config/sendit/images/<name>.sh`, which provision.sh
//! runs. It calls the shared scripts in `~/.config/sendit/provision-scripts/`
//! with `sendit-provision NAME`, and they can call each other the same way;
//! each runs at most once per build. A script can ask for host directories to
//! be shared with the VMs made from the image, in comment lines at its top:
//! `# sendit-mount: HOST[:GUEST][:ro|rw]`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{ByteSize, Config, MountSpec, ProvisionSettings};
use crate::image;
use crate::paths::{ImageName, Paths};
use crate::util;
use crate::vm::{self, ConsoleMode, ProvisionLog, VmDir, VmSpec};

/// Logical size of the base disk. Project VMs grow their clone further, but
/// can't shrink it, so this is also the smallest disk size they can have.
pub const BASE_DISK_SIZE: ByteSize = ByteSize::gib(16);

const USER_DATA: &str = include_str!("assets/user-data.yaml");
const PROVISION_SCRIPT: &str = include_str!("assets/provision.sh");

/// Bumped whenever the base image changes in a way the host code relies on.
/// Revision 2 added the sendit-mounts service, 3 made DHCP leases
/// identifiable by MAC address, 4 shuts the VM down on console logout, 5
/// applies the host terminal's size to the console, 6 its type, 7 takes
/// the guest user's sudo rights and lets the host log in as root, 8 stops
/// systemd from asking the terminal for its size, whose late replies ended
/// up in the shell, 9 gives root's shell colours, a window title and a red
/// prompt, tells the host when the guest starts shutting down, and drops
/// unattended-upgrades, which could hold up shutting down, 10 boots the
/// kernel with 16 KiB pages.
pub const BASE_REVISION: u32 = 10;

/// Starts a comment line in which a custom script asks for a mount.
const MOUNT_COMMENT: &str = "# sendit-mount:";

/// The command with which custom scripts run a shared script.
const RUN_COMMAND: &str = "sendit-provision";

/// Printed by provision.sh as its last line when it succeeded, followed by
/// the build's token.
const SUCCESS_SENTINEL: &str = "SENDIT_PROVISION_OK";

/// Written into the base directory once provisioning has succeeded.
#[derive(Deserialize, Serialize)]
struct Marker {
    revision: u32,
    custom_scripts: Vec<ScriptStamp>,
}

/// A custom provisioning script the base image was built with.
#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
struct ScriptStamp {
    name: String,
    /// Of the script below its header (see `CustomScript::split_header`),
    /// so that changing its description or mounts doesn't mark the image.
    sha256: String,
}

/// A custom provisioning script: the image file of the image being built,
/// or a shared script from `Paths::provision_scripts_dir` that it calls.
struct CustomScript {
    /// For messages and the marker: `images/rust.sh` for an image file, the
    /// file name for a shared script.
    name: String,
    /// The file name, e.g. `cargo.sh`, which `sendit-provision` takes for a
    /// shared script.
    file: String,
    content: Vec<u8>,
}

impl CustomScript {
    fn stamp(&self) -> ScriptStamp {
        ScriptStamp {
            name: self.name.clone(),
            sha256: hex::encode(Sha256::digest(self.split_header().1)),
        }
    }

    /// The header, the blank and comment lines at the top of the script
    /// before its first command, and the rest. provision.sh and
    /// `sendit-provision` run scripts with `bash FILE`, so the header, shebang
    /// included, does nothing. Comments
    /// further down may not be comments, e.g. in a heredoc.
    fn split_header(&self) -> (&[u8], &[u8]) {
        let mut end = 0;
        for line in self.content.split_inclusive(|&b| b == b'\n') {
            let text = line.trim_ascii();
            if !text.is_empty() && !text.starts_with(b"#") {
                break;
            }
            end += line.len();
        }
        self.content.split_at(end)
    }

    /// The mounts asked for in the script's header. Their host paths must be
    /// absolute or start with `~`, as in the config file.
    fn mounts(&self) -> Result<Vec<MountSpec>> {
        let header = String::from_utf8_lossy(self.split_header().0);
        let mut mounts = Vec::new();
        for line in header.lines().map(str::trim) {
            let Some(spec) = line.strip_prefix(MOUNT_COMMENT) else {
                continue;
            };
            let spec: MountSpec = spec
                .trim()
                .parse()
                .with_context(|| format!("in the custom script {}", self.name))?;
            ensure!(
                spec.host.is_absolute() || spec.host.starts_with("~"),
                "in the custom script {}: mount {}: host path must be absolute or start with ~",
                self.name,
                spec.host.display()
            );
            mounts.push(spec);
        }
        Ok(mounts)
    }

    /// The names of the shared scripts that this one calls with
    /// `sendit-provision`, in the order they appear. Every `sendit-provision`
    /// word outside of comments counts, wherever it is, e.g. in an `if`. The
    /// name after it must be a literal file name, which can be resolved
    /// without running anything.
    fn calls(&self) -> Result<Vec<String>> {
        let text = String::from_utf8_lossy(&self.content);
        let mut calls = Vec::new();
        for line in text.lines() {
            let mut words = shell_words(line).into_iter();
            while let Some(word) = words.next() {
                if word != RUN_COMMAND {
                    continue;
                }
                let name = words
                    .next()
                    .filter(|name| !is_operator(name))
                    .unwrap_or_default();
                check_script_name(name).with_context(|| {
                    format!("in the custom script {}: `{}`", self.name, line.trim())
                })?;
                calls.push(name.to_string());
            }
        }
        Ok(calls)
    }
}

/// The words of a line of shell, roughly: split at whitespace, with each of
/// the characters that end a command or redirect (`;&|()<>`) a word of its
/// own, and without a comment at the end. Quotes aren't understood.
fn shell_words(line: &str) -> Vec<&str> {
    let mut words = Vec::new();
    let mut start = None;
    for (i, c) in line.char_indices() {
        let ends = c.is_whitespace() || is_operator(&line[i..i + c.len_utf8()]);
        match start {
            Some(begin) if ends => {
                words.push(&line[begin..i]);
                start = None;
            }
            None if c == '#' => break,
            None if !ends => start = Some(i),
            _ => {}
        }
        if !c.is_whitespace() && ends {
            words.push(&line[i..i + c.len_utf8()]);
        }
    }
    if let Some(begin) = start {
        words.push(&line[begin..]);
    }
    words
}

fn is_operator(word: &str) -> bool {
    matches!(word, ";" | "&" | "|" | "(" | ")" | "<" | ">")
}

/// Fails unless `name`, which a script passes to `sendit-provision`, is a plain
/// file name. It is only ever looked up in the listing of
/// `Paths::provision_scripts_dir`, never joined onto a path.
fn check_script_name(name: &str) -> Result<()> {
    ensure!(!name.is_empty(), "{RUN_COMMAND} names no script");
    ensure!(
        !name.contains('/'),
        "{RUN_COMMAND} {name}: the script must be named without a directory, \
         as a file in the provision-scripts directory"
    );
    ensure!(
        !name.starts_with('.'),
        "{RUN_COMMAND} {name}: script names can't start with a dot"
    );
    ensure!(
        !name.contains(|c: char| c.is_control()),
        "{RUN_COMMAND} {name:?}: script names can't have control characters"
    );
    ensure!(
        name.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c)),
        "{RUN_COMMAND} {name}: expected a literal file name, without quotes, \
         variables or other shell syntax"
    );
    Ok(())
}

fn marker_file(dir: &VmDir) -> PathBuf {
    dir.path().join("provisioned.toml")
}

pub enum BaseState {
    /// Not provisioned yet.
    Missing,
    /// Older than `BASE_REVISION`; project VMs can't be created from it.
    Outdated,
    /// Usable, but its image file or the scripts it calls were added,
    /// removed or edited since it was provisioned.
    ScriptsChanged,
    Current,
}

impl BaseState {
    /// The state of `image` and, unless it is current, the command that
    /// builds it, e.g. "outdated; rebuild it with `sendit provision rust`".
    pub fn describe(&self, image: &ImageName) -> String {
        match self {
            Self::Missing => format!("not provisioned yet; run `{}`", command(image, false)),
            Self::Outdated => format!("outdated; rebuild it with `{}`", command(image, false)),
            Self::ScriptsChanged => format!(
                "custom scripts changed; rebuild it with `{}`",
                command(image, true)
            ),
            Self::Current => "provisioned".to_string(),
        }
    }
}

pub fn base_state(paths: &Paths, image: &ImageName) -> Result<BaseState> {
    let base = VmDir::new(paths.image_dir(image));
    let Some(marker) = util::read_toml::<Marker>(&marker_file(&base))? else {
        return Ok(BaseState::Missing);
    };
    if marker.revision < BASE_REVISION {
        return Ok(BaseState::Outdated);
    }
    // Scripts with a mistake have changed since the build, which resolved
    // them. `provision` tells about it, and `script_mounts` warns.
    check_no_subfolders(paths)?;
    let Ok(scripts) = custom_scripts(paths, image) else {
        return Ok(BaseState::ScriptsChanged);
    };
    let current: Vec<_> = scripts.iter().map(CustomScript::stamp).collect();
    Ok(if marker.custom_scripts != current {
        BaseState::ScriptsChanged
    } else {
        BaseState::Current
    })
}

/// All images: `default`, and every image that the config file names, has
/// an image file or has been provisioned, in name order.
pub fn images(paths: &Paths, config: &Config) -> Result<Vec<ImageName>> {
    check_no_subfolders(paths)?;
    let mut images = BTreeSet::from([ImageName::default()]);
    images.extend(config.images().cloned());
    for entry in util::read_dir(&paths.image_files_dir())? {
        let path = entry?.path();
        let Some(file) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(name) = file.strip_suffix(".sh") else {
            continue;
        };
        // A symlink to nothing counts, so that `--all` tells about it.
        if file.starts_with('.') || path.is_dir() {
            continue;
        }
        let name = name
            .parse()
            .with_context(|| format!("image file {}", paths.display(&path)))?;
        images.insert(name);
    }
    for entry in util::read_dir(&paths.images_dir())? {
        let entry = entry?;
        // Partial images start with a dot, which names can't.
        let name = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok());
        if let Some(name) = name
            && entry.path().is_dir()
        {
            images.insert(name);
        }
    }
    Ok(images.into_iter().collect())
}

/// Fails if `Paths::provision_scripts_dir` has subfolders: the scripts in
/// them, the old way to give an image scripts of its own, would be left out
/// without a word.
fn check_no_subfolders(paths: &Paths) -> Result<()> {
    for entry in util::read_dir(&paths.provision_scripts_dir())? {
        let path = entry?.path();
        let hidden = path
            .file_name()
            .is_some_and(|name| name.as_encoded_bytes().starts_with(b"."));
        ensure!(
            hidden || !path.is_dir(),
            "{}/: subfolders are no longer supported; images are defined by {} \
             (see the CHANGELOG)",
            paths.display(&path),
            paths.display(&paths.image_files_dir().join("NAME.sh"))
        );
    }
    Ok(())
}

/// Where the image file of `image` points if it is a symlink to nothing,
/// e.g. into a dotfiles repository that isn't there (yet). `custom_scripts`
/// finds no image file then, so `provision` refuses to build the image
/// without its scripts, and `script_mounts` warns.
fn dangling_image_file(paths: &Paths, image: &ImageName) -> Option<PathBuf> {
    let file = paths.image_file(image);
    let target = fs::read_link(&file).ok()?;
    match fs::metadata(&file) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(target),
        _ => None,
    }
}

/// Says that the image file of `image` points to `target`, which doesn't
/// exist.
fn describe_dangling(paths: &Paths, image: &ImageName, target: &Path) -> String {
    format!(
        "the image file {} points to {}, which doesn't exist",
        paths.display(&paths.image_file(image)),
        paths.display(target)
    )
}

/// The custom scripts for `image`: its image file first, then the shared
/// scripts that it calls, directly or through other scripts, in the order
/// they are first found. None without an image file, or with one that is a
/// symlink to nothing (see `dangling_image_file`).
fn custom_scripts(paths: &Paths, image: &ImageName) -> Result<Vec<CustomScript>> {
    let dir = paths.provision_scripts_dir();
    check_no_subfolders(paths)?;
    let image_file = paths.image_file(image);
    let content = match fs::read(&image_file) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", image_file.display())),
    };
    let main = CustomScript {
        name: format!("images/{image}.sh"),
        file: format!("{image}.sh"),
        content,
    };

    let mut shared: BTreeMap<_, _> = script_files(&dir)?.into_iter().collect();
    let mut scripts = vec![main];
    add_calls(paths, 0, &mut shared, &mut scripts, &mut Vec::new())?;
    Ok(scripts)
}

/// Appends the scripts that `scripts[caller]` calls to `scripts`, each
/// followed by those it calls in turn, taking them out of `shared`. Those
/// already in `scripts` are left out. `chain` names the shared scripts that
/// led to the caller: calling one of them again would never end.
fn add_calls(
    paths: &Paths,
    caller: usize,
    shared: &mut BTreeMap<String, Vec<u8>>,
    scripts: &mut Vec<CustomScript>,
    chain: &mut Vec<String>,
) -> Result<()> {
    for name in scripts[caller].calls()? {
        if let Some(start) = chain.iter().position(|n| *n == name) {
            bail!(
                "custom scripts call each other in a cycle: {} -> {name}",
                chain[start..].join(" -> ")
            );
        }
        if scripts[1..].iter().any(|s| s.file == name) {
            continue;
        }
        let Some(content) = shared.remove(&name) else {
            bail!(
                "the custom script {} calls `{RUN_COMMAND} {name}`, but there is no {}",
                scripts[caller].name,
                paths.display(&paths.provision_scripts_dir().join(&name))
            );
        };
        scripts.push(CustomScript {
            name: name.clone(),
            file: name.clone(),
            content,
        });
        chain.push(name);
        add_calls(paths, scripts.len() - 1, shared, scripts, chain)?;
        chain.pop();
    }
    Ok(())
}

/// The mounts that the custom scripts for `image` ask for, in the order
/// the scripts are first called, each with the name of its script. None,
/// with a warning, if the image file is a symlink to nothing or the scripts
/// have a mistake: the VMs made from the image can do without them, and
/// `provision` refuses to build it until it is fixed.
pub fn script_mounts(paths: &Paths, image: &ImageName) -> Result<Vec<(String, MountSpec)>> {
    check_no_subfolders(paths)?;
    let leaving_out =
        format!("leaving out the shares that the scripts of the {image} image ask for");
    if let Some(target) = dangling_image_file(paths, image) {
        eprintln!(
            "warning: {}; {leaving_out}",
            describe_dangling(paths, image, &target)
        );
    }
    match custom_scripts(paths, image).and_then(|scripts| mounts_of(&scripts)) {
        Ok(mounts) => Ok(mounts),
        Err(e) => {
            eprintln!("warning: {e:#}; {leaving_out}");
            Ok(Vec::new())
        }
    }
}

fn mounts_of(scripts: &[CustomScript]) -> Result<Vec<(String, MountSpec)>> {
    let mut mounts = Vec::new();
    for script in scripts {
        for spec in script.mounts()? {
            mounts.push((script.name.clone(), spec));
        }
    }
    Ok(mounts)
}

/// Lists `mounts` one per line, as written, each with its script.
fn describe_mounts(mounts: &[(String, MountSpec)]) -> String {
    let mut text = "VMs made from the image share what the scripts ask for:".to_string();
    for (script, spec) in mounts {
        let _ = write!(text, "\n  {spec}  ({script})");
    }
    text
}

/// Names the image file of `image`, and lists the shared scripts it calls
/// (`scripts` after the image file) one per line.
fn describe_scripts(paths: &Paths, image: &ImageName, scripts: &[CustomScript]) -> String {
    let mut text = format!("Image file: {}", paths.display(&paths.image_file(image)));
    if scripts.len() > 1 {
        let _ = write!(
            text,
            "\nScripts it calls, from {}:",
            paths.display(&paths.provision_scripts_dir())
        );
        for script in &scripts[1..] {
            let _ = write!(text, "\n  {}", script.file);
        }
    }
    text
}

/// The names and contents of the `*.sh` files in `dir`.
fn script_files(dir: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    let mut scripts = Vec::new();
    for entry in util::read_dir(dir)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with('.') || !name.ends_with(".sh") || !path.is_file() {
            continue;
        }
        ensure!(
            !name.contains(|c: char| c.is_control()),
            "custom provisioning script {:?} has control characters in its name",
            path.display()
        );
        let content = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        scripts.push((name.to_string(), content));
    }
    Ok(scripts)
}

/// The command that builds `image`, e.g. `sendit provision rust --force`.
fn command(image: &ImageName, force: bool) -> String {
    let mut command = "sendit provision".to_string();
    if *image != ImageName::default() {
        let _ = write!(command, " {image}");
    }
    if force {
        command.push_str(" --force");
    }
    command
}

/// Builds the `name` image unless it is already provisioned (or `force`).
/// An outdated image is rebuilt: nothing can use it anymore.
pub fn provision(
    paths: &Paths,
    name: &ImageName,
    settings: &ProvisionSettings,
    force: bool,
) -> Result<()> {
    let _lock = lock(paths)?;
    // Built without its scripts, the image would look current until they
    // are back. Before `already_provisioned`, which would suggest --force.
    if let Some(target) = dangling_image_file(paths, name) {
        bail!("{}", describe_dangling(paths, name, &target));
    }
    // Mistakes in the scripts show before anything is downloaded, and in
    // place of the --force that `already_provisioned` would suggest.
    let scripts = custom_scripts(paths, name)?;
    let mounts = mounts_of(&scripts)?;
    if !force && let Some(why) = already_provisioned(paths, name)? {
        eprintln!("{why}");
        return Ok(());
    }
    let base = paths.image_dir(name);

    let image = image::debian_image(paths)?;
    fs::create_dir_all(paths.ssh_dir())?;
    let public_key = ssh_public_key(&paths.ssh_key())?;
    let root_public_key = ssh_public_key(&paths.root_ssh_key())?;

    let work = paths.provision_dir(name);
    util::fresh_dir(&work)?;
    let token = random_token()?;
    let seed = build_seed_iso(&work, &public_key, &root_public_key, &scripts, &token)?;

    let log = work.join("console.log");
    eprintln!(
        "Provisioning the {name} image ({} CPUs, {} memory). This takes a few minutes; \
         the console is logged to {}.",
        settings.cpus,
        settings.memory,
        paths.display(&log)
    );
    if !scripts.is_empty() {
        eprintln!("{}", describe_scripts(paths, name, &scripts));
    }
    if !mounts.is_empty() {
        eprintln!("{}", describe_mounts(&mounts));
    }
    let spec = VmSpec {
        cpus: settings.cpus,
        memory: settings.memory,
        shares: Vec::new(),
        seed: Some(seed),
        console: ConsoleMode::Provision(ProvisionLog {
            path: log.clone(),
            last_line: token.clone(),
        }),
        shutdown: None,
    };

    let partial = VmDir::new(paths.image_partial_dir(name));
    util::fresh_dir(partial.path())?;
    let built = (|| {
        image::clone_file(&image, &partial.disk())?;
        image::grow_disk(&partial.disk(), BASE_DISK_SIZE)?;
        vm::run(&partial, &spec)?;
        let output = fs::read(&log).with_context(|| format!("reading {}", log.display()))?;
        ensure!(
            String::from_utf8_lossy(&output).contains(&format!("{SUCCESS_SENTINEL} {token}")),
            "provisioning failed; see {} for details",
            paths.display(&log)
        );
        Ok(())
    })();
    if let Err(e) = built {
        // Its disk holds gigabytes by the time a script fails; the console
        // log tells what went wrong.
        let _ = fs::remove_dir_all(partial.path());
        return Err(e);
    }

    let marker = toml::to_string(&Marker {
        revision: BASE_REVISION,
        custom_scripts: scripts.iter().map(CustomScript::stamp).collect(),
    })?;
    fs::write(marker_file(&partial), marker)?;

    if base.exists() {
        fs::remove_dir_all(&base).with_context(|| format!("removing {}", base.display()))?;
    }
    fs::rename(partial.path(), &base)?;
    eprintln!("The {name} image is ready in {}.", paths.display(&base));
    Ok(())
}

/// Why `provision` leaves the `name` image alone without `force`, if it
/// does: it is provisioned and not outdated.
fn already_provisioned(paths: &Paths, name: &ImageName) -> Result<Option<String>> {
    let but = match base_state(paths, name)? {
        BaseState::Missing | BaseState::Outdated => return Ok(None),
        BaseState::ScriptsChanged => ", but its custom scripts have changed since",
        BaseState::Current => "",
    };
    Ok(Some(format!(
        "The {name} image in {} is already provisioned{but}. Use --force to rebuild it.",
        paths.display(&paths.image_dir(name))
    )))
}

/// Takes the lock that only one sendit process at a time builds images
/// under, waiting for it if another one has it. Two builds of the same
/// image would share its partial directory, and any two would download and
/// unpack Debian into the same files. Released when the file is closed.
fn lock(paths: &Paths) -> Result<File> {
    let path = paths.provision_lock();
    fs::create_dir_all(paths.cache_dir())?;
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(fs::TryLockError::WouldBlock) => {
            eprintln!("Waiting for another sendit process to finish provisioning…");
            file.lock()
                .with_context(|| format!("locking {}", path.display()))?;
        }
        Err(fs::TryLockError::Error(e)) => {
            return Err(e).with_context(|| format!("locking {}", path.display()));
        }
    }
    Ok(file)
}

/// Returns the public key of the SSH keypair `key`, generating it first if
/// needed. Its directory must exist.
fn ssh_public_key(key: &Path) -> Result<String> {
    if !key.exists() {
        util::run(
            Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-C", "sendit", "-f"])
                .arg(key),
        )?;
    }
    let public = key.with_extension("pub");
    Ok(fs::read_to_string(&public)
        .with_context(|| format!("reading {}", public.display()))?
        .trim()
        .to_string())
}

/// Writes the NoCloud seed files into `work/seed/` and packs them into an ISO
/// labelled `cidata`.
fn build_seed_iso(
    work: &Path,
    public_key: &str,
    root_public_key: &str,
    scripts: &[CustomScript],
    token: &str,
) -> Result<PathBuf> {
    let dir = work.join("seed");
    fs::create_dir_all(&dir)?;
    fs::write(
        dir.join("meta-data"),
        format!("instance-id: sendit-{token}\nlocal-hostname: sendit\n"),
    )?;
    fs::write(
        dir.join("user-data"),
        USER_DATA.replace("@SSH_PUBLIC_KEY@", public_key),
    )?;
    fs::write(dir.join("root.pub"), format!("{root_public_key}\n"))?;
    fs::write(dir.join("provision.sh"), PROVISION_SCRIPT)?;
    fs::write(dir.join("token"), format!("{token}\n"))?;
    write_custom_scripts(&dir.join("custom"), scripts)?;

    let iso = work.join("seed.iso");
    util::run_output(
        Command::new("hdiutil")
            .args(["makehybrid", "-quiet", "-iso", "-joliet"])
            .args(["-default-volume-name", "cidata", "-o"])
            .arg(&iso)
            .arg(&dir),
    )
    .context("creating the seed ISO")?;
    Ok(iso)
}

/// Writes `scripts`, as `custom_scripts` returns them, into the seed's
/// `custom` directory. The image file is `main.sh`. The scripts it calls get
/// numbered file names, which survive the ISO's file name limits; `list`
/// maps them back to the names that `sendit-provision` takes.
fn write_custom_scripts(custom: &Path, scripts: &[CustomScript]) -> Result<()> {
    fs::create_dir_all(custom)?;
    let mut list = String::new();
    if let Some((main, called)) = scripts.split_first() {
        fs::write(custom.join("main.sh"), &main.content)?;
        for (i, script) in called.iter().enumerate() {
            let file = format!("{:03}.sh", i + 1);
            fs::write(custom.join(&file), &script.content)?;
            let _ = writeln!(list, "{file} {}", script.file);
        }
    }
    fs::write(custom.join("list"), list)?;
    Ok(())
}

/// A token for provision.sh to print after its success sentinel: 128
/// random bits in hex.
fn random_token() -> Result<String> {
    let mut bytes = [0u8; 16];
    fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .context("reading /dev/urandom")?;
    Ok(hex::encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GUEST_USER;
    use crate::util::TempDir;

    #[test]
    fn assets_fit_together() {
        assert!(USER_DATA.starts_with("#cloud-config\n"));
        assert!(USER_DATA.contains("@SSH_PUBLIC_KEY@"));
        assert!(USER_DATA.contains("/run/sendit-seed/provision.sh"));
        // The script must not contain the sentinel literally, or a failed run
        // that echoes its own source would look successful.
        assert!(!PROVISION_SCRIPT.contains(SUCCESS_SENTINEL));
        assert!(PROVISION_SCRIPT.contains("echo \"SENDIT_PROVISION_${status} $token\""));
        assert!(PROVISION_SCRIPT.contains("\"$seed/custom/list\""));
        assert!(PROVISION_SCRIPT.contains("exec bash -e -l \"$2\"' \\\n"));
        assert!(
            PROVISION_SCRIPT
                .contains("sendit /usr/local/lib/sendit-provision.bash \"$seed/custom/main.sh\")")
        );
        assert!(PROVISION_SCRIPT.contains(&format!("/usr/local/bin/{RUN_COMMAND}\n")));
        assert!(USER_DATA.contains(&format!("- name: {GUEST_USER}\n")));
        assert!(PROVISION_SCRIPT.contains(&format!("\nuser={GUEST_USER}\n")));
    }

    #[test]
    fn rebuilds_only_missing_or_outdated_images() {
        let home = TempDir::new("provisioned");
        let paths = Paths::new(home.path().to_path_buf());
        let rust = image("rust");
        let why = || already_provisioned(&paths, &rust).unwrap();
        assert_eq!(why(), None);

        let base = VmDir::new(paths.image_dir(&rust));
        fs::create_dir_all(base.path()).unwrap();
        let mark = |revision: u32| {
            let marker = Marker {
                revision,
                custom_scripts: Vec::new(),
            };
            fs::write(marker_file(&base), toml::to_string(&marker).unwrap()).unwrap();
        };
        mark(BASE_REVISION - 1);
        assert_eq!(why(), None);
        mark(BASE_REVISION);
        assert_eq!(
            why().unwrap(),
            "The rust image in ~/.cache/sendit/images/rust is already provisioned. \
             Use --force to rebuild it."
        );
        fs::create_dir_all(paths.image_files_dir()).unwrap();
        fs::write(paths.image_file(&rust), "true").unwrap();
        assert!(
            why()
                .unwrap()
                .contains("provisioned, but its custom scripts have changed")
        );
    }

    #[test]
    fn builds_one_image_at_a_time() {
        let home = TempDir::new("provision-lock");
        let paths = Paths::new(home.path().to_path_buf());
        let first = lock(&paths).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let waiting = std::thread::spawn({
            let paths = paths.clone();
            move || {
                let second = lock(&paths);
                tx.send(()).unwrap();
                second.map(drop)
            }
        });
        let wait = std::time::Duration::from_millis(200);
        assert!(rx.recv_timeout(wait).is_err(), "didn't wait");
        drop(first);
        assert!(rx.recv_timeout(wait * 10).is_ok(), "still waiting");
        waiting.join().unwrap().unwrap();
    }

    #[test]
    fn makes_a_new_token_each_time() {
        let token = random_token().unwrap();
        assert_eq!(token.len(), 32);
        assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(token, random_token().unwrap());
    }

    fn image(name: &str) -> ImageName {
        name.parse().unwrap()
    }

    /// Writes `content` to `path` below `~/.config/sendit`.
    fn write_config(paths: &Paths, path: &str, content: &str) {
        let path = paths.config_dir().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// The names of the custom scripts for `image`, in order.
    fn script_names(paths: &Paths, image: &ImageName) -> Vec<String> {
        custom_scripts(paths, image)
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect()
    }

    fn script_error(paths: &Paths, image: &ImageName) -> String {
        match custom_scripts(paths, image) {
            Ok(_) => panic!("no error"),
            Err(e) => format!("{e:#}"),
        }
    }

    #[test]
    fn resolves_the_scripts_an_image_file_calls() {
        let home = TempDir::new("scripts");
        let paths = Paths::new(home.path().to_path_buf());
        let rust = image("rust");
        // Without an image file, shared scripts don't run.
        write_config(&paths, "provision-scripts/unused.sh", "echo unused");
        assert!(custom_scripts(&paths, &rust).unwrap().is_empty());

        write_config(
            &paths,
            "images/rust.sh",
            "#!/bin/bash\n\
             # sendit-provision commented-out.sh\n\
             sendit-provision rust.sh  # the shared one, not this file\n\
             sudo apt-get install -y postgresql-client # sendit-provision no.sh\n\
             (sendit-provision helix.sh)\n\
             if [ -n \"$CI\" ]; then sendit-provision ci.sh; fi\n\
             sendit-provision rust.sh\n",
        );
        write_config(&paths, "provision-scripts/rust.sh", "curl | sh");
        write_config(
            &paths,
            "provision-scripts/helix.sh",
            "  sendit-provision rust.sh\nsendit-provision grammars.sh&&echo done\n",
        );
        write_config(
            &paths,
            "provision-scripts/grammars.sh",
            "hx --grammar fetch",
        );
        write_config(&paths, "provision-scripts/ci.sh", "true");
        // The image file first, then each script it calls, directly or not,
        // once, in the order they are first found.
        assert_eq!(
            script_names(&paths, &rust),
            [
                "images/rust.sh",
                "rust.sh",
                "helix.sh",
                "grammars.sh",
                "ci.sh"
            ]
        );
        let scripts = custom_scripts(&paths, &rust).unwrap();
        assert_eq!(scripts[0].file, "rust.sh");
        assert_eq!(scripts[1].content, b"curl | sh");
        // Other images don't get them.
        assert!(custom_scripts(&paths, &image("go")).unwrap().is_empty());

        // On the seed ISO, the image file is main.sh, and the list names the
        // others as `sendit-provision` does.
        let custom = home.path().join("seed/custom");
        write_custom_scripts(&custom, &scripts).unwrap();
        assert_eq!(
            fs::read_to_string(custom.join("main.sh")).unwrap(),
            String::from_utf8(scripts[0].content.clone()).unwrap()
        );
        assert_eq!(
            fs::read_to_string(custom.join("list")).unwrap(),
            "001.sh rust.sh\n002.sh helix.sh\n003.sh grammars.sh\n004.sh ci.sh\n"
        );
        assert_eq!(
            fs::read_to_string(custom.join("003.sh")).unwrap(),
            "hx --grammar fetch"
        );
        write_custom_scripts(&home.path().join("empty"), &[]).unwrap();
        assert_eq!(
            fs::read_to_string(home.path().join("empty/list")).unwrap(),
            ""
        );
        assert!(!home.path().join("empty/main.sh").exists());
    }

    #[test]
    fn rejects_cycles_and_missing_scripts() {
        let home = TempDir::new("script-cycles");
        let paths = Paths::new(home.path().to_path_buf());
        let rust = image("rust");
        write_config(&paths, "images/rust.sh", "sendit-provision a.sh\n");
        write_config(&paths, "provision-scripts/a.sh", "sendit-provision b.sh\n");
        write_config(&paths, "provision-scripts/b.sh", "sendit-provision c.sh\n");
        write_config(&paths, "provision-scripts/c.sh", "sendit-provision a.sh\n");
        let error = script_error(&paths, &rust);
        assert!(
            error.contains("call each other in a cycle: a.sh -> b.sh -> c.sh -> a.sh"),
            "{error}"
        );
        write_config(&paths, "provision-scripts/c.sh", "sendit-provision c.sh\n");
        let error = script_error(&paths, &rust);
        assert!(error.contains("cycle: c.sh -> c.sh"), "{error}");

        write_config(
            &paths,
            "provision-scripts/c.sh",
            "sendit-provision missing.sh\n",
        );
        assert_eq!(
            script_error(&paths, &rust),
            "the custom script c.sh calls `sendit-provision missing.sh`, but there is no \
             ~/.config/sendit/provision-scripts/missing.sh"
        );
        // Only `*.sh` files that aren't hidden can be called.
        write_config(
            &paths,
            "provision-scripts/c.sh",
            "sendit-provision notes.txt\n",
        );
        write_config(&paths, "provision-scripts/notes.txt", "");
        assert!(script_error(&paths, &rust).contains("there is no"));
    }

    #[test]
    fn calls_scripts_only_by_literal_file_names() {
        let home = TempDir::new("script-names");
        let paths = Paths::new(home.path().to_path_buf());
        let rust = image("rust");
        // Files that a name with a path could reach.
        write_config(&paths, "secret.sh", "");
        write_config(&paths, "provision-scripts/sub.sh", "");
        write_config(&paths, "provision-scripts/.hidden.sh", "");
        for (call, reason) in [
            ("sendit-provision ../secret.sh", "without a directory"),
            ("sendit-provision ./sub.sh", "without a directory"),
            ("sendit-provision /etc/passwd", "without a directory"),
            ("sendit-provision ..", "can't start with a dot"),
            ("sendit-provision .hidden.sh", "can't start with a dot"),
            ("sendit-provision sub\u{1b}.sh", "control characters"),
            ("sendit-provision \"$x\"", "literal file name"),
            ("sendit-provision $x", "literal file name"),
            ("sendit-provision 'sub.sh'", "literal file name"),
            ("sendit-provision `echo sub.sh`", "literal file name"),
            ("sendit-provision sub*.sh", "literal file name"),
            ("sendit-provision", "names no script"),
            ("x && sendit-provision; true", "names no script"),
        ] {
            write_config(&paths, "images/rust.sh", &format!("echo\n{call}\n"));
            let error = script_error(&paths, &rust);
            assert!(
                error.contains(reason) && error.contains("images/rust.sh"),
                "{call}: {error}"
            );
        }
        // The same goes for the scripts it calls.
        write_config(&paths, "images/rust.sh", "sendit-provision sub.sh\n");
        write_config(
            &paths,
            "provision-scripts/sub.sh",
            "sendit-provision ../secret.sh",
        );
        let error = script_error(&paths, &rust);
        assert!(error.contains("in the custom script sub.sh"), "{error}");
    }

    #[test]
    fn refuses_subfolders_of_shared_scripts() {
        let home = TempDir::new("script-subfolders");
        let paths = Paths::new(home.path().to_path_buf());
        // A repository's metadata is fine.
        fs::create_dir_all(paths.provision_scripts_dir().join(".git")).unwrap();
        assert!(custom_scripts(&paths, &image("rust")).unwrap().is_empty());
        // An image's own scripts in the old layout, even without image files.
        write_config(&paths, "provision-scripts/rust/10-cargo.sh", "cargo");
        assert_eq!(
            script_error(&paths, &ImageName::default()),
            "~/.config/sendit/provision-scripts/rust/: subfolders are no longer supported; \
             images are defined by ~/.config/sendit/images/NAME.sh (see the CHANGELOG)"
        );
    }

    #[test]
    fn notices_changes_to_the_scripts_that_run() {
        let home = TempDir::new("script-changes");
        let paths = Paths::new(home.path().to_path_buf());
        let rust = image("rust");
        write_config(&paths, "images/rust.sh", "sendit-provision a.sh\n");
        write_config(&paths, "provision-scripts/a.sh", "sendit-provision b.sh\n");
        write_config(&paths, "provision-scripts/b.sh", "echo b\n");
        write_config(&paths, "provision-scripts/unused.sh", "echo unused\n");
        let base = VmDir::new(paths.image_dir(&rust));
        fs::create_dir_all(base.path()).unwrap();
        let marker = Marker {
            revision: BASE_REVISION,
            custom_scripts: custom_scripts(&paths, &rust)
                .unwrap()
                .iter()
                .map(CustomScript::stamp)
                .collect(),
        };
        let names: Vec<_> = marker.custom_scripts.iter().map(|s| &s.name[..]).collect();
        assert_eq!(names, ["images/rust.sh", "a.sh", "b.sh"]);
        fs::write(marker_file(&base), toml::to_string(&marker).unwrap()).unwrap();
        let changed = || {
            matches!(
                base_state(&paths, &rust).unwrap(),
                BaseState::ScriptsChanged
            )
        };
        assert!(!changed());

        write_config(&paths, "provision-scripts/unused.sh", "echo other\n");
        assert!(!changed());
        write_config(&paths, "provision-scripts/b.sh", "# A comment.\necho b\n");
        assert!(!changed());
        write_config(&paths, "provision-scripts/b.sh", "echo B\n");
        assert!(changed());
        write_config(&paths, "provision-scripts/b.sh", "echo b\n");
        assert!(!changed());
        // So does the order they run in.
        write_config(
            &paths,
            "images/rust.sh",
            "sendit-provision b.sh\nsendit-provision a.sh\n",
        );
        assert!(changed());
        fs::remove_file(paths.image_file(&rust)).unwrap();
        assert!(changed());
        // So do scripts with a mistake, which only `provision` reports.
        write_config(&paths, "images/rust.sh", "sendit-provision missing.sh\n");
        assert!(changed());
    }

    #[test]
    fn describes_custom_scripts() {
        let home = TempDir::new("describe");
        let paths = Paths::new(home.path().to_path_buf());
        let rust = image("rust");
        let describe = || describe_scripts(&paths, &rust, &custom_scripts(&paths, &rust).unwrap());

        write_config(&paths, "images/rust.sh", "cargo install ripgrep\n");
        assert_eq!(describe(), "Image file: ~/.config/sendit/images/rust.sh");

        write_config(
            &paths,
            "images/rust.sh",
            "sendit-provision rust.sh\nsendit-provision helix.sh\n",
        );
        write_config(&paths, "provision-scripts/rust.sh", "rustup");
        write_config(
            &paths,
            "provision-scripts/helix.sh",
            "sendit-provision grammars.sh",
        );
        write_config(&paths, "provision-scripts/grammars.sh", "hx");
        assert_eq!(
            describe(),
            "Image file: ~/.config/sendit/images/rust.sh\n\
             Scripts it calls, from ~/.config/sendit/provision-scripts:\n  \
             rust.sh\n  \
             helix.sh\n  \
             grammars.sh"
        );
    }

    #[test]
    fn stamps_scripts_without_their_header() {
        let stamp = |content: &str| {
            CustomScript {
                name: "tool.sh".into(),
                file: "tool.sh".into(),
                content: content.into(),
            }
            .stamp()
            .sha256
        };
        let body = "set -eu\n# a comment, or maybe not:\ncat <<EOF\n# config\nEOF\n";
        let original = stamp(&format!("#!/bin/sh\n# Installs a tool.\n\n{body}"));
        assert_eq!(stamp(body), original);
        assert_eq!(
            stamp(&format!(
                "#!/bin/bash\n  # Installs a tool.\r\n# sendit-mount: ~/x\n{body}"
            )),
            original
        );
        // Below the header, every byte counts.
        assert_ne!(stamp(&body.replace("# config", "# other")), original);
        assert_ne!(stamp(&format!("{body}\n")), original);
        assert_ne!(stamp(&format!("echo\n{body}")), original);
        // A script of comments only does nothing.
        assert_eq!(stamp("# just notes\n"), stamp(""));
    }

    #[test]
    fn reads_mounts_from_the_top_of_scripts() {
        let home = TempDir::new("script-mounts");
        let paths = Paths::new(home.path().to_path_buf());
        let rust = image("rust");
        let mounts = || script_mounts(&paths, &rust);
        let write = |path: &str, content: &str| write_config(&paths, path, content);

        write(
            "images/rust.sh",
            "#!/bin/bash\n\
             # Rust, with a shared registry.\n\
             # sendit-mount: ~/.cargo/registry:/home/dev/.cargo/registry\n\
             \n  # sendit-mount: /opt/data:rw  \n\
             sendit-provision notes.sh\n\
             # sendit-mount: ~/after-the-first-command\n",
        );
        write("provision-scripts/notes.sh", "# sendit-mount: ~/notes\n");
        write("provision-scripts/unused.sh", "# sendit-mount: ~/unused\n");
        write("images/go.sh", "# sendit-mount: ~/go\n");
        let described: Vec<_> = mounts()
            .unwrap()
            .iter()
            .map(|(script, spec)| format!("{script} {spec}"))
            .collect();
        assert_eq!(
            described,
            [
                "images/rust.sh ~/.cargo/registry:/home/dev/.cargo/registry:ro",
                "images/rust.sh /opt/data:rw",
                "notes.sh ~/notes:ro",
            ]
        );
        assert_eq!(
            describe_mounts(&mounts().unwrap()),
            "VMs made from the image share what the scripts ask for:\n  \
             ~/.cargo/registry:/home/dev/.cargo/registry:ro  (images/rust.sh)\n  \
             /opt/data:rw  (images/rust.sh)\n  \
             ~/notes:ro  (notes.sh)"
        );

        // Mistakes stop a build and name the script, and so does a relative
        // host path, which would depend on where `sendit run` happens to be
        // started. VMs go without the scripts' shares then.
        let build_error = || match custom_scripts(&paths, &rust).and_then(|s| mounts_of(&s)) {
            Ok(_) => panic!("no error"),
            Err(e) => format!("{e:#}"),
        };
        write("provision-scripts/notes.sh", "# sendit-mount: a:b:c:d\n");
        let error = build_error();
        assert!(error.contains("custom script notes.sh"), "{error}");
        assert!(mounts().unwrap().is_empty());
        write("provision-scripts/notes.sh", "# sendit-mount: data:/data\n");
        let error = build_error();
        assert!(
            error.contains("custom script notes.sh") && error.contains("absolute"),
            "{error}"
        );
        assert!(mounts().unwrap().is_empty());
    }

    #[test]
    fn lists_images() {
        let home = TempDir::new("images");
        let paths = Paths::new(home.path().to_path_buf());
        let config = Config::default();
        assert_eq!(images(&paths, &config).unwrap(), [ImageName::default()]);

        let files = paths.image_files_dir();
        fs::create_dir_all(files.join("dir.sh")).unwrap();
        for name in ["rust.sh", ".hidden.sh", "notes.txt"] {
            fs::write(files.join(name), "").unwrap();
        }
        // Only an image file makes an image, not shared scripts. One that
        // is a symlink to nothing does, for `--all` to tell about it.
        write_config(&paths, "provision-scripts/node.sh", "");
        std::os::unix::fs::symlink(home.path().join("gone.sh"), files.join("gone.sh")).unwrap();
        fs::create_dir_all(paths.image_dir(&image("go"))).unwrap();
        fs::create_dir_all(paths.image_partial_dir(&image("zig"))).unwrap();
        let config: Config = toml::from_str("image = \"zig\"").unwrap();
        assert_eq!(
            images(&paths, &config).unwrap(),
            [
                image("default"),
                image("go"),
                image("gone"),
                image("rust"),
                image("zig")
            ]
        );
        assert!(dangling_image_file(&paths, &image("gone")).is_some());
        assert!(dangling_image_file(&paths, &image("rust")).is_none());
        assert!(custom_scripts(&paths, &image("gone")).unwrap().is_empty());

        // The name becomes part of paths, so it must be a valid one.
        fs::write(files.join("Rust 2.sh"), "").unwrap();
        let error = format!("{:#}", images(&paths, &config).unwrap_err());
        assert!(
            error.contains("image file ~/.config/sendit/images/Rust 2.sh")
                && error.contains("invalid image name \"Rust 2\""),
            "{error}"
        );
    }
}
