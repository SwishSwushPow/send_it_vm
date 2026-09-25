//! The commands that inspect and clean up VMs: `status`, `reset`, `list`,
//! `images` and `prune`, plus the confirmation prompts they share with `run`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, IsTerminal, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};

use anyhow::{Result, bail, ensure};

use crate::config::{ByteSize, Config, VmSettings};
use crate::paths::{ImageName, Paths, Project};
use crate::project_vm::{self, State};
use crate::provision::{self, BaseState};
use crate::vm::{self, VmDir};

/// Asks before sharing a project directory that contains the home directory:
/// the VM could then read and change everything there, including sendit's
/// SSH key and the disks of other VMs.
pub fn confirm_home_share(paths: &Paths, project: &Project) -> Result<bool> {
    let home = paths.home();
    let home = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
    if !home.starts_with(&project.root) {
        return Ok(true);
    }
    let root = project.root.display();
    confirm(
        &format!("{root} contains your home directory. Share all of it with the VM, read-write?"),
        &format!(
            "{root} contains your home directory; not sharing all of it with the VM \
             without a terminal to confirm it"
        ),
    )
}

pub fn reset(paths: &Paths, project: &Project, yes: bool) -> Result<()> {
    let dir = project_vm::dir(paths, project);
    if !dir.path().exists() {
        eprintln!("This project has no VM.");
        return Ok(());
    }
    if project_vm::state(&dir)? != State::Stopped {
        bail!("the VM is running; stop it first with `sendit stop`");
    }
    let question = format!(
        "Delete {} and everything stored in it? The next `run` starts from a fresh copy of the base image.",
        paths.display(dir.path())
    );
    if yes || confirm(&question, PASS_YES)? {
        project_vm::delete(&dir)?;
        eprintln!("Deleted {}.", paths.display(dir.path()));
    }
    Ok(())
}

pub fn list(paths: &Paths) -> Result<()> {
    let dirs = project_vm::all(paths)?;
    if dirs.is_empty() {
        eprintln!("No VMs in {}.", paths.display(&paths.vms_dir()));
        return Ok(());
    }
    let mut rows = Vec::new();
    for dir in dirs {
        let metadata = project_vm::metadata(&dir);
        let state = match project_vm::state(&dir)? {
            State::Running(_) => "running",
            State::Stopped if metadata.as_ref().is_ok_and(|m| m.outdated()) => "outdated",
            State::Stopped => "stopped",
        };
        let image = match &metadata {
            Ok(metadata) => metadata.image.to_string(),
            Err(_) => "?".to_string(),
        };
        let project = match metadata {
            Ok(metadata) => {
                let path = metadata.project_path.display();
                match project_dir(&metadata.project_path) {
                    ProjectDir::Present => path.to_string(),
                    ProjectDir::Missing => format!("{path} (missing)"),
                    ProjectDir::Unmounted => format!("{path} (volume not mounted)"),
                    ProjectDir::Inaccessible(_) => format!("{path} (inaccessible)"),
                }
            }
            Err(_) => format!("? ({})", paths.display(dir.path())),
        };
        rows.push((state, allocated(&dir.disk()), image, project));
    }
    let width = column_width("IMAGE", rows.iter().map(|row| &row.2));
    println!("{:<8} {:>9}  {:<width$}  PROJECT", "STATE", "DISK", "IMAGE");
    for (state, disk, image, project) in rows {
        println!(
            "{state:<8} {:>9}  {image:<width$}  {project}",
            disk.approx()
        );
    }
    Ok(())
}

pub fn images(paths: &Paths, config: &Config) -> Result<()> {
    let mut vms = BTreeMap::<ImageName, usize>::new();
    for dir in project_vm::all(paths)? {
        if let Ok(metadata) = project_vm::metadata(&dir) {
            *vms.entry(metadata.image).or_default() += 1;
        }
    }
    // Images that VMs were made from stay listed after they were deleted.
    let images: BTreeSet<_> = provision::images(paths, config)?
        .into_iter()
        .chain(vms.keys().cloned())
        .collect();

    let width = column_width("IMAGE", images.iter().map(ImageName::as_str));
    println!("{:<width$}  {:<15} {:>9}  VMS", "IMAGE", "STATE", "DISK");
    for image in &images {
        let (state, provisioned) = match provision::base_state(paths, image)? {
            BaseState::Missing => ("missing", false),
            BaseState::Outdated => ("outdated", true),
            BaseState::ScriptsChanged => ("scripts changed", true),
            BaseState::Current => ("provisioned", true),
        };
        let disk = if provisioned {
            allocated(&VmDir::new(paths.image_dir(image)).disk()).approx()
        } else {
            "-".to_string()
        };
        let count = vms.get(image).copied().unwrap_or(0);
        println!("{:<width$}  {state:<15} {disk:>9}  {count}", image.as_str());
    }
    Ok(())
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

pub fn prune(paths: &Paths, yes: bool) -> Result<()> {
    let mut orphans = Vec::new();
    for dir in project_vm::all(paths)? {
        let Ok(metadata) = project_vm::metadata(&dir) else {
            continue;
        };
        let path = &metadata.project_path;
        match project_dir(path) {
            ProjectDir::Missing => {}
            ProjectDir::Present => continue,
            ProjectDir::Unmounted => {
                eprintln!("Skipping {}: its volume is not mounted.", path.display());
                continue;
            }
            ProjectDir::Inaccessible(e) => {
                eprintln!("Skipping {}: {e}", path.display());
                continue;
            }
        }
        if project_vm::state(&dir)? != State::Stopped {
            eprintln!("Skipping {}: it is still running.", path.display());
            continue;
        }
        orphans.push((dir, metadata.project_path));
    }
    if orphans.is_empty() {
        eprintln!("No VMs of missing projects.");
        return Ok(());
    }
    eprintln!("VMs whose project directory no longer exists:");
    for (dir, project) in &orphans {
        eprintln!("  {}  ({})", project.display(), paths.display(dir.path()));
    }
    let count = match orphans.len() {
        1 => "1 VM".to_string(),
        n => format!("{n} VMs"),
    };
    if yes || confirm(&format!("Delete {count}?"), PASS_YES)? {
        for (dir, _) in &orphans {
            project_vm::delete(dir)?;
        }
        eprintln!("Deleted {count}.");
    }
    Ok(())
}

/// Whether a VM's project directory is still there.
enum ProjectDir {
    Present,
    Missing,
    /// Below `/Volumes/<name>`, and that volume isn't mounted: the project
    /// may well come back.
    Unmounted,
    /// Can't tell, e.g. for lack of permissions.
    Inaccessible(std::io::Error),
}

fn project_dir(path: &Path) -> ProjectDir {
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
        Err(e) => ProjectDir::Inaccessible(e),
    }
}

const PASS_YES: &str = "not asking for confirmation without a terminal; pass --yes";

/// Asks a yes/no question on the terminal; "no" unless the answer is yes.
/// Fails with `no_terminal` if stdin isn't a terminal.
fn confirm(question: &str, no_terminal: &str) -> Result<bool> {
    ensure!(std::io::stdin().is_terminal(), "{no_terminal}");
    eprint!("{question} [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "Yes"))
}

pub fn status(paths: &Paths, project: &Project, settings: &VmSettings) -> Result<()> {
    let image = &project_vm::image(paths, project, settings.image.as_ref());
    let base_dir = paths.image_dir(image);
    let base_state = match provision::base_state(paths, image)? {
        BaseState::Missing => format!("run `{}`", provision::command(image, false)),
        BaseState::Outdated => {
            format!(
                "outdated; `{}` rebuilds it",
                provision::command(image, true)
            )
        }
        BaseState::ScriptsChanged => format!(
            "custom scripts changed; `{}` rebuilds it",
            provision::command(image, true)
        ),
        BaseState::Current => "provisioned".to_string(),
    };
    let vm_dir = project_vm::dir(paths, project);
    let state = if !vm_dir.path().exists() {
        "not created".to_string()
    } else {
        let metadata = project_vm::metadata(&vm_dir).ok();
        match project_vm::state(&vm_dir)? {
            State::Stopped if metadata.as_ref().is_some_and(|m| m.image != *image) => format!(
                "stopped; made from the {} image, `sendit reset` recreates it from {image}",
                metadata.map(|m| m.image).unwrap_or_default()
            ),
            State::Stopped if metadata.is_some_and(|m| m.outdated()) => {
                "stopped; made from an outdated base image, `sendit reset` recreates it".to_string()
            }
            State::Stopped => "stopped".to_string(),
            State::Running(_) => match vm::net::vm_ip(&vm_dir)? {
                Some(ip) => format!("running at {ip}"),
                None => "running".to_string(),
            },
        }
    };
    println!(
        "image    {image} in {} ({base_state})",
        paths.display(&base_dir)
    );
    println!("project  {}", project.root.display());
    println!("vm       {} ({state})", paths.display(vm_dir.path()));
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
