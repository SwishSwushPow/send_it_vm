//! Building the `VZVirtualMachineConfiguration` for a VM directory.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use objc2::AllocAnyThread;
use objc2::rc::Retained;
use objc2_foundation::{NSArray, NSData, NSError, NSString, NSURL};
use objc2_virtualization::*;

use super::{VmDir, VmSpec};

pub fn build(
    dir: &VmDir,
    spec: &VmSpec,
    console: &VZSerialPortAttachment,
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

        let disk = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_cachingMode_synchronizationMode_error(
            VZDiskImageStorageDeviceAttachment::alloc(),
            &file_url(&dir.disk()),
            false,
            VZDiskImageCachingMode::Automatic,
            VZDiskImageSynchronizationMode::Full,
        )
        .map_err(ns_error)
        .with_context(|| format!("attaching disk {}", dir.disk().display()))?;
        let block = VZVirtioBlockDeviceConfiguration::initWithAttachment(
            VZVirtioBlockDeviceConfiguration::alloc(),
            &disk,
        );
        config.setStorageDevices(&NSArray::from_retained_slice(&[Retained::into_super(
            block,
        )]));

        let network = VZVirtioNetworkDeviceConfiguration::new();
        network.setAttachment(Some(&VZNATNetworkDeviceAttachment::new()));
        let mac = mac_address(&dir.mac())?;
        network.setMACAddress(&mac);
        config.setNetworkDevices(&NSArray::from_retained_slice(&[Retained::into_super(
            network,
        )]));

        let serial = VZVirtioConsoleDeviceSerialPortConfiguration::new();
        serial.setAttachment(Some(console));
        config.setSerialPorts(&NSArray::from_retained_slice(&[Retained::into_super(
            serial,
        )]));

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

/// Loads the VM's persistent machine identifier, creating it on first use.
fn machine_identifier(path: &Path) -> Result<Retained<VZGenericMachineIdentifier>> {
    unsafe {
        if let Ok(bytes) = fs::read(path) {
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
                &file_url(path),
            ));
        }
        VZEFIVariableStore::initCreatingVariableStoreAtURL_options_error(
            VZEFIVariableStore::alloc(),
            &file_url(path),
            VZEFIVariableStoreInitializationOptions::empty(),
        )
        .map_err(ns_error)
        .with_context(|| format!("creating EFI variable store {}", path.display()))
    }
}

/// Loads the VM's persistent MAC address, creating a random one on first use.
fn mac_address(path: &Path) -> Result<Retained<VZMACAddress>> {
    unsafe {
        if let Ok(text) = fs::read_to_string(path) {
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

fn file_url(path: &Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()))
}

pub fn ns_error(error: Retained<NSError>) -> anyhow::Error {
    anyhow::anyhow!("{}", error.localizedDescription())
}
