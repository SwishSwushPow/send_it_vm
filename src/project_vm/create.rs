//! Making a project's VM, as a copy of its parent VM or of the base image,
//! and deleting it.

use std::fs::{self, File};

use anyhow::{Context, Result, bail};

use super::lock::try_lock;
use super::naming::{lock_names, name_unnamed, name_vms, unique_name, wanted_name};
use super::{Metadata, dir, metadata, parent};
use crate::commands;
use crate::paths::{ImageName, Paths, Project};
use crate::provision::{self, BaseState};
use crate::util;
use crate::vm::VmDir;

/// What a project's missing VM is to be made from, decided before its
/// settings are resolved: they share what that build asks for.
pub struct NewVm {
    image: ImageName,
    source: Source,
    /// Why it isn't a copy of its parent VM, if it has one and needn't be
    /// fresh.
    note: Option<String>,
}

impl NewVm {
    /// Whether it is made from the image's latest build, not as a copy of
    /// its parent VM.
    pub fn is_from_image(&self) -> bool {
        matches!(self.source, Source::Base)
    }

    /// Fails unless the image it is made from is provisioned and not
    /// outdated. A copy of a parent VM needs no image.
    pub fn check_image(&self, paths: &Paths) -> Result<()> {
        if !self.is_from_image() {
            return Ok(());
        }
        let image = &self.image;
        let state = provision::base_state(paths, image)?;
        if matches!(state, BaseState::Missing | BaseState::Outdated) {
            bail!("the {image} image is {}", state.describe(image));
        }
        Ok(())
    }
}

/// What the project's VM is to be made from, of `image`, unless it exists:
/// `None` then, and it gets a name if it has none. A `fresh` VM is made
/// from the base image even for a git worktree, whose VM is otherwise a
/// copy of its parent VM; while that runs, only if the user agrees when
/// asked with `ask`.
pub fn new_vm(
    paths: &Paths,
    project: &Project,
    image: &ImageName,
    fresh: bool,
    ask: Ask,
) -> Result<Option<NewVm>> {
    let dir = dir(paths, project);
    if dir.path().exists() {
        // Made before sendit named VMs: `-n` finds it from now on.
        if metadata(&dir).is_ok_and(|metadata| metadata.name.is_none()) {
            name_vms(paths)?;
        }
        return Ok(None);
    }
    plan(paths, project, image, fresh, ask).map(Some)
}

/// What `new_vm` decides, without looking whether the VM exists.
pub(super) fn plan(
    paths: &Paths,
    project: &Project,
    image: &ImageName,
    fresh: bool,
    ask: Ask,
) -> Result<NewVm> {
    let (source, note) = source(paths, project, image, fresh, ask)?;
    Ok(NewVm {
        image: image.clone(),
        source,
        note,
    })
}

/// Deletes a stopped VM. It holds the VM's lock meanwhile, so the VM can't
/// be started while it is being deleted.
pub fn delete(dir: &VmDir) -> Result<()> {
    let Some(_lock) = try_lock(dir)? else {
        bail!("{} is running; stop it first", dir.path().display());
    };
    fs::remove_dir_all(dir.path()).with_context(|| format!("deleting {}", dir.path().display()))
}

/// What a new VM is a copy of.
enum Source {
    Base,
    Parent {
        dir: VmDir,
        metadata: Metadata,
        /// Keeps the parent from starting while it is copied.
        _lock: File,
    },
}

/// Asks a yes/no question, like `commands::confirm`.
pub type Ask<'a> = &'a dyn Fn(&str, &str) -> Result<bool>;

/// The parent VM, locked, if the project has one, it can be copied for
/// `image` and the VM isn't to be `fresh`; else the base image, with a note
/// why if there is a parent and the VM needn't be fresh.
/// While the parent runs, the base image only if the user agrees, else
/// `commands::Declined`.
fn source(
    paths: &Paths,
    project: &Project,
    image: &ImageName,
    fresh: bool,
    ask: Ask,
) -> Result<(Source, Option<String>)> {
    let Some(parent) = parent(paths, project).filter(|_| !fresh) else {
        return Ok((Source::Base, None));
    };
    let why_not = match metadata(&parent) {
        Err(e) => format!("{e:#}"),
        Ok(m) if m.image != *image => format!("it was made from the {} image", m.image),
        Ok(m) if m.outdated() => "it was made from an outdated base image".to_string(),
        Ok(metadata) => match try_lock(&parent)? {
            Some(lock) => {
                let source = Source::Parent {
                    dir: parent,
                    metadata,
                    _lock: lock,
                };
                return Ok((source, None));
            }
            None => {
                let main = paths.display(&metadata.project_path);
                let question = format!(
                    "The VM of {main} is running, and a copy of it would miss what it hasn't \
                     written to its disk yet. Start this worktree's VM from the {image} image \
                     instead?"
                );
                let no_terminal = format!(
                    "the VM of {main} is running; stop it to start this worktree's VM as a \
                     copy of it, or confirm in a terminal to start from the {image} image"
                );
                if ask(&question, &no_terminal)? {
                    return Ok((Source::Base, None));
                }
                let stop = match &metadata.name {
                    Some(name) => format!("`sendit -n {name} stop`"),
                    None => format!("`sendit stop` in {main}"),
                };
                eprintln!("Stop it ({stop}), then run again to start from a copy.");
                return Err(commands::Declined.into());
            }
        },
    };
    let note = format!(
        "This VM started from the {image} image, not as a copy of {}: {why_not}.",
        paths.display(parent.path())
    );
    Ok((Source::Base, Some(note)))
}

/// Makes the project's VM from what `new_vm` decided. Returns the note on
/// why it isn't a copy of its parent VM, if it has one and needn't be
/// fresh.
pub fn create(paths: &Paths, project: &Project, new_vm: NewVm) -> Result<Option<String>> {
    let dir = &dir(paths, project);
    // `sendit run` checked, but the image may have been deleted since.
    new_vm.check_image(paths)?;
    let NewVm {
        image,
        source,
        note,
    } = new_vm;
    let image = &image;
    // The marker of the image's build that the VM is made from, which it
    // keeps a copy of. A parent made before VMs kept one shares what the
    // latest build asked for; so does a copy of it, which keeps none either.
    let (from, base_revision, copy_of, marker) = match &source {
        Source::Parent {
            dir: parent,
            metadata,
            ..
        } => {
            eprintln!(
                "Creating {} as a copy of the VM of {}",
                paths.display(dir.path()),
                paths.display(&metadata.project_path)
            );
            (
                parent.path().to_path_buf(),
                metadata.base_revision,
                Some(metadata.project_path.clone()),
                parent.image_marker(),
            )
        }
        Source::Base => {
            eprintln!(
                "Creating {} from the {image} image",
                paths.display(dir.path())
            );
            (
                paths.image_dir(image),
                provision::BASE_REVISION,
                None,
                provision::marker(paths, image),
            )
        }
    };
    let from = VmDir::new(from);
    let read_marker = || {
        util::if_exists(fs::read(&marker)).with_context(|| format!("reading {}", marker.display()))
    };
    // `provision` swaps in a new build of the image, and `images --delete`
    // deletes it, without waiting for VMs being made from it, nor they for
    // a build, which takes minutes. Unless the image's marker is the same
    // after cloning as before, the disk may be from another build than the
    // marker, or gone.
    let rebuilt = || {
        anyhow::anyhow!(
            "the {image} image was rebuilt or deleted while {} was made from it; run again",
            paths.display(dir.path())
        )
    };
    let copy = read_marker()?;
    if matches!(source, Source::Base) && copy.is_none() {
        return Err(rebuilt());
    }
    let partial = VmDir::new(paths.vms_dir().join(format!(".{}.partial", project.id)));
    util::fresh_dir(partial.path())?;
    let made = (|| {
        let cloned = (|| {
            crate::image::clone_file(&from.disk(), &partial.disk())?;
            crate::image::clone_file(&from.efi_vars(), &partial.efi_vars())
        })();
        // Also after an error, which an image deleted meanwhile causes.
        if matches!(source, Source::Base) && read_marker()? != copy {
            return Err(rebuilt());
        }
        cloned?;
        if let Some(copy) = &copy {
            fs::write(partial.image_marker(), copy)
                .with_context(|| format!("copying {}", marker.display()))?;
        }
        // Until the VM is in place, where the next one made finds its name.
        let _names = lock_names(paths)?;
        // Older VMs first, so that the new one doesn't take the name one of
        // them would get.
        let taken = name_unnamed(paths)?;
        let metadata = toml::to_string(&Metadata {
            project_path: project.root.clone(),
            image: image.clone(),
            base_revision,
            copy_of,
            name: Some(unique_name(wanted_name(paths, project), &taken)),
        })?;
        fs::write(partial.metadata(), metadata)?;
        fs::rename(partial.path(), dir.path())
            .with_context(|| format!("moving the new VM to {}", dir.path().display()))
    })();
    if made.is_err() {
        // Nothing lists it; on another volume, its disk is a full copy.
        let _ = fs::remove_dir_all(partial.path());
    }
    made.map(|()| note)
}

#[cfg(test)]
mod tests {
    use super::super::lock::lock;
    use super::super::test_util::*;
    use super::super::{Origin, State};
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[test]
    fn copies_the_parent_while_it_is_stopped() {
        let temp = crate::util::TempDir::new("vm-fork");
        let paths = Paths::new(temp.path().join("home"));
        let (wt, parent) = worktree_with_parent(temp.path(), &paths);
        let parent_project = temp.path().join("main");
        let rust: ImageName = "rust".parse().unwrap();
        let vm = dir(&paths, &wt);

        let never = |_: &str, _: &str| -> Result<bool> { panic!("asked") };
        let no = |_: &str, _: &str| Ok(false);
        let yes = |_: &str, _: &str| Ok(true);

        // Running: nothing unless the user agrees to the base image, which
        // isn't there.
        let running = lock(&parent, &wt, &[]).unwrap();
        let declined = make_vm(&paths, &wt, &rust, false, &no).unwrap_err();
        assert!(declined.is::<commands::Declined>(), "{declined:#}");
        assert!(!vm.path().exists());
        let error = make_vm(&paths, &wt, &rust, false, &yes).unwrap_err();
        assert!(error.to_string().contains("rust image"), "{error:#}");
        drop(running);
        assert_eq!(state_once_released(&parent), State::Stopped);

        // Made from another image: from the base image without asking.
        let go: ImageName = "go".parse().unwrap();
        assert!(make_vm(&paths, &wt, &go, false, &never).is_err());

        assert_eq!(make_vm(&paths, &wt, &rust, false, &never).unwrap(), None);
        assert_eq!(fs::read_to_string(vm.disk()).unwrap(), "disk");
        assert_eq!(fs::read_to_string(vm.efi_vars()).unwrap(), "efi");
        let metadata = metadata(&vm).unwrap();
        assert_eq!(metadata.project_path, wt.root);
        assert_eq!(metadata.image, rust);
        assert_eq!(metadata.copy_of.as_ref(), Some(&parent_project));
        assert!(!vm.machine_id().exists() && !vm.mac().exists());
        // Named after the parent, which got its name first.
        assert_eq!(metadata.name, Some("main-wt".parse().unwrap()));
        assert_eq!(
            self::metadata(&parent).unwrap().name,
            Some("main".parse().unwrap())
        );
        // The parent can start again.
        assert_eq!(state_once_released(&parent), State::Stopped);
    }

    #[test]
    fn shares_what_the_build_it_is_made_from_asks_for() {
        let temp = crate::util::TempDir::new("vm-build-shares-new");
        let paths = Paths::new(temp.path().join("home"));
        let (wt, parent) = worktree_with_parent(temp.path(), &paths);
        let rust: ImageName = "rust".parse().unwrap();
        let base = VmDir::new(paths.image_dir(&rust));
        write_marker(&provision::marker(&paths, &rust), "~/latest:ro");
        fs::write(base.disk(), "base disk").unwrap();
        fs::write(base.efi_vars(), "base efi").unwrap();
        write_marker(&parent.image_marker(), "~/parent:ro");
        let never = |_: &str, _: &str| -> Result<bool> { panic!("asked") };
        let yes = |_: &str, _: &str| Ok(true);
        let vm = dir(&paths, &wt);

        // Stopped parent: a copy, sharing what the parent's build asked for.
        let new_vm = plan(&paths, &wt, &rust, false, &never).unwrap();
        assert!(!new_vm.is_from_image());
        let before = mounts(Origin::of(&paths, &wt, &rust, new_vm.is_from_image()));
        assert_eq!(before, ["~/parent:ro"]);
        create(&paths, &wt, new_vm).unwrap();
        assert_eq!(mounts(Origin::of_vm(&paths, &vm, &rust)), before);
        assert_eq!(fs::read_to_string(vm.disk()).unwrap(), "disk");
        assert_eq!(state_once_released(&parent), State::Stopped);
        fs::remove_dir_all(vm.path()).unwrap();

        // Running parent, and the user agrees: from the image, sharing what
        // its latest build asks for.
        let _running = lock(&parent, &wt, &[]).unwrap();
        let new_vm = plan(&paths, &wt, &rust, false, &yes).unwrap();
        assert!(new_vm.is_from_image());
        let before = mounts(Origin::of(&paths, &wt, &rust, new_vm.is_from_image()));
        assert_eq!(before, ["~/latest:ro"]);
        create(&paths, &wt, new_vm).unwrap();
        assert_eq!(mounts(Origin::of_vm(&paths, &vm, &rust)), before);
        assert_eq!(fs::read_to_string(vm.disk()).unwrap(), "base disk");

        // Fresh: from the image without asking, even while the parent runs.
        assert!(
            plan(&paths, &wt, &rust, true, &never)
                .unwrap()
                .is_from_image()
        );
    }

    #[test]
    fn leaves_an_existing_vm_alone_and_names_it() {
        let temp = crate::util::TempDir::new("vm-existing");
        let paths = Paths::new(temp.path().join("home"));
        let project = Project::at(temp.path()).unwrap();
        let vm = dir(&paths, &project);
        let never = |_: &str, _: &str| -> Result<bool> { panic!("asked") };
        write_vm(&vm, temp.path(), None);
        assert!(
            new_vm(&paths, &project, &ImageName::default(), false, &never)
                .unwrap()
                .is_none()
        );
        assert!(metadata(&vm).unwrap().name.is_some());
    }

    #[test]
    fn fresh_vms_of_worktrees_start_from_the_base_image() {
        let temp = crate::util::TempDir::new("vm-fresh");
        let paths = Paths::new(temp.path().join("home"));
        let (wt, parent) = worktree_with_parent(temp.path(), &paths);
        let rust: ImageName = "rust".parse().unwrap();
        let base = VmDir::new(paths.image_dir(&rust));
        fs::create_dir_all(base.path()).unwrap();
        fs::write(
            base.path().join("provisioned.toml"),
            format!(
                "revision = {}\ncustom_scripts = []\n",
                provision::BASE_REVISION
            ),
        )
        .unwrap();
        fs::write(base.disk(), "base disk").unwrap();
        fs::write(base.efi_vars(), "base efi").unwrap();
        let vm = dir(&paths, &wt);

        // Without asking, even while the parent runs.
        let never = |_: &str, _: &str| -> Result<bool> { panic!("asked") };
        let _running = lock(&parent, &wt, &[]).unwrap();
        assert_eq!(make_vm(&paths, &wt, &rust, true, &never).unwrap(), None);
        assert_eq!(fs::read_to_string(vm.disk()).unwrap(), "base disk");
        assert_eq!(fs::read_to_string(vm.efi_vars()).unwrap(), "base efi");
        let metadata = metadata(&vm).unwrap();
        assert_eq!(metadata.image, rust);
        assert_eq!(metadata.copy_of, None);
    }

    #[test]
    fn keeps_the_marker_of_the_build_its_disk_is_from() {
        let temp = crate::util::TempDir::new("vm-rebuilt");
        let paths = Paths::new(temp.path().join("home"));
        let project = Project::at(temp.path()).unwrap();
        let rust: ImageName = "rust".parse().unwrap();
        let vm = dir(&paths, &project);
        // Each build's files say which build they are.
        let build = |paths: &Paths, n: u64| {
            let partial = VmDir::new(paths.image_partial_dir(&rust));
            fs::create_dir_all(partial.path()).unwrap();
            fs::write(partial.disk(), n.to_string()).unwrap();
            fs::write(partial.efi_vars(), n.to_string()).unwrap();
            write_built(&partial.path().join("provisioned.toml"), n);
            provision::swap_in(paths, &rust).unwrap();
        };
        build(&paths, 0);
        let done = AtomicBool::new(false);
        let never = |_: &str, _: &str| -> Result<bool> { panic!("asked") };
        let (mut made, mut wrong) = (0, Vec::new());
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for n in 1.. {
                    if done.load(Ordering::Relaxed) {
                        break;
                    }
                    build(&paths, n);
                    std::thread::sleep(Duration::from_micros(500));
                }
            });
            for _ in 0..300 {
                match make_vm(&paths, &project, &rust, false, &never) {
                    Ok(_) => made += 1,
                    // Missing for a moment, or rebuilt meanwhile.
                    Err(e) if e.to_string().starts_with("the rust image") => continue,
                    Err(e) => {
                        wrong.push(format!("{e:#}"));
                        continue;
                    }
                }
                let disk = fs::read_to_string(vm.disk()).unwrap();
                let built = provision::build(&vm.image_marker())
                    .unwrap()
                    .and_then(|build| build.built);
                if built.map(|n| n.to_string()).as_ref() != Some(&disk) {
                    wrong.push(format!("disk of build {disk}, marker of {built:?}"));
                }
                fs::remove_dir_all(vm.path()).unwrap();
            }
            done.store(true, Ordering::Relaxed);
        });
        assert!(wrong.is_empty(), "{wrong:#?}");
        assert!(made > 0, "never made");
    }

    #[test]
    fn leaves_nothing_behind_when_creating_fails() {
        let temp = crate::util::TempDir::new("vm-create-fails");
        let paths = Paths::new(temp.path().join("home"));
        let (wt, parent) = worktree_with_parent(temp.path(), &paths);
        fs::remove_file(parent.efi_vars()).unwrap();
        let never = |_: &str, _: &str| -> Result<bool> { panic!("asked") };
        assert!(make_vm(&paths, &wt, &"rust".parse().unwrap(), false, &never).is_err());
        let left: Vec<_> = fs::read_dir(paths.vms_dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(left, [parent.path().file_name().unwrap()]);
    }
}
