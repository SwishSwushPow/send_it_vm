//! Building the named base images that project VMs are cloned from.
//!
//! The Debian cloud image boots with a cloud-init seed ISO attached, runs
//! `assets/provision.sh` and powers itself off. An image is built in
//! `images/.<name>.partial/` and only replaces `images/<name>/` once it
//! succeeded. One sendit process at a time builds images; others wait.
//!
//! The image's custom scripts (see `scripts`) travel on the seed ISO too and
//! run after the built-in provisioning, e.g. to install more tools.

mod scripts;
mod shell;

use std::collections::BTreeSet;
use std::fmt::Write;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::config::{ByteSize, Config, ProvisionSettings};
use crate::image;
use crate::paths::{ImageName, Paths};
use crate::util;
use crate::vm::{self, ConsoleMode, ProvisionLog, VmDir, VmSpec};
use scripts::{
    CustomScript, ScriptStamp, custom_scripts, dangling_image_file, describe_dangling,
    describe_mounts, describe_scripts, mounts_of,
};
pub use scripts::{ScriptMount, check_no_subfolders};

/// Logical size of the base disk. Project VMs grow their clone further, but
/// can't shrink it, so this is also the smallest disk size they can have.
pub const BASE_DISK_SIZE: ByteSize = ByteSize::gib(16);

const USER_DATA: &str = include_str!("../assets/user-data.yaml");
const PROVISION_SCRIPT: &str = include_str!("../assets/provision.sh");

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

/// Printed by provision.sh as its last line when it succeeded, followed by
/// the build's token.
const SUCCESS_SENTINEL: &str = "SENDIT_PROVISION_OK";

/// Written into the base directory once provisioning has succeeded. Each
/// project VM keeps a copy of the marker of the image it was made from.
#[derive(Deserialize, Serialize)]
struct Marker {
    revision: u32,
    custom_scripts: Vec<ScriptStamp>,
    /// What the scripts asked to share when the image was built: the VMs
    /// made from it share that, whatever the scripts ask for now. Empty for
    /// images built before sendit recorded it.
    #[serde(default)]
    mounts: Vec<ScriptMount>,
    /// When the image was built, in seconds since the Unix epoch, which
    /// tells a VM's build from a later one. None for images built before
    /// sendit recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    built: Option<u64>,
}

fn marker_file(dir: &VmDir) -> PathBuf {
    dir.path().join("provisioned.toml")
}

/// The marker of the `image` image.
pub fn marker(paths: &Paths, image: &ImageName) -> PathBuf {
    marker_file(&VmDir::new(paths.image_dir(image)))
}

/// The mounts recorded in `marker`, an image's marker or a VM's copy of one;
/// none if it doesn't exist.
pub fn marker_mounts(marker: &Path) -> Result<Vec<ScriptMount>> {
    Ok(util::read_toml::<Marker>(marker)?
        .map(|marker| marker.mounts)
        .unwrap_or_default())
}

/// When the build that `marker` records was made (see `Marker::built`);
/// None if it doesn't exist or doesn't say.
pub fn marker_built(marker: &Path) -> Result<Option<u64>> {
    Ok(util::read_toml::<Marker>(marker)?.and_then(|marker| marker.built))
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
    check_no_subfolders(paths)?;
    let base = VmDir::new(paths.image_dir(image));
    let Some(marker) = util::read_toml::<Marker>(&marker_file(&base))? else {
        return Ok(BaseState::Missing);
    };
    if marker.revision < BASE_REVISION {
        return Ok(BaseState::Outdated);
    }
    // Scripts with a mistake have changed since the build, which resolved
    // them. `provision` tells about it.
    let Ok(scripts) = custom_scripts(paths, image) else {
        return Ok(BaseState::ScriptsChanged);
    };
    let current: Vec<_> = scripts.iter().map(CustomScript::stamp).collect();
    let mounts = mounts_of(&scripts).ok();
    Ok(
        if marker.custom_scripts != current || mounts.as_ref() != Some(&marker.mounts) {
            BaseState::ScriptsChanged
        } else {
            BaseState::Current
        },
    )
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
        mounts,
        built: Some(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs()),
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
    use super::scripts::RUN_COMMAND;
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
        assert!(PROVISION_SCRIPT.contains("bash -e -o pipefail -l \"${list%/*}/$file\""));
        assert!(PROVISION_SCRIPT.contains("exec bash -e -o pipefail -l \"$2\"' \\\n"));
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
                mounts: Vec::new(),
                built: None,
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

    pub(super) fn image(name: &str) -> ImageName {
        name.parse().unwrap()
    }

    /// Writes `content` to `path` below `~/.config/sendit`.
    pub(super) fn write_config(paths: &Paths, path: &str, content: &str) {
        let path = paths.config_dir().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn writes_custom_scripts_onto_the_seed() {
        let home = TempDir::new("seed-scripts");
        let script = |name: &str, content: &str| CustomScript {
            name: name.into(),
            file: name.rsplit('/').next().unwrap().into(),
            content: content.into(),
        };
        let scripts = [
            script("images/rust.sh", "sendit-provision rust.sh\n"),
            script("rust.sh", "curl | sh"),
            script("helix.sh", "sendit-provision grammars.sh\n"),
            script("grammars.sh", "hx --grammar fetch"),
            script("ci.sh", "true"),
        ];
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
        let scripts = custom_scripts(&paths, &rust).unwrap();
        let marker = Marker {
            revision: BASE_REVISION,
            custom_scripts: scripts.iter().map(CustomScript::stamp).collect(),
            mounts: mounts_of(&scripts).unwrap(),
            built: None,
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
        // A mount takes effect with the next build, so it counts.
        write_config(
            &paths,
            "provision-scripts/b.sh",
            "# sendit-mount: ~/data\necho b\n",
        );
        assert!(changed());
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
    fn reads_the_mounts_and_build_recorded_in_a_marker() {
        let home = TempDir::new("marker-mounts");
        let file = home.path().join("provisioned.toml");
        assert!(marker_mounts(&file).unwrap().is_empty());
        assert_eq!(marker_built(&file).unwrap(), None);

        let mounts = vec![ScriptMount {
            script: "images/rust.sh".into(),
            mount: "~/.cargo/registry:/home/dev/.cargo/registry"
                .parse()
                .unwrap(),
        }];
        let marker = Marker {
            revision: BASE_REVISION,
            custom_scripts: Vec::new(),
            mounts: mounts.clone(),
            built: Some(1_760_000_000),
        };
        fs::write(&file, toml::to_string(&marker).unwrap()).unwrap();
        assert_eq!(marker_mounts(&file).unwrap(), mounts);
        assert_eq!(marker_built(&file).unwrap(), Some(1_760_000_000));

        // Images built before sendit recorded them have none.
        fs::write(&file, "revision = 10\ncustom_scripts = []\n").unwrap();
        assert!(marker_mounts(&file).unwrap().is_empty());
        assert_eq!(marker_built(&file).unwrap(), None);
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
