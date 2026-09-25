use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::config::{ByteSize, MountSpec, Settings};

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
    /// Download Debian and build the shared base image
    Provision {
        /// Rebuild the base image even if it already exists
        #[arg(long)]
        force: bool,
    },
    /// Boot the project's VM and attach to its console
    Run(RunArgs),
    /// Open an SSH session to the project's running VM
    Ssh {
        /// Command to run instead of a login shell
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, value_name = "COMMAND")]
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
    /// Number of virtual CPUs
    #[arg(long)]
    pub cpus: Option<u32>,

    /// Memory size, e.g. 4G or 512M
    #[arg(long)]
    pub memory: Option<ByteSize>,

    /// Extra directory to share: HOST[:GUEST][:ro|rw] (repeatable).
    /// Without GUEST it is mounted at /mnt/<name>.
    #[arg(long = "mount", value_name = "SPEC")]
    pub mounts: Vec<MountSpec>,

    /// Let the VM see the project's .git directory (hidden by default)
    #[arg(long)]
    pub expose_git: bool,
}

impl RunArgs {
    pub fn settings(&self) -> Settings {
        Settings {
            cpus: self.cpus,
            memory: self.memory,
            disk_size: None,
            mounts: self.mounts.clone(),
            expose_git: self.expose_git.then_some(true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    }

    #[test]
    fn passes_ssh_commands_through() {
        let cli = Cli::try_parse_from(["sendit", "ssh", "ls", "-la", "/workspace"]).unwrap();
        let Command::Ssh { command } = cli.command else {
            panic!("expected ssh")
        };
        assert_eq!(command, ["ls", "-la", "/workspace"]);
    }

    #[test]
    fn rejects_bad_values() {
        assert!(Cli::try_parse_from(["sendit", "run", "--memory", "8"]).is_err());
        assert!(Cli::try_parse_from(["sendit", "run", "--mount", "/a:/b:/c"]).is_err());
    }
}
