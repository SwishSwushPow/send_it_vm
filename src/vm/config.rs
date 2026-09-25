//! Building the `VZVirtualMachineConfiguration` for a VM directory.

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

use super::{VmDir, VmSpec};
use crate::mounts::Share;
use crate::util::if_exists;

pub fn build(
    dir: &VmDir,
    spec: &VmSpec,
    serial_ports: &[&VZSerialPortAttachment],
) -> Result<Retained<VZVirtualMachineConfiguration>> {
    unsafe {
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
    unsafe {
        let bytes =
            if_exists(fs::read(path)).with_context(|| format!("reading {}", path.display()))?;
        if let Some(bytes) = bytes {
            return VZGenericMachineIdentifier::initWithDataRepresentation(
                VZGenericMachineIdentifier::alloc(),
                &NSData::with_bytes(&bytes),
            )
            .with_context(|| format!("invalid machine identifier in {}", path.display()));
        }
        let id = VZGenericMachineIdentifier::new();
        fs::write(path, id.dataRepresentation().to_vec())
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(id)
    }
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
    unsafe {
        let text = if_exists(fs::read_to_string(path))
            .with_context(|| format!("reading {}", path.display()))?;
        if let Some(text) = text {
            return VZMACAddress::initWithString(
                VZMACAddress::alloc(),
                &NSString::from_str(text.trim()),
            )
            .with_context(|| format!("invalid MAC address in {}", path.display()));
        }
        let mac = VZMACAddress::randomLocallyAdministeredAddress();
        fs::write(path, mac.string().to_string())
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(mac)
    }
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

pub fn ns_error(error: Retained<NSError>) -> anyhow::Error {
    anyhow::anyhow!("{}", error.localizedDescription())
}
