#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
compile_error!("sendit requires macOS on Apple silicon");

mod cli;
mod commands;
mod config;
mod image;
mod mounts;
mod paths;
mod project_vm;
mod provision;
mod sign;
mod status;
mod util;
mod vm;
mod worktree;

use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Command};
use crate::commands::{Declined, PruneScope};
use crate::config::{Config, Settings};
use crate::paths::{Paths, Project};

fn main() -> ExitCode {
    match sendit() {
        Ok(code) => code,
        // The user said no to a question; there is nothing to add.
        Err(e) if e.is::<Declined>() => ExitCode::FAILURE,
        // As for an error returned from `main`.
        Err(e) => {
            eprintln!("Error: {e:?}");
            ExitCode::FAILURE
        }
    }
}

fn sendit() -> Result<ExitCode> {
    let cli = Cli::parse();
    sign::ensure_entitled()?;
    let paths = Paths::from_env()?;
    // Every time, so that directories made by older versions, or deleted
    // and made again, are excluded too.
    for dir in paths.rebuildable_dirs() {
        if let Err(error) = util::exclude_from_backups(&dir) {
            eprintln!(
                "warning: could not exclude {} from Time Machine backups: {error:#}",
                paths.display(&dir)
            );
        }
    }
    let cwd = std::env::current_dir()?;
    let project = || Project::at(cli.project.as_deref().unwrap_or(&cwd));
    let resolve = |project: &Project, cli: &Settings| {
        Config::load(&paths)?.resolve(&paths, project, cli, &cwd)
    };

    let done = match &cli.command {
        Command::Status => {
            let project = project()?;
            let settings = resolve(&project, &Settings::default())?;
            commands::status(&paths, &project, &settings)
        }
        Command::Run(args) => {
            let project = match &args.worktree {
                Some(branch) => Project::at(&worktree::open(
                    cli.project.as_deref().unwrap_or(&cwd),
                    branch,
                    args.from.as_deref(),
                    args.worktree_path.as_deref(),
                )?)?,
                None => project()?,
            };
            let settings = resolve(&project, &args.settings())?;
            commands::confirm_shares(&paths, &settings)?;
            let code = project_vm::run(&paths, &project, &settings, &args.command)?;
            // Exit codes and 128 plus a signal number fit.
            return Ok(ExitCode::from(u8::try_from(code).unwrap_or(u8::MAX)));
        }
        Command::Provision {
            image,
            all,
            force,
            resources,
        } => {
            let config = Config::load(&paths)?;
            let images = match image {
                _ if *all => provision::images(&paths, &config)?,
                Some(image) => vec![image.clone()],
                None => {
                    let project = project()?;
                    let chosen = config.resolve_image(&paths, &project)?;
                    vec![project_vm::image(&paths, &project, chosen.as_ref())]
                }
            };
            provision::provision_all(&paths, &config, &images, resources, *force)
        }
        Command::Ssh { root, command } => project_vm::ssh(&paths, &project()?, *root, command),
        Command::Stop => project_vm::stop(&paths, &project()?),
        Command::Reset { yes } => commands::reset(&paths, &project()?, *yes),
        Command::List => commands::list(&paths),
        Command::Images => commands::images(&paths, &Config::load(&paths)?),
        Command::Prune { outdated, all, yes } => {
            let scope = if *all {
                PruneScope::All
            } else if *outdated {
                PruneScope::Outdated
            } else {
                PruneScope::Missing
            };
            commands::prune(&paths, scope, *yes)
        }
    };
    done.map(|()| ExitCode::SUCCESS)
}
