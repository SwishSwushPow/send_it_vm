//! Where sendit keeps its state. Nothing is ever written into project folders:
//!
//! - `~/.cache/sendit/`: downloads, the provisioned base image, provisioning scratch
//! - `~/.sendit/<project>_<uuid>/`: one directory per project VM
//! - `~/.config/sendit/config.toml`: user configuration
//! - `~/.config/sendit/provision-scripts/`: custom provisioning scripts for the base image

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
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

    pub fn cache_dir(&self) -> PathBuf {
        self.home.join(".cache/sendit")
    }

    /// Downloaded Debian cloud images and their checksums.
    pub fn downloads_dir(&self) -> PathBuf {
        self.cache_dir().join("downloads")
    }

    /// The provisioned base image that project VMs are cloned from.
    pub fn base_dir(&self) -> PathBuf {
        self.cache_dir().join("base")
    }

    /// Where a base image is built before it replaces `base_dir`.
    pub fn base_partial_dir(&self) -> PathBuf {
        self.cache_dir().join("base.partial")
    }

    /// Scratch space for provisioning runs (seed ISO, console log).
    pub fn provision_dir(&self) -> PathBuf {
        self.cache_dir().join("provision")
    }

    /// The keypair whose public key is baked into the base image.
    pub fn ssh_dir(&self) -> PathBuf {
        self.cache_dir().join("ssh")
    }

    pub fn config_file(&self) -> PathBuf {
        self.home.join(".config/sendit/config.toml")
    }

    /// Custom provisioning scripts (`*.sh`), run in name order after the
    /// built-in provisioning of the base image.
    pub fn provision_scripts_dir(&self) -> PathBuf {
        self.home.join(".config/sendit/provision-scripts")
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
    fn layout() {
        let paths = Paths::new(PathBuf::from("/home/me"));
        assert_eq!(paths.base_dir(), Path::new("/home/me/.cache/sendit/base"));
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
