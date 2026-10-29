//! Where sendit keeps its state. Nothing is ever written into project folders:
//!
//! - `~/.cache/sendit/`: downloads, the provisioned base images, the SSH keys
//!   baked into them, provisioning scratch
//! - `~/.sendit/<project>_<uuid>/`: one directory per project VM, or in
//!   `$SENDIT_VM_DIR` instead, e.g. on an external drive; next to them,
//!   `.names.lock`, held while VM names are given out or changed
//! - `~/.config/sendit/config.toml`: user configuration
//! - `~/.config/sendit/images/<image>.sh`: the image files, one for each
//!   base image with custom provisioning
//! - `~/.config/sendit/provision-scripts/`: the shared provisioning scripts
//!   that image files call

use std::fmt;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::util::canonical;

/// Names the directory that holds the project VMs instead of `~/.sendit`.
pub const VMS_DIR_ENV: &str = "SENDIT_VM_DIR";

#[derive(Clone, Debug)]
pub struct Paths {
    home: PathBuf,
    vms_dir: PathBuf,
}

impl Paths {
    pub fn new(home: PathBuf) -> Self {
        let vms_dir = home.join(".sendit");
        Self { home, vms_dir }
    }

    pub fn from_env() -> Result<Self> {
        let home = std::env::home_dir().context("cannot determine the home directory")?;
        let paths = Self::new(home);
        match std::env::var_os(VMS_DIR_ENV).filter(|dir| !dir.is_empty()) {
            Some(dir) => paths
                .with_vms_dir(Path::new(&dir))
                .with_context(|| format!("invalid {VMS_DIR_ENV}")),
            None => Ok(paths),
        }
    }

    /// Keeps the project VMs in `dir` instead of `~/.sendit`. It must exist
    /// already, so that a drive that isn't connected is an error rather
    /// than VMs made somewhere else. Every start keeps it out of backups,
    /// so it must not be the root of a volume or hold the home directory or
    /// sendit's other files.
    pub fn with_vms_dir(mut self, dir: &Path) -> Result<Self> {
        // Without a trailing slash, for display.
        let dir: PathBuf = self.expand_tilde(dir).components().collect();
        ensure!(
            dir.is_absolute(),
            "{} is not an absolute path",
            dir.display()
        );
        ensure!(
            dir.is_dir(),
            "{} is not a directory; create it, or connect the drive it is on",
            dir.display()
        );
        let real = canonical(&dir);
        let volume_root = match real.parent() {
            None => true,
            Some(parent) => fs::metadata(parent)?.dev() != fs::metadata(&real)?.dev(),
        };
        ensure!(
            !volume_root,
            "{} is the root of a volume; use a directory on it, e.g. {}",
            dir.display(),
            dir.join("sendit").display()
        );
        ensure!(
            !canonical(&self.home).starts_with(&real),
            "{} contains your home directory",
            dir.display()
        );
        for other in [self.config_dir(), self.cache_dir()] {
            let other_real = canonical(&other);
            ensure!(
                !other_real.starts_with(&real) && !real.starts_with(&other_real),
                "{} must not contain or be inside {}",
                dir.display(),
                self.display(&other)
            );
        }
        self.vms_dir = dir;
        Ok(self)
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.home.join(".cache/sendit")
    }

    /// Downloaded Debian cloud images and their checksums.
    pub fn downloads_dir(&self) -> PathBuf {
        self.cache_dir().join("downloads")
    }

    /// Parent of all provisioned base images.
    pub fn images_dir(&self) -> PathBuf {
        self.cache_dir().join("images")
    }

    /// A provisioned base image that project VMs are cloned from.
    pub fn image_dir(&self, image: &ImageName) -> PathBuf {
        self.images_dir().join(image.as_str())
    }

    /// Where a base image is built before it replaces `image_dir`.
    pub fn image_partial_dir(&self, image: &ImageName) -> PathBuf {
        self.images_dir().join(format!(".{image}.partial"))
    }

    /// Where a base image is moved to be deleted, so that nothing finds
    /// part of it at `image_dir`.
    pub fn image_old_dir(&self, image: &ImageName) -> PathBuf {
        self.images_dir().join(format!(".{image}.old"))
    }

    /// Scratch space for provisioning an image (seed ISO, console log).
    pub fn provision_dir(&self, image: &ImageName) -> PathBuf {
        self.cache_dir().join("provision").join(image.as_str())
    }

    /// Held while a base image is built.
    pub fn provision_lock(&self) -> PathBuf {
        self.cache_dir().join("provision.lock")
    }

    /// The keypairs whose public keys are baked into the base image.
    pub fn ssh_dir(&self) -> PathBuf {
        self.cache_dir().join("ssh")
    }

    /// The private key for logging in as the guest user.
    pub fn ssh_key(&self) -> PathBuf {
        self.ssh_dir().join("id_ed25519")
    }

    /// The private key for logging in as root: only the host can become
    /// root in a VM.
    pub fn root_ssh_key(&self) -> PathBuf {
        self.ssh_dir().join("id_ed25519_root")
    }

    /// The user's configuration: `config.toml`, the image files and the
    /// provisioning scripts they call.
    pub fn config_dir(&self) -> PathBuf {
        self.home.join(".config/sendit")
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir().join("config.toml")
    }

    /// The image files, `<image>.sh` for each image with custom
    /// provisioning.
    pub fn image_files_dir(&self) -> PathBuf {
        self.config_dir().join("images")
    }

    /// The main custom provisioning script of `image`, run after the built-in
    /// provisioning of the base image. It calls the shared scripts with
    /// `sendit-provision`.
    pub fn image_file(&self, image: &ImageName) -> PathBuf {
        self.image_files_dir().join(format!("{image}.sh"))
    }

    /// The shared custom provisioning scripts (`*.sh`) that image files call.
    pub fn provision_scripts_dir(&self) -> PathBuf {
        self.config_dir().join("provision-scripts")
    }

    /// Parent of all project VM directories.
    pub fn vms_dir(&self) -> PathBuf {
        self.vms_dir.clone()
    }

    /// The directories holding only what sendit can make again: downloads,
    /// base images, SSH keys baked into them, and project VMs, which are
    /// clones of the base images. Not worth backing up; only the
    /// configuration is.
    pub fn rebuildable_dirs(&self) -> [PathBuf; 2] {
        [self.cache_dir(), self.vms_dir()]
    }

    pub fn vm_dir(&self, project: &Project) -> PathBuf {
        self.vms_dir().join(&project.id)
    }

    /// Held while VM names are given out or changed, so that no two VMs
    /// get the same one.
    pub fn names_lock(&self) -> PathBuf {
        self.vms_dir().join(".names.lock")
    }

    /// Expands a leading `~` to the home directory.
    pub fn expand_tilde(&self, path: &Path) -> PathBuf {
        match path.strip_prefix("~") {
            Ok(rest) => self.home.join(rest),
            Err(_) => path.to_path_buf(),
        }
    }

    /// Shortens paths below the home directory to `~/…` for display.
    pub fn display(&self, path: &Path) -> String {
        match path.strip_prefix(&self.home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
            Ok(rest) => Path::new("~").join(rest).display().to_string(),
            Err(_) => path.display().to_string(),
        }
    }
}

/// The name of a base image: lowercase letters, digits and dashes. It is
/// part of paths, so nothing else is allowed.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct ImageName(String);

impl ImageName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The image used when none is chosen.
impl Default for ImageName {
    fn default() -> Self {
        Self("default".into())
    }
}

impl FromStr for ImageName {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        if !is_name(s) {
            bail!("invalid image name {s:?}: {NAME_RULE}");
        }
        Ok(Self(s.into()))
    }
}

impl TryFrom<String> for ImageName {
    type Error = anyhow::Error;

    fn try_from(s: String) -> Result<Self> {
        s.parse()
    }
}

impl From<ImageName> for String {
    fn from(image: ImageName) -> Self {
        image.0
    }
}

impl fmt::Display for ImageName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The longest image or VM name.
const MAX_NAME: usize = 64;

/// What `is_name` allows, for error messages.
const NAME_RULE: &str =
    "expected up to 64 lowercase letters, digits and dashes, not starting with a dash";

/// Whether `s` is a valid image or VM name: up to `MAX_NAME` lowercase
/// letters, digits and dashes, not starting with a dash.
fn is_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_NAME
        && !s.starts_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The name of a project VM, which `-n` takes in place of the project
/// directory. It follows the rules for image names, so that it is easy to
/// type. No two VMs have the same one.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct VmName(String);

impl VmName {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The name made of `text`, e.g. a folder name: lowercased, each run of
    /// characters other than ASCII letters and digits turned into a dash,
    /// with none at either end, and cut to `MAX_NAME`. `None` if nothing is
    /// left.
    pub fn from_text(text: &str) -> Option<Self> {
        let mut name = String::new();
        for c in text.chars() {
            if c.is_ascii_alphanumeric() {
                name.push(c.to_ascii_lowercase());
            } else if !name.is_empty() && !name.ends_with('-') {
                name.push('-');
            }
        }
        name.truncate(MAX_NAME);
        let name = name.trim_end_matches('-');
        (!name.is_empty()).then(|| Self(name.into()))
    }

    /// This name with `-<n>` at the end, cut short as needed to fit.
    pub fn numbered(&self, n: u32) -> Self {
        let suffix = format!("-{n}");
        let keep = self.0.len().min(MAX_NAME - suffix.len());
        Self(format!("{}{suffix}", self.0[..keep].trim_end_matches('-')))
    }
}

impl FromStr for VmName {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        if !is_name(s) {
            bail!("invalid VM name {s:?}: {NAME_RULE}");
        }
        Ok(Self(s.into()))
    }
}

impl TryFrom<String> for VmName {
    type Error = anyhow::Error;

    fn try_from(s: String) -> Result<Self> {
        s.parse()
    }
}

impl From<VmName> for String {
    fn from(name: VmName) -> Self {
        name.0
    }
}

impl fmt::Display for VmName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A project directory and the stable ID of its VM.
#[derive(Clone, Debug)]
pub struct Project {
    /// Canonical absolute path of the project directory. For a project
    /// found by its VM's name, the one recorded when the VM was made, which
    /// may be gone.
    pub root: PathBuf,
    /// `<sanitized folder name>_<UUIDv5 of root>`.
    pub id: String,
}

impl Project {
    pub fn at(dir: &Path) -> Result<Self> {
        let root = dir
            .canonicalize()
            .with_context(|| format!("project directory {}", dir.display()))?;
        ensure!(root.is_dir(), "{} is not a directory", root.display());
        let id = project_id(&root);
        Ok(Self { root, id })
    }
}

fn project_id(root: &Path) -> String {
    let name: String = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".into())
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '_',
        })
        .collect();
    let uuid = Uuid::new_v5(&Uuid::NAMESPACE_URL, &file_url(root));
    format!("{name}_{uuid}")
}

/// Whether `name` has the form of a project ID, `<name>_<UUID>`. Other
/// directories next to the VMs, e.g. in `$SENDIT_VM_DIR`, aren't VMs.
pub fn is_project_id(name: &str) -> bool {
    name.rsplit_once('_').is_some_and(|(name, uuid)| {
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
            && uuid.len() == 36
            && Uuid::try_parse(uuid).is_ok()
    })
}

fn file_url(path: &Path) -> Vec<u8> {
    let mut url = b"file://".to_vec();
    url.extend_from_slice(path.as_os_str().as_bytes());
    url
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::TempDir;

    #[test]
    fn project_ids_are_stable_and_distinct() {
        let a = project_id(Path::new("/Users/me/dev/my app"));
        assert!(a.starts_with("my_app_"), "{a}");
        assert_eq!(a, project_id(Path::new("/Users/me/dev/my app")));
        assert_ne!(a, project_id(Path::new("/Users/me/other/my app")));
        assert!(project_id(Path::new("/")).starts_with("root_"));
    }

    #[test]
    fn recognizes_project_ids() {
        for path in ["/Users/me/dev/my app", "/x/.dotted_name", "/"] {
            assert!(is_project_id(&project_id(Path::new(path))), "{path}");
        }
        let uuid = "6ba7b811-9dad-11d1-80b4-00c04fd430c8";
        for name in [
            "photos",
            "my_app",
            &format!("_{uuid}"),
            &format!("app_{}", uuid.replace('-', "")),
            &format!(".app_{uuid}.partial"),
            &format!("my app_{uuid}"),
        ] {
            assert!(!is_project_id(name), "{name}");
        }
    }

    #[test]
    fn validates_image_names() {
        for name in ["default", "rust", "node-22", "1"] {
            assert_eq!(name.parse::<ImageName>().unwrap().as_str(), name);
        }
        for name in ["", "-x", "Rust", "a.b", "a/b", "..", "a b", &"x".repeat(65)] {
            assert!(name.parse::<ImageName>().is_err(), "{name:?}");
        }
    }

    #[test]
    fn validates_vm_names() {
        for name in ["app", "send-it", "node-22", "1"] {
            assert_eq!(name.parse::<VmName>().unwrap().as_str(), name);
        }
        for name in ["", "-x", "App", "a.b", "a_b", "a b", &"x".repeat(65)] {
            let error = name.parse::<VmName>().unwrap_err().to_string();
            assert!(error.starts_with("invalid VM name"), "{error}");
        }
    }

    #[test]
    fn makes_vm_names_of_folder_names() {
        let name = |text: &str| VmName::from_text(text).map(String::from);
        assert_eq!(name("send_it").as_deref(), Some("send-it"));
        assert_eq!(name("My App (2)").as_deref(), Some("my-app-2"));
        assert_eq!(name("--x__y--").as_deref(), Some("x-y"));
        assert_eq!(name("Über.rs").as_deref(), Some("ber-rs"));
        assert_eq!(name("日本"), None);
        assert_eq!(name(""), None);
        // Cut to 64 characters, without a dash at the end.
        let long = format!("{}-{}", "a".repeat(63), "b".repeat(10));
        assert_eq!(name(&long), Some("a".repeat(63)));
        assert_eq!(name(&"c".repeat(70)), Some("c".repeat(64)));
    }

    #[test]
    fn numbers_vm_names() {
        let app: VmName = "app".parse().unwrap();
        assert_eq!(app.numbered(2).as_str(), "app-2");
        // Cut short to fit, without a double dash.
        let long: VmName = format!("{}-{}", "a".repeat(61), "bc").parse().unwrap();
        assert_eq!(long.numbered(2).as_str(), format!("{}-2", "a".repeat(61)));
        let full: VmName = "x".repeat(64).parse().unwrap();
        assert_eq!(full.numbered(10).as_str(), format!("{}-10", "x".repeat(61)));
    }

    #[test]
    fn layout() {
        let paths = Paths::new(PathBuf::from("/home/me"));
        let rust: ImageName = "rust".parse().unwrap();
        assert_eq!(
            paths.image_dir(&rust),
            Path::new("/home/me/.cache/sendit/images/rust")
        );
        assert_eq!(
            paths.image_partial_dir(&rust),
            Path::new("/home/me/.cache/sendit/images/.rust.partial")
        );
        assert_eq!(
            paths.image_old_dir(&rust),
            Path::new("/home/me/.cache/sendit/images/.rust.old")
        );
        assert_eq!(
            paths.image_file(&rust),
            Path::new("/home/me/.config/sendit/images/rust.sh")
        );
        assert_eq!(
            paths.config_file(),
            Path::new("/home/me/.config/sendit/config.toml")
        );
        assert_eq!(paths.vms_dir(), Path::new("/home/me/.sendit"));
        assert_eq!(
            paths.names_lock(),
            Path::new("/home/me/.sendit/.names.lock")
        );
        assert_eq!(
            paths.expand_tilde(Path::new("~/x")),
            Path::new("/home/me/x")
        );
        assert_eq!(paths.expand_tilde(Path::new("/x/~")), Path::new("/x/~"));
        assert_eq!(paths.display(Path::new("/home/me/.sendit")), "~/.sendit");
    }

    #[test]
    fn vms_dir_can_move() {
        let temp = TempDir::new("vms-dir");
        let home = temp.path().join("home");
        let external = temp.path().join("external");
        for dir in [".config/sendit", ".cache/sendit", "vms"] {
            fs::create_dir_all(home.join(dir)).unwrap();
        }
        fs::create_dir_all(&external).unwrap();
        let paths = Paths::new(home.clone());
        let moved = |dir: &Path| paths.clone().with_vms_dir(dir).map(|p| p.vms_dir());

        assert_eq!(moved(&external).unwrap(), external);
        assert_eq!(moved(Path::new("~/vms/")).unwrap(), home.join("vms"));
        assert_eq!(paths.display(&home.join("vms")), "~/vms");

        let error = |dir: &Path| format!("{:#}", moved(dir).unwrap_err());
        assert!(error(Path::new("vms")).contains("not an absolute path"));
        assert!(error(&temp.path().join("missing")).contains("connect the drive"));
        assert!(error(Path::new("/")).contains("root of a volume"));
        assert!(error(&home).contains("contains your home directory"));
        assert!(error(temp.path()).contains("contains your home directory"));
        assert!(
            error(&home.join(".cache")).contains("must not contain or be inside ~/.cache/sendit")
        );
        assert!(error(&home.join(".cache/sendit")).contains("~/.cache/sendit"));
        assert!(error(&home.join(".config/sendit")).contains("~/.config/sendit"));
        // Through a symlink, too.
        std::os::unix::fs::symlink(&home, temp.path().join("link")).unwrap();
        assert!(error(&temp.path().join("link")).contains("contains your home directory"));
    }
}
