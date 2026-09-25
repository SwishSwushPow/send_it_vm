mod cli;
mod config;
mod image;
mod paths;
mod provision;
mod vm;

use anyhow::{Result, bail};
use clap::Parser;

use crate::cli::{Cli, Command};
use crate::config::{Config, Settings, VmSettings};
use crate::paths::{Paths, Project};

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
            print_status(&paths, &project, &settings);
            Ok(())
        }
        Command::Run(args) => {
            let project = project()?;
            let settings =
                Config::load(&paths)?.resolve(&paths, &project, &args.settings(), &cwd)?;
            print_status(&paths, &project, &settings);
            bail!("booting VMs is not implemented yet")
        }
        Command::Provision { force } => {
            provision::provision(&paths, &Config::load(&paths)?, *force)
        }
        Command::Ssh
        | Command::Stop
        | Command::Reset { .. }
        | Command::List
        | Command::Prune { .. } => bail!("not implemented yet"),
    }
}

fn print_status(paths: &Paths, project: &Project, settings: &VmSettings) {
    let base_dir = paths.base_dir();
    let base_state = if provision::marker(paths).exists() {
        "provisioned"
    } else {
        "run `send_it provision`"
    };
    let vm_dir = paths.vm_dir(project);
    let state = if vm_dir.exists() {
        "created"
    } else {
        "not created"
    };
    println!("base     {} ({base_state})", paths.display(&base_dir));
    println!("project  {}", project.root.display());
    println!("vm       {} ({state})", paths.display(&vm_dir));
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
}
