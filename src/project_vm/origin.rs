//! Which image build a project's VM is made from, or a new one would be,
//! next to the image's latest build: what its scripts asked to share, and
//! whether the VM is behind.

use std::path::Path;

use anyhow::{Context, Result};

use super::{dir, metadata, parent};
use crate::paths::{ImageName, Paths, Project};
use crate::provision::{self, Build, ScriptMount};
use crate::vm::VmDir;

/// The image build that a project's VM is made from, or that a new one
/// would be made from, next to the image's latest build.
#[derive(Debug)]
pub struct Origin {
    /// The build that the VM was made from, as the copy of its marker that
    /// the VM keeps says, if the VM counts: one of the image, made from a
    /// build that this version of sendit can run. A new VM is made from the
    /// image's latest build instead of copying any other, so none counts
    /// then.
    kept: Option<Build>,
    /// The image's latest build; None before it has one.
    latest: Option<Build>,
}

impl Origin {
    /// The build that `vm` was made from: the copy of that build's marker
    /// that it keeps. For a VM of another image or an outdated one, which
    /// `run` won't copy, the latest build's.
    pub fn of_vm(paths: &Paths, vm: &VmDir, image: &ImageName) -> Result<Self> {
        Self::read(paths, Some(vm), image)
    }

    /// The build that a new VM of the project would be made from: its
    /// parent VM's if `run` copies that, unless the VM is to be `fresh`,
    /// else the latest build.
    pub fn of_new_vm(
        paths: &Paths,
        project: &Project,
        image: &ImageName,
        fresh: bool,
    ) -> Result<Self> {
        let parent = parent(paths, project).filter(|_| !fresh);
        Self::read(paths, parent.as_ref(), image)
    }

    /// The build that the project's VM is made from, or before it exists,
    /// what `of_new_vm` says.
    pub fn of(paths: &Paths, project: &Project, image: &ImageName, fresh: bool) -> Result<Self> {
        let dir = dir(paths, project);
        if dir.path().exists() {
            Self::of_vm(paths, &dir, image)
        } else {
            Self::of_new_vm(paths, project, image, fresh)
        }
    }

    fn read(paths: &Paths, vm: Option<&VmDir>, image: &ImageName) -> Result<Self> {
        let kept = match vm.filter(|vm| made_from(vm, image)) {
            Some(vm) => {
                let copy = vm.image_marker();
                let build = provision::build(&copy)
                    .and_then(|build| build.context("it is missing, or from an outdated build"))
                    .with_context(|| broken_marker_copy(paths, &copy))?;
                Some(build)
            }
            None => None,
        };
        Ok(Self {
            kept,
            latest: provision::build(&provision::marker(paths, image))?,
        })
    }

    /// What the image's custom scripts asked to share in the build: the
    /// VM's copy's, else the latest build's, else nothing.
    pub fn mounts(&self) -> &[ScriptMount] {
        self.kept
            .as_ref()
            .or(self.latest.as_ref())
            .map_or(&[], |build| &build.mounts)
    }

    /// Whether the VM was made from an earlier build than the latest one.
    pub fn earlier_build(&self) -> bool {
        match (&self.kept, &self.latest) {
            (Some(kept), Some(latest)) => kept.built != latest.built,
            _ => false,
        }
    }

    /// Whether the image's latest build asked to share other directories.
    pub fn other_shares(&self) -> bool {
        self.latest
            .as_ref()
            .is_some_and(|latest| !provision::same_mounts(self.mounts(), &latest.mounts))
    }

    /// Whether the VM is behind the image's latest build, in what it shares
    /// or what it was made from.
    pub fn behind(&self) -> bool {
        self.other_shares() || self.earlier_build()
    }
}

/// Whether `vm` was made from a build of `image` that this version of
/// sendit can run.
fn made_from(vm: &VmDir, image: &ImageName) -> bool {
    metadata(vm).is_ok_and(|m| m.image == *image && !m.outdated())
}

/// Says how to get past a VM's `copy` of its build's marker that can't be
/// read, e.g. after it was edited or deleted by hand.
pub(super) fn broken_marker_copy(paths: &Paths, copy: &Path) -> String {
    format!(
        "reading {}; `sendit reset` in the VM's project makes it again",
        paths.display(copy)
    )
}

#[cfg(test)]
mod tests {
    use super::super::dir;
    use super::super::test_util::*;
    use super::*;
    use std::fs;

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
            Origin::of_new_vm(&paths, &wt, image, fresh)
                .unwrap()
                .earlier_build()
        };
        let made = || Origin::of_vm(&paths, &vm, &rust).unwrap().earlier_build();
        let never = |_: &str, _: &str| -> Result<bool> { panic!("asked") };

        // Not before the image has a build.
        assert!(!new_vm(&rust, false));

        // A parent from an earlier build than the latest is behind, and so
        // would be a copy of it, unless the VM is to be fresh, or of another
        // image.
        build(2);
        assert!(new_vm(&rust, false));
        assert!(!new_vm(&rust, true));
        write_built(&provision::marker(&paths, &go), 1);
        assert!(!new_vm(&go, false));

        // A copy of a parent made from the latest build is too, until the
        // image is built again; made again, it would be a copy again.
        write_built(&parent.image_marker(), 2);
        assert!(!new_vm(&rust, false));
        make_vm(&paths, &wt, &rust, false, &never).unwrap();
        assert!(!made());
        build(3);
        assert!(made());
        assert!(new_vm(&rust, false));
        assert!(!new_vm(&rust, true));
        // Not from an earlier build of an image it wasn't made from.
        assert!(!Origin::of_vm(&paths, &vm, &go).unwrap().earlier_build());

        // Made from the image, it is from the latest build until the next.
        fs::remove_dir_all(vm.path()).unwrap();
        let base = VmDir::new(paths.image_dir(&rust));
        fs::write(base.disk(), "base disk").unwrap();
        fs::write(base.efi_vars(), "base efi").unwrap();
        make_vm(&paths, &wt, &rust, true, &never).unwrap();
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
        let shares = |image: &ImageName, fresh: bool| mounts(Origin::of(&paths, &wt, image, fresh));
        let never = |_: &str, _: &str| -> Result<bool> { panic!("asked") };

        assert!(shares(&rust, false).is_empty());

        // A parent's copy is what a copy of it would share, unless the VM
        // is to be fresh, or of another image: then it is made from the
        // image.
        build("~/first:ro");
        write_marker(&parent.image_marker(), "~/parent:ro");
        assert_eq!(shares(&rust, false), ["~/parent:ro"]);
        assert_eq!(shares(&rust, true), ["~/first:ro"]);
        write_marker(&provision::marker(&paths, &go), "~/go:ro");
        assert_eq!(shares(&go, false), ["~/go:ro"]);

        // A copy keeps the parent's, also after the image is built again,
        // and made again it would be a copy of the parent again.
        make_vm(&paths, &wt, &rust, false, &never).unwrap();
        assert_eq!(
            fs::read_to_string(vm.image_marker()).unwrap(),
            fs::read_to_string(parent.image_marker()).unwrap()
        );
        build("~/second:ro");
        assert_eq!(shares(&rust, false), ["~/parent:ro"]);
        let new_vm = |fresh| mounts(Origin::of_new_vm(&paths, &wt, &rust, fresh));
        assert_eq!(new_vm(false), ["~/parent:ro"]);
        assert_eq!(new_vm(true), ["~/second:ro"]);

        // Made from the image, it keeps that build's.
        fs::remove_dir_all(vm.path()).unwrap();
        let base = VmDir::new(paths.image_dir(&rust));
        fs::write(base.disk(), "base disk").unwrap();
        fs::write(base.efi_vars(), "base efi").unwrap();
        make_vm(&paths, &wt, &rust, true, &never).unwrap();
        build("~/third:ro");
        assert_eq!(shares(&rust, false), ["~/second:ro"]);

        // A copy that can't be read, e.g. after it was edited by hand, says
        // how to get past it.
        fs::write(vm.image_marker(), "mounts = 1\n").unwrap();
        for error in [
            Origin::of(&paths, &wt, &rust, false).unwrap_err(),
            Origin::of_vm(&paths, &vm, &rust).unwrap_err(),
        ] {
            let error = format!("{error:#}");
            assert!(
                error.contains("image.toml; `sendit reset` in the VM's project makes it again"),
                "{error}"
            );
        }
    }
}
