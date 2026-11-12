//! The commands that run, inspect and clean up VMs: `run`, `status`,
//! `reset`, `list`, `images` and `prune`, plus their confirmation prompts.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, IsTerminal, Write};
use std::net::Ipv4Addr;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail, ensure};

use crate::cli::{Cli, RunArgs};
use crate::config::{ByteSize, Config, VmSettings};
use crate::network;
use crate::paths::{ImageName, Paths, Project, VmName};
use crate::project_vm::{self, Detach, Metadata, NewVm, Origin, State};
use crate::provision::{self, BaseState};
use crate::util::canonical;
use crate::vm::{self, VmDir};
use crate::worktree;

/// `sendit run`: returns the exit code for the process.
pub fn run(paths: &Paths, cli: &Cli, args: &RunArgs, cwd: &Path) -> Result<i32> {
    let background = project_vm::background_project();
    let detach = if background.is_some() {
        Detach::Background
    } else if args.detach {
        Detach::Start
    } else {
        Detach::No
    };
    let project = match (&background, &args.worktree) {
        (Some(root), _) => Project::at(root)?,
        (None, Some(branch)) => {
            let dir = match &cli.name {
                Some(name) => project_vm::find(paths, name)?.root,
                None => cli.project.clone().unwrap_or_else(|| cwd.to_path_buf()),
            };
            Project::at(&worktree::open(
                &dir,
                branch,
                args.from.as_deref(),
                args.worktree_path.as_deref(),
            )?)?
        }
        // Found by name, the project directory may be gone, but it must be
        // there to run, and still lead to the same VM.
        (None, None) => match &cli.name {
            Some(name) => {
                let found = project_vm::find(paths, name)?;
                let project = Project::at(&found.root)?;
                ensure!(
                    project.id == found.id,
                    "the VM's project directory {} now resolves to {}, a project of its own",
                    found.root.display(),
                    project.root.display()
                );
                project
            }
            None => Project::at(cli.project.as_deref().unwrap_or(cwd))?,
        },
    };
    // From before deciding whether the VM is to be made until it has
    // stopped: no other sendit process makes, runs or deletes it meanwhile.
    let lock = project_vm::lock_to_run(paths, &project, &args.command, detach)?;
    let config = Config::load(paths)?;
    let flags = args.settings();
    let chosen = match &flags.image {
        Some(image) => Some(image.clone()),
        None => config.resolve_image(paths, &project)?,
    };
    let image = project_vm::image(paths, &project, chosen.as_ref());
    // Before the settings: a new VM shares what the build it is made from
    // asks for, and that may take asking whether to copy the parent VM.
    let new_vm = project_vm::new_vm(paths, &project, &image, args.fresh, &confirm)?;
    let from_image = new_vm.as_ref().map_or(args.fresh, NewVm::is_from_image);
    let settings = config.resolve(paths, &project, &flags, cwd, from_image)?;
    // Before asking about shares, for a run that can't start anyway.
    if let Some(new_vm) = &new_vm {
        new_vm.check_image(paths)?;
    }
    // In the background, the process that started this one has asked, and
    // there is no terminal to ask on.
    if detach != Detach::Background && !args.yes {
        confirm_shares(paths, &settings)?;
    }
    let note = match new_vm {
        Some(new_vm) => project_vm::create(paths, &project, new_vm)?,
        None => None,
    };
    project_vm::run(
        paths,
        &project,
        &settings,
        note,
        &args.command,
        detach,
        lock,
    )
}

/// Asks before sharing directories that contain the home directory, or
/// contain or sit inside sendit's own state. Through those the VM could
/// become root (the root SSH key), add shares on the next run (the config
/// file) or run commands as root on the next boot (a VM's mount script).
/// Read-only shares count too: reading the root key is enough. Unless
/// `.git` is exposed, the same goes for the project's git repository, whose
/// hooks and config git on the Mac runs, through any share but the
/// project's own, which hides `.git` alone. Fails with `Declined` if the
/// user says no.
pub fn confirm_shares(paths: &Paths, settings: &VmSettings) -> Result<()> {
    let risky = risky_shares(paths, settings);
    if risky.is_empty() {
        return Ok(());
    }
    let list: String = risky.iter().map(|line| format!("\n  {line}")).collect();
    confirmed(
        &format!(
            "These shares let the VM get around its limits, e.g. become root or run \
             commands on this Mac:{list}\nShare them anyway?"
        ),
        &format!("not sharing these without a terminal to confirm it, or --yes:{list}"),
    )
}

/// One line for each mount that `confirm_shares` asks about.
fn risky_shares(paths: &Paths, settings: &VmSettings) -> Vec<String> {
    // Resolved, like the mounts' host paths, so that a symlinked
    // ~/.config/sendit is caught through a share of its target. Each link
    // on the way counts too, directory links included: the VM could point
    // it elsewhere. The files most worth protecting come last, for when
    // they are symlinks of their own, e.g. into a dotfiles repo.
    let mut state = Vec::new();
    for (path, what) in [
        (paths.config_dir(), "sendit's configuration"),
        (paths.cache_dir(), "sendit's images and SSH keys"),
        (paths.vms_dir(), "sendit's VMs"),
        (paths.config_file(), "sendit's configuration"),
        (
            paths.provision_scripts_dir(),
            "sendit's provisioning scripts",
        ),
        (paths.image_files_dir(), "sendit's image files"),
        (paths.ssh_dir(), "sendit's SSH keys"),
    ] {
        state.extend(
            protected_paths(&path)
                .into_iter()
                .map(|(path, link)| (path, link, what)),
        );
    }
    // So can each script and image file, which runs with sudo in the next
    // build.
    for (dir, what) in [
        (
            paths.provision_scripts_dir(),
            "sendit's provisioning scripts",
        ),
        (paths.image_files_dir(), "sendit's image files"),
    ] {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            state.extend(
                protected_paths(&entry.path())
                    .into_iter()
                    .map(|(path, link)| (path, link, what)),
            );
        }
    }
    // The project's share hides only `<project>/.git`. Above the project
    // there may be more, e.g. for a subdirectory of a repository, and a
    // `.git` file or symlink leads to the repository elsewhere: in the main
    // worktree's `.git` for a linked worktree, or e.g. in `.bare` in the
    // project, which its own share then exposes too.
    let mut git = Vec::new();
    if !settings.expose_git {
        for dir in worktree::git_dirs(&settings.project().host) {
            git.extend(
                protected_paths(&dir)
                    .into_iter()
                    .map(|(path, link)| (path, link, "the project's git repository")),
            );
        }
    }
    let hidden = settings.project().host.join(".git");
    let home = canonical(paths.home());

    settings
        .mounts
        .iter()
        .enumerate()
        .filter_map(|(i, mount)| {
            let host = &mount.host;
            let git = git
                .iter()
                .filter(|(path, _, _)| i > 0 || !path.starts_with(&hidden));
            let reason = if home.starts_with(host) {
                "contains your home directory".to_string()
            } else {
                state.iter().chain(git).find_map(|(path, link, what)| {
                    // A link reads as the way there, which the VM could
                    // change, not as what it protects.
                    if path.starts_with(host) && *link {
                        let path = paths.display(path);
                        Some(format!("contains {path}, a symlink on the way to {what}"))
                    } else if path.starts_with(host) {
                        Some(format!("contains {what} ({})", paths.display(path)))
                    } else if host.starts_with(path) {
                        Some(format!("is inside {what} ({})", paths.display(path)))
                    } else {
                        None
                    }
                })?
            };
            let mode = if mount.read_only {
                "read-only"
            } else {
                "read-write"
            };
            Some(format!("{} ({mode}) {reason}", paths.display(host)))
        })
        .collect()
}

/// The paths through which the VM could change what the absolute `path`
/// leads to, each with whether it is a symlink. `path` is resolved one
/// component at a time, as the kernel does, and each symlink on the way
/// counts as itself, directory links included, with the directories before
/// it resolved. Last comes where `path` ends up, or, once a component
/// doesn't exist, where it would be in the last directory that does: the VM
/// could create it there. A loop ends after 32 links, as it does for macOS
/// (kern.maxsymlinks).
fn protected_paths(path: &Path) -> Vec<(PathBuf, bool)> {
    fn push(rest: &mut Vec<OsString>, path: &Path) {
        rest.extend(path.components().rev().map(|c| c.as_os_str().to_owned()));
    }
    let mut found = Vec::new();
    let mut dir = PathBuf::from("/");
    // The components left to resolve, the next one last.
    let mut rest = Vec::new();
    push(&mut rest, path);
    let mut links = 0;
    while let Some(name) = rest.pop() {
        if name == "/" {
            dir = PathBuf::from("/");
        } else if name == ".." {
            // `dir` has no links in it, so this is the directory `..` is.
            dir.pop();
        } else if name != "." {
            let next = dir.join(&name);
            match fs::symlink_metadata(&next) {
                Ok(metadata) if metadata.is_symlink() => {
                    links += 1;
                    let target = fs::read_link(&next);
                    found.push((next, true));
                    match target {
                        Ok(target) if links <= 32 => push(&mut rest, &target),
                        _ => return found,
                    }
                }
                Ok(_) => dir = next,
                Err(_) => {
                    let path = rest.iter().rev().fold(next, |path, name| path.join(name));
                    found.push((path, false));
                    return found;
                }
            }
        }
    }
    found.push((dir, false));
    found
}

/// Deletes the project's VM, after asking unless `yes`. `chosen` is the
/// image the config file chooses for the project, if it does.
pub fn reset(
    paths: &Paths,
    project: &Project,
    chosen: Option<&ImageName>,
    yes: bool,
) -> Result<()> {
    let dir = project_vm::dir(paths, project);
    if !dir.path().exists() {
        eprintln!("This project has no VM.");
        return Ok(());
    }
    match project_vm::state(&dir)? {
        State::Stopped => {}
        State::Busy => bail!("{}; try again in a moment", project_vm::BUSY),
        State::Running(_) => bail!(
            "the VM is running; stop it first with `{} stop`",
            project_vm::sendit_for(&dir, project)
        ),
    }
    let parent = project_vm::parent_metadata(paths, project);
    let next = match &parent {
        Some(parent) => format!(
            "of the VM of {}, or of the base image if that is running then",
            paths.display(&parent.project_path)
        ),
        None => "of the base image".to_string(),
    };
    // The image a new VM would be made from, as `project_vm::image` finds
    // it once this one is gone.
    let image = chosen
        .cloned()
        .or_else(|| parent.map(|parent| parent.image))
        .unwrap_or_default();
    let missing = match provision::base_state(paths, &image) {
        Ok(BaseState::Missing) => format!(
            " (the {image} image is {})",
            BaseState::Missing.describe(&image)
        ),
        _ => String::new(),
    };
    let vm = match project_vm::metadata(&dir).ok().and_then(|m| m.name) {
        Some(name) => format!("the VM {name} ({})", paths.display(dir.path())),
        None => paths.display(dir.path()),
    };
    let question = format!(
        "Delete {vm} and everything stored in it? The next `run` starts from a fresh copy \
         {next}{missing}."
    );
    if !yes {
        confirmed(&question, PASS_YES)?;
    }
    project_vm::delete(&dir)?;
    eprintln!("Deleted {}.", paths.display(dir.path()));
    Ok(())
}

/// What `list` shows of a VM's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmState {
    Running,
    /// Another sendit process is making, starting, copying or deleting it.
    Busy,
    /// Stopped, and made from an outdated base image, so it can't run
    /// until it is reset.
    Outdated,
    Stopped,
}

impl VmState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Busy => "busy",
            Self::Outdated => "outdated",
            Self::Stopped => "stopped",
        }
    }
}

/// One VM, as `list` shows it.
#[derive(Clone, Debug)]
pub struct VmRow {
    pub dir: VmDir,
    pub name: Option<VmName>,
    pub state: VmState,
    /// The space its disk takes up.
    pub disk: ByteSize,
    /// What its metadata says, unless that can't be read.
    pub info: Option<VmInfo>,
}

/// What a VM's metadata says, for `VmRow`.
#[derive(Clone, Debug)]
pub struct VmInfo {
    pub project_path: PathBuf,
    pub project_dir: ProjectDir,
    pub image: ImageName,
    /// Made from an earlier build of its image than the latest.
    pub earlier_build: bool,
    /// The project whose VM this one was made as a copy of.
    pub copy_of: Option<PathBuf>,
}

/// Every project VM, as `list` shows them, sorted by directory.
pub fn list_rows(paths: &Paths) -> Result<Vec<VmRow>> {
    let mut rows = Vec::new();
    for dir in project_vm::all(paths)? {
        let metadata = project_vm::metadata(&dir);
        let state = match project_vm::state(&dir)? {
            State::Running(_) => VmState::Running,
            State::Busy => VmState::Busy,
            State::Stopped if metadata.as_ref().is_ok_and(|m| m.outdated()) => VmState::Outdated,
            State::Stopped => VmState::Stopped,
        };
        let (name, info) = match metadata {
            Ok(metadata) => {
                // Only a hint: a marker that can't be read doesn't stop the listing.
                let origin = Origin::of_vm(paths, &dir, &metadata.image);
                let info = VmInfo {
                    project_dir: project_dir(&metadata.project_path),
                    project_path: metadata.project_path,
                    earlier_build: origin.is_ok_and(|origin| origin.earlier_build()),
                    image: metadata.image,
                    copy_of: metadata.copy_of,
                };
                (metadata.name, Some(info))
            }
            Err(_) => (None, None),
        };
        rows.push(VmRow {
            disk: allocated(&dir.disk()),
            dir,
            name,
            state,
            info,
        });
    }
    Ok(rows)
}

pub fn list(paths: &Paths) -> Result<()> {
    let rows = list_rows(paths)?;
    if rows.is_empty() {
        eprintln!("No VMs in {}.", paths.display(&paths.vms_dir()));
        return Ok(());
    }
    let rows: Vec<_> = rows
        .into_iter()
        .map(|row| {
            let name = row.name.as_ref().map_or("?", VmName::as_str).to_string();
            let (image, project) = match row.info {
                Some(info) => {
                    let path = info.project_path.display();
                    let project = match info.project_dir {
                        ProjectDir::Present => path.to_string(),
                        ProjectDir::Missing => format!("{path} (missing)"),
                        ProjectDir::Unmounted => format!("{path} (volume not mounted)"),
                        ProjectDir::Inaccessible(_) => format!("{path} (inaccessible)"),
                    };
                    let image = if info.earlier_build {
                        format!("{} (earlier build)", info.image)
                    } else {
                        info.image.to_string()
                    };
                    (image, project)
                }
                None => (
                    "?".to_string(),
                    format!("? ({})", paths.display(row.dir.path())),
                ),
            };
            (name, row.state.as_str(), row.disk, image, project)
        })
        .collect();
    let names = column_width("NAME", rows.iter().map(|row| &row.0));
    let images = column_width("IMAGE", rows.iter().map(|row| &row.3));
    println!(
        "{:<names$}  {:<8} {:>9}  {:<images$}  PROJECT",
        "NAME", "STATE", "DISK", "IMAGE"
    );
    for (name, state, disk, image, project) in rows {
        println!(
            "{name:<names$}  {state:<8} {:>9}  {image:<images$}  {project}",
            disk.approx()
        );
    }
    Ok(())
}

/// One base image, as `images` shows it.
#[derive(Clone, Debug)]
pub struct ImageRow {
    pub image: ImageName,
    pub state: BaseState,
    /// The space its disk takes up, if it is provisioned.
    pub disk: Option<ByteSize>,
    /// How many VMs were made from it.
    pub vms: usize,
}

/// The images sendit knows of, and those VMs were made from, which stay
/// listed after they were deleted. `vm_images` has the image of each VM,
/// e.g. from the rows of `list_rows`, which a caller may have read already.
pub fn image_rows(
    paths: &Paths,
    config: &Config,
    vm_images: impl IntoIterator<Item = ImageName>,
) -> Result<Vec<ImageRow>> {
    let vms = vms_per_image(vm_images);
    let images: BTreeSet<_> = provision::images(paths, config)?
        .into_iter()
        .chain(vms.keys().cloned())
        .collect();
    let mut rows = Vec::new();
    for image in images {
        let state = provision::base_state(paths, &image)?;
        let disk = (state != BaseState::Missing)
            .then(|| allocated(&VmDir::new(paths.image_dir(&image)).disk()));
        rows.push(ImageRow {
            vms: vms.get(&image).copied().unwrap_or(0),
            image,
            state,
            disk,
        });
    }
    Ok(rows)
}

pub fn images(paths: &Paths, config: &Config) -> Result<()> {
    let rows = image_rows(paths, config, vm_images(paths)?)?;
    let width = column_width("IMAGE", rows.iter().map(|row| row.image.as_str()));
    println!("{:<width$}  {:<15} {:>9}  VMS", "IMAGE", "STATE", "DISK");
    for row in &rows {
        let state = match row.state {
            BaseState::Missing => "missing",
            BaseState::Outdated => "outdated",
            BaseState::ScriptsChanged => "scripts changed",
            BaseState::Current => "provisioned",
        };
        let disk = row.disk.map_or("-".to_string(), ByteSize::approx);
        println!(
            "{:<width$}  {state:<15} {disk:>9}  {}",
            row.image.as_str(),
            row.vms
        );
    }
    Ok(())
}

/// Deletes the `images`, which must all be provisioned or have a build
/// that left something behind, after asking unless `yes`. Tells which of
/// them `provision --all` would build again.
pub fn delete_images(
    paths: &Paths,
    config: &Config,
    images: &[ImageName],
    yes: bool,
) -> Result<()> {
    let mut images = images.to_vec();
    images.sort();
    images.dedup();
    for image in &images {
        // A first build that failed or was killed leaves no image.
        let left = [
            paths.image_partial_dir(image),
            paths.image_old_dir(image),
            paths.provision_dir(image),
        ];
        ensure!(
            paths.image_dir(image).is_dir() || left.iter().any(|dir| dir.exists()),
            "the {image} image isn't provisioned"
        );
    }
    let vms = vms_per_image(vm_images(paths)?);
    eprintln!("Base images to delete:");
    for image in &images {
        let dir = if paths.image_dir(image).is_dir() {
            paths.display(&paths.image_dir(image))
        } else {
            "what its builds left behind".to_string()
        };
        match vms.get(image).copied().unwrap_or(0) {
            0 => eprintln!("  {image}  ({dir})"),
            1 => eprintln!("  {image}  ({dir}; 1 VM was made from it)"),
            n => eprintln!("  {image}  ({dir}; {n} VMs were made from it)"),
        }
    }
    let what = match images.len() {
        1 => format!("the {} image", images[0]),
        n => format!("{n} images"),
    };
    if !yes {
        confirmed(&format!("Delete {what}?"), PASS_YES)?;
    }
    provision::delete_images(paths, &images)?;
    eprintln!("Deleted {what}.");
    for image in &images {
        // As `provision::images` finds them, and `provision` takes them.
        if let Some(target) = provision::dangling_image_file(paths, image) {
            eprintln!(
                "`sendit provision --all` stops with an error while {}.",
                provision::describe_dangling(paths, image, &target)
            );
            continue;
        }
        let file = paths.image_file(image);
        let when = if fs::symlink_metadata(&file).is_ok() && !file.is_dir() {
            format!(" while {} exists", paths.display(&file))
        } else if config.images().any(|named| named == image) {
            " while the config file names it".to_string()
        } else if *image == ImageName::default() {
            String::new()
        } else {
            continue;
        };
        eprintln!("`sendit provision --all` builds the {image} image again{when}.");
    }
    Ok(())
}

/// The image each VM was made from, of those whose metadata can be read,
/// for `image_rows`.
pub fn vm_images(paths: &Paths) -> Result<Vec<ImageName>> {
    Ok(project_vm::all(paths)?
        .iter()
        .filter_map(|dir| Some(project_vm::metadata(dir).ok()?.image))
        .collect())
}

/// How many VMs were made from each image, of those that have any, from
/// the image of each VM.
fn vms_per_image(vm_images: impl IntoIterator<Item = ImageName>) -> BTreeMap<ImageName, usize> {
    let mut vms = BTreeMap::<ImageName, usize>::new();
    for image in vm_images {
        *vms.entry(image).or_default() += 1;
    }
    vms
}

/// The space a disk file takes up. Blocks shared with other files (as
/// APFS clones) count too.
fn allocated(disk: &Path) -> ByteSize {
    ByteSize(fs::metadata(disk).map_or(0, |m| m.blocks() * 512))
}

/// The width of a column with `header` and `values`.
fn column_width<S: AsRef<str>>(header: &str, values: impl Iterator<Item = S>) -> usize {
    values
        .map(|value| value.as_ref().len())
        .fold(header.len(), usize::max)
}

/// Which VMs `prune` deletes. Each scope takes in the ones before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PruneScope {
    /// VMs whose project directory no longer exists.
    Missing,
    /// Also VMs made from an outdated base image. They can't run anymore,
    /// so their project directory doesn't matter.
    Outdated,
    /// Every VM that isn't running.
    All,
}

/// What `prune` would delete, and the VMs it skips.
#[derive(Clone, Debug, Default)]
pub struct PrunePlan {
    /// VMs whose project directory no longer exists.
    pub orphans: Vec<PruneVm>,
    /// VMs made from an outdated base image.
    pub outdated: Vec<PruneVm>,
    /// Other stopped VMs, for `PruneScope::All`.
    pub others: Vec<PruneVm>,
    /// VMs that would be deleted, but are left alone.
    pub skipped: Vec<Skipped>,
}

impl PrunePlan {
    /// Every VM to delete, in the order `prune` lists them.
    pub fn vms(&self) -> impl Iterator<Item = &PruneVm> {
        self.orphans
            .iter()
            .chain(&self.outdated)
            .chain(&self.others)
    }
}

/// A VM that `prune` would delete.
#[derive(Clone, Debug)]
pub struct PruneVm {
    pub dir: VmDir,
    pub name: Option<VmName>,
    /// Its project directory, unless its metadata can't be read.
    pub project: Option<PathBuf>,
}

impl PruneVm {
    /// Its project directory, or "?" if that isn't known.
    pub fn project_shown(&self) -> String {
        self.project
            .as_ref()
            .map_or("?".to_string(), |project| project.display().to_string())
    }

    /// How `prune` lists it: its name, if it has one, and its project.
    pub fn listed(&self) -> String {
        match &self.name {
            Some(name) => format!("{name}  {}", self.project_shown()),
            None => self.project_shown(),
        }
    }
}

/// A VM that `prune` leaves alone, though its scope takes it in.
#[derive(Clone, Debug)]
pub struct Skipped {
    pub vm: PruneVm,
    pub reason: SkipReason,
}

/// Why `prune` leaves a VM alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// Its project is below `/Volumes/<name>`, which isn't mounted: the
    /// project may well come back.
    Unmounted,
    /// Whether its project directory is there can't be told: why.
    Inaccessible(String),
    /// Another sendit process is using it.
    Busy,
    Running,
}

impl Skipped {
    /// What `prune` says about it, e.g. "Skipping /path: it is still
    /// running."
    pub fn message(&self) -> String {
        let project = self.vm.project_shown();
        match &self.reason {
            SkipReason::Unmounted => format!("Skipping {project}: its volume is not mounted."),
            SkipReason::Inaccessible(e) => format!("Skipping {project}: {e}"),
            SkipReason::Busy => {
                format!("Skipping {project}: another sendit process is using it.")
            }
            SkipReason::Running => format!("Skipping {project}: it is still running."),
        }
    }
}

/// The VMs `prune` deletes for `scope`, which aren't running or busy.
pub fn prune_plan(paths: &Paths, scope: PruneScope) -> Result<PrunePlan> {
    let mut plan = PrunePlan::default();
    let all = scope == PruneScope::All;
    for dir in project_vm::all(paths)? {
        let (vm, group) = match project_vm::metadata(&dir) {
            Ok(metadata) => {
                let project_dir = project_dir(&metadata.project_path);
                let outdated = metadata.outdated();
                let vm = PruneVm {
                    dir,
                    name: metadata.name,
                    project: Some(metadata.project_path),
                };
                let group = match project_dir {
                    ProjectDir::Missing => &mut plan.orphans,
                    _ if scope >= PruneScope::Outdated && outdated => &mut plan.outdated,
                    _ if all => &mut plan.others,
                    ProjectDir::Present => continue,
                    ProjectDir::Unmounted => {
                        let reason = SkipReason::Unmounted;
                        plan.skipped.push(Skipped { vm, reason });
                        continue;
                    }
                    ProjectDir::Inaccessible(e) => {
                        let reason = SkipReason::Inaccessible(e);
                        plan.skipped.push(Skipped { vm, reason });
                        continue;
                    }
                };
                (vm, group)
            }
            Err(_) if all => {
                let vm = PruneVm {
                    dir,
                    name: None,
                    project: None,
                };
                (vm, &mut plan.others)
            }
            Err(_) => continue,
        };
        let reason = match project_vm::state(&vm.dir)? {
            State::Stopped => {
                group.push(vm);
                continue;
            }
            State::Busy => SkipReason::Busy,
            State::Running(_) => SkipReason::Running,
        };
        plan.skipped.push(Skipped { vm, reason });
    }
    Ok(plan)
}

pub fn prune(paths: &Paths, scope: PruneScope, yes: bool) -> Result<()> {
    let plan = prune_plan(paths, scope)?;
    for skipped in &plan.skipped {
        eprintln!("{}", skipped.message());
    }
    if plan.vms().next().is_none() {
        eprintln!(
            "{}",
            match scope {
                PruneScope::Missing => "No VMs of missing projects.",
                PruneScope::Outdated => {
                    "No VMs of missing projects or made from an outdated base image."
                }
                PruneScope::All => "No stopped VMs.",
            }
        );
        return Ok(());
    }
    for (heading, vms) in [
        (
            "VMs whose project directory no longer exists:",
            &plan.orphans,
        ),
        ("VMs made from an outdated base image:", &plan.outdated),
        ("Other VMs:", &plan.others),
    ] {
        if vms.is_empty() {
            continue;
        }
        eprintln!("{heading}");
        for vm in vms {
            eprintln!("  {}  ({})", vm.listed(), paths.display(vm.dir.path()));
        }
    }
    let count = match plan.vms().count() {
        1 => "1 VM".to_string(),
        n => format!("{n} VMs"),
    };
    if !yes {
        confirmed(&format!("Delete {count}?"), PASS_YES)?;
    }
    for vm in plan.vms() {
        project_vm::delete(&vm.dir)?;
    }
    eprintln!("Deleted {count}.");
    Ok(())
}

/// Whether a VM's project directory is still there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectDir {
    Present,
    Missing,
    /// Below `/Volumes/<name>`, and that volume isn't mounted: the project
    /// may well come back.
    Unmounted,
    /// Can't tell, e.g. for lack of permissions: why.
    Inaccessible(String),
}

pub fn project_dir(path: &Path) -> ProjectDir {
    match path.try_exists() {
        Ok(true) => ProjectDir::Present,
        Ok(false) => {
            let mut components = path.components().skip(1);
            match (components.next(), components.next()) {
                (Some(Component::Normal(volumes)), Some(Component::Normal(name)))
                    if volumes == "Volumes" && !Path::new("/Volumes").join(name).exists() =>
                {
                    ProjectDir::Unmounted
                }
                _ => ProjectDir::Missing,
            }
        }
        Err(e) => ProjectDir::Inaccessible(e.to_string()),
    }
}

pub const PASS_YES: &str = "not asking for confirmation without a terminal; pass --yes";

/// The user answered no to a question, so nothing was done. `main` exits
/// with 1 for it, without a message: the user knows.
#[derive(Debug)]
pub struct Declined;

impl std::fmt::Display for Declined {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("declined")
    }
}

impl std::error::Error for Declined {}

/// Like `confirm`, but fails with `Declined` unless the answer is yes.
fn confirmed(question: &str, no_terminal: &str) -> Result<()> {
    if confirm(question, no_terminal)? {
        Ok(())
    } else {
        Err(Declined.into())
    }
}

/// Asks a yes/no question on the terminal; "no" unless the answer is yes.
/// Fails with `no_terminal` if stdin isn't a terminal.
pub fn confirm(question: &str, no_terminal: &str) -> Result<bool> {
    ensure!(std::io::stdin().is_terminal(), "{no_terminal}");
    eprint!("{question} [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "Yes"))
}

/// What `status` tells of the project's VM.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VmStatus {
    /// Not made yet. `run` makes it as a copy of a parent project's VM,
    /// e.g. that of a linked git worktree's main worktree, if there is one.
    NotCreated { copies: Option<ParentVm> },
    Made {
        activity: Activity,
        /// Why it is behind its image, if it is: then a reset makes it
        /// again. Also while it runs: the config file may have changed
        /// since it started.
        stale: Option<Stale>,
    },
}

/// The VM that `run` copies to make a project's VM.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParentVm {
    /// The project it belongs to.
    pub project: PathBuf,
    /// Made from an earlier build of the image than the latest: `run
    /// --fresh` makes a VM from the current one instead.
    pub behind: bool,
}

/// What a VM that was made is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Activity {
    Stopped,
    /// Another sendit process is making, starting, copying or deleting it.
    Busy,
    /// With its IP address, once it has one.
    Running {
        ip: Option<Ipv4Addr>,
    },
}

impl Activity {
    /// In words, as `status` shows it, e.g. "running at 192.168.64.3".
    pub fn describe(&self) -> String {
        match self {
            Self::Stopped => "stopped".to_string(),
            Self::Busy => {
                "busy, another sendit process is making, starting, copying or deleting it"
                    .to_string()
            }
            Self::Running { ip: Some(ip) } => format!("running at {ip}"),
            Self::Running { ip: None } => "running".to_string(),
        }
    }
}

/// Why a VM is behind the image its project uses, so that a reset makes it
/// again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stale {
    /// Made from another image, named here.
    OtherImage(ImageName),
    /// Made from an outdated base image: it can't start until it is reset.
    Outdated,
    /// Made from an earlier build of the image, or sharing what one asked
    /// for (`shares`). With `fresh_too`, a VM made again would be a copy of
    /// a parent VM that is behind too, which `run --fresh` avoids.
    EarlierBuild { shares: bool, fresh_too: bool },
}

impl Stale {
    /// What makes the VM stale, in words, e.g. "made from an outdated base
    /// image"; `image` is the one its project uses.
    pub fn describe(&self, image: &ImageName) -> String {
        match self {
            Self::OtherImage(made_from) => format!("made from the {made_from} image"),
            Self::Outdated => "made from an outdated base image".to_string(),
            Self::EarlierBuild { shares: true, .. } => {
                format!("shares what an earlier build of the {image} image asked for")
            }
            Self::EarlierBuild { shares: false, .. } => {
                format!("made from an earlier build of the {image} image")
            }
        }
    }
}

/// The state of the project's VM for `status`, and while it runs, what its
/// network rules let it connect to, which flags of `sendit run` or a later
/// change to the config file may have made different from its settings.
fn vm_status(
    paths: &Paths,
    project: &Project,
    image: &ImageName,
    vm_dir: &VmDir,
    metadata: Option<&Metadata>,
) -> Result<(VmStatus, Option<String>)> {
    // Whether a new VM would be made from an earlier build than the latest.
    let new_vm_behind =
        || -> Result<bool> { Ok(Origin::of_new_vm(paths, project, image, false)?.behind()) };
    if !vm_dir.path().exists() {
        let copies = match project_vm::parent_metadata(paths, project) {
            Some(m) if m.image == *image && !m.outdated() => Some(ParentVm {
                project: m.project_path,
                behind: new_vm_behind()?,
            }),
            _ => None,
        };
        return Ok((VmStatus::NotCreated { copies }, None));
    }
    let mut applied = None;
    let activity = match project_vm::state(vm_dir)? {
        State::Stopped => Activity::Stopped,
        State::Busy => Activity::Busy,
        State::Running(_) => {
            applied = network::applied(&vm_dir.meta());
            Activity::Running {
                ip: vm::net::vm_ip(vm_dir)?,
            }
        }
    };
    let stale = match metadata {
        Some(m) if m.image != *image => Some(Stale::OtherImage(m.image.clone())),
        Some(m) if m.outdated() => Some(Stale::Outdated),
        Some(_) => {
            let origin = Origin::of_vm(paths, vm_dir, image)?;
            let shares = origin.other_shares();
            if shares || origin.earlier_build() {
                // A worktree's VM made again is a copy of its parent VM,
                // which may be behind too.
                Some(Stale::EarlierBuild {
                    shares,
                    fresh_too: new_vm_behind()?,
                })
            } else {
                None
            }
        }
        None => None,
    };
    Ok((VmStatus::Made { activity, stale }, applied))
}

/// `status` in words, e.g. "stopped; made from an earlier build …", with
/// the sendit commands that make the VM again if it is behind.
fn describe_vm(paths: &Paths, project: &Project, report: &StatusReport) -> String {
    let sendit = project_vm::sendit_for(&report.vm_dir, project);
    let image = &report.image;
    let (activity, stale) = match &report.vm {
        VmStatus::NotCreated { copies: None } => return "not created".to_string(),
        VmStatus::NotCreated {
            copies: Some(parent),
        } => {
            let copy = format!(
                "not created; `{sendit} run` copies the VM of {}",
                paths.display(&parent.project)
            );
            return if parent.behind {
                format!(
                    "{copy}, made from an earlier build of the {image} image; \
                     `{sendit} run --fresh` makes it from the current one"
                )
            } else {
                copy
            };
        }
        VmStatus::Made { activity, stale } => (activity, stale),
    };
    let state = activity.describe();
    let Some(stale) = stale else {
        return state;
    };
    let how = match stale {
        Stale::OtherImage(_) => format!("`{sendit} reset` recreates it from {image}"),
        Stale::Outdated => format!("`{sendit} reset` recreates it"),
        Stale::EarlierBuild {
            fresh_too: true, ..
        } => {
            // Without the name, which goes with the VM.
            let again = project_vm::sendit_in(project);
            format!("`{sendit} reset` and `{again} run --fresh` recreate it from the current one")
        }
        Stale::EarlierBuild {
            fresh_too: false, ..
        } => format!("`{sendit} reset` recreates it from the current one"),
    };
    format!("{state}; {}, {how}", stale.describe(image))
}

/// What `status` shows of a project's VM.
#[derive(Clone, Debug)]
pub struct StatusReport {
    /// The image the project uses, and its state.
    pub image: ImageName,
    pub base_dir: PathBuf,
    pub base_state: BaseState,
    pub project: PathBuf,
    pub vm_dir: VmDir,
    pub vm: VmStatus,
    /// While it runs, what its network rules let it connect to.
    pub applied_network: Option<String>,
    pub name: Option<VmName>,
    /// The project whose VM this one was made as a copy of.
    pub copy_of: Option<PathBuf>,
    /// The project's effective settings.
    pub settings: VmSettings,
}

/// The project's VM and its effective `settings`, for `status`.
pub fn status_report(
    paths: &Paths,
    project: &Project,
    settings: &VmSettings,
) -> Result<StatusReport> {
    let image = project_vm::image(paths, project, settings.image.as_ref());
    let base_dir = paths.image_dir(&image);
    let base_state = provision::base_state(paths, &image)?;
    let vm_dir = project_vm::dir(paths, project);
    let metadata = project_vm::metadata(&vm_dir).ok();
    let (vm, applied_network) = vm_status(paths, project, &image, &vm_dir, metadata.as_ref())?;
    let (name, copy_of) = metadata.map_or((None, None), |m| (m.name, m.copy_of));
    Ok(StatusReport {
        image,
        base_dir,
        base_state,
        project: project.root.clone(),
        vm_dir,
        vm,
        applied_network,
        name,
        copy_of,
        settings: settings.clone(),
    })
}

pub fn status(paths: &Paths, project: &Project, settings: &VmSettings) -> Result<()> {
    let report = status_report(paths, project, settings)?;
    let image = &report.image;
    println!(
        "image    {image} in {} ({})",
        paths.display(&report.base_dir),
        report.base_state.describe(image)
    );
    println!("project  {}", report.project.display());
    println!(
        "vm       {} ({})",
        paths.display(report.vm_dir.path()),
        describe_vm(paths, project, &report)
    );
    if let Some(name) = &report.name {
        println!("name     {name}");
    }
    if let Some(copy_of) = &report.copy_of {
        println!("copy of  the VM of {}", paths.display(copy_of));
    }
    let settings = &report.settings;
    println!("cpus     {}", settings.cpus);
    println!("memory   {}", settings.memory);
    println!("disk     {} max", settings.disk_size);
    println!(
        ".git     {}",
        if settings.expose_git {
            "visible"
        } else {
            "hidden"
        }
    );
    let network = settings.network.describe();
    match &report.applied_network {
        Some(applied) if *applied != network => {
            println!("network  {applied}");
            println!("         configured: {network}, from the VM's next start");
        }
        _ => println!("network  {network}"),
    }
    for (i, mount) in settings.mounts.iter().enumerate() {
        println!(
            "{:<8} {} -> {} ({})",
            if i == 0 { "mounts" } else { "" },
            paths.display(&mount.host),
            mount.guest.display(),
            mount.mode(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Mount;
    use crate::util::TempDir;

    /// Settings that share `mounts`, the first one as the project.
    fn sharing(mounts: &[Mount], expose_git: bool) -> VmSettings {
        VmSettings {
            cpus: 1,
            memory: ByteSize::gib(1),
            disk_size: ByteSize::gib(8),
            mounts: mounts.to_vec(),
            expose_git,
            banner: true,
            image: None,
            network: network::Network::default(),
        }
    }

    #[test]
    fn prunes_more_vms_as_the_scope_widens() {
        let temp = TempDir::new("prune");
        let paths = Paths::new(temp.path().join("home"));
        let present = temp.path().join("present");
        fs::create_dir_all(&present).unwrap();
        let vm_dir = |name: &str| {
            let dir = paths
                .vms_dir()
                .join(format!("{name}_6ba7b811-9dad-11d1-80b4-00c04fd430c8"));
            fs::create_dir_all(&dir).unwrap();
            dir
        };
        let vm = |name: &str, project: &Path, base_revision: u32| {
            let dir = vm_dir(name);
            fs::write(
                dir.join("project.toml"),
                format!(
                    "project_path = \"{}\"\nimage = \"default\"\nbase_revision = {base_revision}\n",
                    project.display()
                ),
            )
            .unwrap();
            dir
        };
        let current = provision::BASE_REVISION;
        let kept = vm("kept", &present, current);
        let outdated = vm("outdated", &present, current - 1);
        let orphan = vm("orphan", &temp.path().join("gone"), current);
        let broken = vm_dir("broken");
        // Not a VM, e.g. a folder that the user put in ~/.sendit.
        let other = paths.vms_dir().join("photos");
        fs::create_dir_all(&other).unwrap();

        prune(&paths, PruneScope::Missing, true).unwrap();
        assert!(!orphan.exists());
        assert!(outdated.exists());

        prune(&paths, PruneScope::Outdated, true).unwrap();
        assert!(!outdated.exists());
        assert!(kept.exists());
        assert!(broken.exists());

        prune(&paths, PruneScope::All, true).unwrap();
        assert!(!kept.exists());
        assert!(!broken.exists());
        assert!(other.exists());
    }

    #[test]
    fn lists_vms_and_plans_prunes_as_data() {
        let temp = TempDir::new("rows");
        let paths = Paths::new(temp.path().join("home"));
        let present = temp.path().join("present");
        fs::create_dir_all(&present).unwrap();
        let vm = |id: &str, name: &str, project: &Path, base_revision: u32| {
            let dir = paths
                .vms_dir()
                .join(format!("{id}_6ba7b811-9dad-11d1-80b4-00c04fd430c8"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("project.toml"),
                format!(
                    "project_path = \"{}\"\nimage = \"rust\"\nbase_revision = {base_revision}\n\
                     name = \"{name}\"\n",
                    project.display()
                ),
            )
            .unwrap();
        };
        let current = provision::BASE_REVISION;
        vm("a", "kept", &present, current);
        vm("b", "old", &present, current - 1);
        vm("c", "orphan", &temp.path().join("gone"), current);

        let rows = list_rows(&paths).unwrap();
        let summary: Vec<_> = rows
            .iter()
            .map(|row| {
                let info = row.info.as_ref().unwrap();
                (
                    row.name.as_ref().unwrap().as_str(),
                    row.state,
                    info.image.as_str(),
                    info.project_dir.clone(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("kept", VmState::Stopped, "rust", ProjectDir::Present),
                ("old", VmState::Outdated, "rust", ProjectDir::Present),
                ("orphan", VmState::Stopped, "rust", ProjectDir::Missing),
            ]
        );

        let names = |plan: &PrunePlan| -> Vec<String> {
            plan.vms()
                .map(|vm| vm.name.as_ref().unwrap().to_string())
                .collect()
        };
        assert_eq!(
            names(&prune_plan(&paths, PruneScope::Missing).unwrap()),
            ["orphan"]
        );
        let plan = prune_plan(&paths, PruneScope::Outdated).unwrap();
        assert_eq!(names(&plan), ["orphan", "old"]);
        assert_eq!(
            plan.outdated[0].listed(),
            format!("old  {}", present.display())
        );
        assert_eq!(
            names(&prune_plan(&paths, PruneScope::All).unwrap()),
            ["orphan", "old", "kept"]
        );
        // Nothing was deleted.
        assert_eq!(list_rows(&paths).unwrap().len(), 3);
    }

    #[test]
    fn tells_why_vms_are_skipped() {
        let temp = TempDir::new("prune-skip");
        let paths = Paths::new(temp.path().join("home"));
        let dir = paths
            .vms_dir()
            .join("app_6ba7b811-9dad-11d1-80b4-00c04fd430c8");
        fs::create_dir_all(&dir).unwrap();
        // Of a project on a volume that isn't mounted.
        fs::write(
            dir.join("project.toml"),
            format!(
                "project_path = \"/Volumes/sendit-not-mounted/app\"\nimage = \"default\"\n\
                 base_revision = {}\nname = \"app\"\n",
                provision::BASE_REVISION
            ),
        )
        .unwrap();

        let plan = prune_plan(&paths, PruneScope::Missing).unwrap();
        assert_eq!(plan.vms().count(), 0);
        let [skipped] = &plan.skipped[..] else {
            panic!("{:?}", plan.skipped);
        };
        assert_eq!(skipped.reason, SkipReason::Unmounted);
        assert_eq!(skipped.vm.name.as_ref().map(VmName::as_str), Some("app"));
        assert_eq!(
            skipped.message(),
            "Skipping /Volumes/sendit-not-mounted/app: its volume is not mounted."
        );
    }

    #[test]
    fn rows_images_with_their_vms() {
        let temp = TempDir::new("image-rows");
        let paths = Paths::new(temp.path().join("home"));
        let config = Config::parse("image = \"go\"\n[images.rust]\ncpus = 2\n").unwrap();
        let rows = image_rows(&paths, &config, vm_images(&paths).unwrap()).unwrap();
        let rows: Vec<_> = rows
            .iter()
            .map(|row| (row.image.as_str(), row.state, row.disk, row.vms))
            .collect();
        assert_eq!(
            rows,
            [
                ("default", BaseState::Missing, None, 0),
                ("go", BaseState::Missing, None, 0),
                ("rust", BaseState::Missing, None, 0),
            ]
        );
        assert!(Config::parse("cpus = \"many\"").is_err());
    }

    #[test]
    fn flags_shares_of_home_and_sendit_state() {
        let temp = TempDir::new("risky-shares");
        let home = temp.path().join("home");
        for dir in [
            ".config/sendit",
            ".cache/sendit/ssh",
            "code/proj",
            "dotfiles",
        ] {
            fs::create_dir_all(home.join(dir)).unwrap();
        }
        let paths = Paths::new(home.clone());
        let mount = |host: &Path, read_only| Mount {
            host: host.to_path_buf(),
            guest: "/mnt/x".into(),
            read_only,
        };
        let risky = |mounts: &[Mount]| risky_shares(&paths, &sharing(mounts, true));

        assert!(risky(&[mount(&home.join("code/proj"), false)]).is_empty());
        assert_eq!(
            risky(&[mount(temp.path(), true)]),
            [format!(
                "{} (read-only) contains your home directory",
                temp.path().display()
            )]
        );
        assert_eq!(
            risky(&[mount(&home, false)]),
            ["~ (read-write) contains your home directory"]
        );
        assert_eq!(
            risky(&[mount(&home.join(".config"), true)]),
            ["~/.config (read-only) contains sendit's configuration (~/.config/sendit)"]
        );
        assert_eq!(
            risky(&[mount(&home.join(".cache/sendit/ssh"), true)]),
            [
                "~/.cache/sendit/ssh (read-only) is inside sendit's images and SSH keys (~/.cache/sendit)"
            ]
        );

        // ~/.sendit doesn't exist yet, but will.
        fs::create_dir_all(home.join("vm-parent")).unwrap();
        assert!(risky(&[mount(&home.join("vm-parent"), false)]).is_empty());
        assert_eq!(risky(&[mount(&home.join(".cache"), false)]).len(), 1);

        // A config file symlinked into a dotfiles repo.
        let dotfiles = home.join("dotfiles");
        fs::write(dotfiles.join("config.toml"), "").unwrap();
        std::os::unix::fs::symlink(
            dotfiles.join("config.toml"),
            home.join(".config/sendit/config.toml"),
        )
        .unwrap();
        assert_eq!(
            risky(&[mount(&dotfiles, false)]),
            ["~/dotfiles (read-write) contains sendit's configuration (~/dotfiles/config.toml)"]
        );

        // Image files, which run with sudo in every build of their image,
        // and the shared scripts they call, also through symlinks.
        assert_eq!(
            risky(&[mount(&home.join(".config/sendit/images"), true)]),
            [
                "~/.config/sendit/images (read-only) is inside sendit's configuration (~/.config/sendit)"
            ]
        );
        for (dir, what) in [
            ("images", "sendit's image files"),
            ("provision-scripts", "sendit's provisioning scripts"),
        ] {
            let target = dotfiles.join(format!("sendit-{dir}"));
            fs::create_dir_all(target.join("sub")).unwrap();
            std::os::unix::fs::symlink(&target, home.join(".config/sendit").join(dir)).unwrap();
            assert_eq!(
                risky(&[mount(&target, false)]),
                [format!(
                    "~/dotfiles/sendit-{dir} (read-write) contains {what} (~/dotfiles/sendit-{dir})"
                )]
            );
            assert_eq!(
                risky(&[mount(&target.join("sub"), true)]),
                [format!(
                    "~/dotfiles/sendit-{dir}/sub (read-only) is inside {what} (~/dotfiles/sendit-{dir})"
                )]
            );
        }

        // A single script or image file that is a symlink, relative or not,
        // even when its target doesn't exist yet.
        for dir in ["elsewhere", "planned"] {
            fs::create_dir_all(home.join(dir)).unwrap();
        }
        fs::write(home.join("elsewhere/rust.sh"), "").unwrap();
        let scripts = home.join(".config/sendit/provision-scripts");
        std::os::unix::fs::symlink("../../elsewhere/rust.sh", scripts.join("rust.sh")).unwrap();
        let images = home.join(".config/sendit/images");
        std::os::unix::fs::symlink(home.join("planned/go.sh"), images.join("go.sh")).unwrap();
        assert_eq!(
            risky(&[mount(&home.join("elsewhere"), false)]),
            [
                "~/elsewhere (read-write) contains sendit's provisioning scripts (~/elsewhere/rust.sh)"
            ]
        );
        assert_eq!(
            risky(&[mount(&home.join("planned"), true)]),
            ["~/planned (read-only) contains sendit's image files (~/planned/go.sh)"]
        );

        // A symlink to a symlink: the VM could point the one in between
        // elsewhere. A loop ends.
        for dir in ["links", "store"] {
            fs::create_dir_all(home.join(dir)).unwrap();
        }
        fs::write(home.join("store/helix.sh"), "").unwrap();
        std::os::unix::fs::symlink("../store/helix.sh", home.join("links/helix.sh")).unwrap();
        std::os::unix::fs::symlink(home.join("links/helix.sh"), scripts.join("helix.sh")).unwrap();
        std::os::unix::fs::symlink("loop.sh", scripts.join("loop.sh")).unwrap();
        assert_eq!(
            risky(&[mount(&home.join("links"), false)]),
            [
                "~/links (read-write) contains ~/links/helix.sh, a symlink on the way to sendit's provisioning scripts"
            ]
        );
        assert_eq!(
            risky(&[mount(&home.join("store"), true)]),
            ["~/store (read-only) contains sendit's provisioning scripts (~/store/helix.sh)"]
        );

        // The same goes for sendit's own directories.
        fs::create_dir_all(home.join("keys")).unwrap();
        let ssh = home.join(".cache/sendit/ssh");
        fs::remove_dir(&ssh).unwrap();
        std::os::unix::fs::symlink(home.join("keys"), home.join("links/ssh")).unwrap();
        std::os::unix::fs::symlink(home.join("links/ssh"), &ssh).unwrap();
        assert_eq!(
            risky(&[mount(&home.join("links"), false)]),
            [
                "~/links (read-write) contains ~/links/ssh, a symlink on the way to sendit's SSH keys"
            ]
        );
    }

    #[test]
    fn flags_other_shares_of_the_project_repository() {
        let temp = TempDir::new("risky-git");
        let paths = Paths::new(temp.path().join("home"));
        let code = temp.path().join("code");
        let app = code.join("app");
        for dir in [".git/hooks", "src"] {
            fs::create_dir_all(app.join(dir)).unwrap();
        }
        let mount = |host: &Path, read_only| Mount {
            host: host.to_path_buf(),
            guest: "/mnt/x".into(),
            read_only,
        };
        let project = mount(&app, false);
        let risky = |extra: &Path, expose_git| {
            risky_shares(
                &paths,
                &sharing(&[project.clone(), mount(extra, true)], expose_git),
            )
        };
        let shown = |path: &Path| path.display().to_string();

        // The project's own share hides `.git`.
        assert!(risky_shares(&paths, &sharing(std::slice::from_ref(&project), false)).is_empty());
        assert!(risky(&app.join("src"), false).is_empty());
        assert_eq!(
            risky(&code, false),
            [format!(
                "{} (read-only) contains the project's git repository ({})",
                shown(&code),
                shown(&app.join(".git"))
            )]
        );
        // The project again, at another guest path.
        assert_eq!(risky(&app, false).len(), 1);
        assert_eq!(
            risky(&app.join(".git/hooks"), false),
            [format!(
                "{} (read-only) is inside the project's git repository ({})",
                shown(&app.join(".git/hooks")),
                shown(&app.join(".git"))
            )]
        );
        assert!(risky(&code, true).is_empty());

        // The repository a subdirectory is in.
        let web = app.join("src/web");
        fs::create_dir_all(&web).unwrap();
        let project = mount(&web, false);
        let risky = |extra: &Path| {
            risky_shares(
                &paths,
                &sharing(&[project.clone(), mount(extra, true)], false),
            )
        };
        assert!(risky(&app.join("src")).is_empty());
        assert_eq!(risky(&app).len(), 1);

        // A linked worktree's repository, in its main worktree.
        let (main, wt) = worktree::layout(temp.path());
        let project = mount(&wt, false);
        let risky = |extra: &Path| {
            risky_shares(
                &paths,
                &sharing(&[project.clone(), mount(extra, true)], false),
            )
        };
        assert_eq!(
            risky(&main),
            [format!(
                "{} (read-only) contains the project's git repository ({})",
                shown(&main),
                shown(&main.join(".git"))
            )]
        );

        // A repository in the project that `.git` leads to, which the
        // project's own share exposes: the bare repository of worktrees
        // inside the project, and a `.git` symlink.
        let alone =
            |project: &Path| risky_shares(&paths, &sharing(&[mount(project, false)], false));
        let bare = temp.path().join("bare");
        fs::create_dir_all(bare.join(".bare")).unwrap();
        fs::write(bare.join(".git"), "gitdir: ./.bare\n").unwrap();
        assert_eq!(
            alone(&bare),
            [format!(
                "{} (read-write) contains the project's git repository ({})",
                shown(&bare),
                shown(&bare.join(".bare"))
            )]
        );
        let linked = temp.path().join("linked");
        fs::create_dir_all(linked.join(".realgit")).unwrap();
        std::os::unix::fs::symlink(".realgit", linked.join(".git")).unwrap();
        assert_eq!(
            alone(&linked),
            [format!(
                "{} (read-write) contains the project's git repository ({})",
                shown(&linked),
                shown(&linked.join(".realgit"))
            )]
        );
    }

    #[test]
    fn flags_shares_of_directory_links_on_the_way() {
        let temp = TempDir::new("risky-dir-links");
        let mount = |host: PathBuf| Mount {
            host,
            guest: "/mnt/x".into(),
            read_only: false,
        };

        // A script linked into a dotfiles repo whose scripts directory is a
        // link of its own: the VM could point that one elsewhere.
        let home = temp.path().join("a");
        for dir in [".config/sendit/provision-scripts", "store/scripts"] {
            fs::create_dir_all(home.join(dir)).unwrap();
        }
        fs::create_dir_all(home.join("dotfiles")).unwrap();
        fs::write(home.join("store/scripts/rust.sh"), "").unwrap();
        std::os::unix::fs::symlink(home.join("store/scripts"), home.join("dotfiles/scripts"))
            .unwrap();
        std::os::unix::fs::symlink(
            home.join("dotfiles/scripts/rust.sh"),
            home.join(".config/sendit/provision-scripts/rust.sh"),
        )
        .unwrap();
        let paths = Paths::new(home.clone());
        let risky = |dir: &str| risky_shares(&paths, &sharing(&[mount(home.join(dir))], true));
        assert_eq!(
            risky("dotfiles"),
            [
                "~/dotfiles (read-write) contains ~/dotfiles/scripts, a symlink on the way to sendit's provisioning scripts"
            ]
        );
        assert_eq!(
            risky("store"),
            [
                "~/store (read-write) contains sendit's provisioning scripts (~/store/scripts/rust.sh)"
            ]
        );
        // Where the script would be once the VM creates the directory.
        fs::remove_dir_all(home.join("store/scripts")).unwrap();
        assert_eq!(
            risky("store"),
            [
                "~/store (read-write) contains sendit's provisioning scripts (~/store/scripts/rust.sh)"
            ]
        );

        // A linked ~/.config, through a link in a dotfiles repo.
        let home = temp.path().join("b");
        for dir in ["dotfiles", "store/config/sendit"] {
            fs::create_dir_all(home.join(dir)).unwrap();
        }
        std::os::unix::fs::symlink(home.join("store/config"), home.join("dotfiles/config"))
            .unwrap();
        std::os::unix::fs::symlink(home.join("dotfiles/config"), home.join(".config")).unwrap();
        let paths = Paths::new(home.clone());
        let risky = |dir: &str| risky_shares(&paths, &sharing(&[mount(home.join(dir))], true));
        assert_eq!(
            risky("dotfiles"),
            [
                "~/dotfiles (read-write) contains ~/dotfiles/config, a symlink on the way to sendit's configuration"
            ]
        );
        assert_eq!(
            risky("store"),
            ["~/store (read-write) contains sendit's configuration (~/store/config/sendit)"]
        );
    }
}
