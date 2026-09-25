//! Building the named base images that project VMs are cloned from.
//!
//! The Debian cloud image boots with a cloud-init seed ISO attached, runs
//! `assets/provision.sh` and powers itself off. An image is built in
//! `images/.<name>.partial/` and only replaces `images/<name>/` once it
//! succeeded.
//!
//! Custom scripts travel on the seed ISO too and run after the built-in
//! provisioning, e.g. to install more tools: those in
//! `~/.config/sendit/provision-scripts/` for every image, and those in its
//! `<name>/` subdirectory for that image only.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{ByteSize, ProvisionSettings};
use crate::image;
use crate::paths::{ImageName, Paths};
use crate::util;
use crate::vm::{self, VmDir, VmSpec};

/// Logical size of the base disk. Project VMs grow their clone further.
const BASE_DISK_SIZE: ByteSize = ByteSize::gib(8);

const USER_DATA: &str = include_str!("assets/user-data.yaml");
const PROVISION_SCRIPT: &str = include_str!("assets/provision.sh");
const BANNER: &str = include_str!("assets/banner.txt");

/// Bumped whenever the base image changes in a way the host code relies on.
/// Revision 2 added the sendit-mounts service, 3 made DHCP leases
/// identifiable by MAC address, 4 shuts the VM down on console logout, 5
/// applies the host terminal's size to the console, 6 its type, 7 takes
/// the guest user's sudo rights and lets the host log in as root.
pub const BASE_REVISION: u32 = 7;

/// Printed by provision.sh as its last line when it succeeded.
const SUCCESS_SENTINEL: &str = "SENDIT_PROVISION_OK";

/// Written into the base directory once provisioning has succeeded.
#[derive(Deserialize, Serialize)]
struct Marker {
    /// Bases from before revisions were recorded are revision 1.
    #[serde(default = "first_revision")]
    revision: u32,
    #[serde(default)]
    sendit_version: String,
    #[serde(default)]
    debian_image_sha512: String,
    #[serde(default)]
    provisioned_at_unix: u64,
    #[serde(default)]
    custom_scripts: Vec<ScriptStamp>,
}

/// A custom provisioning script the base image was built with.
#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
struct ScriptStamp {
    name: String,
    sha256: String,
}

/// A custom provisioning script from `Paths::provision_scripts_dir`, or
/// from `Paths::image_scripts_dir` of the image being built.
struct CustomScript {
    /// Relative to `Paths::provision_scripts_dir`, e.g. `10-apt.sh` or
    /// `rust/20-cargo.sh`.
    name: String,
    content: Vec<u8>,
}

impl CustomScript {
    fn stamp(&self) -> ScriptStamp {
        ScriptStamp {
            name: self.name.clone(),
            sha256: hex::encode(Sha256::digest(&self.content)),
        }
    }
}

fn marker_file(dir: &VmDir) -> PathBuf {
    dir.path().join("provisioned.toml")
}

pub fn first_revision() -> u32 {
    1
}

pub enum BaseState {
    /// Not provisioned yet.
    Missing,
    /// Older than `BASE_REVISION`; project VMs can't be created from it.
    Outdated,
    /// Usable, but the custom provisioning scripts were added, removed or
    /// edited since it was provisioned.
    ScriptsChanged,
    Current,
}

pub fn base_state(paths: &Paths, image: &ImageName) -> Result<BaseState> {
    let base = VmDir::new(paths.image_dir(image));
    let Some(marker) = util::read_toml::<Marker>(&marker_file(&base))? else {
        return Ok(BaseState::Missing);
    };
    if marker.revision < BASE_REVISION {
        return Ok(BaseState::Outdated);
    }
    let current: Vec<_> = custom_scripts(paths, image)?
        .iter()
        .map(CustomScript::stamp)
        .collect();
    Ok(if marker.custom_scripts != current {
        BaseState::ScriptsChanged
    } else {
        BaseState::Current
    })
}

/// All images: `default`, and every image that has custom scripts of its
/// own or has been provisioned, in name order.
pub fn images(paths: &Paths) -> Result<Vec<ImageName>> {
    let mut images = BTreeSet::from([ImageName::default()]);
    for dir in [paths.provision_scripts_dir(), paths.images_dir()] {
        let Some(entries) = util::if_exists(fs::read_dir(&dir))
            .with_context(|| format!("reading {}", dir.display()))?
        else {
            continue;
        };
        for entry in entries {
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
    }
    Ok(images.into_iter().collect())
}

/// Moves the single base image of sendit versions before named images to
/// the `default` image.
pub fn migrate_legacy_base(paths: &Paths) -> Result<()> {
    let legacy = paths.legacy_base_dir();
    let default = paths.image_dir(&ImageName::default());
    if !legacy.exists() || default.exists() {
        return Ok(());
    }
    fs::create_dir_all(paths.images_dir())?;
    // Another sendit process may have moved it meanwhile.
    util::if_exists(fs::rename(&legacy, &default))
        .with_context(|| format!("moving {} to {}", legacy.display(), default.display()))?;
    Ok(())
}

/// The custom scripts for `image`, in file name order: the shared ones and
/// the image's own, which replace shared ones of the same file name.
fn custom_scripts(paths: &Paths, image: &ImageName) -> Result<Vec<CustomScript>> {
    let mut scripts = BTreeMap::new();
    for (file, content) in script_files(&paths.provision_scripts_dir())? {
        let name = file.clone();
        scripts.insert(file, CustomScript { name, content });
    }
    for (file, content) in script_files(&paths.image_scripts_dir(image))? {
        let name = format!("{image}/{file}");
        scripts.insert(file, CustomScript { name, content });
    }
    Ok(scripts.into_values().collect())
}

/// The names and contents of the `*.sh` files in `dir`.
fn script_files(dir: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    let Some(entries) =
        util::if_exists(fs::read_dir(dir)).with_context(|| format!("reading {}", dir.display()))?
    else {
        return Ok(Vec::new());
    };
    let mut scripts = Vec::new();
    for entry in entries {
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

pub fn provision(
    paths: &Paths,
    name: &ImageName,
    settings: &ProvisionSettings,
    force: bool,
) -> Result<()> {
    let base = paths.image_dir(name);
    if marker_file(&VmDir::new(base.clone())).exists() && !force {
        eprintln!(
            "The {name} image in {} is already provisioned. Use --force to rebuild it.",
            paths.display(&base)
        );
        return Ok(());
    }

    let image = image::debian_image(paths)?;
    let public_key = ssh_public_key(&paths.ssh_key())?;
    let root_public_key = ssh_public_key(&paths.root_ssh_key())?;
    let scripts = custom_scripts(paths, name)?;

    let work = paths.provision_dir(name);
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work)?;
    let seed = build_seed_iso(&work, &public_key, &root_public_key, &scripts)?;

    let partial = VmDir::new(paths.image_partial_dir(name));
    let _ = fs::remove_dir_all(partial.path());
    fs::create_dir_all(partial.path())?;
    image::clone_file(&image.path, &partial.disk())?;
    image::grow_disk(&partial.disk(), BASE_DISK_SIZE)?;

    let log = work.join("console.log");
    eprintln!(
        "Provisioning the {name} image ({} CPUs, {} memory). This takes a few minutes; \
         the console is logged to {}.",
        settings.cpus,
        settings.memory,
        paths.display(&log)
    );
    if !scripts.is_empty() {
        let names: Vec<_> = scripts.iter().map(|script| script.name.as_str()).collect();
        eprintln!(
            "Custom scripts from {}: {}",
            paths.display(&paths.provision_scripts_dir()),
            names.join(", ")
        );
    }
    let spec = VmSpec {
        cpus: settings.cpus,
        memory: settings.memory,
        shares: Vec::new(),
        seed: Some(seed),
        provision_log: Some(log.clone()),
    };
    vm::run(&partial, &spec)?;

    let output = fs::read(&log).with_context(|| format!("reading {}", log.display()))?;
    ensure!(
        String::from_utf8_lossy(&output).contains(SUCCESS_SENTINEL),
        "provisioning failed; see {} for details",
        paths.display(&log)
    );

    let marker = toml::to_string(&Marker {
        revision: BASE_REVISION,
        sendit_version: env!("CARGO_PKG_VERSION").into(),
        debian_image_sha512: image.sha512,
        provisioned_at_unix: util::unix_now()?,
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

/// Returns the public key of the SSH keypair `key`, generating it first if
/// needed.
fn ssh_public_key(key: &Path) -> Result<String> {
    if !key.exists() {
        if let Some(dir) = key.parent() {
            fs::create_dir_all(dir)?;
        }
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
) -> Result<PathBuf> {
    ensure!(
        ![public_key, root_public_key]
            .iter()
            .any(|key| key.contains(['\n', '"'])),
        "unexpected SSH public key format"
    );
    let dir = work.join("seed");
    fs::create_dir_all(&dir)?;
    let instance_id = format!("sendit-{}", util::unix_now()?);
    fs::write(
        dir.join("meta-data"),
        format!("instance-id: {instance_id}\nlocal-hostname: sendit\n"),
    )?;
    fs::write(
        dir.join("user-data"),
        USER_DATA.replace("@SSH_PUBLIC_KEY@", public_key),
    )?;
    fs::write(dir.join("root.pub"), format!("{root_public_key}\n"))?;
    fs::write(dir.join("provision.sh"), PROVISION_SCRIPT)?;
    fs::write(dir.join("banner.txt"), BANNER)?;
    // Numbered file names survive the ISO's file name limits; `list` maps
    // them back to the original names for the log.
    let custom = dir.join("custom");
    fs::create_dir_all(&custom)?;
    let mut list = String::new();
    for (i, script) in scripts.iter().enumerate() {
        let file = format!("{:03}.sh", i + 1);
        fs::write(custom.join(&file), &script.content)?;
        writeln!(list, "{file} {}", script.name)?;
    }
    fs::write(custom.join("list"), list)?;

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
        assert!(PROVISION_SCRIPT.contains("SENDIT_PROVISION_${status}"));
        assert!(PROVISION_SCRIPT.contains("$seed/custom/list"));
        assert!(USER_DATA.contains(&format!("- name: {GUEST_USER}\n")));
        assert!(PROVISION_SCRIPT.contains(&format!("\nuser={GUEST_USER}\n")));
    }

    fn image(name: &str) -> ImageName {
        name.parse().unwrap()
    }

    #[test]
    fn collects_custom_scripts_in_name_order() {
        let home = TempDir::new("scripts");
        let paths = Paths::new(home.path().to_path_buf());
        let rust = image("rust");
        assert!(custom_scripts(&paths, &rust).unwrap().is_empty());

        let dir = paths.provision_scripts_dir();
        fs::create_dir_all(dir.join("30-dir.sh")).unwrap();
        for name in ["20-node.sh", "10-editors.sh", ".hidden.sh", "notes.txt"] {
            fs::write(dir.join(name), name).unwrap();
        }
        let names = |image: &ImageName| -> Vec<(String, String)> {
            custom_scripts(&paths, image)
                .unwrap()
                .into_iter()
                .map(|s| (s.name, String::from_utf8(s.content).unwrap()))
                .collect()
        };
        let shared = |name: &str| (name.to_string(), name.to_string());
        assert_eq!(
            names(&rust),
            [shared("10-editors.sh"), shared("20-node.sh")]
        );

        // The image's own scripts join in name order and replace shared
        // ones of the same name; other images don't see them.
        let own = paths.image_scripts_dir(&rust);
        fs::create_dir_all(&own).unwrap();
        fs::write(own.join("15-cargo.sh"), "cargo").unwrap();
        fs::write(own.join("20-node.sh"), "no node").unwrap();
        assert_eq!(
            names(&rust),
            [
                shared("10-editors.sh"),
                ("rust/15-cargo.sh".into(), "cargo".into()),
                ("rust/20-node.sh".into(), "no node".into()),
            ]
        );
        assert_eq!(
            names(&ImageName::default()),
            [shared("10-editors.sh"), shared("20-node.sh")]
        );
    }

    #[test]
    fn lists_images() {
        let home = TempDir::new("images");
        let paths = Paths::new(home.path().to_path_buf());
        assert_eq!(images(&paths).unwrap(), [ImageName::default()]);

        let scripts = paths.provision_scripts_dir();
        fs::create_dir_all(scripts.join("rust")).unwrap();
        fs::create_dir_all(scripts.join("Not An Image")).unwrap();
        fs::write(scripts.join("node"), "a file").unwrap();
        fs::create_dir_all(paths.image_dir(&image("go"))).unwrap();
        fs::create_dir_all(paths.image_partial_dir(&image("zig"))).unwrap();
        assert_eq!(
            images(&paths).unwrap(),
            [image("default"), image("go"), image("rust")]
        );
    }

    #[test]
    fn migrates_the_legacy_base_image() {
        let home = TempDir::new("migrate");
        let paths = Paths::new(home.path().to_path_buf());
        migrate_legacy_base(&paths).unwrap();
        assert!(!paths.images_dir().exists());

        let legacy = paths.legacy_base_dir();
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("provisioned.toml"), "").unwrap();
        migrate_legacy_base(&paths).unwrap();
        assert!(!legacy.exists());
        let default = paths.image_dir(&ImageName::default());
        assert!(default.join("provisioned.toml").exists());

        // An existing default image is never replaced.
        fs::create_dir_all(&legacy).unwrap();
        migrate_legacy_base(&paths).unwrap();
        assert!(legacy.exists());
    }
}
