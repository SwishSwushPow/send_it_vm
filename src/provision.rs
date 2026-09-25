//! Building the shared base image that project VMs are cloned from.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::config::{ByteSize, Config, DEFAULT_CPUS, DEFAULT_MEMORY};
use crate::image;
use crate::paths::Paths;
use crate::vm::{self, VmDir, VmSpec};

/// Logical size of the base disk. Project VMs grow their clone further.
const BASE_DISK_SIZE: ByteSize = ByteSize::gib(8);

/// Written once provisioning has completed successfully.
pub fn marker(paths: &Paths) -> PathBuf {
    paths.base_dir().join("provisioned.json")
}

pub fn provision(paths: &Paths, config: &Config, force: bool) -> Result<()> {
    let base = VmDir::new(paths.base_dir());
    if force && base.path().exists() {
        fs::remove_dir_all(base.path())
            .with_context(|| format!("removing {}", base.path().display()))?;
    }

    let image = image::debian_image(paths)?;
    if !base.disk().exists() {
        fs::create_dir_all(base.path())?;
        image::clone_file(&image, &base.disk())?;
        image::grow_disk(&base.disk(), BASE_DISK_SIZE)?;
    }

    let spec = VmSpec {
        cpus: config.defaults.cpus.unwrap_or(DEFAULT_CPUS),
        memory: config.defaults.memory.unwrap_or(DEFAULT_MEMORY),
    };
    vm::run(&base, &spec)
}
