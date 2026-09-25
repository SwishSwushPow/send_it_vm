//! Small helpers for files, external commands and time.

use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use serde::de::DeserializeOwned;

/// Turns a "not found" error into `None`, e.g. `if_exists(fs::read(path))`.
pub fn if_exists<T>(result: io::Result<T>) -> io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
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

/// Seconds since the Unix epoch.
pub fn unix_now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

/// A directory for a test, deleted again when dropped.
#[cfg(test)]
pub struct TempDir(std::path::PathBuf);

#[cfg(test)]
impl TempDir {
    /// Creates an empty, canonical directory unique to `name` and this process.
    pub fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("sendit-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
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
