//! macOS: APFS, Time Machine and the virtualization entitlement.

mod sign;

use std::ffi::CString;
use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr::NonNull;

use anyhow::{Context, Result};
use objc2::rc::Retained;
use objc2_foundation::{NSNumber, NSURL, NSURLIsExcludedFromBackupKey};

pub use sign::ensure_entitled;

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

/// Copies `src` to `dst` as an APFS copy-on-write clone. Fails with `EXDEV`
/// across volumes, and with `ENOTSUP` on other file systems.
pub fn clonefile(src: &Path, dst: &Path) -> io::Result<()> {
    let c_src = CString::new(src.as_os_str().as_bytes())?;
    let c_dst = CString::new(dst.as_os_str().as_bytes())?;
    // SAFETY: both arguments are valid NUL-terminated paths.
    if unsafe { libc::clonefile(c_src.as_ptr(), c_dst.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Deallocates `len` bytes of `file` from `offset`, which then read as zeros.
pub fn punch_hole(file: &File, offset: u64, len: u64) -> io::Result<()> {
    let args = libc::fpunchhole_t {
        fp_flags: 0,
        reserved: 0,
        fp_offset: offset as libc::off_t,
        fp_length: len as libc::off_t,
    };
    // SAFETY: F_PUNCHHOLE takes a pointer to a valid fpunchhole_t.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PUNCHHOLE, &args) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
