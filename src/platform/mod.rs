//! The system calls that differ between macOS, where sendit runs VMs, and
//! Linux, where it only builds for developing it (see .cargo/config.toml).
//! Both modules have the same functions, apart from what only the VM code,
//! which is macOS-only, uses.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;
