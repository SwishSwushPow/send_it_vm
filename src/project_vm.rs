//! Per-project VMs, created from the base image on first `run`.
//!
//! A project VM's disk and EFI variables are APFS clones of the base image's,
//! so creating one is instant and takes no space until the guest writes. The
//! machine identifier and MAC address are not copied: `vm::config` generates
//! new ones on first boot and saves them, so every VM has its own. The VM is
//! built in `.<id>.partial/` and only renamed into place once complete.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::config::{ByteSize, VmSettings};
use crate::image;
use crate::mounts;
use crate::paths::{Paths, Project};
use crate::provision;
use crate::vm::{self, VmDir, VmSpec};

/// Written into a project VM's directory when it is created.
#[derive(Serialize)]
struct Metadata<'a> {
    project_path: &'a Path,
    base_revision: u32,
    send_it_version: &'a str,
    created_at_unix: u64,
}

fn metadata_file(dir: &VmDir) -> PathBuf {
    dir.path().join("project.toml")
}

/// Boots the project's VM, creating it first if needed, and attaches the
/// console until it stops.
pub fn run(paths: &Paths, project: &Project, settings: &VmSettings) -> Result<()> {
    let dir = VmDir::new(paths.vm_dir(project));
    if !dir.path().exists() {
        create(paths, project, &dir)?;
    }
    ensure!(
        base_revision(&dir)? >= provision::BASE_REVISION,
        "{} was created from an older base image that this version of send_it \
         can't run; delete it to start over from the current base image",
        paths.display(dir.path())
    );
    let _lock = lock(&dir)?;
    resize_disk(paths, &dir, settings)?;

    eprintln!(
        "Starting {} ({} CPUs, {} memory, {} disk)",
        paths.display(dir.path()),
        settings.cpus,
        settings.memory,
        settings.disk_size
    );
    let spec = VmSpec {
        cpus: settings.cpus,
        memory: settings.memory,
        shares: mounts::prepare(settings, &dir.path().join("run/meta"))?,
        seed: None,
        provision_log: None,
    };
    vm::run(&dir, &spec)
}

fn create(paths: &Paths, project: &Project, dir: &VmDir) -> Result<()> {
    ensure!(
        provision::marker(paths).exists(),
        "there is no base image yet; run `send_it provision` first"
    );
    ensure!(
        provision::base_revision(paths)? >= provision::BASE_REVISION,
        "the base image is outdated; rebuild it with `send_it provision --force`"
    );
    let base = VmDir::new(paths.base_dir());
    let partial = VmDir::new(paths.vms_dir().join(format!(".{}.partial", project.id)));
    let _ = fs::remove_dir_all(partial.path());
    fs::create_dir_all(partial.path())
        .with_context(|| format!("creating {}", partial.path().display()))?;

    eprintln!("Creating {} from the base image", paths.display(dir.path()));
    image::clone_file(&base.disk(), &partial.disk())?;
    image::clone_file(&base.efi_vars(), &partial.efi_vars())?;
    let metadata = toml::to_string(&Metadata {
        project_path: &project.root,
        base_revision: provision::BASE_REVISION,
        send_it_version: env!("CARGO_PKG_VERSION"),
        created_at_unix: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
    })?;
    fs::write(metadata_file(&partial), metadata)?;

    fs::rename(partial.path(), dir.path())
        .with_context(|| format!("moving the new VM to {}", dir.path().display()))
}

/// The revision of the base image `dir` was cloned from.
fn base_revision(dir: &VmDir) -> Result<u32> {
    #[derive(Deserialize)]
    struct Revision {
        #[serde(default = "provision::first_revision")]
        base_revision: u32,
    }
    let path = metadata_file(dir);
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let metadata: Revision =
        toml::from_str(&text).with_context(|| format!("in {}", path.display()))?;
    Ok(metadata.base_revision)
}

/// Grows the disk to the configured size; the guest grows its root
/// filesystem to match on boot. Disks never shrink.
fn resize_disk(paths: &Paths, dir: &VmDir, settings: &VmSettings) -> Result<()> {
    let current = fs::metadata(dir.disk())
        .with_context(|| format!("reading {}", dir.disk().display()))?
        .len();
    if current > settings.disk_size.0 {
        eprintln!(
            "warning: the disk of {} is already {} and can't shrink to {}; `send_it reset` recreates it",
            paths.display(dir.path()),
            ByteSize(current),
            settings.disk_size
        );
    }
    image::grow_disk(&dir.disk(), settings.disk_size)
}

/// Makes sure only one process runs a VM at a time; two would corrupt its
/// disk. The lock is released when the returned file is closed, even if the
/// process dies.
fn lock(dir: &VmDir) -> Result<File> {
    let run_dir = dir.path().join("run");
    fs::create_dir_all(&run_dir)?;
    let path = run_dir.join("lock");
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(fs::TryLockError::WouldBlock) => bail!("this project's VM is already running"),
        Err(fs::TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("locking {}", path.display()))
        }
    }
}
