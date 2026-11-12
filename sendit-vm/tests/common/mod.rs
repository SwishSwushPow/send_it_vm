//! Helpers that the CLI and VM tests share.

use std::path::Path;
use std::process::{Command, Output};

pub const SENDIT: &str = env!("CARGO_BIN_EXE_sendit");

/// `program`, sendit or a copy of it, with `home` as its home and the VMs
/// in `home/.sendit`, whatever the environment says.
pub fn command(program: &Path, home: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.env("HOME", home).env_remove("SENDIT_VM_DIR");
    cmd
}

/// `sendit` with `home` as its home, as in `command`.
pub fn sendit(home: &Path) -> Command {
    command(Path::new(SENDIT), home)
}

pub fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Fails unless `output` failed with `message` in its stderr.
pub fn assert_fails_with(output: &Output, message: &str) {
    let stderr = text(&output.stderr);
    assert!(
        !output.status.success() && stderr.contains(message),
        "expected an error saying {message:?}, got {}:\n{stderr}",
        output.status
    );
}
