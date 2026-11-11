//! What the app shows, read through sendit's library off the main thread.

use std::path::Path;

use anyhow::Result;
use send_it_vm::{
    Config, ImageRow, Paths, PrunePlan, PruneScope, Settings, StatusReport, VmName, VmRow,
};

/// The VMs, images and the like at one moment.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub vms: Vec<VmRow>,
    pub vms_error: Option<String>,
    pub images: Vec<ImageRow>,
    pub images_error: Option<String>,
    pub ssh_set_up: bool,
}

pub fn take(paths: &Paths) -> Snapshot {
    let (vms, vms_error) = match send_it_vm::list_rows(paths) {
        Ok(vms) => (vms, None),
        Err(e) => (Vec::new(), Some(format!("{e:#}"))),
    };
    // Counted from the rows just read, rather than reading every VM again.
    let vm_images = vms
        .iter()
        .filter_map(|row| Some(row.info.as_ref()?.image.clone()));
    let (images, images_error) = match Config::load(paths)
        .and_then(|config| send_it_vm::image_rows(paths, &config, vm_images))
    {
        Ok(images) => (images, None),
        Err(e) => (Vec::new(), Some(format!("{e:#}"))),
    };
    Snapshot {
        vms,
        vms_error,
        images,
        images_error,
        ssh_set_up: send_it_vm::ssh_is_set_up(paths),
    }
}

/// `sendit -n NAME status`, as data.
pub fn status(paths: &Paths, name: &VmName) -> Result<StatusReport> {
    let project = send_it_vm::find(paths, name)?;
    let settings =
        Config::load(paths)?.resolve(paths, &project, &Settings::default(), paths.home(), false)?;
    send_it_vm::status_report(paths, &project, &settings)
}

/// What `sendit status` would show for a project directory without a VM
/// yet, e.g. before a new VM is started.
pub fn status_of_dir(paths: &Paths, dir: &Path) -> Result<StatusReport> {
    let project = send_it_vm::Project::at(dir)?;
    let settings =
        Config::load(paths)?.resolve(paths, &project, &Settings::default(), paths.home(), false)?;
    send_it_vm::status_report(paths, &project, &settings)
}

pub fn prune_plan(paths: &Paths, scope: PruneScope) -> Result<PrunePlan> {
    send_it_vm::prune_plan(paths, scope)
}
