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

/// The names the VMs have.
pub(super) fn names(paths: &Paths) -> Result<BTreeSet<VmName>> {
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
    fn finds_vms_by_name_and_renames_them() {
        let temp = crate::util::TempDir::new("vm-names");
        let paths = Paths::new(temp.path().join("home"));
        let project = |path: &str| {
            let root = temp.path().join(path);
            fs::create_dir_all(&root).unwrap();
            let project = Project::at(&root).unwrap();
            (dir(&paths, &project), project)
        };
        let (a, app) = project("a/My App");
        let (_, other_app) = project("b/my_app");
        let (c, named) = project("c/web");
        let (_, none) = project("d/none");
        write_vm(&a, &app.root, Some("my-app-2"));
        write_vm(&dir(&paths, &other_app), &other_app.root, Some("my-app-3"));
        write_vm(&c, &named.root, Some("my-app"));
        let name = |vm: &VmDir| metadata(vm).unwrap().name.unwrap().to_string();

        let found = find(&paths, &"my-app-2".parse().unwrap()).unwrap();
        assert_eq!((found.root, found.id), (app.root.clone(), app.id.clone()));
        let error = find(&paths, &"web".parse().unwrap()).unwrap_err();
        assert_eq!(
            error.to_string(),
            "no VM is named web; `sendit list` shows the names"
        );
        // Also once the project directory is gone.
        fs::remove_dir(&app.root).unwrap();
        assert_eq!(
            find(&paths, &"my-app-2".parse().unwrap()).unwrap().id,
            app.id
        );

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
    fn renaming_names_a_vm_without_a_name() {
        let temp = crate::util::TempDir::new("vm-names-unnamed");
        let paths = Paths::new(temp.path().join("home"));
        let project = Project::at(temp.path()).unwrap();
        let vm = dir(&paths, &project);
        // As made before sendit named VMs.
        write_vm(&vm, temp.path(), None);
        let app: VmName = "app".parse().unwrap();
        rename(&paths, &project, &app).unwrap();
        assert_eq!(metadata(&vm).unwrap().name, Some(app));
    }

    #[test]
    fn names_vms_of_worktrees_after_their_main_worktree() {
        let temp = crate::util::TempDir::new("vm-names-worktrees");
        let paths = Paths::new(temp.path().join("home"));
        let (wt, parent) = subdirectory_with_parent(temp.path(), &paths, "apps/web");
        assert_eq!(wanted_name(&paths, &wt).as_str(), "web-wt");

        // After the name of the main worktree's VM, not its folder.
        let parent_project = project_of(&parent, &metadata(&parent).unwrap());
        rename(&paths, &parent_project, &"site".parse().unwrap()).unwrap();
        assert_eq!(wanted_name(&paths, &wt).as_str(), "site-wt");
        // Without a VM there, after the folder.
        fs::remove_dir_all(parent.path()).unwrap();
        assert_eq!(wanted_name(&paths, &wt).as_str(), "web-wt");
    }
}
