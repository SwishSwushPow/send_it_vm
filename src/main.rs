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
mod util;
mod vm;
mod worktree;

use std::process::ExitCode;

use anyhow::{Result, ensure};

use crate::cli::{Cli, Command};
use crate::commands::{Declined, PruneScope};
use crate::config::{Config, Settings};
use crate::paths::{Paths, Project};
use crate::project_vm::Detach;

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
    let cli = Cli::try_parse_checked_from(std::env::args_os()).unwrap_or_else(|e| e.exit());
    sign::ensure_entitled()?;
    let paths = Paths::from_env()?;
    // Every time, so that directories made by older versions, or deleted
    // and made again, are private and excluded too.
    for dir in paths.rebuildable_dirs() {
        if let Err(error) = util::make_private(&dir) {
            eprintln!(
                "warning: could not make {} private to you: {error:#}",
                paths.display(&dir)
            );
        }
        if let Err(error) = util::exclude_from_backups(&dir) {
            eprintln!(
                "warning: could not exclude {} from Time Machine backups: {error:#}",
                paths.display(&dir)
            );
        }
    }
    let cwd = std::env::current_dir()?;
    let project = || match &cli.name {
        Some(name) => project_vm::find(&paths, name),
        None => Project::at(cli.project.as_deref().unwrap_or(&cwd)),
    };
    let resolve = |project: &Project, cli: &Settings, fresh: bool| {
        Config::load(&paths)?.resolve(&paths, project, cli, &cwd, fresh)
    };

    let done = match &cli.command {
        Command::Status => {
            let project = project()?;
            let settings = resolve(&project, &Settings::default(), false)?;
            commands::status(&paths, &project, &settings)
        }
        Command::Run(args) => {
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
                        Some(name) => project_vm::find(&paths, name)?.root,
                        None => cli.project.clone().unwrap_or_else(|| cwd.clone()),
                    };
                    Project::at(&worktree::open(
                        &dir,
                        branch,
                        args.from.as_deref(),
                        args.worktree_path.as_deref(),
                    )?)?
                }
                // Found by name, the project directory may be gone, but it
                // must be there to run, and still lead to the same VM.
                (None, None) if cli.name.is_some() => {
                    let found = project()?;
                    let project = Project::at(&found.root)?;
                    ensure!(
                        project.id == found.id,
                        "the VM's project directory {} now resolves to {}, a project \
                         of its own",
                        found.root.display(),
                        project.root.display()
                    );
                    project
                }
                (None, None) => project()?,
            };
            let mut settings = resolve(&project, &args.settings(), args.fresh)?;
            // In the background, the process that started this one has
            // asked, and there is no terminal to ask on.
            if detach != Detach::Background {
                commands::confirm_shares(&paths, &settings)?;
            }
            let created = project_vm::create_if_missing(
                &paths,
                &project,
                &settings,
                args.fresh,
                &commands::confirm,
            )?;
            // Made from the image instead of its running parent VM, from a
            // build whose scripts asked for other shares than the parent's.
            if created.as_ref().is_some_and(|created| created.other_shares) {
                settings = resolve(&project, &args.settings(), args.fresh)?;
                commands::confirm_shares(&paths, &settings)?;
            }
            let note = created.and_then(|created| created.note);
            let code = project_vm::run(
                &paths,
                &project,
                &settings,
                args.fresh,
                note,
                &args.command,
                detach,
            )?;
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
            for image in &images {
                let settings = config.resolve_provision(image, resources)?;
                provision::provision(&paths, image, &settings, *force)?;
            }
            Ok(())
        }
        Command::Ssh { root, command } => project_vm::ssh(&paths, &project()?, *root, command),
        Command::Stop => project_vm::stop(&paths, &project()?),
        Command::Rename { new_name } => project_vm::rename(&paths, &project()?, new_name),
        Command::Reset { yes } => {
            let project = project()?;
            // Only for a hint: a config file sendit can't read doesn't stop it.
            let chosen = Config::load(&paths)
                .and_then(|config| config.resolve_image(&paths, &project))
                .ok()
                .flatten();
            commands::reset(&paths, &project, chosen.as_ref(), *yes)
        }
        Command::List => commands::list(&paths),
        Command::Images { delete, yes } if !delete.is_empty() => {
            commands::delete_images(&paths, &Config::load(&paths)?, delete, *yes)
        }
        Command::Images { .. } => commands::images(&paths, &Config::load(&paths)?),
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
