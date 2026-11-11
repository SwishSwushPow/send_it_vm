//! sendit's internals, as a library for sendit-ui, the desktop app in
//! `sendit-ui/`, which reads VMs, images and the config file through it.
//! This is not a stable API: it changes with sendit, without notice.

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
compile_error!("sendit requires macOS on Apple silicon");

pub mod cli;
pub mod commands;
pub mod config;
pub mod image;
pub mod mounts;
pub mod network;
pub mod paths;
pub mod project_vm;
pub mod provision;
pub mod sign;
pub mod ssh_config;
pub mod util;
pub mod vm;
pub mod worktree;
