//! Making a project's VM, as a copy of its parent VM or of the base image,
//! and deleting it.

use std::fs::{self, File};

use anyhow::{Context, Result, bail};

use super::lock::try_lock;
use super::naming::{lock_names, name_unnamed, name_vms, unique_name, wanted_name};
use super::{Metadata, Origin, dir, image, metadata, parent};
use crate::commands;
use crate::config::VmSettings;
use crate::paths::{ImageName, Paths, Project};
use crate::provision::{self, BaseState};
use crate::util;
use crate::vm::VmDir;

/// What `create_if_missing` did.
pub struct Created {
    /// Why the VM isn't a copy of its parent VM, if it has one and needn't
    /// be fresh.
    pub note: Option<String>,
    /// Whether the image's scripts share other directories with the new VM
    /// than `new_vm_mounts` said before: the user chose to make it from the
    /// image instead of its running parent VM, which came from another
    /// build.
    pub other_shares: bool,
}

/// Creates the project's VM unless it exists, from the image `settings`
/// choose. A `fresh` VM is created from the base image even for a git
/// worktree, whose VM is otherwise a copy of its parent VM; while that
/// runs, only if the user agrees when asked with `ask`.
pub fn create_if_missing(
    paths: &Paths,
    project: &Project,
    settings: &VmSettings,
    fresh: bool,
    ask: Ask,
) -> Result<Option<Created>> {
    let dir = dir(paths, project);
    if dir.path().exists() {
        // Made before sendit named VMs: `-n` finds it from now on.
        if metadata(&dir).is_ok_and(|metadata| metadata.name.is_none()) {
            name_vms(paths)?;
        }
        return Ok(None);
    }
    let image = image(paths, project, settings.image.as_ref());
    let expected = Origin::of_new_vm(paths, project, &image, fresh)?
        .mounts()
        .to_vec();
    let note = create(paths, project, &dir, &image, fresh, ask)?;
    Ok(Some(Created {
        note,
        other_shares: !provision::same_mounts(
            Origin::of(paths, project, &image, fresh)?.mounts(),
            &expected,
        ),
    }))
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

/// Creates the project's VM as a copy of its parent VM, or else, or if it
/// is to be `fresh`, of the base image, asking with `ask` first if the
/// parent is running. Returns a note on why it isn't a copy of its parent
/// VM if it has one and the VM needn't be fresh.
pub(super) fn create(
    paths: &Paths,
    project: &Project,
    dir: &VmDir,
    image: &ImageName,
    fresh: bool,
    ask: Ask,
) -> Result<Option<String>> {
    let (source, note) = source(paths, project, image, fresh, ask)?;
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
            let state = provision::base_state(paths, image)?;
            if matches!(state, BaseState::Missing | BaseState::Outdated) {
                bail!("the {image} image is {}", state.describe(image));
            }
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
    use super::super::State;
    use super::super::lock::lock;
    use super::super::test_util::*;
    use super::*;
    use crate::config::ByteSize;
    use crate::network::Network;
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
        let declined = create(&paths, &wt, &vm, &rust, false, &no).unwrap_err();
        assert!(declined.is::<commands::Declined>(), "{declined:#}");
        assert!(!vm.path().exists());
        let error = create(&paths, &wt, &vm, &rust, false, &yes).unwrap_err();
        assert!(error.to_string().contains("rust image"), "{error:#}");
        drop(running);
        assert_eq!(state_once_released(&parent), State::Stopped);

        // Made from another image: from the base image without asking.
        let go: ImageName = "go".parse().unwrap();
        assert!(create(&paths, &wt, &vm, &go, false, &never).is_err());

        assert_eq!(
            create(&paths, &wt, &vm, &rust, false, &never).unwrap(),
            None
        );
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
    fn asks_about_shares_again_if_the_image_asks_for_others() {
        let temp = crate::util::TempDir::new("vm-other-shares");
        let paths = Paths::new(temp.path().join("home"));
        let (wt, parent) = worktree_with_parent(temp.path(), &paths);
        let rust: ImageName = "rust".parse().unwrap();
        let base = VmDir::new(paths.image_dir(&rust));
        write_marker(&provision::marker(&paths, &rust), "~/latest:ro");
        fs::write(base.disk(), "base disk").unwrap();
        fs::write(base.efi_vars(), "base efi").unwrap();
        let settings = VmSettings {
            cpus: 1,
            memory: ByteSize::gib(1),
            disk_size: ByteSize::gib(8),
            mounts: Vec::new(),
            expose_git: false,
            banner: true,
            image: None,
            network: Network::default(),
        };
        let yes = |_: &str, _: &str| Ok(true);
        let vm = dir(&paths, &wt);
        let _running = lock(&parent, &wt, &[]).unwrap();
        let other_shares = |script: &str, mount: &str| {
            write_marker_of(&parent.image_marker(), script, mount);
            let created = create_if_missing(&paths, &wt, &settings, false, &yes).unwrap();
            assert_eq!(fs::read_to_string(vm.disk()).unwrap(), "base disk");
            fs::remove_dir_all(vm.path()).unwrap();
            created.unwrap().other_shares
        };

        // Confirmed for the parent's build, made from the latest. Only the
        // shares count, not the scripts asking for them.
        assert!(other_shares("images/rust.sh", "~/parent:ro"));
        assert!(!other_shares("images/rust.sh", "~/latest:ro"));
        assert!(!other_shares("renamed.sh", "~/latest:ro"));
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
        assert_eq!(create(&paths, &wt, &vm, &rust, true, &never).unwrap(), None);
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
                match create(&paths, &project, &vm, &rust, false, &never) {
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
        let vm = dir(&paths, &wt);
        assert!(create(&paths, &wt, &vm, &"rust".parse().unwrap(), false, &never).is_err());
        let left: Vec<_> = fs::read_dir(paths.vms_dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(left, [parent.path().file_name().unwrap()]);
    }
}
