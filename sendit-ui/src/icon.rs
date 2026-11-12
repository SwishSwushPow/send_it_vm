//! The app icon, the S of the banner a VM shows when it starts, light or
//! dark as the app is. icon/make-icon.sh draws it.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, LazyLock};

use gpui_kit::{Image, ImageFormat};
use objc2::{AllocAnyThread, MainThreadMarker};
use objc2_app_kit::{NSApplication, NSImage};
use objc2_foundation::{NSBundle, NSData};

/// The icon's tile alone, for the sidebar, at 72 px for 36 points.
static SIDEBAR_LIGHT: LazyLock<Arc<Image>> =
    LazyLock::new(|| png(include_bytes!("../icon/sidebar-light.png")));
static SIDEBAR_DARK: LazyLock<Arc<Image>> =
    LazyLock::new(|| png(include_bytes!("../icon/sidebar-dark.png")));

fn png(bytes: &[u8]) -> Arc<Image> {
    Arc::new(Image::from_bytes(ImageFormat::Png, bytes.to_vec()))
}

/// The icon for the sidebar, in dark mode or light.
pub fn sidebar(dark: bool) -> Arc<Image> {
    if dark {
        SIDEBAR_DARK.clone()
    } else {
        SIDEBAR_LIGHT.clone()
    }
}

/// Shows the icon in the Dock, dark or light, when the app runs on its own,
/// e.g. with `cargo run`. In "Send it.app", macOS takes it from the bundle,
/// light, dark or tinted as set in System Settings, and setting it here
/// would keep macOS from doing so.
pub fn set_dock_icon(dark: bool) {
    // Bundled, the main bundle is the app's, with its identifier; on its
    // own, it is the binary's folder, without one.
    static BUNDLED: LazyLock<bool> =
        LazyLock::new(|| NSBundle::mainBundle().bundleIdentifier().is_some());
    // What was set last: 0 nothing, 1 light, 2 dark. Only the main thread
    // sets it.
    static SHOWN: AtomicU8 = AtomicU8::new(0);
    let wanted = if dark { 2 } else { 1 };
    if *BUNDLED || SHOWN.load(Ordering::Relaxed) == wanted {
        return;
    }
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let bytes: &[u8] = if dark {
        include_bytes!("../icon/dock-dark.png")
    } else {
        include_bytes!("../icon/dock-light.png")
    };
    let data = NSData::with_bytes(bytes);
    if let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) {
        let app = NSApplication::sharedApplication(mtm);
        // SAFETY: on the main thread, with an image that lives as long as
        // the call; AppKit retains it.
        unsafe { app.setApplicationIconImage(Some(&image)) };
        SHOWN.store(wanted, Ordering::Relaxed);
    }
}
