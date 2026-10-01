//! Small helpers for files and external commands.

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;
use std::ptr::NonNull;

use anyhow::{Context, Result, ensure};
use objc2::rc::Retained;
use objc2_foundation::{NSNumber, NSURL, NSURLIsExcludedFromBackupKey};
use serde::de::DeserializeOwned;

/// Turns a "not found" error into `None`, e.g. `if_exists(fs::read(path))`.
pub fn if_exists<T>(result: io::Result<T>) -> io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// The entries of `dir`; none if it doesn't exist.
pub fn read_dir(dir: &Path) -> Result<impl Iterator<Item = io::Result<fs::DirEntry>>> {
    let entries =
        if_exists(fs::read_dir(dir)).with_context(|| format!("reading {}", dir.display()))?;
    Ok(entries.into_iter().flatten())
}

/// Creates `dir` empty, deleting whatever was there before.
pub fn fresh_dir(dir: &Path) -> Result<()> {
    let _ = fs::remove_dir_all(dir);
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))
}

/// Creates `dir` if needed and keeps it and everything in it out of Time
/// Machine backups, like `tmutil addexclusion`: the mark sits on the
/// directory itself and moves with it.
pub fn exclude_from_backups(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let url = file_url(dir)?;
    let yes = NSNumber::new_bool(true);
    // SAFETY: the key takes an NSNumber holding a boolean.
    unsafe { url.setResourceValue_forKey_error(Some(&yes), NSURLIsExcludedFromBackupKey) }
        .map_err(|error| anyhow::anyhow!(error.localizedDescription().to_string()))
}

/// A file URL for `path`, built from its bytes so that paths which aren't
/// valid UTF-8 still point at the right file.
pub fn file_url(path: &Path) -> Result<Retained<NSURL>> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("path {} contains a NUL byte", path.display()))?;
    // SAFETY: `c_path` is a valid NUL-terminated string that outlives the call.
    Ok(unsafe {
        NSURL::fileURLWithFileSystemRepresentation_isDirectory_relativeToURL(
            NonNull::new_unchecked(c_path.as_ptr().cast_mut()),
            path.is_dir(),
            None,
        )
    })
}

/// Reads and parses a TOML file; `None` if it doesn't exist.
pub fn read_toml<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let Some(text) = if_exists(fs::read_to_string(path))
        .with_context(|| format!("reading {}", path.display()))?
    else {
        return Ok(None);
    };
    toml::from_str(&text)
        .map(Some)
        .with_context(|| format!("in {}", path.display()))
}

/// Runs `cmd` with inherited stdio and fails unless it succeeds.
pub fn run(cmd: &mut Command) -> Result<()> {
    let program = cmd.get_program().display().to_string();
    let status = cmd.status().with_context(|| format!("running {program}"))?;
    ensure!(status.success(), "{program} failed with {status}");
    Ok(())
}

/// Runs `cmd` and returns its stdout; on failure, the error carries its stderr.
pub fn run_output(cmd: &mut Command) -> Result<Vec<u8>> {
    let program = cmd.get_program().display().to_string();
    let output = cmd.output().with_context(|| format!("running {program}"))?;
    ensure!(
        output.status.success(),
        "{program} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

/// A directory for a test, deleted again when dropped.
#[cfg(test)]
pub struct TempDir(std::path::PathBuf);

#[cfg(test)]
impl TempDir {
    /// Creates an empty, canonical directory unique to `name` and this process.
    pub fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("sendit-test-{name}-{}", std::process::id()));
        fresh_dir(&dir).unwrap();
        Self(dir.canonicalize().unwrap())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

#[cfg(test)]
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
