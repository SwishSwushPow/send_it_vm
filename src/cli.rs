use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::config::{MountSpec, Resources, Settings};
use crate::paths::ImageName;

/// Light and fast per-project Debian VMs on macOS.
#[derive(Debug, Parser)]
#[command(name = "sendit", version)]
pub struct Cli {
    /// Project directory [default: current directory]
    #[arg(short = 'C', long = "project", global = true, value_name = "DIR")]
    pub project: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Download Debian and build a base image
    Provision {
        /// The image to build [default: the project's image]
        #[arg(value_name = "IMAGE", conflicts_with = "all")]
        image: Option<ImageName>,

        /// Build every image: `default`, those with their own directory of
        /// provisioning scripts, and those built before
        #[arg(long)]
        all: bool,

        /// Rebuild images even if they already exist
        #[arg(long)]
        force: bool,

        #[command(flatten)]
        resources: Resources,
    },
    /// Boot the project's VM and attach to its console
    Run(RunArgs),
    /// Open an SSH session to the project's running VM
    Ssh {
        /// Log in as root, e.g. to install packages; the VM's user has no
        /// sudo rights
        #[arg(long)]
        root: bool,

        /// Command to run instead of a login shell. Its arguments arrive
        /// unchanged; for pipes and the like, run `sh -c '...'`
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "COMMAND"
        )]
        command: Vec<String>,
    },
    /// Show the project's VM and its effective settings
    Status,
    /// Shut down the project's running VM
    Stop,
    /// Delete the project's VM; the next `run` starts from a fresh copy of the base image
    Reset {
        /// Don't ask for confirmation
        #[arg(short, long)]
        yes: bool,
    },
    /// List all project VMs
    List,
    /// Delete VMs whose project directory no longer exists
    Prune {
        /// Don't ask for confirmation
        #[arg(short, long)]
        yes: bool,
    },
}

#[derive(Debug, Args)]
pub struct RunArgs {
    #[command(flatten)]
    pub resources: Resources,

    /// Extra directory to share: HOST[:GUEST][:ro|rw] (repeatable).
    /// Without GUEST it is mounted at /mnt/<name>.
    #[arg(long = "mount", value_name = "SPEC")]
    pub mounts: Vec<MountSpec>,

    /// Let the VM see the project's .git directory (hidden by default)
    #[arg(long)]
    pub expose_git: bool,

    /// Base image to create the VM from [default: default]
    #[arg(long, value_name = "IMAGE")]
    pub image: Option<ImageName>,
}

impl RunArgs {
    pub fn settings(&self) -> Settings {
        Settings {
            cpus: self.resources.cpus,
            memory: self.resources.memory,
            disk_size: None,
            mounts: self.mounts.clone(),
            expose_git: self.expose_git.then_some(true),
            image: self.image.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ByteSize;
    use clap::CommandFactory;

    #[test]
    fn cli_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_run_flags() {
        let cli = Cli::try_parse_from([
            "sendit",
            "run",
            "--cpus",
            "4",
            "--memory",
            "8G",
            "--mount",
            "/a:/b:ro",
            "--mount",
            "/c",
            "--expose-git",
            "--image",
            "rust",
        ])
        .unwrap();
        let Command::Run(args) = cli.command else {
            panic!("expected run")
        };
        let settings = args.settings();
        assert_eq!(settings.cpus, Some(4));
        assert_eq!(settings.memory, Some(ByteSize::gib(8)));
        assert_eq!(settings.mounts.len(), 2);
        assert_eq!(settings.expose_git, Some(true));
        assert_eq!(settings.image, Some("rust".parse().unwrap()));
    }

    #[test]
    fn passes_ssh_commands_through() {
        let cli = Cli::try_parse_from(["sendit", "ssh", "ls", "-la", "/workspace"]).unwrap();
        let Command::Ssh { root, command } = cli.command else {
            panic!("expected ssh")
        };
        assert!(!root);
        assert_eq!(command, ["ls", "-la", "/workspace"]);

        let cli = Cli::try_parse_from(["sendit", "ssh", "--root", "apt", "--help"]).unwrap();
        let Command::Ssh { root, command } = cli.command else {
            panic!("expected ssh")
        };
        assert!(root);
        assert_eq!(command, ["apt", "--help"]);
    }

    #[test]
    fn rejects_bad_values() {
        assert!(Cli::try_parse_from(["sendit", "run", "--memory", "8"]).is_err());
        assert!(Cli::try_parse_from(["sendit", "run", "--mount", "/a:/b:/c"]).is_err());
    }

    #[test]
    fn parses_provision_flags() {
        let cli =
            Cli::try_parse_from(["sendit", "provision", "--cpus", "6", "--memory", "12G"]).unwrap();
        let Command::Provision {
            image,
            all,
            force,
            resources,
        } = cli.command
        else {
            panic!("expected provision")
        };
        assert_eq!(image, None);
        assert!(!all && !force);
        assert_eq!(resources.cpus, Some(6));
        assert_eq!(resources.memory, Some(ByteSize::gib(12)));

        let cli = Cli::try_parse_from(["sendit", "provision", "rust", "--force"]).unwrap();
        let Command::Provision { image, force, .. } = cli.command else {
            panic!("expected provision")
        };
        assert_eq!(image, Some("rust".parse().unwrap()));
        assert!(force);

        assert!(Cli::try_parse_from(["sendit", "provision", "rust", "--all"]).is_err());
        assert!(Cli::try_parse_from(["sendit", "provision", "Rust"]).is_err());
    }
}
