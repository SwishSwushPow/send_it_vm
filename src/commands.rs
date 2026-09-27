//! The commands that inspect and clean up VMs: `status`, `reset`, `list`,
//! `images` and `prune`, plus the confirmation prompts they share with `run`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, IsTerminal, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail, ensure};

use crate::config::{ByteSize, Config, Mount, VmSettings};
use crate::paths::{ImageName, Paths, Project};
use crate::project_vm::{self, State};
use crate::provision::{self, BaseState};
use crate::vm::{self, VmDir};

/// Asks before sharing directories that contain the home directory, or
/// contain or sit inside sendit's own state. Through those the VM could
/// become root (the root SSH key), add shares on the next run (the config
/// file) or run commands as root on the next boot (a VM's mount script).
/// Read-only shares count too: reading the root key is enough.
pub fn confirm_shares(paths: &Paths, settings: &VmSettings) -> Result<bool> {
    let risky = risky_shares(paths, &settings.mounts);
    if risky.is_empty() {
        return Ok(true);
    }
    let list: String = risky.iter().map(|line| format!("\n  {line}")).collect();
    confirm(
        &format!(
            "These shares let the VM get around its limits, e.g. become root or run \
             commands on this Mac:{list}\nShare them anyway?"
        ),
        &format!("not sharing these without a terminal to confirm it:{list}"),
    )
}

/// One line for each mount that `confirm_shares` asks about.
fn risky_shares(paths: &Paths, mounts: &[Mount]) -> Vec<String> {
    // Canonical, like the mounts' host paths, so that a symlinked
    // ~/.config/sendit is caught through a share of its target. The files
    // most worth protecting come last, for when they are symlinks of their
    // own, e.g. into a dotfiles repo.
    let state = [
        (paths.config_dir(), "sendit's configuration"),
        (paths.cache_dir(), "sendit's images and SSH keys"),
        (paths.vms_dir(), "sendit's VMs"),
        (paths.config_file(), "sendit's configuration"),
        (
            paths.provision_scripts_dir(),
            "sendit's provisioning scripts",
        ),
        (paths.ssh_dir(), "sendit's SSH keys"),
    ]
    .map(|(path, what)| (canonical(&path), what));
    let home = canonical(paths.home());

    mounts
        .iter()
        .filter_map(|mount| {
            let host = &mount.host;
            let reason = if home.starts_with(host) {
                "contains your home directory".to_string()
            } else {
                state.iter().find_map(|(path, what)| {
                    if path.starts_with(host) {
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

/// `path` with symlinks resolved as far as it exists.
fn canonical(path: &Path) -> PathBuf {
    if let Ok(path) = path.canonicalize() {
        return path;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => canonical(parent).join(name),
        _ => path.to_path_buf(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::TempDir;

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
        let risky = |mounts: &[Mount]| risky_shares(&paths, mounts);

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
    }
}
