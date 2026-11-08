//! Linux, where sendit builds for developing it but runs no VMs: what only
//! macOS has does nothing, or fails as on a file system without it.

use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

use anyhow::{Context, Result};

/// Nothing needs the virtualization entitlement.
pub fn ensure_entitled() -> Result<()> {
    Ok(())
}

/// Creates `dir` if needed; there are no Time Machine backups to keep it out
/// of.
pub fn exclude_from_backups(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))
}

/// Fails with `ENOTSUP`, as `clonefile` does off APFS.
pub fn clonefile(_src: &Path, _dst: &Path) -> io::Result<()> {
    Err(io::Error::from_raw_os_error(libc::ENOTSUP))
}

/// Deallocates `len` bytes of `file` from `offset`, which then read as zeros.
pub fn punch_hole(file: &File, offset: u64, len: u64) -> io::Result<()> {
    let mode = libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE;
    // SAFETY: plain syscall on an open file.
    let result = unsafe {
        libc::fallocate(
            file.as_raw_fd(),
            mode,
            offset as libc::off_t,
            len as libc::off_t,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
