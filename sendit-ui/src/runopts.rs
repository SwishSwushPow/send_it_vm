//! The options of `sendit run` that the app offers, turned into arguments
//! and checked with sendit's own parsers first.

use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use send_it_vm::{Allow, Mode};
use send_it_vm::{ByteSize, MountSpec};
use send_it_vm::{ImageName, VmName};

/// The project a command is for: a VM by its name, or a project directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Name(VmName),
    Dir(PathBuf),
}

impl Target {
    /// The arguments that pick it, before the subcommand.
    pub fn args(&self) -> Vec<String> {
        match self {
            Self::Name(name) => vec!["-n".into(), name.to_string()],
            Self::Dir(dir) => vec!["-C".into(), dir.display().to_string()],
        }
    }
}

/// What the run-options form holds, as typed. Empty means sendit's own
/// choice, from the config file or its defaults.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunOptions {
    pub cpus: String,
    pub memory: String,
    pub image: String,
    pub network: Option<Mode>,
    /// One destination per line.
    pub network_allow: String,
    /// One `HOST[:GUEST][:ro|rw]` per line.
    pub mounts: String,
    pub read_only: bool,
    pub expose_git: bool,
    pub fresh: bool,
    /// Boot this branch's worktree instead, made if needed.
    pub worktree: String,
    pub from: String,
    pub worktree_path: String,
}

fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines().map(str::trim).filter(|line| !line.is_empty())
}

impl RunOptions {
    /// The arguments of `sendit` that start `target`'s VM in the
    /// background, with these options. `yes` shares risky directories
    /// without asking, once the user has agreed.
    pub fn args(&self, target: &Target, yes: bool) -> Result<Vec<String>> {
        let mut args = target.args();
        args.extend(["run".into(), "-d".into()]);
        if yes {
            args.push("--yes".into());
        }
        let cpus = self.cpus.trim();
        if !cpus.is_empty() {
            let n: u32 = cpus
                .parse()
                .with_context(|| format!("CPUs: {cpus:?} is not a number"))?;
            ensure!(n > 0, "CPUs: must be at least 1");
            args.extend(["--cpus".into(), n.to_string()]);
        }
        let memory = self.memory.trim();
        if !memory.is_empty() {
            memory
                .parse::<ByteSize>()
                .with_context(|| format!("memory: {memory:?}"))?;
            args.extend(["--memory".into(), memory.to_string()]);
        }
        let image = self.image.trim();
        if !image.is_empty() {
            image
                .parse::<ImageName>()
                .with_context(|| format!("image: {image:?}"))?;
            args.extend(["--image".into(), image.to_string()]);
        }
        if let Some(mode) = self.network {
            args.extend(["--network".into(), mode.to_string()]);
        }
        for allow in lines(&self.network_allow) {
            allow
                .parse::<Allow>()
                .with_context(|| format!("network-allow: {allow:?}"))?;
            args.extend(["--network-allow".into(), allow.to_string()]);
        }
        for mount in lines(&self.mounts) {
            let spec = mount
                .parse::<MountSpec>()
                .with_context(|| format!("mount: {mount:?}"))?;
            // sendit would resolve a relative one against the app's
            // working directory, which means nothing here.
            ensure!(
                spec.host.is_absolute() || spec.host.starts_with("~"),
                "mount {mount:?}: the host path must be absolute or start with ~"
            );
            args.extend(["--mount".into(), mount.to_string()]);
        }
        if self.read_only {
            args.push("--read-only".into());
        }
        if self.expose_git {
            args.push("--expose-git".into());
        }
        if self.fresh {
            args.push("--fresh".into());
        }
        let branch = self.worktree.trim();
        let from = self.from.trim();
        let path = self.worktree_path.trim();
        if branch.is_empty() {
            if !from.is_empty() || !path.is_empty() {
                bail!("a start commit or worktree folder needs a branch");
            }
        } else {
            args.extend(["--worktree".into(), branch.to_string()]);
            if !from.is_empty() {
                args.extend(["--from".into(), from.to_string()]);
            }
            if !path.is_empty() {
                let path = PathBuf::from(path);
                ensure!(
                    path.is_absolute(),
                    "worktree folder {}: must be an absolute path",
                    path.display()
                );
                args.extend(["--worktree-path".into(), path.display().to_string()]);
            }
        }
        Ok(args)
    }
}

/// `args` of a `run -d` again, with `flag` (e.g. `--yes`) added after
/// `-d`, unless it is there already.
pub fn with_flag(args: &[String], flag: &str) -> Vec<String> {
    let mut args = args.to_vec();
    if !args.iter().any(|arg| arg == flag) {
        let at = args
            .iter()
            .position(|arg| arg == "-d")
            .map_or(args.len(), |i| i + 1);
        args.insert(at, flag.to_string());
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(name: &str) -> Target {
        Target::Name(name.parse().unwrap())
    }

    #[test]
    fn starts_with_defaults() {
        let args = RunOptions::default().args(&name("app"), false).unwrap();
        assert_eq!(args, ["-n", "app", "run", "-d"]);
        let args = RunOptions::default()
            .args(&Target::Dir("/p q".into()), true)
            .unwrap();
        assert_eq!(args, ["-C", "/p q", "run", "-d", "--yes"]);
    }

    #[test]
    fn passes_every_option() {
        let options = RunOptions {
            cpus: " 4 ".into(),
            memory: "8G".into(),
            image: "rust".into(),
            network: Some(Mode::Offline),
            network_allow: "host:11434\n\n  10.0.0.1:80 \n".into(),
            mounts: "~/.cargo/registry:/home/dev/.cargo/registry\n/data:rw".into(),
            read_only: true,
            expose_git: true,
            fresh: true,
            worktree: "feature/x".into(),
            from: "main".into(),
            worktree_path: "/w/x".into(),
        };
        let args = options.args(&name("app"), false).unwrap();
        assert_eq!(
            args,
            [
                "-n",
                "app",
                "run",
                "-d",
                "--cpus",
                "4",
                "--memory",
                "8G",
                "--image",
                "rust",
                "--network",
                "none",
                "--network-allow",
                "host:11434",
                "--network-allow",
                "10.0.0.1:80",
                "--mount",
                "~/.cargo/registry:/home/dev/.cargo/registry",
                "--mount",
                "/data:rw",
                "--read-only",
                "--expose-git",
                "--fresh",
                "--worktree",
                "feature/x",
                "--from",
                "main",
                "--worktree-path",
                "/w/x",
            ]
        );
        // What sendit itself accepts.
        let cli = send_it_vm::Cli::try_parse_checked_from(
            std::iter::once("sendit".to_string()).chain(args),
        )
        .unwrap();
        assert!(matches!(cli.command, send_it_vm::Command::Run(_)));
    }

    #[test]
    fn adds_flags_for_a_second_try() {
        let args = RunOptions {
            cpus: "2".into(),
            ..RunOptions::default()
        }
        .args(&name("app"), false)
        .unwrap();
        let yes = with_flag(&args, "--yes");
        assert_eq!(yes, ["-n", "app", "run", "-d", "--yes", "--cpus", "2"]);
        assert_eq!(with_flag(&yes, "--yes"), yes);
        assert_eq!(
            with_flag(&yes, "--fresh"),
            ["-n", "app", "run", "-d", "--fresh", "--yes", "--cpus", "2"]
        );
    }

    #[test]
    fn rejects_what_sendit_would() {
        for options in [
            RunOptions {
                cpus: "two".into(),
                ..RunOptions::default()
            },
            RunOptions {
                cpus: "0".into(),
                ..RunOptions::default()
            },
            RunOptions {
                memory: "8".into(),
                ..RunOptions::default()
            },
            RunOptions {
                image: "Rust".into(),
                ..RunOptions::default()
            },
            RunOptions {
                network_allow: "example.com".into(),
                ..RunOptions::default()
            },
            RunOptions {
                mounts: "relative/dir".into(),
                ..RunOptions::default()
            },
            RunOptions {
                mounts: "/a:/b:/c".into(),
                ..RunOptions::default()
            },
            RunOptions {
                from: "main".into(),
                ..RunOptions::default()
            },
            RunOptions {
                worktree: "x".into(),
                worktree_path: "rel".into(),
                ..RunOptions::default()
            },
        ] {
            assert!(options.args(&name("app"), false).is_err(), "{options:?}");
        }
    }
}
