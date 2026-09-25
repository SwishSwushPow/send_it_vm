//! Building the shared base image that project VMs are cloned from.
//!
//! The Debian cloud image boots with a cloud-init seed ISO attached, runs
//! `assets/provision.sh` and powers itself off. The image is built in
//! `base.partial/` and only replaces `base/` once it succeeded.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::config::{ByteSize, Config, DEFAULT_CPUS, DEFAULT_MEMORY};
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
/// identifiable by MAC address.
pub const BASE_REVISION: u32 = 3;

/// Printed by provision.sh as its last line when it succeeded.
const SUCCESS_SENTINEL: &str = "SENDIT_PROVISION_OK";

/// Written into the base directory once provisioning has succeeded.
#[derive(Serialize)]
struct Marker<'a> {
    revision: u32,
    send_it_version: &'a str,
    debian_image_sha512: &'a str,
    provisioned_at_unix: u64,
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

pub fn provision(paths: &Paths, config: &Config, force: bool) -> Result<()> {
    if marker(paths).exists() && !force {
        eprintln!(
            "The base image in {} is already provisioned. Use --force to rebuild it.",
            paths.display(&paths.base_dir())
        );
        return Ok(());
    }

    let image = image::debian_image(paths)?;
    let public_key = ssh_public_key(paths)?;

    let work = paths.provision_dir();
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work)?;
    let seed = build_seed_iso(&work, &public_key)?;

    let partial = VmDir::new(paths.base_partial_dir());
    let _ = fs::remove_dir_all(partial.path());
    fs::create_dir_all(partial.path())?;
    image::clone_file(&image.path, &partial.disk())?;
    image::grow_disk(&partial.disk(), BASE_DISK_SIZE)?;

    let log = work.join("console.log");
    eprintln!(
        "Provisioning the base image. This takes a few minutes; the console is logged to {}.",
        paths.display(&log)
    );
    let spec = VmSpec {
        cpus: config.defaults.cpus.unwrap_or(DEFAULT_CPUS),
        memory: config.defaults.memory.unwrap_or(DEFAULT_MEMORY),
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
        send_it_version: env!("CARGO_PKG_VERSION"),
        debian_image_sha512: &image.sha512,
        provisioned_at_unix: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
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

/// Returns the public key of send_it's SSH keypair, generating it first if needed.
fn ssh_public_key(paths: &Paths) -> Result<String> {
    let dir = paths.ssh_dir();
    let key = dir.join("id_ed25519");
    if !key.exists() {
        fs::create_dir_all(&dir)?;
        let status = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "send_it", "-f"])
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
fn build_seed_iso(work: &Path, public_key: &str) -> Result<PathBuf> {
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
    }
}
