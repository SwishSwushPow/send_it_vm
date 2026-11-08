//! VM names: giving them out, finding VMs by them, and renaming.

use std::collections::BTreeSet;
use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};

use super::{Metadata, all, dir, metadata, parent_metadata, sendit_for, write_metadata};
use crate::paths::{Paths, Project, VmName};
use crate::vm::VmDir;
use crate::worktree;

/// The project of the VM in `dir`, whose metadata is `metadata`. Its
/// directory may be gone.
fn project_of(dir: &VmDir, metadata: &Metadata) -> Project {
    Project {
        root: metadata.project_path.clone(),
        id: dir
            .path()
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into(),
    }
}

/// The project whose VM is named `name`.
pub fn find(paths: &Paths, name: &VmName) -> Result<Project> {
    name_vms(paths)?;
    for dir in all(paths)? {
        if let Ok(metadata) = metadata(&dir)
            && metadata.name.as_ref() == Some(name)
        {
            return Ok(project_of(&dir, &metadata));
        }
    }
    bail!("no VM is named {name}; `sendit list` shows the names")
}

/// Renames the project's VM to `name`, unless another VM has that name.
pub fn rename(paths: &Paths, project: &Project, name: &VmName) -> Result<()> {
    let dir = dir(paths, project);
    ensure!(
        dir.path().exists(),
        "this project has no VM yet; `{} run` creates it",
        sendit_for(&dir, project)
    );
    let _names = lock_names(paths)?;
    let mut metadata = metadata(&dir)?;
    if metadata.name.as_ref() == Some(name) {
        eprintln!("The VM is named {name} already.");
        return Ok(());
    }
    for other in all(paths)? {
        if let Ok(other) = self::metadata(&other)
            && other.name.as_ref() == Some(name)
        {
            bail!(
                "the VM of {} is named {name} already",
                paths.display(&other.project_path)
            );
        }
    }
    let old = metadata.name.replace(name.clone());
    write_metadata(&dir, &metadata)?;
    match old {
        Some(old) => eprintln!("Renamed the VM {old} to {name}."),
        None => eprintln!("Named the VM {name}."),
    }
    Ok(())
}

/// Gives each VM without a name, made before sendit named VMs, the name it
/// would get if it were made now. Commands that show or look up names call
/// this first.
pub fn name_vms(paths: &Paths) -> Result<()> {
    // Without taking the lock, which would leave its file behind, unless
    // there is a VM to name.
    let unnamed = all(paths)?
        .iter()
        .any(|dir| metadata(dir).is_ok_and(|metadata| metadata.name.is_none()));
    if !unnamed {
        return Ok(());
    }
    let _names = lock_names(paths)?;
    name_unnamed(paths).map(drop)
}

/// What `name_vms` does, for a caller that holds the names lock. Returns
/// the names the VMs have then. A VM whose metadata can't be written stays
/// without a name, with a warning: that doesn't stop the command.
pub(super) fn name_unnamed(paths: &Paths) -> Result<BTreeSet<VmName>> {
    let mut unnamed = Vec::new();
    for dir in all(paths)? {
        if let Ok(metadata) = metadata(&dir)
            && metadata.name.is_none()
        {
            unnamed.push((dir, metadata));
        }
    }
    let mut taken = names(paths)?;
    // Main worktrees first: the names of linked worktrees' VMs start with
    // theirs. The sort is stable, so the order is otherwise that of `all`.
    unnamed.sort_by_key(|(_, metadata)| worktree::main_worktree(&metadata.project_path).is_some());
    for (dir, mut metadata) in unnamed {
        let name = unique_name(wanted_name(paths, &project_of(&dir, &metadata)), &taken);
        metadata.name = Some(name.clone());
        match write_metadata(&dir, &metadata) {
            Ok(()) => {
                taken.insert(name);
            }
            Err(error) => eprintln!(
                "warning: could not name the VM in {}: {error:#}",
                paths.display(dir.path())
            ),
        }
    }
    Ok(taken)
}

/// The names the VMs have.
fn names(paths: &Paths) -> Result<BTreeSet<VmName>> {
    Ok(all(paths)?
        .iter()
        .filter_map(|dir| metadata(dir).ok()?.name)
        .collect())
}

/// Takes the lock that VM names are given out and changed under; it is
/// released when the returned file is closed.
pub(super) fn lock_names(paths: &Paths) -> Result<File> {
    let path = paths.names_lock();
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.lock()
        .with_context(|| format!("locking {}", path.display()))?;
    Ok(file)
}

/// The name the VM of `project` gets, unless another VM has it: its
/// folder's name, or in a linked git worktree, the name of the VM of its
/// counterpart in the main worktree (see `parent`), or while that has none,
/// the counterpart's folder name, followed by the linked worktree's folder
/// name, e.g. `send-it-feature-x`, or for the subdirectory `apps/web` of
/// that worktree, `web-feature-x`. `vm` if those names have no letters or
/// digits.
pub(super) fn wanted_name(paths: &Paths, project: &Project) -> VmName {
    let folder = |path: &Path| {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
    };
    let name = match worktree::linked_worktree(&project.root) {
        Some((main, top)) => {
            let main = parent_metadata(paths, project)
                .and_then(|metadata| metadata.name)
                .map(String::from)
                .or_else(|| folder(&main))
                .unwrap_or_default();
            VmName::from_text(&format!("{main}-{}", folder(top).unwrap_or_default()))
        }
        None => folder(&project.root).and_then(|name| VmName::from_text(&name)),
    };
    name.unwrap_or_else(|| "vm".parse().expect("valid name"))
}

/// `wanted`, or if it is `taken`, the first of `wanted-2`, `wanted-3`, …
/// that isn't.
pub(super) fn unique_name(wanted: VmName, taken: &BTreeSet<VmName>) -> VmName {
    if !taken.contains(&wanted) {
        return wanted;
    }
    (2..)
        .map(|n| wanted.numbered(n))
        .find(|name| !taken.contains(name))
        .expect("a free number")
}

#[cfg(test)]
mod tests {
    use super::super::test_util::*;
    use super::*;
    use std::fs;

    #[test]
    fn names_vms_and_finds_them_by_name() {
        let temp = crate::util::TempDir::new("vm-names");
        let paths = Paths::new(temp.path().join("home"));
        let project = |path: &str| {
            let root = temp.path().join(path);
            fs::create_dir_all(&root).unwrap();
            let project = Project::at(&root).unwrap();
            (dir(&paths, &project), project)
        };
        let (a, app) = project("a/My App");
        let (b, other_app) = project("b/my_app");
        let (c, named) = project("c/web");
        let (_, none) = project("d/none");
        write_vm(&a, &app.root, None);
        write_vm(&b, &other_app.root, None);
        write_vm(&c, &named.root, Some("my-app"));
        let name = |vm: &VmDir| metadata(vm).unwrap().name.unwrap().to_string();

        // The names taken already stay; those made from the folder names
        // get numbers, in the order of the VMs' directories.
        name_vms(&paths).unwrap();
        let (first, second) = if a.path() < b.path() {
            (&a, &b)
        } else {
            (&b, &a)
        };
        assert_eq!(name(first), "my-app-2");
        assert_eq!(name(second), "my-app-3");
        assert_eq!(name(&c), "my-app");

        let found = find(&paths, &name(&a).parse().unwrap()).unwrap();
        assert_eq!((found.root, found.id), (app.root.clone(), app.id.clone()));
        let error = find(&paths, &"web".parse().unwrap()).unwrap_err();
        assert_eq!(
            error.to_string(),
            "no VM is named web; `sendit list` shows the names"
        );
        // Also once the project directory is gone.
        fs::remove_dir(&app.root).unwrap();
        assert_eq!(find(&paths, &name(&a).parse().unwrap()).unwrap().id, app.id);

        let web: VmName = "web".parse().unwrap();
        rename(&paths, &named, &web).unwrap();
        assert_eq!(name(&c), "web");
        assert_eq!(find(&paths, &web).unwrap().id, named.id);
        rename(&paths, &named, &web).unwrap();
        let error = rename(&paths, &other_app, &web).unwrap_err();
        assert!(
            error.to_string().ends_with("is named web already"),
            "{error}"
        );
        assert_eq!(name(&c), "web");
        let error = rename(&paths, &none, &web).unwrap_err();
        assert!(error.to_string().contains("has no VM yet"), "{error}");
        // The old name is free again, for the next VM made.
        assert_eq!(
            unique_name("my-app".parse().unwrap(), &names(&paths).unwrap()).as_str(),
            "my-app"
        );
    }

    #[test]
    fn takes_the_names_lock_only_to_name_vms() {
        let temp = crate::util::TempDir::new("vm-names-lock");
        let paths = Paths::new(temp.path().join("home"));
        let project = Project::at(temp.path()).unwrap();
        write_vm(&dir(&paths, &project), temp.path(), Some("app"));
        name_vms(&paths).unwrap();
        assert!(!paths.names_lock().exists());
    }

    #[test]
    fn names_what_it_can() {
        use std::os::unix::fs::PermissionsExt;
        // Root writes into a read-only directory all the same.
        // SAFETY: plain syscall.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let temp = crate::util::TempDir::new("vm-names-read-only");
        let paths = Paths::new(temp.path().join("home"));
        let project = Project::at(temp.path()).unwrap();
        let vm = dir(&paths, &project);
        write_vm(&vm, temp.path(), None);
        let mode = |mode| fs::set_permissions(vm.path(), fs::Permissions::from_mode(mode));

        // A VM whose metadata can't be written stays without a name, with
        // a warning, and the command carries on.
        mode(0o555).unwrap();
        let named = name_vms(&paths);
        mode(0o755).unwrap();
        named.unwrap();
        assert_eq!(metadata(&vm).unwrap().name, None);
        // Renaming it gives it a name.
        let app: VmName = "app".parse().unwrap();
        rename(&paths, &project, &app).unwrap();
        assert_eq!(metadata(&vm).unwrap().name, Some(app));
    }

    #[test]
    fn names_vms_of_worktrees_after_their_main_worktree() {
        let temp = crate::util::TempDir::new("vm-names-worktrees");
        let paths = Paths::new(temp.path().join("home"));
        let (wt, parent) = subdirectory_with_parent(temp.path(), &paths, "apps/web");
        let vm = dir(&paths, &wt);
        write_vm(&vm, &wt.root, None);
        let name = |vm: &VmDir| metadata(vm).unwrap().name.unwrap().to_string();

        name_vms(&paths).unwrap();
        assert_eq!(name(&parent), "web");
        assert_eq!(name(&vm), "web-wt");

        // After the name of the main worktree's VM, not its folder.
        fs::remove_file(vm.metadata()).unwrap();
        write_vm(&vm, &wt.root, None);
        rename(
            &paths,
            &project_of(&parent, &metadata(&parent).unwrap()),
            &"site".parse().unwrap(),
        )
        .unwrap();
        name_vms(&paths).unwrap();
        assert_eq!(name(&vm), "site-wt");
    }
}
