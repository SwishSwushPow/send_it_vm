use std::path::PathBuf;

use clap::error::ErrorKind;
use clap::{Args, CommandFactory, Parser, Subcommand};

use crate::config::{MountSpec, Resources, Settings};
use crate::paths::{ImageName, VmName};

/// Light and fast per-project Debian VMs on macOS.
#[derive(Debug, Parser)]
#[command(name = "sendit", version)]
pub struct Cli {
    /// Project directory [default: current directory]
    #[arg(short = 'C', long = "project", global = true, value_name = "DIR")]
    pub project: Option<PathBuf>,

    /// The project whose VM has this name (see `sendit list`), instead of
    /// a project directory
    #[arg(short = 'n', long = "name", global = true, value_name = "NAME")]
    pub name: Option<VmName>,

    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    /// Parses `args` like `try_parse_from`, and also rejects `-n` together
    /// with `-C`: clap's `conflicts_with` misses them when they are given
    /// on different levels, e.g. `sendit -n app stop -C dir`.
    pub fn try_parse_checked_from<I, T>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        let cli = Self::try_parse_from(args)?;
        if cli.name.is_some() && cli.project.is_some() {
            return Err(Self::command().error(
                ErrorKind::ArgumentConflict,
                "the argument '--name <NAME>' cannot be used with '--project <DIR>'",
            ));
        }
        Ok(cli)
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Download Debian and build a base image
    Provision {
        /// The image to build [default: the project's image]
        #[arg(value_name = "IMAGE", conflicts_with = "all")]
        image: Option<ImageName>,

        /// Build every image: `default`, those named in the config file or
        /// with an image file in ~/.config/sendit/images, and those built
        /// before
        #[arg(long)]
        all: bool,

        /// Rebuild images even if they already exist
        #[arg(long)]
        force: bool,

        #[command(flatten)]
        resources: Resources,
    },
    /// Boot the project's VM and attach to its console, run a command in it,
    /// or run it in the background
    Run(RunArgs),
    /// Open an SSH session to the project's running VM
    Ssh {
        /// Log in as root, e.g. to install packages; the VM's user has no
        /// sudo rights
        #[arg(long)]
        root: bool,

        /// Command to run instead of an interactive shell: in a login shell
        /// in the project directory, on a terminal if this is one. Its
        /// arguments arrive unchanged; for pipes and the like, run
        /// `sh -c '...'`. One that starts with - goes after --
        #[arg(trailing_var_arg = true, value_name = "COMMAND")]
        command: Vec<String>,
    },
    /// Show the project's VM and its effective settings
    Status,
    /// Shut down the project's running VM
    Stop,
    /// Rename the project's VM, e.g. `sendit -n old rename new`
    Rename {
        /// The new name: up to 64 lowercase letters, digits and dashes, not
        /// starting with a dash
        #[arg(value_name = "NEW_NAME")]
        new_name: VmName,
    },
    /// Delete the project's VM; the next `run` starts from a fresh copy of the base image,
    /// or for a git worktree, of its main worktree's VM
    Reset {
        /// Don't ask for confirmation
        #[arg(short, long)]
        yes: bool,
    },
    /// List all project VMs and their names
    List,
    /// List the base images and how many VMs were made from each
    Images {
        /// Delete the IMAGE base image instead (repeatable). The VMs made
        /// from it keep working; once one is reset, `sendit run` can't make
        /// it again from the image until `sendit provision IMAGE` builds it
        /// again
        #[arg(long, value_name = "IMAGE")]
        delete: Vec<ImageName>,
        /// Don't ask for confirmation
        #[arg(short, long, requires = "delete")]
        yes: bool,
    },
    /// Delete VMs whose project directory no longer exists
    Prune {
        /// Also delete VMs made from an outdated base image, which can't run
        /// until they are reset
        #[arg(long)]
        outdated: bool,
        /// Delete every VM that isn't running, whatever its project or base
        /// image
        #[arg(long)]
        all: bool,
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
    /// Read-only unless :rw is given. Without GUEST it is mounted at
    /// /mnt/<name>.
    #[arg(long = "mount", value_name = "SPEC")]
    pub mounts: Vec<MountSpec>,

    /// Mount the project directory read-only
    #[arg(long)]
    pub read_only: bool,

    /// Let the VM see the project's .git directory (hidden by default)
    #[arg(long)]
    pub expose_git: bool,

    /// Base image to create the VM from; an existing VM must have been
    /// made from it [default: `image` from the config file, else the one
    /// the VM was made from, else default]
    #[arg(long, value_name = "IMAGE")]
    pub image: Option<ImageName>,

    /// Check out BRANCH in a git worktree of the project and boot that
    /// worktree's VM instead. The worktree goes next to the main one, into
    /// <repo>.worktrees/<branch> with each / replaced by -, and the branch
    /// is created if it doesn't exist; if it has a worktree already, that
    /// one is used. For a project in a subdirectory of the repository, the
    /// same subdirectory of the worktree is booted
    #[arg(long, value_name = "BRANCH")]
    pub worktree: Option<String>,

    /// Commit to start a new --worktree branch from [default: HEAD]
    #[arg(long, value_name = "COMMIT", requires = "worktree")]
    pub from: Option<String>,

    /// Where to put a new --worktree [default: <repo>.worktrees/<branch>]
    #[arg(long, value_name = "DIR", requires = "worktree")]
    pub worktree_path: Option<PathBuf>,

    /// Create a git worktree's VM from the base image instead of as a copy
    /// of its main worktree's VM. A VM that is such a copy already must be
    /// deleted first with `sendit reset`
    #[arg(long)]
    pub fresh: bool,

    /// Run the VM in the background: sendit returns once it answers over
    /// SSH, `sendit ssh` reaches it and `sendit stop` shuts it down. Its
    /// console output goes to run/console.log in the VM's directory
    #[arg(short, long, conflicts_with = "command")]
    pub detach: bool,

    /// Command to run once the VM is up, instead of attaching its console:
    /// over SSH as `sendit ssh <COMMAND>` does, and the VM shuts down when
    /// it ends, with sendit exiting with its exit code. Its arguments
    /// arrive unchanged; for pipes and the like, run `sh -c '...'`. One
    /// that starts with - goes after --
    #[arg(trailing_var_arg = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

impl RunArgs {
    pub fn settings(&self) -> Settings {
        Settings {
            cpus: self.resources.cpus,
            memory: self.resources.memory,
            disk_size: None,
            mounts: self.mounts.clone(),
            read_only: self.read_only.then_some(true),
            expose_git: self.expose_git.then_some(true),
            banner: None,
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
            "--read-only",
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
        assert_eq!(settings.read_only, Some(true));
        assert_eq!(settings.expose_git, Some(true));
        assert_eq!(settings.image, Some("rust".parse().unwrap()));
    }

    #[test]
    fn parses_worktree_flags() {
        let cli = Cli::try_parse_from([
            "sendit",
            "run",
            "--worktree",
            "feature/x",
            "--from",
            "main",
            "--worktree-path",
            "/w",
            "--cpus",
            "2",
        ])
        .unwrap();
        let Command::Run(args) = cli.command else {
            panic!("expected run")
        };
        assert_eq!(args.worktree.as_deref(), Some("feature/x"));
        assert_eq!(args.from.as_deref(), Some("main"));
        assert_eq!(args.worktree_path, Some(PathBuf::from("/w")));
        assert_eq!(args.settings().cpus, Some(2));
        assert!(args.command.is_empty());
        // Only with --worktree.
        assert!(Cli::try_parse_from(["sendit", "run", "--from", "main"]).is_err());
        assert!(Cli::try_parse_from(["sendit", "run", "--worktree-path", "/w"]).is_err());
    }

    #[test]
    fn passes_run_commands_through() {
        let cli =
            Cli::try_parse_from(["sendit", "run", "--cpus", "2", "cargo", "test", "--", "-q"])
                .unwrap();
        let Command::Run(args) = cli.command else {
            panic!("expected run")
        };
        assert_eq!(args.settings().cpus, Some(2));
        assert_eq!(args.command, ["cargo", "test", "--", "-q"]);

        // Flags after the command are the command's.
        let cli = Cli::try_parse_from(["sendit", "run", "ls", "--cpus", "2"]).unwrap();
        let Command::Run(args) = cli.command else {
            panic!("expected run")
        };
        assert_eq!(args.settings().cpus, None);
        assert_eq!(args.command, ["ls", "--cpus", "2"]);
    }

    #[test]
    fn rejects_unknown_flags_before_the_command() {
        for args in [
            &["sendit", "run", "--deatach"][..],
            &["sendit", "run", "--cpus", "2", "-x", "ls"],
            &["sendit", "ssh", "--rot", "ls"],
            &["sendit", "ssh", "-x"],
        ] {
            let error = Cli::try_parse_from(args).unwrap_err();
            assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
        }
        // A command that starts with - goes after --.
        let cli = Cli::try_parse_from(["sendit", "run", "--", "-x", "--y"]).unwrap();
        let Command::Run(args) = cli.command else {
            panic!("expected run")
        };
        assert_eq!(args.command, ["-x", "--y"]);
        let cli = Cli::try_parse_from(["sendit", "ssh", "--root", "--", "-x"]).unwrap();
        let Command::Ssh { root, command } = cli.command else {
            panic!("expected ssh")
        };
        assert!(root);
        assert_eq!(command, ["-x"]);
    }

    #[test]
    fn parses_detach() {
        for flag in ["-d", "--detach"] {
            let cli = Cli::try_parse_from(["sendit", "run", flag, "--cpus", "2"]).unwrap();
            let Command::Run(args) = cli.command else {
                panic!("expected run")
            };
            assert!(args.detach);
            assert!(args.command.is_empty());
        }
        // `sendit ssh` runs commands in a detached VM.
        assert!(Cli::try_parse_from(["sendit", "run", "-d", "cargo", "test"]).is_err());
        // After the command, it is the command's.
        let cli = Cli::try_parse_from(["sendit", "run", "ls", "-d"]).unwrap();
        let Command::Run(args) = cli.command else {
            panic!("expected run")
        };
        assert!(!args.detach);
        assert_eq!(args.command, ["ls", "-d"]);
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
    fn parses_names() {
        // Before or after the subcommand, and before a command to run.
        for args in [
            &["sendit", "-n", "app", "ssh", "ls"][..],
            &["sendit", "ssh", "-n", "app", "ls"],
            &["sendit", "ssh", "--name", "app", "ls"],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert_eq!(cli.name, Some("app".parse().unwrap()), "{args:?}");
            let Command::Ssh { command, .. } = cli.command else {
                panic!("expected ssh")
            };
            assert_eq!(command, ["ls"]);
        }
        // After the command, it is the command's.
        let cli = Cli::try_parse_from(["sendit", "ssh", "ls", "-n", "app"]).unwrap();
        assert_eq!(cli.name, None);

        // The new name is not the global one.
        for (args, old) in [
            (&["sendit", "-n", "old", "rename", "new"][..], Some("old")),
            (&["sendit", "rename", "-n", "old", "new"], Some("old")),
            (&["sendit", "rename", "new"], None),
        ] {
            let cli = Cli::try_parse_checked_from(args).unwrap();
            assert_eq!(cli.name, old.map(|old| old.parse().unwrap()), "{args:?}");
            let Command::Rename { new_name } = cli.command else {
                panic!("expected rename")
            };
            assert_eq!(new_name, "new".parse().unwrap());
        }
        let cli = Cli::try_parse_checked_from(["sendit", "-C", "/p", "rename", "new"]).unwrap();
        assert_eq!(cli.project, Some(PathBuf::from("/p")));
        assert_eq!(cli.name, None);

        for args in [
            &["sendit", "stop", "-n", "App"][..],
            &["sendit", "rename", "my_vm"],
            &["sendit", "rename"],
        ] {
            assert!(Cli::try_parse_checked_from(args).is_err(), "{args:?}");
        }
        // Not both -n and -C, on any level.
        for args in [
            &["sendit", "-n", "app", "-C", "/p", "stop"][..],
            &["sendit", "stop", "-n", "app", "-C", "/p"],
            &["sendit", "-n", "app", "stop", "-C", "/p"],
            &["sendit", "-C", "/p", "run", "-n", "app", "-d"],
        ] {
            let error = Cli::try_parse_checked_from(args).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::ArgumentConflict, "{args:?}");
        }
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
