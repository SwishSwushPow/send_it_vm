//! Per-project VMs, created from the base image on first `run`.
//!
//! A project VM's disk and EFI variables are APFS clones of the base image's,
//! so creating one is instant and takes no space until the guest writes. The
//! machine identifier and MAC address are not copied: `vm::config` generates
//! new ones on first boot and saves them, so every VM has its own. The VM is
//! built in `.<id>.partial/` and only renamed into place once complete.
//!
//! The VM of a linked git worktree is instead a copy of its main worktree's
//! VM, if that is stopped: it starts with everything installed and cached
//! there. A running VM's disk changes under the copy and the guest's page
//! cache isn't on it yet, so a copy then would only be as good as after a
//! power cut; `run` offers to start from the base image instead.
//!
//! Only one sendit process at a time makes, runs, copies or deletes a VM:
//! it holds the VM's lock, `.<dir>.lock` next to the VM's directory, the
//! whole time. `sendit run` takes it before deciding whether to make the VM
//! and keeps it until the VM has stopped, with its PID in it once the VM
//! starts; that is how the other commands find it.
//!
//! `run --detach` runs the VM in a second `sendit run` process, which it
//! starts in a session of its own, without a terminal, and leaves running
//! once the VM answers over SSH. It passes the VM's lock on to that process
//! rather than letting go of it. It is sendit run again rather than a fork:
//! forking without exec isn't safe once Objective-C is loaded.
//!
//! Each VM has a name of its own, kept in its metadata, by which `-n` finds
//! it from anywhere: its folder's name, or for a linked git worktree, its
//! main worktree's VM's name and the worktree's folder name, with a number
//! added if another VM has that name already. VMs made before sendit named
//! them get one when a command next shows or looks up names, or makes or
//! runs a VM. Names are given out and changed while holding a lock, so no
//! two VMs get the same.
//!
//! This module has the VMs' metadata; the submodules tell which image build
//! they are made from (`origin`), and make (`create`), name (`naming`), run
//! and stop (`run`), lock (`lock`) and reach (`ssh`) them.

mod create;
mod lock;
mod naming;
mod origin;
mod run;
mod ssh;
#[cfg(test)]
mod test_util;

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::{ImageName, Paths, Project, VmName, is_project_id};
use crate::provision;
use crate::util;
use crate::vm::VmDir;
use crate::worktree;
use ssh::quote;

pub use create::{NewVm, create, delete, new_vm};
pub use lock::{BUSY, State, state};
pub use naming::{find, name_vms, rename};
pub use origin::Origin;
pub use run::{Detach, background_project, lock_to_run, run, stop};
pub use ssh::ssh;

/// Written into a project VM's directory when it is created, and again
/// when it gets a name later or is renamed.
#[derive(Debug, Serialize, Deserialize)]
pub struct Metadata {
    pub project_path: PathBuf,
    pub image: ImageName,
    pub base_revision: u32,
    /// The project whose VM this one was made as a copy of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_of: Option<PathBuf>,
    /// The name `-n` finds the VM by. Missing only for VMs made before
    /// sendit named them, until `name_vms` gives them one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<VmName>,
}

impl Metadata {
    /// Whether the VM was created from a base image older than this version
    /// of sendit can run.
    pub fn outdated(&self) -> bool {
        self.base_revision < provision::BASE_REVISION
    }
}

/// The image `project` uses: the `chosen` one, else the one its VM was
/// created from, else, before it has a VM, the one its parent VM was, else
/// `default`.
pub fn image(paths: &Paths, project: &Project, chosen: Option<&ImageName>) -> ImageName {
    if let Some(image) = chosen {
        return image.clone();
    }
    let dir = dir(paths, project);
    let dir = match parent(paths, project) {
        Some(parent) if !dir.path().exists() => parent,
        _ => dir,
    };
    metadata(&dir)
        .map(|metadata| metadata.image)
        .unwrap_or_default()
}

/// The directory of the project's VM.
pub fn dir(paths: &Paths, project: &Project) -> VmDir {
    VmDir::new(paths.vm_dir(project))
}

/// The VM that the project's VM is made as a copy of: its main worktree's,
/// if the project is a linked git worktree and the main worktree has one.
pub fn parent(paths: &Paths, project: &Project) -> Option<VmDir> {
    let main = Project::at(&worktree::main_worktree(&project.root)?).ok()?;
    let dir = dir(paths, &main);
    dir.path().exists().then_some(dir)
}

/// The metadata of the project's parent VM, if it has one that can be read.
pub fn parent_metadata(paths: &Paths, project: &Project) -> Option<Metadata> {
    metadata(&parent(paths, project)?).ok()
}

pub fn metadata(dir: &VmDir) -> Result<Metadata> {
    let path = dir.metadata();
    util::read_toml(&path)?.with_context(|| format!("{} is missing", path.display()))
}

fn write_metadata(dir: &VmDir, metadata: &Metadata) -> Result<()> {
    util::write_atomic(&dir.metadata(), toml::to_string(metadata)?.as_bytes())
}

/// All project VM directories, sorted by name: those named like a project
/// ID. Unfinished `.partial` directories and anything else are skipped.
pub fn all(paths: &Paths) -> Result<Vec<VmDir>> {
    let mut dirs = Vec::new();
    for entry in util::read_dir(&paths.vms_dir())? {
        let entry = entry?;
        let is_vm = entry.file_name().to_str().is_some_and(is_project_id);
        if is_vm && entry.file_type()?.is_dir() {
            dirs.push(VmDir::new(entry.path()));
        }
    }
    dirs.sort_by(|a, b| a.path().cmp(b.path()));
    Ok(dirs)
}

/// The sendit command line for `project`, whose VM is in `dir`, from here,
/// for hints on what to run next. Run elsewhere, e.g. with -n or
/// --worktree, the VM must be given: by its name, or if it has none, e.g.
/// before it is made, by the project directory. A step after `reset`
/// takes `sendit_in` instead: the name goes with the VM.
pub fn sendit_for(dir: &VmDir, project: &Project) -> String {
    match metadata(dir).ok().and_then(|metadata| metadata.name) {
        Some(name) if !in_project(project) => format!("sendit -n {name}"),
        _ => sendit_in(project),
    }
}

/// The sendit command line for `project` from here, by its directory
/// unless that is here.
pub fn sendit_in(project: &Project) -> String {
    if in_project(project) {
        "sendit".to_string()
    } else {
        format!("sendit -C {}", quote(&project.root.to_string_lossy()))
    }
}

/// Whether sendit runs in the directory of `project`.
fn in_project(project: &Project) -> bool {
    std::env::current_dir()
        .and_then(|cwd| cwd.canonicalize())
        .is_ok_and(|cwd| cwd == project.root)
}

#[cfg(test)]
mod tests {
    use super::test_util::*;
    use super::*;
    use std::fs;

    #[test]
    fn keeps_the_image_a_vm_was_made_from() {
        let temp = crate::util::TempDir::new("vm-image");
        let paths = Paths::new(temp.path().join("home"));
        fs::create_dir_all(temp.path().join("proj")).unwrap();
        let project = Project::at(&temp.path().join("proj")).unwrap();
        let rust: ImageName = "rust".parse().unwrap();
        let go: ImageName = "go".parse().unwrap();

        assert_eq!(image(&paths, &project, None), ImageName::default());
        assert_eq!(image(&paths, &project, Some(&go)), go);

        let vm = dir(&paths, &project);
        fs::create_dir_all(vm.path()).unwrap();
        fs::write(
            vm.metadata(),
            "project_path = \"/p\"\nimage = \"rust\"\nbase_revision = 7\n",
        )
        .unwrap();
        assert_eq!(image(&paths, &project, None), rust);
        assert_eq!(image(&paths, &project, Some(&go)), go);
    }

    #[test]
    fn worktrees_take_the_image_of_their_parent() {
        let temp = crate::util::TempDir::new("vm-parent-image");
        let paths = Paths::new(temp.path().join("home"));
        let (wt, _) = worktree_with_parent(temp.path(), &paths);
        let go: ImageName = "go".parse().unwrap();
        assert_eq!(image(&paths, &wt, None), "rust".parse().unwrap());
        assert_eq!(image(&paths, &wt, Some(&go)), go);
    }

    #[test]
    fn subdirectories_of_worktrees_take_the_image_of_their_parent() {
        let temp = crate::util::TempDir::new("vm-parent-image-sub");
        let paths = Paths::new(temp.path().join("home"));
        let (wt, vm) = subdirectory_with_parent(temp.path(), &paths, "apps/web");
        assert_eq!(
            parent(&paths, &wt).map(|dir| dir.path().to_path_buf()),
            Some(vm.path().to_path_buf())
        );
        assert_eq!(image(&paths, &wt, None), "rust".parse().unwrap());
        // Not for the directory above it, whose counterpart has no VM.
        let apps = Project::at(wt.root.parent().unwrap()).unwrap();
        assert!(parent(&paths, &apps).is_none());
        assert_eq!(image(&paths, &apps, None), ImageName::default());
    }
}
