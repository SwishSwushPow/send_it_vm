//! What the app shows, read through sendit's library off the main thread.

use std::path::Path;

use anyhow::Result;
use send_it_vm::{
    Config, ImageRow, Paths, Project, PrunePlan, PruneScope, Settings, StatusReport, VmName, VmRow,
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
    let listed = send_it_vm::list_rows(paths);
    // Counted from the rows just read, rather than reading every VM again,
    // unless the listing failed.
    let vm_images = match &listed {
        Ok(rows) => Ok(rows
            .iter()
            .filter_map(|row| Some(row.info.as_ref()?.image.clone()))
            .collect()),
        Err(_) => send_it_vm::vm_images(paths),
    };
    let (vms, vms_error) = match listed {
        Ok(vms) => (vms, None),
        Err(e) => (Vec::new(), Some(format!("{e:#}"))),
    };
    let (images, images_error) = match Config::load(paths)
        .and_then(|config| send_it_vm::image_rows(paths, &config, vm_images?))
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
    status_of(paths, &send_it_vm::find(paths, name)?)
}

/// What `sendit status` would show for a project directory without a VM
/// yet, e.g. before a new VM is started.
pub fn status_of_dir(paths: &Paths, dir: &Path) -> Result<StatusReport> {
    status_of(paths, &Project::at(dir)?)
}

fn status_of(paths: &Paths, project: &Project) -> Result<StatusReport> {
    let settings =
        Config::load(paths)?.resolve(paths, project, &Settings::default(), paths.home(), false)?;
    send_it_vm::status_report(paths, project, &settings)
}

pub fn prune_plan(paths: &Paths, scope: PruneScope) -> Result<PrunePlan> {
    send_it_vm::prune_plan(paths, scope)
}
