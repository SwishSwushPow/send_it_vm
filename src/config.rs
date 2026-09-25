//! User configuration (`~/.config/sendit/config.toml`) and resolution of the
//! effective VM settings for a project.
//!
//! Layers, lowest to highest precedence: built-in defaults, top-level keys of
//! the config file, the matching `[projects."<path>"]` table, CLI flags.
//! Scalars are overridden by higher layers; mounts accumulate across layers.
//! Provisioning the base image takes its CPUs and memory from the top-level
//! keys, then the `[provision]` table, then the flags of `sendit provision`.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

use crate::paths::{Paths, Project};

pub const DEFAULT_CPUS: u32 = 2;
pub const DEFAULT_MEMORY: ByteSize = ByteSize::gib(4);
pub const DEFAULT_DISK_SIZE: ByteSize = ByteSize::gib(64);

const MIN_MEMORY: ByteSize = ByteSize::mib(512);
const MIN_DISK_SIZE: ByteSize = ByteSize::gib(8);

/// The guest user's home directory. The project directory is mounted below
/// it under its own name, e.g. `~/Code/app` at `/home/dev/app`.
pub const GUEST_HOME: &str = "/home/dev";

/// A size in bytes, written with a binary unit: `512M`, `4G`, `64GiB`, `1T`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(try_from = "String")]
pub struct ByteSize(pub u64);

impl ByteSize {
    pub const fn mib(n: u64) -> Self {
        Self(n << 20)
    }

    pub const fn gib(n: u64) -> Self {
        Self(n << 30)
    }
}

impl FromStr for ByteSize {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim();
        let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        let (num, unit) = s.split_at(split);
        let shift = match unit.trim().to_ascii_uppercase().as_str() {
            "K" | "KB" | "KIB" => 10,
            "M" | "MB" | "MIB" => 20,
            "G" | "GB" | "GIB" => 30,
            "T" | "TB" | "TIB" => 40,
            _ => bail!("invalid size {s:?}: expected a number with a unit, e.g. 512M or 4G"),
        };
        let num: u64 = num.parse().with_context(|| format!("invalid size {s:?}"))?;
        num.checked_mul(1 << shift)
            .map(ByteSize)
            .with_context(|| format!("size {s:?} is too large"))
    }
}

impl TryFrom<String> for ByteSize {
    type Error = anyhow::Error;

    fn try_from(s: String) -> Result<Self> {
        s.parse()
    }
}

impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [(u32, &str); 4] = [(40, "TiB"), (30, "GiB"), (20, "MiB"), (10, "KiB")];
        for (shift, name) in UNITS {
            let unit = 1u64 << shift;
            if self.0 >= unit && self.0.is_multiple_of(unit) {
                return write!(f, "{} {name}", self.0 / unit);
            }
        }
        write!(f, "{} B", self.0)
    }
}

/// A mount as written by the user: `HOST[:GUEST][:ro|rw]`.
///
/// Without `GUEST`, the directory is mounted at `/mnt/<basename of HOST>`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct MountSpec {
    pub host: PathBuf,
    pub guest: Option<PathBuf>,
    pub read_only: bool,
}

impl FromStr for MountSpec {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let mut parts: Vec<&str> = s.split(':').collect();
        let read_only = match parts.last() {
            Some(&"ro") => true,
            Some(&"rw") => false,
            _ => {
                parts.push("rw");
                false
            }
        };
        parts.pop();
        let (host, guest) = match parts.as_slice() {
            [host] => (*host, None),
            [host, guest] => (*host, Some(PathBuf::from(guest))),
            _ => bail!("invalid mount {s:?}: expected HOST[:GUEST][:ro|rw]"),
        };
        ensure!(!host.is_empty(), "invalid mount {s:?}: host path is empty");
        Ok(Self {
            host: PathBuf::from(host),
            guest,
            read_only,
        })
    }
}

impl TryFrom<String> for MountSpec {
    type Error = anyhow::Error;

    fn try_from(s: String) -> Result<Self> {
        s.parse()
    }
}

/// One layer of optional settings (config file section or CLI flags).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Settings {
    pub cpus: Option<u32>,
    pub memory: Option<ByteSize>,
    pub disk_size: Option<ByteSize>,
    #[serde(default)]
    pub mounts: Vec<MountSpec>,
    pub expose_git: Option<bool>,
}

/// Optional CPUs and memory for provisioning (`[provision]` table or CLI flags).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    pub cpus: Option<u32>,
    pub memory: Option<ByteSize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    #[serde(flatten)]
    pub defaults: Settings,
    #[serde(default)]
    pub provision: Resources,
    #[serde(default)]
    pub projects: BTreeMap<PathBuf, Settings>,
}

/// Fully resolved CPUs and memory for provisioning the base image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProvisionSettings {
    pub cpus: u32,
    pub memory: ByteSize,
}

/// A host directory shared into the guest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    pub host: PathBuf,
    pub guest: PathBuf,
    pub read_only: bool,
}

/// Fully resolved settings for one VM run.
#[derive(Clone, Debug)]
pub struct VmSettings {
    pub cpus: u32,
    pub memory: ByteSize,
    pub disk_size: ByteSize,
    /// The project mount first, then extra mounts in precedence order.
    pub mounts: Vec<Mount>,
    pub expose_git: bool,
}

impl Config {
    /// Loads the config file, or the empty config if it doesn't exist.
    pub fn load(paths: &Paths) -> Result<Self> {
        let file = paths.config_file();
        match fs::read_to_string(&file) {
            Ok(text) => Self::parse(&text).with_context(|| format!("in {}", file.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", file.display())),
        }
    }

    pub fn parse(text: &str) -> Result<Self> {
        Ok(toml::from_str(text)?)
    }

    /// The `[projects."<path>"]` table matching `project`, if any.
    fn project_settings(&self, paths: &Paths, project: &Project) -> Option<&Settings> {
        self.projects.iter().find_map(|(key, settings)| {
            let key = paths.expand_tilde(key);
            let key = key.canonicalize().unwrap_or(key);
            (key == project.root).then_some(settings)
        })
    }

    /// Merges all layers for `project`. Relative mount paths given on the
    /// command line are resolved against `cwd`; in the config file they must
    /// be absolute or start with `~`.
    pub fn resolve(
        &self,
        paths: &Paths,
        project: &Project,
        cli: &Settings,
        cwd: &Path,
    ) -> Result<VmSettings> {
        let layers = [
            (Some(&self.defaults), None),
            (self.project_settings(paths, project), None),
            (Some(cli), Some(cwd)),
        ];

        let mut cpus = DEFAULT_CPUS;
        let mut memory = DEFAULT_MEMORY;
        let mut disk_size = DEFAULT_DISK_SIZE;
        let mut expose_git = false;
        let mut mounts = vec![Mount {
            host: project.root.clone(),
            guest: project_guest_path(project),
            read_only: false,
        }];

        for (layer, base) in layers {
            let Some(layer) = layer else { continue };
            cpus = layer.cpus.unwrap_or(cpus);
            memory = layer.memory.unwrap_or(memory);
            disk_size = layer.disk_size.unwrap_or(disk_size);
            expose_git = layer.expose_git.unwrap_or(expose_git);
            for spec in &layer.mounts {
                mounts.push(resolve_mount(paths, spec, base)?);
            }
        }

        let settings = VmSettings {
            cpus,
            memory,
            disk_size,
            mounts,
            expose_git,
        };
        settings.validate()?;
        Ok(settings)
    }
}

impl Config {
    /// Merges the layers that apply to provisioning with the CLI flags.
    pub fn resolve_provision(&self, cli: &Resources) -> Result<ProvisionSettings> {
        let mut cpus = self.defaults.cpus.unwrap_or(DEFAULT_CPUS);
        let mut memory = self.defaults.memory.unwrap_or(DEFAULT_MEMORY);
        for layer in [&self.provision, cli] {
            cpus = layer.cpus.unwrap_or(cpus);
            memory = layer.memory.unwrap_or(memory);
        }
        validate_resources(cpus, memory)?;
        Ok(ProvisionSettings { cpus, memory })
    }
}

fn project_guest_path(project: &Project) -> PathBuf {
    let name = project.root.file_name().unwrap_or("project".as_ref());
    Path::new(GUEST_HOME).join(name)
}

fn resolve_mount(paths: &Paths, spec: &MountSpec, base: Option<&Path>) -> Result<Mount> {
    let host = paths.expand_tilde(&spec.host);
    let host = match base {
        _ if host.is_absolute() => host,
        Some(base) => base.join(host),
        None => bail!(
            "mount {}: host path in the config file must be absolute or start with ~",
            spec.host.display()
        ),
    };
    let host = host
        .canonicalize()
        .with_context(|| format!("mount {}", host.display()))?;
    ensure!(host.is_dir(), "mount {}: not a directory", host.display());

    let guest = match &spec.guest {
        Some(guest) => guest.clone(),
        None => {
            let name = host.file_name().with_context(|| {
                format!("mount {}: give a guest path explicitly", host.display())
            })?;
            Path::new("/mnt").join(name)
        }
    };
    Ok(Mount {
        host,
        guest,
        read_only: spec.read_only,
    })
}

fn validate_resources(cpus: u32, memory: ByteSize) -> Result<()> {
    let host_cpus = std::thread::available_parallelism().map_or(1, |n| n.get() as u32);
    ensure!(
        (1..=host_cpus).contains(&cpus),
        "cpus must be between 1 and {host_cpus}, got {cpus}"
    );
    ensure!(
        memory >= MIN_MEMORY,
        "memory must be at least {MIN_MEMORY}, got {memory}"
    );
    ensure!(
        memory.0.is_multiple_of(ByteSize::mib(1).0),
        "memory must be a whole number of MiB, got {memory}"
    );
    Ok(())
}

impl VmSettings {
    fn validate(&self) -> Result<()> {
        validate_resources(self.cpus, self.memory)?;
        ensure!(
            self.disk_size >= MIN_DISK_SIZE,
            "disk-size must be at least {MIN_DISK_SIZE}, got {}",
            self.disk_size
        );

        for (i, mount) in self.mounts.iter().enumerate() {
            let guest = &mount.guest;
            ensure!(
                guest.is_absolute()
                    && guest.components().count() > 1
                    && guest
                        .components()
                        .all(|c| matches!(c, Component::RootDir | Component::Normal(_))),
                "mount {}: guest path {} must be an absolute path below /",
                mount.host.display(),
                guest.display()
            );
            if let Some(dup) = self.mounts[..i].iter().find(|m| m.guest == *guest) {
                bail!(
                    "mounts {} and {} both use guest path {}",
                    dup.host.display(),
                    mount.host.display(),
                    guest.display()
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sizes() {
        assert_eq!("4G".parse::<ByteSize>().unwrap(), ByteSize::gib(4));
        assert_eq!("64GiB".parse::<ByteSize>().unwrap(), ByteSize::gib(64));
        assert_eq!("512m".parse::<ByteSize>().unwrap(), ByteSize::mib(512));
        assert_eq!("1 TB".parse::<ByteSize>().unwrap(), ByteSize::gib(1024));
        assert!("4096".parse::<ByteSize>().is_err());
        assert!("G".parse::<ByteSize>().is_err());
        assert!("4X".parse::<ByteSize>().is_err());
        assert!("99999999999T".parse::<ByteSize>().is_err());
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(ByteSize::gib(64).to_string(), "64 GiB");
        assert_eq!(ByteSize::mib(1536).to_string(), "1536 MiB");
        assert_eq!(ByteSize(100).to_string(), "100 B");
    }

    #[test]
    fn parses_mount_specs() {
        let spec: MountSpec = "~/data".parse().unwrap();
        assert_eq!(spec.host, PathBuf::from("~/data"));
        assert_eq!(spec.guest, None);
        assert!(!spec.read_only);

        let spec: MountSpec = "/a:/b:ro".parse().unwrap();
        assert_eq!(spec.host, PathBuf::from("/a"));
        assert_eq!(spec.guest, Some(PathBuf::from("/b")));
        assert!(spec.read_only);

        let spec: MountSpec = "/a:ro".parse().unwrap();
        assert_eq!(spec.guest, None);
        assert!(spec.read_only);

        assert!("/a:/b:/c".parse::<MountSpec>().is_err());
        assert!(":/b".parse::<MountSpec>().is_err());
    }

    #[test]
    fn parses_config_file() {
        let config = Config::parse(
            r#"
            cpus = 4
            memory = "8G"
            mounts = ["~/.cargo/registry:/home/dev/.cargo/registry:ro"]

            [projects."/tmp/foo"]
            cpus = 8
            expose-git = true
            "#,
        )
        .unwrap();
        assert_eq!(config.defaults.cpus, Some(4));
        assert_eq!(config.defaults.memory, Some(ByteSize::gib(8)));
        assert_eq!(config.defaults.mounts.len(), 1);
        let project = &config.projects[Path::new("/tmp/foo")];
        assert_eq!(project.cpus, Some(8));
        assert_eq!(project.expose_git, Some(true));
    }

    #[test]
    fn resolves_provision_resources() {
        let resolve =
            |config: &str, cli: Resources| Config::parse(config).unwrap().resolve_provision(&cli);
        let none = Resources::default;
        let defaults = ProvisionSettings {
            cpus: DEFAULT_CPUS,
            memory: DEFAULT_MEMORY,
        };
        assert_eq!(resolve("", none()).unwrap(), defaults);

        let top_level = "cpus = 1\nmemory = \"2G\"\n";
        let settings = resolve(top_level, none()).unwrap();
        assert_eq!((settings.cpus, settings.memory), (1, ByteSize::gib(2)));

        let table = format!("{top_level}[provision]\nmemory = \"3G\"\n");
        let settings = resolve(&table, none()).unwrap();
        assert_eq!((settings.cpus, settings.memory), (1, ByteSize::gib(3)));

        let cli = Resources {
            cpus: None,
            memory: Some(ByteSize::gib(1)),
        };
        assert_eq!(resolve(&table, cli).unwrap().memory, ByteSize::gib(1));

        assert!(resolve("[provision]\ncpus = 0", none()).is_err());
        assert!(resolve("[provision]\nmemory = \"256M\"", none()).is_err());
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(Config::parse("cpu = 4").is_err());
        assert!(Config::parse("[projects.\"/x\"]\nram = \"4G\"").is_err());
        assert!(Config::parse("[provision]\nmounts = []").is_err());
    }

    struct Fixture {
        dir: PathBuf,
        paths: Paths,
        project: Project,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("sendit-test-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join("home/extra")).unwrap();
            fs::create_dir_all(dir.join("proj")).unwrap();
            let dir = dir.canonicalize().unwrap();
            let paths = Paths::new(dir.join("home"));
            let project = Project::at(&dir.join("proj")).unwrap();
            Self {
                dir,
                paths,
                project,
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn merges_layers() {
        let fx = Fixture::new("merge");
        let config = Config::parse(&format!(
            r#"
            cpus = 1
            memory = "2G"
            mounts = ["~/extra:ro"]

            [projects."{}"]
            memory = "3G"
            expose-git = true
            "#,
            fx.project.root.display()
        ))
        .unwrap();
        let cli = Settings {
            memory: Some(ByteSize::gib(1)),
            mounts: vec!["proj:/data".parse().unwrap()],
            ..Settings::default()
        };

        let vm = config
            .resolve(&fx.paths, &fx.project, &cli, &fx.dir)
            .unwrap();
        assert_eq!(vm.cpus, 1);
        assert_eq!(vm.memory, ByteSize::gib(1));
        assert_eq!(vm.disk_size, DEFAULT_DISK_SIZE);
        assert!(vm.expose_git);
        let mounts: Vec<_> = vm
            .mounts
            .iter()
            .map(|m| (m.host.clone(), m.guest.clone(), m.read_only))
            .collect();
        assert_eq!(
            mounts,
            [
                (
                    fx.project.root.clone(),
                    PathBuf::from("/home/dev/proj"),
                    false
                ),
                (fx.dir.join("home/extra"), PathBuf::from("/mnt/extra"), true),
                (fx.dir.join("proj"), PathBuf::from("/data"), false),
            ]
        );
    }

    #[test]
    fn validates_settings() {
        let fx = Fixture::new("validate");
        let resolve = |config: &str, cli: Settings| {
            Config::parse(config)
                .unwrap()
                .resolve(&fx.paths, &fx.project, &cli, &fx.dir)
        };

        assert!(resolve("", Settings::default()).is_ok());
        assert!(resolve("cpus = 0", Settings::default()).is_err());
        assert!(resolve("memory = \"256M\"", Settings::default()).is_err());
        assert!(resolve("disk-size = \"4G\"", Settings::default()).is_err());
        // Relative host paths are only allowed on the command line.
        assert!(resolve("mounts = [\"proj\"]", Settings::default()).is_err());
        // Guest paths must be absolute, and may not collide.
        let cli = |spec: &str| Settings {
            mounts: vec![spec.parse().unwrap()],
            ..Settings::default()
        };
        assert!(resolve("", cli("proj:data")).is_err());
        assert!(resolve("", cli("proj:/")).is_err());
        assert!(resolve("", cli("proj:/home/dev/proj")).is_err());
        assert!(resolve("", cli("missing:/data")).is_err());
    }
}
