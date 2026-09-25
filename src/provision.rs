//! Building the shared base image that project VMs are cloned from.
//!
//! The Debian cloud image boots with a cloud-init seed ISO attached, runs
//! `assets/provision.sh` and powers itself off. The image is built in
//! `base.partial/` and only replaces `base/` once it succeeded.
//!
//! Custom scripts from `~/.config/sendit/provision-scripts/` travel on the
//! seed ISO too and run after the built-in provisioning, e.g. to install more
//! tools.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{ByteSize, ProvisionSettings};
use crate::image;
use crate::paths::Paths;
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

/// A custom provisioning script from `Paths::provision_scripts_dir`.
struct CustomScript {
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

pub fn base_state(paths: &Paths) -> Result<BaseState> {
    let base = VmDir::new(paths.base_dir());
    let Some(marker) = util::read_toml::<Marker>(&marker_file(&base))? else {
        return Ok(BaseState::Missing);
    };
    if marker.revision < BASE_REVISION {
        return Ok(BaseState::Outdated);
    }
    let current: Vec<_> = custom_scripts(paths)?
        .iter()
        .map(CustomScript::stamp)
        .collect();
    Ok(if marker.custom_scripts != current {
        BaseState::ScriptsChanged
    } else {
        BaseState::Current
    })
}

/// The `*.sh` files in the custom scripts directory, in name order.
fn custom_scripts(paths: &Paths) -> Result<Vec<CustomScript>> {
    let dir = paths.provision_scripts_dir();
    let Some(entries) = util::if_exists(fs::read_dir(&dir))
        .with_context(|| format!("reading {}", dir.display()))?
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
        scripts.push(CustomScript {
            name: name.to_string(),
            content,
        });
    }
    scripts.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(scripts)
}

pub fn provision(paths: &Paths, settings: &ProvisionSettings, force: bool) -> Result<()> {
    if marker_file(&VmDir::new(paths.base_dir())).exists() && !force {
        eprintln!(
            "The base image in {} is already provisioned. Use --force to rebuild it.",
            paths.display(&paths.base_dir())
        );
        return Ok(());
    }

    let image = image::debian_image(paths)?;
    let public_key = ssh_public_key(&paths.ssh_key())?;
    let root_public_key = ssh_public_key(&paths.root_ssh_key())?;
    let scripts = custom_scripts(paths)?;

    let work = paths.provision_dir();
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work)?;
    let seed = build_seed_iso(&work, &public_key, &root_public_key, &scripts)?;

    let partial = VmDir::new(paths.base_partial_dir());
    let _ = fs::remove_dir_all(partial.path());
    fs::create_dir_all(partial.path())?;
    image::clone_file(&image.path, &partial.disk())?;
    image::grow_disk(&partial.disk(), BASE_DISK_SIZE)?;

    let log = work.join("console.log");
    eprintln!(
        "Provisioning the base image ({} CPUs, {} memory). This takes a few minutes; \
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
        contains(&output, SUCCESS_SENTINEL.as_bytes()),
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

    let base = paths.base_dir();
    if base.exists() {
        fs::remove_dir_all(&base).with_context(|| format!("removing {}", base.display()))?;
    }
    fs::rename(partial.path(), &base)?;
    eprintln!("Base image ready in {}.", paths.display(&base));
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
        list.push_str(&format!("{file} {}\n", script.name));
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

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    #[test]
    fn collects_custom_scripts_in_name_order() {
        let home = std::env::temp_dir().join(format!("sendit-scripts-{}", std::process::id()));
        let paths = Paths::new(home.clone());
        assert!(custom_scripts(&paths).unwrap().is_empty());

        let dir = paths.provision_scripts_dir();
        fs::create_dir_all(dir.join("30-dir.sh")).unwrap();
        for name in ["20-node.sh", "10-editors.sh", ".hidden.sh", "notes.txt"] {
            fs::write(dir.join(name), name).unwrap();
        }
        let scripts = custom_scripts(&paths).unwrap();
        fs::remove_dir_all(&home).unwrap();

        let names: Vec<_> = scripts.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["10-editors.sh", "20-node.sh"]);
        assert_eq!(scripts[0].content, b"10-editors.sh");
    }
}
