//! Fixtures that the tests of `project_vm`'s modules share.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;

use super::create::{Ask, create, plan};
use super::{Metadata, Origin, State, dir, state};
use crate::paths::{ImageName, Paths, Project, VmName};
use crate::provision;
use crate::vm::VmDir;
use crate::worktree;

/// A worktree `wt` of `main` in `temp`, and the VM of `main`, made from
/// the `rust` image.
pub(super) fn worktree_with_parent(temp: &Path, paths: &Paths) -> (Project, VmDir) {
    subdirectory_with_parent(temp, paths, "")
}

/// The subdirectory `sub` of a worktree `wt` of `main` in `temp`, and
/// the VM of the same subdirectory of `main`, made from the `rust` image.
pub(super) fn subdirectory_with_parent(temp: &Path, paths: &Paths, sub: &str) -> (Project, VmDir) {
    let (main, wt) = worktree::layout(temp);
    let (main, wt) = (main.join(sub), wt.join(sub));
    fs::create_dir_all(&main).unwrap();
    fs::create_dir_all(&wt).unwrap();
    let main = Project::at(&main).unwrap();
    let parent = dir(paths, &main);
    fs::create_dir_all(parent.path()).unwrap();
    let metadata = Metadata {
        project_path: main.root.clone(),
        image: "rust".parse().unwrap(),
        base_revision: provision::BASE_REVISION,
        copy_of: None,
        // Named after its folder, as sendit names a new VM.
        name: main
            .root
            .file_name()
            .and_then(|name| VmName::from_text(&name.to_string_lossy())),
    };
    fs::write(parent.metadata(), toml::to_string(&metadata).unwrap()).unwrap();
    fs::write(parent.disk(), "disk").unwrap();
    fs::write(parent.efi_vars(), "efi").unwrap();
    (Project::at(&wt).unwrap(), parent)
}

/// The VM's state after this process has dropped its lock. A process
/// another test spawns at that moment holds a copy of the lock until it
/// has exec'd (macOS's `posix_spawn` copies even close-on-exec
/// descriptors first), so the lock may stay taken for some milliseconds.
pub(super) fn state_once_released(dir: &VmDir) -> State {
    let start = Instant::now();
    loop {
        let state = state(dir).unwrap();
        if state == State::Stopped || start.elapsed() > Duration::from_secs(2) {
            return state;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Writes the marker of a build made at `built`.
pub(super) fn write_built(path: &Path, built: u64) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        format!(
            "revision = {}\ncustom_scripts = []\nbuilt = {built}\n",
            provision::BASE_REVISION
        ),
    )
    .unwrap();
}

/// Writes an image's marker, or a VM's copy of one, to `path`, saying
/// that the scripts asked for `mount`.
pub(super) fn write_marker(path: &Path, mount: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        format!(
            "revision = {}\ncustom_scripts = []\n\
             [[mounts]]\nscript = \"images/rust.sh\"\nmount = \"{mount}\"\n",
            provision::BASE_REVISION
        ),
    )
    .unwrap();
}

/// Writes the metadata of a VM in `vm` for the project at `root`, with
/// `name` if given, else as made before sendit named VMs.
pub(super) fn write_vm(vm: &VmDir, root: &Path, name: Option<&str>) {
    fs::create_dir_all(vm.path()).unwrap();
    let metadata = Metadata {
        project_path: root.to_path_buf(),
        image: ImageName::default(),
        base_revision: provision::BASE_REVISION,
        copy_of: None,
        name: name.map(|name| name.parse().unwrap()),
    };
    fs::write(vm.metadata(), toml::to_string(&metadata).unwrap()).unwrap();
}

/// Makes the project's VM as `sendit run` does, once it has decided
/// what from. Returns the note on why it isn't a copy of its parent.
pub(super) fn make_vm(
    paths: &Paths,
    project: &Project,
    image: &ImageName,
    fresh: bool,
    ask: Ask,
) -> Result<Option<String>> {
    create(paths, project, plan(paths, project, image, fresh, ask)?)
}

/// The specs of the shares that `origin` says the build asked for.
pub(super) fn mounts(origin: Result<Origin>) -> Vec<String> {
    origin
        .unwrap()
        .mounts()
        .iter()
        .map(|m| m.mount.to_string())
        .collect()
}
