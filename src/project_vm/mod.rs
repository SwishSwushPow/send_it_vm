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
//! While a VM runs, its `sendit run` process holds a lock on `run/lock` and
//! has written its PID there; that is how the other commands find it.
//!
//! `run --detach` runs the VM in a second `sendit run` process, which it
//! starts in a session of its own, without a terminal, and leaves running
//! once the VM answers over SSH. It is sendit run again rather than a fork:
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
//! This module has the VMs' metadata and which image build they are made
//! from; the submodules make (`create`), name (`naming`), run and stop
//! (`run`), lock (`lock`) and reach (`ssh`) them.

mod create;
mod lock;
mod naming;
mod run;
mod ssh;
#[cfg(test)]
mod test_util;

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::{ImageName, Paths, Project, VmName, is_project_id};
use crate::provision::{self, ScriptMount};
use crate::util;
use crate::vm::VmDir;
use crate::worktree;
use ssh::quote;

pub use create::{create_if_missing, delete};
pub use lock::{State, state};
pub use naming::{find, name_vms, rename};
pub use run::{Detach, background_project, run, stop};
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

/// What the custom scripts of `image` asked to share in the build of it
/// that the project's VM is made from: the VM's, or before it exists, what
/// `new_vm_mounts` says.
pub fn script_mounts(
    paths: &Paths,
    project: &Project,
    image: &ImageName,
    fresh: bool,
) -> Result<Vec<ScriptMount>> {
    let dir = dir(paths, project);
    if dir.path().exists() {
        build_mounts(paths, Some(dir), image)
    } else {
        new_vm_mounts(paths, project, image, fresh)
    }
}

/// What the custom scripts of `image` asked to share in the build of it
/// that a new VM of the project would be made from: its parent VM's if `run`
/// copies that, unless the VM is to be `fresh`, else the latest build's.
pub fn new_vm_mounts(
    paths: &Paths,
    project: &Project,
    image: &ImageName,
    fresh: bool,
) -> Result<Vec<ScriptMount>> {
    build_mounts(paths, parent(paths, project).filter(|_| !fresh), image)
}

/// What the custom scripts of `image` asked to share in the build of it
/// that `vm` was made from: the copy of that build's marker that it keeps.
/// Without a VM, or with one of another image or an outdated one, which
/// `run` won't copy, the latest build's; so too for a VM made before VMs
/// kept a copy.
fn build_mounts(paths: &Paths, vm: Option<VmDir>, image: &ImageName) -> Result<Vec<ScriptMount>> {
    let copy = vm
        .filter(|vm| made_from(vm, image))
        .map(|vm| vm.image_marker())
        .filter(|copy| copy.exists());
    match copy {
        Some(copy) => {
            provision::marker_mounts(&copy).with_context(|| broken_marker_copy(paths, &copy))
        }
        None => provision::marker_mounts(&provision::marker(paths, image)),
    }
}

/// Says how to get past a VM's `copy` of its build's marker that can't be
/// read, e.g. after it was edited by hand.
fn broken_marker_copy(paths: &Paths, copy: &std::path::Path) -> String {
    format!(
        "reading {}; `sendit reset` in the VM's project makes it again",
        paths.display(copy)
    )
}

/// Whether a new VM of the project would be made from an earlier build of
/// `image` than the latest one: whether its parent VM was, if `run` copies
/// that, unless the VM is to be `fresh`.
pub fn new_vm_from_earlier_build(
    paths: &Paths,
    project: &Project,
    image: &ImageName,
    fresh: bool,
) -> Result<bool> {
    match parent(paths, project).filter(|_| !fresh) {
        Some(parent) => earlier_build(paths, &parent, image),
        None => Ok(false),
    }
}

/// Whether `vm` was made from an earlier build of `image` than the latest
/// one, by the copy of that build's marker that it keeps. Not if it is a VM
/// of another image or an outdated one, which `run` won't copy: a new one
/// would be made from the latest build. Unknown, and so false, while the
/// latest build doesn't record when it was made; a VM whose copy doesn't,
/// or that has none, was made before it.
pub fn earlier_build(paths: &Paths, vm: &VmDir, image: &ImageName) -> Result<bool> {
    if !made_from(vm, image) {
        return Ok(false);
    }
    let Some(latest) = provision::marker_built(&provision::marker(paths, image))? else {
        return Ok(false);
    };
    let copy = vm.image_marker();
    let built = provision::marker_built(&copy).with_context(|| broken_marker_copy(paths, &copy))?;
    Ok(built != Some(latest))
}

/// Whether `vm` was made from a build of `image` that this version of
/// sendit can run.
fn made_from(vm: &VmDir, image: &ImageName) -> bool {
    metadata(vm).is_ok_and(|m| m.image == *image && !m.outdated())
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
    use super::create::create;
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

    #[test]
    fn knows_whether_it_was_made_from_an_earlier_build() {
        let temp = crate::util::TempDir::new("vm-earlier-build");
        let paths = Paths::new(temp.path().join("home"));
        let (wt, parent) = worktree_with_parent(temp.path(), &paths);
        let rust: ImageName = "rust".parse().unwrap();
        let go: ImageName = "go".parse().unwrap();
        let vm = dir(&paths, &wt);
        let build = |built: u64| write_built(&provision::marker(&paths, &rust), built);
        let new_vm = |image: &ImageName, fresh: bool| {
            new_vm_from_earlier_build(&paths, &wt, image, fresh).unwrap()
        };
        let made = || earlier_build(&paths, &vm, &rust).unwrap();
        let never = |_: &str, _: &str| -> Result<bool> { panic!("asked") };

        // Unknown while the latest build doesn't say when it was made.
        assert!(!new_vm(&rust, false));
        write_marker(&provision::marker(&paths, &rust), "~/old:ro");
        assert!(!new_vm(&rust, false));

        // A parent made before VMs kept a copy, or before the builds were
        // recorded, is from an earlier one, and so would be a copy of it,
        // unless the VM is to be fresh, or of another image.
        build(2);
        assert!(new_vm(&rust, false));
        write_marker(&parent.image_marker(), "~/old:ro");
        assert!(new_vm(&rust, false));
        assert!(!new_vm(&rust, true));
        write_built(&provision::marker(&paths, &go), 1);
        assert!(!new_vm(&go, false));

        // A copy of a parent made from the latest build is too, until the
        // image is built again; made again, it would be a copy again.
        write_built(&parent.image_marker(), 2);
        assert!(!new_vm(&rust, false));
        create(&paths, &wt, &vm, &rust, false, &never).unwrap();
        assert!(!made());
        build(3);
        assert!(made());
        assert!(new_vm(&rust, false));
        assert!(!new_vm(&rust, true));
        // Not from an earlier build of an image it wasn't made from.
        assert!(!earlier_build(&paths, &vm, &go).unwrap());

        // Made from the image, it is from the latest build until the next.
        fs::remove_dir_all(vm.path()).unwrap();
        let base = VmDir::new(paths.image_dir(&rust));
        fs::write(base.disk(), "base disk").unwrap();
        fs::write(base.efi_vars(), "base efi").unwrap();
        create(&paths, &wt, &vm, &rust, true, &never).unwrap();
        assert!(!made());
        build(4);
        assert!(made());
    }

    #[test]
    fn keeps_the_shares_of_the_build_it_was_made_from() {
        let temp = crate::util::TempDir::new("vm-build-shares");
        let paths = Paths::new(temp.path().join("home"));
        let (wt, parent) = worktree_with_parent(temp.path(), &paths);
        let rust: ImageName = "rust".parse().unwrap();
        let go: ImageName = "go".parse().unwrap();
        let vm = dir(&paths, &wt);
        let build = |marker: &str| write_marker(&provision::marker(&paths, &rust), marker);
        let described = |mounts: Result<Vec<ScriptMount>>| -> Vec<String> {
            mounts
                .unwrap()
                .iter()
                .map(|m| m.mount.to_string())
                .collect()
        };
        let shares =
            |image: &ImageName, fresh: bool| described(script_mounts(&paths, &wt, image, fresh));
        let never = |_: &str, _: &str| -> Result<bool> { panic!("asked") };

        assert!(shares(&rust, false).is_empty());
        // A parent made before VMs kept a copy of the image's marker shares
        // what the latest build asked for, and so does a copy of it.
        build("~/first:ro");
        assert_eq!(shares(&rust, false), ["~/first:ro"]);
        create(&paths, &wt, &vm, &rust, false, &never).unwrap();
        assert!(!vm.image_marker().exists());
        build("~/second:ro");
        assert_eq!(shares(&rust, false), ["~/second:ro"]);
        fs::remove_dir_all(vm.path()).unwrap();

        // A parent's copy is what a copy of it would share, unless the VM
        // is to be fresh, or of another image: then it is made from the
        // image.
        write_marker(&parent.image_marker(), "~/parent:ro");
        assert_eq!(shares(&rust, false), ["~/parent:ro"]);
        assert_eq!(shares(&rust, true), ["~/second:ro"]);
        write_marker(&provision::marker(&paths, &go), "~/go:ro");
        assert_eq!(shares(&go, false), ["~/go:ro"]);

        // A copy keeps the parent's, also after the image is built again,
        // and made again it would be a copy of the parent again.
        create(&paths, &wt, &vm, &rust, false, &never).unwrap();
        assert_eq!(
            fs::read_to_string(vm.image_marker()).unwrap(),
            fs::read_to_string(parent.image_marker()).unwrap()
        );
        build("~/third:ro");
        assert_eq!(shares(&rust, false), ["~/parent:ro"]);
        let new_vm = |fresh| described(new_vm_mounts(&paths, &wt, &rust, fresh));
        assert_eq!(new_vm(false), ["~/parent:ro"]);
        assert_eq!(new_vm(true), ["~/third:ro"]);

        // Made from the image, it keeps that build's.
        fs::remove_dir_all(vm.path()).unwrap();
        let base = VmDir::new(paths.image_dir(&rust));
        fs::write(base.disk(), "base disk").unwrap();
        fs::write(base.efi_vars(), "base efi").unwrap();
        create(&paths, &wt, &vm, &rust, true, &never).unwrap();
        build("~/fourth:ro");
        assert_eq!(shares(&rust, false), ["~/third:ro"]);

        // A copy that can't be read, e.g. after it was edited by hand, says
        // how to get past it.
        fs::write(vm.image_marker(), "mounts = 1\n").unwrap();
        // Only read once the latest build says when it was made.
        let marker = provision::marker(&paths, &rust);
        let latest = fs::read_to_string(&marker).unwrap();
        fs::write(&marker, format!("built = 1\n{latest}")).unwrap();
        for error in [
            script_mounts(&paths, &wt, &rust, false).unwrap_err(),
            earlier_build(&paths, &vm, &rust).unwrap_err(),
        ] {
            let error = format!("{error:#}");
            assert!(
                error.contains("image.toml; `sendit reset` in the VM's project makes it again"),
                "{error}"
            );
        }

        // A VM made before VMs kept a copy shares what the latest build
        // asked for.
        fs::remove_file(vm.image_marker()).unwrap();
        assert_eq!(shares(&rust, false), ["~/fourth:ro"]);
    }
}
