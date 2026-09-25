//! The virtualization entitlement, which Virtualization.framework requires.
//!
//! Builds from this repository are signed with it by
//! `scripts/link-and-sign.sh`. `cargo install` from crates.io ignores the
//! package's `.cargo/config.toml`, so that binary starts without it: it
//! signs a copy of itself, puts the copy in its place and runs that instead.

use std::ffi::c_void;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use objc2_foundation::ns_string;

use crate::util;

const ENTITLEMENTS: &[u8] = include_bytes!("../entitlements.plist");

/// Set for the re-executed binary, so a signature that didn't take effect
/// fails instead of looping.
const SIGNED_ENV: &str = "SENDIT_SIGNED";

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    fn SecTaskCreateFromSelf(allocator: *const c_void) -> *const c_void;
    fn SecTaskCopyValueForEntitlement(
        task: *const c_void,
        entitlement: *const c_void,
        error: *mut *const c_void,
    ) -> *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFBooleanTrue: *const c_void;
    fn CFRelease(cf: *const c_void);
}

/// Whether this process has the virtualization entitlement.
fn entitled() -> bool {
    // NSString is toll-free bridged to CFString.
    let key = ns_string!("com.apple.security.virtualization");
    // SAFETY: the task and the value are released once, after their last
    // use; `key` is a valid, static CFString.
    unsafe {
        let task = SecTaskCreateFromSelf(std::ptr::null());
        if task.is_null() {
            return false;
        }
        let key = key as *const _ as *const c_void;
        let value = SecTaskCopyValueForEntitlement(task, key, std::ptr::null_mut());
        CFRelease(task);
        if value.is_null() {
            return false;
        }
        let entitled = value == kCFBooleanTrue;
        CFRelease(value);
        entitled
    }
}

/// Makes sure this process has the virtualization entitlement: if it
/// hasn't, signs the executable and runs it again with the same arguments.
pub fn ensure_entitled() -> Result<()> {
    if entitled() {
        return Ok(());
    }
    let exe = std::env::current_exe()
        .and_then(fs::canonicalize)
        .context("finding the sendit executable")?;
    if std::env::var_os(SIGNED_ENV).is_some() {
        bail!(
            "{} is signed but still lacks the virtualization entitlement",
            exe.display()
        );
    }
    eprintln!(
        "sendit: signing {} with the virtualization entitlement",
        exe.display()
    );
    sign(&exe).with_context(|| format!("signing {}", exe.display()))?;
    let error = Command::new(&exe)
        .args(std::env::args_os().skip(1))
        .env(SIGNED_ENV, "1")
        .exec();
    Err(error).with_context(|| format!("running {}", exe.display()))
}

/// Signs a copy of `exe` and renames it over `exe`. Signing the file in
/// place would leave the kernel with the old signature cached for it, and
/// macOS kills processes whose executable doesn't match that.
fn sign(exe: &Path) -> Result<()> {
    let name = exe.file_name().context("executable has no file name")?;
    let pid = std::process::id();
    let copy = exe.with_file_name(format!(".{}.signing-{pid}", name.display()));
    let entitlements = std::env::temp_dir().join(format!("sendit-entitlements-{pid}.plist"));
    let result = (|| -> Result<()> {
        fs::write(&entitlements, ENTITLEMENTS)?;
        fs::copy(exe, &copy)?;
        util::run_output(
            Command::new("codesign")
                .args(["--sign", "-", "--force", "--entitlements"])
                .arg(&entitlements)
                .arg(&copy),
        )?;
        fs::rename(&copy, exe)?;
        Ok(())
    })();
    let _ = fs::remove_file(&entitlements);
    let _ = fs::remove_file(&copy);
    result
}
