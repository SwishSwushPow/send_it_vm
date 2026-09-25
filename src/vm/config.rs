//! Building the `VZVirtualMachineConfiguration` for a VM directory.
//!
//! objc2-virtualization marks every method `unsafe`. Unless a `SAFETY`
//! comment says otherwise, the `unsafe` blocks here only call them with
//! valid, retained objects on the main thread.

use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr::NonNull;

use anyhow::{Context, Result, ensure};
use objc2::AllocAnyThread;
use objc2::rc::Retained;
use objc2_foundation::{NSArray, NSData, NSError, NSString, NSURL};
use objc2_virtualization::*;

use super::{VmDir, VmSpec, ns_message};
use crate::mounts::Share;
use crate::util::if_exists;

pub fn build(
    dir: &VmDir,
    spec: &VmSpec,
    serial_ports: &[&VZSerialPortAttachment],
) -> Result<Retained<VZVirtualMachineConfiguration>> {
    unsafe {
        // `Config::resolve` has checked these against the host already; the
        // framework's own limits are the ones that count, though.
        let min_cpus = VZVirtualMachineConfiguration::minimumAllowedCPUCount();
        let max_cpus = VZVirtualMachineConfiguration::maximumAllowedCPUCount();
        ensure!(
            (min_cpus..=max_cpus).contains(&(spec.cpus as usize)),
            "cpus must be between {min_cpus} and {max_cpus}, got {}",
            spec.cpus
        );
        let min_mem = VZVirtualMachineConfiguration::minimumAllowedMemorySize();
        let max_mem = VZVirtualMachineConfiguration::maximumAllowedMemorySize();
        ensure!(
            (min_mem..=max_mem).contains(&spec.memory.0),
            "memory must be between {} and {}, got {}",
            crate::config::ByteSize(min_mem),
            crate::config::ByteSize(max_mem),
            spec.memory
        );

        let config = VZVirtualMachineConfiguration::new();
        config.setCPUCount(spec.cpus as usize);
        config.setMemorySize(spec.memory.0);

        let platform = VZGenericPlatformConfiguration::new();
        let machine_id = machine_identifier(&dir.machine_id())?;
        platform.setMachineIdentifier(&machine_id);
        config.setPlatform(&platform);

        let boot_loader = VZEFIBootLoader::new();
        let variable_store = efi_variable_store(&dir.efi_vars())?;
        boot_loader.setVariableStore(Some(&variable_store));
        config.setBootLoader(Some(&boot_loader));

        // The root disk is /dev/vda; a seed disk, if any, is /dev/vdb.
        let mut disks = vec![block_device(&dir.disk(), false)?];
        if let Some(seed) = &spec.seed {
            disks.push(block_device(seed, true)?);
        }
        config.setStorageDevices(&NSArray::from_retained_slice(&disks));

        let shares = spec
            .shares
            .iter()
            .map(directory_share)
            .collect::<Result<Vec<_>>>()?;
        config.setDirectorySharingDevices(&NSArray::from_retained_slice(&shares));

        let network = VZVirtioNetworkDeviceConfiguration::new();
        network.setAttachment(Some(&VZNATNetworkDeviceAttachment::new()));
        let mac = mac_address(&dir.mac())?;
        network.setMACAddress(&mac);
        config.setNetworkDevices(&NSArray::from_retained_slice(&[Retained::into_super(
            network,
        )]));

        // Each port becomes its own virtio console: /dev/hvc0, /dev/hvc1, ...
        let serial_ports: Vec<Retained<VZSerialPortConfiguration>> = serial_ports
            .iter()
            .map(|attachment| {
                let port = VZVirtioConsoleDeviceSerialPortConfiguration::new();
                port.setAttachment(Some(attachment));
                Retained::into_super(port)
            })
            .collect();
        config.setSerialPorts(&NSArray::from_retained_slice(&serial_ports));

        config.setEntropyDevices(&NSArray::from_retained_slice(&[Retained::into_super(
            VZVirtioEntropyDeviceConfiguration::new(),
        )]));
        config.setMemoryBalloonDevices(&NSArray::from_retained_slice(&[Retained::into_super(
            VZVirtioTraditionalMemoryBalloonDeviceConfiguration::new(),
        )]));

        config
            .validateWithError()
            .map_err(ns_error)
            .context("invalid VM configuration")?;
        Ok(config)
    }
}

fn block_device(path: &Path, read_only: bool) -> Result<Retained<VZStorageDeviceConfiguration>> {
    unsafe {
        let attachment =
            VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_cachingMode_synchronizationMode_error(
                VZDiskImageStorageDeviceAttachment::alloc(),
                &*file_url(path)?,
                read_only,
                VZDiskImageCachingMode::Automatic,
                VZDiskImageSynchronizationMode::Full,
            )
            .map_err(ns_error)
            .with_context(|| format!("attaching disk {}", path.display()))?;
        let device = VZVirtioBlockDeviceConfiguration::initWithAttachment(
            VZVirtioBlockDeviceConfiguration::alloc(),
            &attachment,
        );
        Ok(Retained::into_super(device))
    }
}

fn directory_share(share: &Share) -> Result<Retained<VZDirectorySharingDeviceConfiguration>> {
    unsafe {
        let directory = VZSharedDirectory::initWithURL_readOnly(
            VZSharedDirectory::alloc(),
            &*file_url(&share.host)?,
            share.read_only,
        );
        let single =
            VZSingleDirectoryShare::initWithDirectory(VZSingleDirectoryShare::alloc(), &directory);
        let device = VZVirtioFileSystemDeviceConfiguration::initWithTag(
            VZVirtioFileSystemDeviceConfiguration::alloc(),
            &NSString::from_str(&share.tag),
        );
        device.setShare(Some(&single));
        Ok(Retained::into_super(device))
    }
}

/// Loads the VM's persistent machine identifier, creating it on first use.
fn machine_identifier(path: &Path) -> Result<Retained<VZGenericMachineIdentifier>> {
    load_or_create(
        path,
        "machine identifier",
        |bytes| unsafe {
            VZGenericMachineIdentifier::initWithDataRepresentation(
                VZGenericMachineIdentifier::alloc(),
                &NSData::with_bytes(bytes),
            )
        },
        || {
            let id = unsafe { VZGenericMachineIdentifier::new() };
            let bytes = unsafe { id.dataRepresentation() }.to_vec();
            (id, bytes)
        },
    )
}

fn efi_variable_store(path: &Path) -> Result<Retained<VZEFIVariableStore>> {
    unsafe {
        if path.exists() {
            return Ok(VZEFIVariableStore::initWithURL(
                VZEFIVariableStore::alloc(),
                &*file_url(path)?,
            ));
        }
        VZEFIVariableStore::initCreatingVariableStoreAtURL_options_error(
            VZEFIVariableStore::alloc(),
            &*file_url(path)?,
            VZEFIVariableStoreInitializationOptions::empty(),
        )
        .map_err(ns_error)
        .with_context(|| format!("creating EFI variable store {}", path.display()))
    }
}

/// Loads the VM's persistent MAC address, creating a random one on first use.
fn mac_address(path: &Path) -> Result<Retained<VZMACAddress>> {
    load_or_create(
        path,
        "MAC address",
        |bytes| {
            let text = std::str::from_utf8(bytes).ok()?;
            unsafe {
                VZMACAddress::initWithString(
                    VZMACAddress::alloc(),
                    &NSString::from_str(text.trim()),
                )
            }
        },
        || {
            let mac = unsafe { VZMACAddress::randomLocallyAdministeredAddress() };
            let text = unsafe { mac.string() }.to_string();
            (mac, text.into_bytes())
        },
    )
}

/// Loads a value the VM keeps across boots from `path` with `parse`, or
/// on first use makes one with `create` and saves its bytes there.
fn load_or_create<T>(
    path: &Path,
    what: &str,
    parse: impl FnOnce(&[u8]) -> Option<T>,
    create: impl FnOnce() -> (T, Vec<u8>),
) -> Result<T> {
    if let Some(bytes) =
        if_exists(fs::read(path)).with_context(|| format!("reading {}", path.display()))?
    {
        return parse(&bytes).with_context(|| format!("invalid {what} in {}", path.display()));
    }
    let (value, bytes) = create();
    fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(value)
}

/// A file URL for `path`, built from its bytes so that paths which aren't
/// valid UTF-8 still point at the right file.
fn file_url(path: &Path) -> Result<Retained<NSURL>> {
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

fn ns_error(error: Retained<NSError>) -> anyhow::Error {
    anyhow::anyhow!(ns_message(&error))
}
