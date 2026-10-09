//! The commands that inspect and clean up VMs: `status`, `reset`, `list`,
//! `images` and `prune`, plus the confirmation prompts they share with `run`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, IsTerminal, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};

use anyhow::{Result, bail, ensure};

use crate::config::{ByteSize, Config, Mount, VmSettings};
use crate::paths::{ImageName, Paths, Project};
use crate::project_vm::{self, State};
use crate::provision::{self, BaseState};
use crate::status::Status;
use crate::util::canonical;
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

pub fn reset(paths: &Paths, project: &Project, yes: bool) -> Result<()> {
    let dir = project_vm::dir(paths, project);
    if !dir.path().exists() {
        eprintln!("This project has no VM.");
        return Ok(());
    }
    if project_vm::state(&dir)? != State::Stopped {
        bail!("the VM is running; stop it first with `sendit stop`");
    }
    let parent = project_vm::parent(paths, project).and_then(|p| project_vm::metadata(&p).ok());
    let next = match parent {
        Some(parent) => format!(
            "of the VM of {}, or of the base image if that is running then",
            paths.display(&parent.project_path)
        ),
        None => "of the base image".to_string(),
    };
    let question = format!(
        "Delete {} and everything stored in it? The next `run` starts from a fresh copy {next}.",
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
        let (image, project) = match metadata {
            Ok(metadata) => {
                let path = metadata.project_path.display();
                let project = match project_dir(&metadata.project_path) {
                    ProjectDir::Present => path.to_string(),
                    ProjectDir::Missing => format!("{path} (missing)"),
                    ProjectDir::Unmounted => format!("{path} (volume not mounted)"),
                    ProjectDir::Inaccessible(_) => format!("{path} (inaccessible)"),
                };
                (metadata.image.to_string(), project)
            }
            Err(_) => (
                "?".to_string(),
                format!("? ({})", paths.display(dir.path())),
            ),
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

pub fn prune(paths: &Paths, scope: PruneScope, yes: bool) -> Result<()> {
    let mut orphans = Vec::new();
    let mut old = Vec::new();
    let mut others = Vec::new();
    for dir in project_vm::all(paths)? {
        let all = scope == PruneScope::All;
        let (project, group) = match project_vm::metadata(&dir) {
            Ok(metadata) => {
                let path = &metadata.project_path;
                let group = match project_dir(path) {
                    ProjectDir::Missing => &mut orphans,
                    _ if scope >= PruneScope::Outdated && metadata.outdated() => &mut old,
                    _ if all => &mut others,
                    ProjectDir::Present => continue,
                    ProjectDir::Unmounted => {
                        eprintln!("Skipping {}: its volume is not mounted.", path.display());
                        continue;
                    }
                    ProjectDir::Inaccessible(e) => {
                        eprintln!("Skipping {}: {e}", path.display());
                        continue;
                    }
                };
                (path.display().to_string(), group)
            }
            Err(_) if all => ("?".to_string(), &mut others),
            Err(_) => continue,
        };
        if project_vm::state(&dir)? != State::Stopped {
            eprintln!("Skipping {project}: it is still running.");
            continue;
        }
        group.push((dir, project));
    }
    if orphans.is_empty() && old.is_empty() && others.is_empty() {
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
        ("VMs whose project directory no longer exists:", &orphans),
        ("VMs made from an outdated base image:", &old),
        ("Other VMs:", &others),
    ] {
        if vms.is_empty() {
            continue;
        }
        eprintln!("{heading}");
        for (dir, project) in vms {
            eprintln!("  {project}  ({})", paths.display(dir.path()));
        }
    }
    let count = match orphans.len() + old.len() + others.len() {
        1 => "1 VM".to_string(),
        n => format!("{n} VMs"),
    };
    if yes || confirm(&format!("Delete {count}?"), PASS_YES)? {
        for (dir, _) in orphans.iter().chain(&old).chain(&others) {
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
pub fn confirm(question: &str, no_terminal: &str) -> Result<bool> {
    ensure!(std::io::stdin().is_terminal(), "{no_terminal}");
    Status::Question(question).report();
    eprint!("{question} [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    let read = std::io::stdin().lock().read_line(&mut answer);
    Status::Clear.report();
    read?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "Yes"))
}

pub fn status(paths: &Paths, project: &Project, settings: &VmSettings) -> Result<()> {
    let image = &project_vm::image(paths, project, settings.image.as_ref());
    let base_dir = paths.image_dir(image);
    let base_state = provision::base_state(paths, image)?.describe(image);
    let vm_dir = project_vm::dir(paths, project);
    let metadata = project_vm::metadata(&vm_dir).ok();
    let state = if !vm_dir.path().exists() {
        let parent = project_vm::parent(paths, project).and_then(|p| project_vm::metadata(&p).ok());
        match parent {
            Some(m) if m.image == *image && !m.outdated() => format!(
                "not created; `sendit run` copies the VM of {}",
                paths.display(&m.project_path)
            ),
            _ => "not created".to_string(),
        }
    } else {
        match (project_vm::state(&vm_dir)?, &metadata) {
            (State::Stopped, Some(m)) if m.image != *image => format!(
                "stopped; made from the {} image, `sendit reset` recreates it from {image}",
                m.image
            ),
            (State::Stopped, Some(m)) if m.outdated() => {
                "stopped; made from an outdated base image, `sendit reset` recreates it".to_string()
            }
            (State::Stopped, _) => "stopped".to_string(),
            (State::Running(_), _) => match vm::net::vm_ip(&vm_dir)? {
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
    if let Some(copy_of) = metadata.and_then(|m| m.copy_of) {
        println!("copy of  the VM of {}", paths.display(&copy_of));
    }
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
        // Not a VM, e.g. in a $SENDIT_VM_DIR that holds other things too.
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
