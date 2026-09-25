mod cli;
mod config;
mod image;
mod mounts;
mod paths;
mod project_vm;
mod provision;
mod vm;

use std::io::{BufRead, IsTerminal, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};

use anyhow::{Result, bail};
use clap::Parser;

use crate::cli::{Cli, Command};
use crate::config::{Config, Settings, VmSettings};
use crate::paths::{Paths, Project};
use crate::project_vm::State;
use crate::vm::VmDir;

fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = Paths::from_env()?;
    let cwd = std::env::current_dir()?;
    let project = || Project::at(cli.project.as_deref().unwrap_or(&cwd));

    match &cli.command {
        Command::Status => {
            let project = project()?;
            let settings =
                Config::load(&paths)?.resolve(&paths, &project, &Settings::default(), &cwd)?;
            print_status(&paths, &project, &settings)
        }
        Command::Run(args) => {
            let project = project()?;
            let settings =
                Config::load(&paths)?.resolve(&paths, &project, &args.settings(), &cwd)?;
            if !confirm_home_share(&paths, &project)? {
                return Ok(());
            }
            project_vm::run(&paths, &project, &settings)
        }
        Command::Provision { force, resources } => {
            let settings = Config::load(&paths)?.resolve_provision(&resources.resources())?;
            provision::provision(&paths, &settings, *force)
        }
        Command::Ssh { command } => project_vm::ssh(&paths, &project()?, command),
        Command::Stop => project_vm::stop(&paths, &project()?),
        Command::Reset { yes } => reset(&paths, &project()?, *yes),
        Command::List => list(&paths),
        Command::Prune { yes } => prune(&paths, *yes),
    }
}

/// Asks before sharing a project directory that contains the home directory:
/// the VM could then read and change everything there, including sendit's
/// SSH key and the disks of other VMs.
fn confirm_home_share(paths: &Paths, project: &Project) -> Result<bool> {
    let home = paths.home();
    let home = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
    if !home.starts_with(&project.root) {
        return Ok(true);
    }
    let root = project.root.display();
    if !std::io::stdin().is_terminal() {
        bail!(
            "{root} contains your home directory; not sharing all of it with the VM \
             without a terminal to confirm it"
        );
    }
    confirm(&format!(
        "{root} contains your home directory. Share all of it with the VM, read-write?"
    ))
}

fn reset(paths: &Paths, project: &Project, yes: bool) -> Result<()> {
    let dir = VmDir::new(paths.vm_dir(project));
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
    if yes || confirm(&question)? {
        project_vm::delete(&dir)?;
        eprintln!("Deleted {}.", paths.display(dir.path()));
    }
    Ok(())
}

fn list(paths: &Paths) -> Result<()> {
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
        let disk = std::fs::metadata(dir.disk()).map_or(0, |m| m.blocks() * 512);
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
        println!("{state:<8} {:>9}  {project}", approx_size(disk));
    }
    Ok(())
}

fn prune(paths: &Paths, yes: bool) -> Result<()> {
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
            eprintln!(
                "Skipping {}: it is still running.",
                metadata.project_path.display()
            );
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
    if yes || confirm(&format!("Delete {count}?"))? {
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

/// Asks a yes/no question on the terminal; "no" unless the answer is yes.
fn confirm(question: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("not asking for confirmation without a terminal; pass --yes");
    }
    eprint!("{question} [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "Yes"))
}

/// Formats a size for humans, e.g. `1.8 GiB`.
fn approx_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

fn print_status(paths: &Paths, project: &Project, settings: &VmSettings) -> Result<()> {
    let base_dir = paths.base_dir();
    let base_state = if !provision::marker(paths).exists() {
        "run `sendit provision`"
    } else if provision::base_revision(paths)? < provision::BASE_REVISION {
        "outdated; `sendit provision --force` rebuilds it"
    } else if provision::custom_scripts_changed(paths)? {
        "custom scripts changed; `sendit provision --force` rebuilds it"
    } else {
        "provisioned"
    };
    let vm_dir = VmDir::new(paths.vm_dir(project));
    let state = if !vm_dir.path().exists() {
        "not created".to_string()
    } else {
        match project_vm::state(&vm_dir)? {
            State::Stopped if project_vm::metadata(&vm_dir).is_ok_and(|m| m.outdated()) => {
                "stopped; made from an outdated base image, `sendit reset` recreates it".to_string()
            }
            State::Stopped => "stopped".to_string(),
            State::Running(_) => {
                let mac = std::fs::read_to_string(vm_dir.mac()).unwrap_or_default();
                match vm::net::lease_for(mac.trim())? {
                    Some(ip) => format!("running at {ip}"),
                    None => "running".to_string(),
                }
            }
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
            if mount.read_only { "ro" } else { "rw" },
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_approximate_sizes() {
        assert_eq!(approx_size(512), "512 B");
        assert_eq!(approx_size(1536), "1.5 KiB");
        assert_eq!(approx_size(1_932_735_283), "1.8 GiB");
    }
}
