//! Where sendit keeps its state. Nothing is ever written into project folders:
//!
//! - `~/.cache/sendit/`: downloads, the provisioned base images, provisioning scratch
//! - `~/.sendit/<project>_<uuid>/`: one directory per project VM
//! - `~/.config/sendit/config.toml`: user configuration
//! - `~/.config/sendit/provision-scripts/`: custom provisioning scripts for
//!   all base images, and in `<image>/` for one of them

use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, bail, ensure};
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct Paths {
    home: PathBuf,
}

impl Paths {
    pub fn new(home: PathBuf) -> Self {
        Self { home }
    }

    pub fn from_env() -> Result<Self> {
        let home = std::env::home_dir().context("cannot determine the home directory")?;
        Ok(Self::new(home))
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

    /// The single base image of sendit versions before named images; it
    /// becomes the `default` image.
    pub fn legacy_base_dir(&self) -> PathBuf {
        self.cache_dir().join("base")
    }

    /// Scratch space for provisioning an image (seed ISO, console log).
    pub fn provision_dir(&self, image: &ImageName) -> PathBuf {
        self.cache_dir().join("provision").join(image.as_str())
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

    pub fn config_file(&self) -> PathBuf {
        self.home.join(".config/sendit/config.toml")
    }

    /// Custom provisioning scripts (`*.sh`) for all images, run in name order
    /// after the built-in provisioning of a base image.
    pub fn provision_scripts_dir(&self) -> PathBuf {
        self.home.join(".config/sendit/provision-scripts")
    }

    /// Custom provisioning scripts for `image` only.
    pub fn image_scripts_dir(&self, image: &ImageName) -> PathBuf {
        self.provision_scripts_dir().join(image.as_str())
    }

    /// Parent of all project VM directories.
    pub fn vms_dir(&self) -> PathBuf {
        self.home.join(".sendit")
    }

    pub fn vm_dir(&self, project: &Project) -> PathBuf {
        self.vms_dir().join(&project.id)
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
            Ok(rest) => Path::new("~").join(rest).display().to_string(),
            Err(_) => path.display().to_string(),
        }
    }
}

/// The name of a base image: lowercase letters, digits and dashes. It is
/// part of paths, so nothing else is allowed.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
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
        let valid = !s.is_empty()
            && s.len() <= 64
            && !s.starts_with('-')
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !valid {
            bail!(
                "invalid image name {s:?}: expected up to 64 lowercase letters, digits \
                 and dashes, not starting with a dash"
            );
        }
        Ok(Self(s.into()))
    }
}

impl fmt::Display for ImageName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A project directory and the stable ID of its VM.
#[derive(Clone, Debug)]
pub struct Project {
    /// Canonical absolute path of the project directory.
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

fn file_url(path: &Path) -> Vec<u8> {
    let mut url = b"file://".to_vec();
    url.extend_from_slice(path.as_os_str().as_bytes());
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_ids_are_stable_and_distinct() {
        let a = project_id(Path::new("/Users/me/dev/my app"));
        assert!(a.starts_with("my_app_"), "{a}");
        assert_eq!(a, project_id(Path::new("/Users/me/dev/my app")));
        assert_ne!(a, project_id(Path::new("/Users/me/other/my app")));
        assert!(project_id(Path::new("/")).starts_with("root_"));
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
            paths.image_scripts_dir(&rust),
            Path::new("/home/me/.config/sendit/provision-scripts/rust")
        );
        assert_eq!(
            paths.config_file(),
            Path::new("/home/me/.config/sendit/config.toml")
        );
        assert_eq!(paths.vms_dir(), Path::new("/home/me/.sendit"));
        assert_eq!(
            paths.expand_tilde(Path::new("~/x")),
            Path::new("/home/me/x")
        );
        assert_eq!(paths.expand_tilde(Path::new("/x/~")), Path::new("/x/~"));
        assert_eq!(paths.display(Path::new("/home/me/.sendit")), "~/.sendit");
    }
}
