//! sendit's internals, as a library for sendit-ui, the desktop app in
//! `sendit-ui/`, which reads VMs, images and the config file through it.
//! This is not a stable API: it changes with sendit, without notice.
//!
//! Only what reads, plus the few helpers the app shares, is exported. The
//! rest, e.g. running a VM or writing the ssh config, re-runs or names
//! the binary it is in, which is only right in sendit's own: the app runs
//! `sendit` for those.

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
compile_error!("sendit requires macOS on Apple silicon");

mod cli;
mod commands;
mod config;
mod dispatch;
mod image;
mod mounts;
mod network;
mod paths;
mod project_vm;
mod provision;
mod sign;
mod ssh_config;
mod util;
mod vm;
mod worktree;

pub use cli::{Cli, Command};
pub use commands::{
    Activity, ImageRow, ParentVm, ProjectDir, PrunePlan, PruneScope, PruneVm, SkipReason, Skipped,
    Stale, StatusReport, VmInfo, VmRow, VmState, VmStatus, image_rows, list_rows, prune_plan,
    status_report,
};
pub use config::{ByteSize, Config, MountSpec, Resources, Settings};
pub use network::{Allow, Mode};
pub use paths::{ImageName, Paths, Project, VmName};
pub use project_vm::find;
pub use provision::BaseState;
pub use ssh_config::{HOST_PREFIX, is_set_up as ssh_is_set_up};
pub use util::{canonical, read_toml, write_atomic};

/// sendit's command line, which its binary runs; nothing else should, as
/// it re-runs the binary it is in.
#[doc(hidden)]
pub fn cli_main() -> std::process::ExitCode {
    dispatch::main()
}
