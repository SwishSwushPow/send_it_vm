//! The commands that inspect and clean up VMs: `status`, `reset`, `list`
//! and `prune`, plus the confirmation prompts they share with `run`.

use std::fs;
use std::io::{BufRead, IsTerminal, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};

use anyhow::{Result, bail, ensure};

use crate::config::{ByteSize, VmSettings};
use crate::paths::{ImageName, Paths, Project};
use crate::project_vm::{self, State};
use crate::provision::{self, BaseState};
use crate::vm;

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
    println!("{:<8} {:>9}  PROJECT", "STATE", "DISK");
    for dir in dirs {
        let metadata = project_vm::metadata(&dir);
        let state = match project_vm::state(&dir)? {
            State::Running(_) => "running",
            State::Stopped if metadata.as_ref().is_ok_and(|m| m.outdated()) => "outdated",
            State::Stopped => "stopped",
        };
        // Blocks shared with the base image (as APFS clones) count too.
        let disk = ByteSize(fs::metadata(dir.disk()).map_or(0, |m| m.blocks() * 512));
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
        println!("{state:<8} {:>9}  {project}", disk.approx());
    }
    Ok(())
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
    let image = ImageName::default();
    let base_dir = paths.image_dir(&image);
    let base_state = match provision::base_state(paths, &image)? {
        BaseState::Missing => "run `sendit provision`",
        BaseState::Outdated => "outdated; `sendit provision --force` rebuilds it",
        BaseState::ScriptsChanged => {
            "custom scripts changed; `sendit provision --force` rebuilds it"
        }
        BaseState::Current => "provisioned",
    };
    let vm_dir = project_vm::dir(paths, project);
    let state = if !vm_dir.path().exists() {
        "not created".to_string()
    } else {
        match project_vm::state(&vm_dir)? {
            State::Stopped if project_vm::metadata(&vm_dir).is_ok_and(|m| m.outdated()) => {
                "stopped; made from an outdated base image, `sendit reset` recreates it".to_string()
            }
            State::Stopped => "stopped".to_string(),
            State::Running(_) => match vm::net::vm_ip(&vm_dir)? {
                Some(ip) => format!("running at {ip}"),
                None => "running".to_string(),
            },
        }
    };
    println!("base     {} ({base_state})", paths.display(&base_dir));
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
