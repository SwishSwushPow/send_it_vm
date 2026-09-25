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
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{ByteSize, ProvisionSettings};
use crate::image;
use crate::paths::Paths;
use crate::vm::{self, VmDir, VmSpec};

/// Logical size of the base disk. Project VMs grow their clone further.
const BASE_DISK_SIZE: ByteSize = ByteSize::gib(8);

const USER_DATA: &str = include_str!("assets/user-data.yaml");
const PROVISION_SCRIPT: &str = include_str!("assets/provision.sh");
const BANNER: &str = include_str!("assets/banner.txt");

/// Bumped whenever the base image changes in a way the host code relies on.
/// Revision 2 added the sendit-mounts service, 3 made DHCP leases
/// identifiable by MAC address, 4 shuts the VM down on console logout, 5
/// applies the host terminal's size to the console.
pub const BASE_REVISION: u32 = 5;

/// Printed by provision.sh as its last line when it succeeded.
const SUCCESS_SENTINEL: &str = "SENDIT_PROVISION_OK";

/// Written into the base directory once provisioning has succeeded.
#[derive(Serialize)]
struct Marker<'a> {
    revision: u32,
    sendit_version: &'a str,
    debian_image_sha512: &'a str,
    provisioned_at_unix: u64,
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

pub fn marker(paths: &Paths) -> PathBuf {
    paths.base_dir().join("provisioned.toml")
}

/// The revision of the provisioned base image; bases from before revisions
/// were recorded are revision 1.
pub fn base_revision(paths: &Paths) -> Result<u32> {
    #[derive(Deserialize)]
    struct Revision {
        #[serde(default = "first_revision")]
        revision: u32,
    }
    let path = marker(paths);
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let marker: Revision =
        toml::from_str(&text).with_context(|| format!("in {}", path.display()))?;
    Ok(marker.revision)
}

pub fn first_revision() -> u32 {
    1
}

/// Whether the custom provisioning scripts were added, removed or edited
/// since the base image was provisioned.
pub fn custom_scripts_changed(paths: &Paths) -> Result<bool> {
    #[derive(Deserialize)]
    struct Scripts {
        #[serde(default)]
        custom_scripts: Vec<ScriptStamp>,
    }
    let path = marker(paths);
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let marker: Scripts =
        toml::from_str(&text).with_context(|| format!("in {}", path.display()))?;
    let current: Vec<_> = custom_scripts(paths)?
        .iter()
        .map(CustomScript::stamp)
        .collect();
    Ok(marker.custom_scripts != current)
}

/// The `*.sh` files in the custom scripts directory, in name order.
fn custom_scripts(paths: &Paths) -> Result<Vec<CustomScript>> {
    let dir = paths.provision_scripts_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
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
    if marker(paths).exists() && !force {
        eprintln!(
            "The base image in {} is already provisioned. Use --force to rebuild it.",
            paths.display(&paths.base_dir())
        );
        return Ok(());
    }

    let image = image::debian_image(paths)?;
    let public_key = ssh_public_key(paths)?;
    let scripts = custom_scripts(paths)?;

    let work = paths.provision_dir();
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work)?;
    let seed = build_seed_iso(&work, &public_key, &scripts)?;

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

    let marker_text = toml::to_string(&Marker {
        revision: BASE_REVISION,
        sendit_version: env!("CARGO_PKG_VERSION"),
        debian_image_sha512: &image.sha512,
        provisioned_at_unix: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        custom_scripts: scripts.iter().map(CustomScript::stamp).collect(),
    })?;
    fs::write(partial.path().join("provisioned.toml"), marker_text)?;

    let base = paths.base_dir();
    if base.exists() {
        fs::remove_dir_all(&base).with_context(|| format!("removing {}", base.display()))?;
    }
    fs::rename(partial.path(), &base)?;
    eprintln!("Base image ready in {}.", paths.display(&base));
    Ok(())
}

/// Returns the public key of sendit's SSH keypair, generating it first if needed.
fn ssh_public_key(paths: &Paths) -> Result<String> {
    let dir = paths.ssh_dir();
    let key = dir.join("id_ed25519");
    if !key.exists() {
        fs::create_dir_all(&dir)?;
        let status = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "sendit", "-f"])
            .arg(&key)
            .status()
            .context("running ssh-keygen")?;
        ensure!(status.success(), "ssh-keygen failed with {status}");
    }
    let public = key.with_extension("pub");
    Ok(fs::read_to_string(&public)
        .with_context(|| format!("reading {}", public.display()))?
        .trim()
        .to_string())
}

/// Writes the NoCloud seed files into `work/seed/` and packs them into an ISO
/// labelled `cidata`.
fn build_seed_iso(work: &Path, public_key: &str, scripts: &[CustomScript]) -> Result<PathBuf> {
    ensure!(
        !public_key.contains(['\n', '"']),
        "unexpected SSH public key format"
    );
    let dir = work.join("seed");
    fs::create_dir_all(&dir)?;
    let instance_id = format!(
        "sendit-{}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs()
    );
    fs::write(
        dir.join("meta-data"),
        format!("instance-id: {instance_id}\nlocal-hostname: sendit\n"),
    )?;
    fs::write(
        dir.join("user-data"),
        USER_DATA.replace("@SSH_PUBLIC_KEY@", public_key),
    )?;
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
    let output = Command::new("hdiutil")
        .args(["makehybrid", "-quiet", "-iso", "-joliet"])
        .args(["-default-volume-name", "cidata", "-o"])
        .arg(&iso)
        .arg(&dir)
        .output()
        .context("running hdiutil")?;
    if !output.status.success() {
        bail!(
            "creating the seed ISO failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
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
