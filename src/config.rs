//! User configuration (`~/.config/sendit/config.toml`) and resolution of the
//! effective VM settings for a project.
//!
//! Layers, lowest to highest precedence: built-in defaults, top-level keys of
//! the config file, the `[images.<name>]` table of the VM's image, the
//! matching `[projects."<path>"]` table, CLI flags. Scalars are overridden by
//! higher layers; mounts accumulate across layers.
//! Provisioning a base image takes its CPUs and memory from the top-level
//! keys, then the `[provision]` table, then the image's `[provision.<name>]`
//! table, then the flags of `sendit provision`.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, bail, ensure};
use clap::Args;
use serde::Deserialize;

use crate::paths::{ImageName, Paths, Project};
use crate::{project_vm, util};

const DEFAULT_CPUS: u32 = 2;
const DEFAULT_MEMORY: ByteSize = ByteSize::gib(4);
const DEFAULT_DISK_SIZE: ByteSize = ByteSize::gib(64);

const MIN_MEMORY: ByteSize = ByteSize::mib(512);
const MIN_DISK_SIZE: ByteSize = ByteSize::gib(8);

/// The guest's login user, created by `assets/user-data.yaml`.
pub const GUEST_USER: &str = "dev";

/// The guest user's home directory. The project directory is mounted below
/// it under its own name, e.g. `~/Code/app` at `/home/dev/app`.
const GUEST_HOME: &str = "/home/dev";

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

impl ByteSize {
    /// Formats the size for humans, rounded to one decimal, e.g. `1.8 GiB`.
    pub fn approx(self) -> String {
        const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
        let mut size = self.0 as f64;
        let mut unit = 0;
        while size >= 1024.0 && unit < UNITS.len() - 1 {
            size /= 1024.0;
            unit += 1;
        }
        if unit == 0 {
            format!("{} B", self.0)
        } else {
            format!("{size:.1} {}", UNITS[unit])
        }
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
    /// The base image a new VM is created from, and the one an existing VM
    /// must have been created from.
    pub image: Option<ImageName>,
}

/// Optional CPUs and memory: the `[images.<name>]` and `[provision.<name>]` tables, or the
/// flags that `run` and `provision` share.
#[derive(Args, Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    /// Number of virtual CPUs
    #[arg(long)]
    pub cpus: Option<u32>,

    /// Memory size, e.g. 4G or 512M
    #[arg(long)]
    pub memory: Option<ByteSize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    #[serde(flatten)]
    pub defaults: Settings,
    #[serde(default)]
    pub provision: Provision,
    /// Resources for the VMs made from single images.
    #[serde(default)]
    pub images: BTreeMap<ImageName, Resources>,
    #[serde(default)]
    pub projects: BTreeMap<PathBuf, Settings>,
}

/// The `[provision]` table: resources for building all base images, and
/// `[provision.<name>]` tables for single images.
#[derive(Debug, Default, Deserialize)]
#[serde(try_from = "BTreeMap<String, toml::Value>")]
pub struct Provision {
    pub resources: Resources,
    pub images: BTreeMap<ImageName, Resources>,
}

/// Parsed by hand, since `#[serde(flatten)]` would report a bad key in
/// `[provision.<name>]` as the unknown field `<name>`.
impl TryFrom<BTreeMap<String, toml::Value>> for Provision {
    type Error = String;

    fn try_from(table: BTreeMap<String, toml::Value>) -> Result<Self, String> {
        let mut provision = Self::default();
        for (key, value) in table {
            match key.as_str() {
                "cpus" => {
                    provision.resources.cpus =
                        Some(value.try_into().map_err(|e| format!("cpus: {e}"))?)
                }
                "memory" => {
                    provision.resources.memory =
                        Some(value.try_into().map_err(|e| format!("memory: {e}"))?)
                }
                _ if value.is_table() => {
                    let image = key.parse().map_err(|e| format!("{e:#}"))?;
                    let resources = value
                        .try_into()
                        .map_err(|e| format!("in [provision.{key}]: {e}"))?;
                    provision.images.insert(image, resources);
                }
                _ => {
                    return Err(format!(
                        "unknown field `{key}`, expected `cpus`, `memory` or an image's table"
                    ));
                }
            }
        }
        Ok(provision)
    }
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

impl Mount {
    /// `ro` or `rw`, as in mount options.
    pub fn mode(&self) -> &'static str {
        if self.read_only { "ro" } else { "rw" }
    }
}

/// Fully resolved settings for one VM run.
#[derive(Clone, Debug)]
pub struct VmSettings {
    pub cpus: u32,
    pub memory: ByteSize,
    pub disk_size: ByteSize,
    /// The project mount first, then extra mounts in precedence order.
    /// Guest paths are normalized absolute UTF-8 paths without control
    /// characters or surrounding spaces.
    pub mounts: Vec<Mount>,
    pub expose_git: bool,
    /// `None` unless a layer chooses one: the VM keeps its image, and a new
    /// one is created from `default`.
    pub image: Option<ImageName>,
}

impl Config {
    /// Loads the config file, or the empty config if it doesn't exist.
    pub fn load(paths: &Paths) -> Result<Self> {
        Ok(util::read_toml(&paths.config_file())?.unwrap_or_default())
    }

    /// The `[projects."<path>"]` table matching `project`, if any. It is an
    /// error for several tables to match, e.g. `~/app` and `/Users/me/app`.
    fn project_settings(&self, paths: &Paths, project: &Project) -> Result<Option<&Settings>> {
        let mut matches = self.projects.iter().filter(|(key, _)| {
            let key = paths.expand_tilde(key);
            let key = key.canonicalize().unwrap_or(key);
            key == project.root
        });
        let first = matches.next();
        if let (Some((a, _)), Some((b, _))) = (first, matches.next()) {
            bail!(
                "[projects.\"{}\"] and [projects.\"{}\"] both configure {}",
                a.display(),
                b.display(),
                project.root.display()
            );
        }
        Ok(first.map(|(_, settings)| settings))
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
        let project_settings = self.project_settings(paths, project)?;
        let chosen = [Some(cli), project_settings, Some(&self.defaults)]
            .into_iter()
            .flatten()
            .find_map(|layer| layer.image.clone());
        // The chosen image, else the one the VM was made from. `run` refuses
        // to start a VM when the two differ.
        let image_settings = self
            .images
            .get(&project_vm::image(paths, project, chosen.as_ref()))
            .map(|resources| Settings {
                cpus: resources.cpus,
                memory: resources.memory,
                ..Settings::default()
            });
        let layers = [
            (Some(&self.defaults), None),
            (image_settings.as_ref(), None),
            (project_settings, None),
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
            image: chosen,
        };
        settings.validate()?;
        Ok(settings)
    }

    /// The image the config file chooses for `project`, if any, without
    /// resolving (and validating) the other settings.
    pub fn resolve_image(&self, paths: &Paths, project: &Project) -> Result<Option<ImageName>> {
        let project = self.project_settings(paths, project)?;
        Ok([project, Some(&self.defaults)]
            .into_iter()
            .flatten()
            .find_map(|layer| layer.image.clone()))
    }

    /// Every image the config file names.
    pub fn images(&self) -> impl Iterator<Item = &ImageName> {
        let chosen = [&self.defaults]
            .into_iter()
            .chain(self.projects.values())
            .filter_map(|settings| settings.image.as_ref());
        self.images
            .keys()
            .chain(self.provision.images.keys())
            .chain(chosen)
    }

    /// Merges the layers that apply to provisioning `image` with the CLI flags.
    pub fn resolve_provision(
        &self,
        image: &ImageName,
        cli: &Resources,
    ) -> Result<ProvisionSettings> {
        let mut cpus = self.defaults.cpus.unwrap_or(DEFAULT_CPUS);
        let mut memory = self.defaults.memory.unwrap_or(DEFAULT_MEMORY);
        for layer in [&self.provision.resources]
            .into_iter()
            .chain(self.provision.images.get(image))
            .chain([cli])
        {
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
        // Normalized, e.g. without a trailing slash, which would hide a
        // symlink from the guest's check for them (`[ -L dir/ ]`).
        Some(guest) => guest.components().collect(),
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
    /// The project directory's mount.
    pub fn project(&self) -> &Mount {
        &self.mounts[0]
    }

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
            // The guest reads guest paths from a line-based manifest.
            let text = guest.to_str().with_context(|| {
                format!(
                    "mount {}: guest path {} is not valid UTF-8",
                    mount.host.display(),
                    guest.display()
                )
            })?;
            ensure!(
                !text.contains(char::is_control) && text.trim() == text,
                "mount {}: guest path {text:?} may not contain control characters or surrounding spaces",
                mount.host.display()
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
    use crate::util::TempDir;
    use std::fs;

    impl Config {
        fn parse(text: &str) -> Result<Self> {
            Ok(toml::from_str(text)?)
        }
    }

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
    fn formats_approximate_sizes() {
        assert_eq!(ByteSize(512).approx(), "512 B");
        assert_eq!(ByteSize(1536).approx(), "1.5 KiB");
        assert_eq!(ByteSize(1_932_735_283).approx(), "1.8 GiB");
    }

    #[test]
    fn guest_home_belongs_to_guest_user() {
        assert_eq!(GUEST_HOME, format!("/home/{GUEST_USER}"));
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

            [provision]
            cpus = 3

            [provision.rust]
            memory = "16G"

            [images.rust]
            memory = "12G"

            [projects."/tmp/foo"]
            cpus = 8
            expose-git = true
            image = "rust"
            "#,
        )
        .unwrap();
        assert_eq!(config.defaults.cpus, Some(4));
        assert_eq!(config.defaults.memory, Some(ByteSize::gib(8)));
        assert_eq!(config.defaults.mounts.len(), 1);
        let project = &config.projects[Path::new("/tmp/foo")];
        assert_eq!(project.cpus, Some(8));
        assert_eq!(project.expose_git, Some(true));
        assert_eq!(project.image, Some("rust".parse().unwrap()));
        let rust = "rust".parse().unwrap();
        assert_eq!(config.images[&rust].memory, Some(ByteSize::gib(12)));
        assert_eq!(config.provision.resources.cpus, Some(3));
        assert_eq!(
            config.provision.images[&rust].memory,
            Some(ByteSize::gib(16))
        );

        assert!(Config::parse("image = \"Rust\"").is_err());
        assert!(Config::parse("[images.\"../x\"]\ncpus = 1").is_err());
        assert!(Config::parse("[provision.\"../x\"]\ncpus = 1").is_err());
    }

    #[test]
    fn resolves_provision_resources() {
        let rust: ImageName = "rust".parse().unwrap();
        let resolve = |config: &str, cli: Resources| {
            Config::parse(config)
                .unwrap()
                .resolve_provision(&rust, &cli)
        };
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
        assert_eq!(
            resolve(&table, cli.clone()).unwrap().memory,
            ByteSize::gib(1)
        );

        // Only the image's own table applies, above [provision], and the
        // resources for running VMs don't.
        let images = format!(
            "{table}[provision.rust]\ncpus = 2\n[provision.go]\ncpus = 6\n\
             [images.rust]\ncpus = 7\nmemory = \"7G\"\n"
        );
        let settings = resolve(&images, none()).unwrap();
        assert_eq!((settings.cpus, settings.memory), (2, ByteSize::gib(3)));
        assert_eq!(resolve(&images, cli).unwrap().memory, ByteSize::gib(1));

        assert!(resolve("[provision]\ncpus = 0", none()).is_err());
        assert!(resolve("[provision]\nmemory = \"256M\"", none()).is_err());
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(Config::parse("cpu = 4").is_err());
        assert!(Config::parse("[projects.\"/x\"]\nram = \"4G\"").is_err());
        assert!(Config::parse("[provision]\nmounts = []").is_err());
        assert!(Config::parse("[images.rust]\nimage = \"go\"").is_err());
        assert!(Config::parse("[provision]\ndisk-size = \"8G\"").is_err());
        assert!(Config::parse("[provision.rust]\nmounts = []").is_err());
    }

    struct Fixture {
        dir: PathBuf,
        paths: Paths,
        project: Project,
        _temp: TempDir,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let temp = TempDir::new(name);
            let dir = temp.path().to_path_buf();
            fs::create_dir_all(dir.join("home/extra")).unwrap();
            fs::create_dir_all(dir.join("proj")).unwrap();
            let paths = Paths::new(dir.join("home"));
            let project = Project::at(&dir.join("proj")).unwrap();
            Self {
                dir,
                paths,
                project,
                _temp: temp,
            }
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
        assert_eq!(vm.image, None);
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
    fn resolves_images() {
        let fx = Fixture::new("chosen-images");
        let image = |name: &str| Some(name.parse::<ImageName>().unwrap());
        let project =
            |settings: &str| format!("[projects.\"{}\"]\n{settings}\n", fx.project.root.display());
        let resolve = |config: &str, cli: Option<&str>| {
            let config = Config::parse(config).unwrap();
            let cli = Settings {
                image: cli.and_then(image),
                ..Settings::default()
            };
            let vm = config
                .resolve(&fx.paths, &fx.project, &cli, &fx.dir)
                .unwrap();
            if cli.image.is_none() {
                assert_eq!(
                    config.resolve_image(&fx.paths, &fx.project).unwrap(),
                    vm.image
                );
            }
            vm.image
        };

        assert_eq!(resolve("", None), None);
        assert_eq!(resolve("image = \"go\"", None), image("go"));
        let both = format!("image = \"go\"\n{}", project("image = \"rust\""));
        assert_eq!(resolve(&both, None), image("rust"));
        assert_eq!(resolve(&both, Some("zig")), image("zig"));
        // A project table without an image keeps the top-level one.
        let other = format!("image = \"go\"\n{}", project("cpus = 3"));
        assert_eq!(resolve(&other, None), image("go"));

        let config = Config::parse(&format!(
            "[images.a]\n[provision.c]\n{}",
            project("image = \"b\"")
        ))
        .unwrap();
        let named: Vec<_> = config.images().map(ImageName::as_str).collect();
        assert_eq!(named, ["a", "c", "b"]);
    }

    #[test]
    fn resolves_image_resources() {
        let fx = Fixture::new("image-resources");
        let resolve = |config: &str, cli: Settings| {
            let vm = Config::parse(config)
                .unwrap()
                .resolve(&fx.paths, &fx.project, &cli, &fx.dir)
                .unwrap();
            (vm.cpus, vm.memory)
        };
        let none = Settings::default;
        let images = "cpus = 1\nmemory = \"2G\"\n\
                      [images.default]\ncpus = 3\n\
                      [images.rust]\ncpus = 5\nmemory = \"6G\"\n\
                      [provision.default]\ncpus = 8\n";
        // Without a choice, and without a VM, it's the default image.
        assert_eq!(resolve(images, none()), (3, ByteSize::gib(2)));
        let rust = Settings {
            image: Some("rust".parse().unwrap()),
            ..Settings::default()
        };
        assert_eq!(resolve(images, rust.clone()), (5, ByteSize::gib(6)));

        // The project's table and the flags rank above the image's.
        let project = format!(
            "image = \"rust\"\n{images}[projects.\"{}\"]\ncpus = 4\n",
            fx.project.root.display()
        );
        assert_eq!(resolve(&project, none()), (4, ByteSize::gib(6)));
        let cli = Settings {
            memory: Some(ByteSize::gib(1)),
            ..rust
        };
        assert_eq!(resolve(&project, cli), (4, ByteSize::gib(1)));
    }

    #[test]
    fn resolves_resources_of_the_vm_image() {
        let fx = Fixture::new("vm-image-resources");
        let vm = project_vm::dir(&fx.paths, &fx.project);
        fs::create_dir_all(vm.path()).unwrap();
        fs::write(vm.metadata(), "project_path = \"/p\"\nimage = \"rust\"\n").unwrap();
        let vm = Config::parse("[images.rust]\ncpus = 5\n")
            .unwrap()
            .resolve(&fx.paths, &fx.project, &Settings::default(), &fx.dir)
            .unwrap();
        assert_eq!(vm.cpus, 5);
        assert_eq!(vm.image, None);
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
        assert!(resolve("", cli("proj:/mnt/a\nhide /")).is_err());
        assert!(resolve("", cli("proj:/mnt/a ")).is_err());
    }

    #[test]
    fn rejects_duplicate_project_tables() {
        let fx = Fixture::new("duplicate");
        let config = Config::parse(&format!(
            "[projects.\"{}\"]\ncpus = 1\n[projects.\"{}/../proj\"]\ncpus = 2\n",
            fx.project.root.display(),
            fx.project.root.display()
        ))
        .unwrap();
        let error = config
            .resolve(&fx.paths, &fx.project, &Settings::default(), &fx.dir)
            .unwrap_err();
        assert!(error.to_string().contains("both configure"), "{error}");
    }
}
