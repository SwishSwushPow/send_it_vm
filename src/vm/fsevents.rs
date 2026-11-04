//! Watching a directory tree with FSEvents.
//!
//! The stream delivers its events on a private serial dispatch queue, so it
//! needs no run loop and never blocks the main one, which drives the VM.

use std::ffi::{CStr, OsStr, c_char, c_void};
use std::os::unix::ffi::OsStrExt;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::ptr;
use std::time::Duration;

use anyhow::{Result, ensure};

/// The event flags FSEvents reports for each path.
pub mod flags {
    pub const MUST_SCAN_SUB_DIRS: u32 = 0x1;
    pub const USER_DROPPED: u32 = 0x2;
    pub const KERNEL_DROPPED: u32 = 0x4;
    pub const ITEM_CREATED: u32 = 0x100;
    pub const ITEM_REMOVED: u32 = 0x200;
    pub const ITEM_INODE_META_MOD: u32 = 0x400;
    pub const ITEM_RENAMED: u32 = 0x800;
    pub const ITEM_MODIFIED: u32 = 0x1000;
    pub const ITEM_FINDER_INFO_MOD: u32 = 0x2000;
    pub const ITEM_CHANGE_OWNER: u32 = 0x4000;
    pub const ITEM_XATTR_MOD: u32 = 0x8000;
}

type CFTypeRef = *const c_void;
type CFIndex = isize;
type FSEventStreamRef = *mut c_void;
type DispatchQueue = *mut c_void;

/// Called by FSEvents with `num_events` paths and their flags.
type FSEventStreamCallback = extern "C" fn(
    stream: FSEventStreamRef,
    info: *mut c_void,
    num_events: usize,
    paths: *mut c_void,
    flags: *const u32,
    ids: *const u64,
);

#[repr(C)]
struct FSEventStreamContext {
    version: CFIndex,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
}

/// Opaque: only its address is passed on.
#[repr(C)]
struct CFArrayCallBacks {
    _private: [u8; 0],
}

const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const K_FS_EVENT_STREAM_EVENT_ID_SINCE_NOW: u64 = u64::MAX;
const K_FS_EVENT_STREAM_CREATE_FLAG_NO_DEFER: u32 = 0x2;
const K_FS_EVENT_STREAM_CREATE_FLAG_FILE_EVENTS: u32 = 0x10;

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventStreamCreate(
        allocator: CFTypeRef,
        callback: FSEventStreamCallback,
        context: *const FSEventStreamContext,
        paths_to_watch: CFTypeRef,
        since_when: u64,
        latency: f64,
        flags: u32,
    ) -> FSEventStreamRef;
    fn FSEventStreamSetDispatchQueue(stream: FSEventStreamRef, queue: DispatchQueue);
    fn FSEventStreamStart(stream: FSEventStreamRef) -> u8;
    fn FSEventStreamStop(stream: FSEventStreamRef);
    fn FSEventStreamInvalidate(stream: FSEventStreamRef);
    fn FSEventStreamRelease(stream: FSEventStreamRef);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeArrayCallBacks: CFArrayCallBacks;
    fn CFStringCreateWithBytes(
        allocator: CFTypeRef,
        bytes: *const u8,
        len: CFIndex,
        encoding: u32,
        external: u8,
    ) -> CFTypeRef;
    fn CFArrayCreate(
        allocator: CFTypeRef,
        values: *const CFTypeRef,
        len: CFIndex,
        callbacks: *const CFArrayCallBacks,
    ) -> CFTypeRef;
    fn CFRelease(object: CFTypeRef);
}

// In libSystem, which is always linked.
unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> DispatchQueue;
    fn dispatch_sync_f(
        queue: DispatchQueue,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
    fn dispatch_release(object: DispatchQueue);
}

/// The callback `watch` calls with each batch of paths and their flags.
type OnEvents = Box<dyn Fn(Vec<(PathBuf, u32)>) + Send>;

/// A running FSEvents stream. Dropping it stops the stream, and once it has
/// returned, `on_events` isn't running and won't be called again.
pub struct EventStream {
    stream: FSEventStreamRef,
    queue: DispatchQueue,
    /// Whether FSEventStreamStart succeeded, so `drop` has to stop it.
    started: bool,
    /// The stream's `info`: a `Box<OnEvents>`, freed in `drop`.
    on_events: *mut OnEvents,
}

// SAFETY: neither FSEvents nor dispatch tie a stream or a queue to the
// thread that created it, and `EventStream` has no methods besides `drop`,
// so it is never used from two threads at once (it isn't `Sync`).
// `on_events` is `Send`: it only runs on the serial queue, one batch at a
// time, and `drop` frees it only after the queue has finished with it.
unsafe impl Send for EventStream {}

impl EventStream {
    /// Watches `root` (canonical path) and calls `on_events` with each
    /// batch FSEvents delivers, on a private serial dispatch queue.
    ///
    /// `on_events` must not block, and it must not drop the stream: `drop`
    /// waits for the queue it runs on.
    pub fn watch(root: &Path, latency: Duration, on_events: OnEvents) -> Result<EventStream> {
        let bytes = root.as_os_str().as_bytes();
        // SAFETY: `bytes` is valid for reads of its length. Both objects are
        // released here; the array retains the string, and the stream copies
        // the paths it is given.
        let paths = unsafe {
            let path = CFStringCreateWithBytes(
                ptr::null(),
                bytes.as_ptr(),
                bytes.len() as CFIndex,
                K_CF_STRING_ENCODING_UTF8,
                0,
            );
            ensure!(!path.is_null(), "{} isn't valid UTF-8", root.display());
            let paths = CFArrayCreate(ptr::null(), &path, 1, &raw const kCFTypeArrayCallBacks);
            CFRelease(path);
            paths
        };
        ensure!(
            !paths.is_null(),
            "couldn't create the list of paths to watch"
        );

        // SAFETY: a null attribute makes a serial queue; the label is copied.
        let queue = unsafe { dispatch_queue_create(c"sendit.fsevents".as_ptr(), ptr::null()) };
        if queue.is_null() {
            // SAFETY: `paths` is no longer needed.
            unsafe { CFRelease(paths) };
            anyhow::bail!("couldn't create a dispatch queue");
        }

        let on_events = Box::into_raw(Box::new(on_events));
        let context = FSEventStreamContext {
            version: 0,
            info: on_events.cast(),
            retain: ptr::null(),
            release: ptr::null(),
            copy_description: ptr::null(),
        };
        // SAFETY: the context is copied by FSEventStreamCreate, and its
        // `info` stays valid until `drop` (or the error path below) frees it.
        let stream = unsafe {
            let stream = FSEventStreamCreate(
                ptr::null(),
                on_fs_events,
                &context,
                paths,
                K_FS_EVENT_STREAM_EVENT_ID_SINCE_NOW,
                latency.as_secs_f64(),
                K_FS_EVENT_STREAM_CREATE_FLAG_NO_DEFER | K_FS_EVENT_STREAM_CREATE_FLAG_FILE_EVENTS,
            );
            CFRelease(paths);
            stream
        };
        if stream.is_null() {
            // SAFETY: nothing refers to the callback or the queue.
            unsafe {
                drop(Box::from_raw(on_events));
                dispatch_release(queue);
            }
            anyhow::bail!("couldn't create an FSEvents stream for {}", root.display());
        }

        // SAFETY: `stream` and `queue` are valid; the stream retains the
        // queue.
        unsafe { FSEventStreamSetDispatchQueue(stream, queue) };
        // From here on, `drop` cleans up after an error.
        let mut events = EventStream {
            stream,
            queue,
            started: false,
            on_events,
        };
        // SAFETY: `stream` is valid and scheduled on `queue`.
        events.started = unsafe { FSEventStreamStart(stream) } != 0;
        ensure!(
            events.started,
            "couldn't start watching {} with FSEvents",
            root.display()
        );
        Ok(events)
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        // SAFETY: `stream`, `queue` and `on_events` are valid and owned by
        // this `EventStream`.
        //
        // FSEvents doesn't call the callback after FSEventStreamStop, but it
        // may be running it on the queue right now. The queue is serial, so
        // an empty function run synchronously on it returns only once that
        // call (and any other one queued before it) has finished. Only then
        // is `on_events` freed. This is simpler than the context's
        // retain/release callbacks, which FSEvents doesn't document as
        // covering a callback in progress.
        unsafe {
            if self.started {
                FSEventStreamStop(self.stream);
            }
            FSEventStreamInvalidate(self.stream);
            dispatch_sync_f(self.queue, ptr::null_mut(), nothing);
            FSEventStreamRelease(self.stream);
            drop(Box::from_raw(self.on_events));
            dispatch_release(self.queue);
        }
    }
}

extern "C" fn nothing(_: *mut c_void) {}

extern "C" fn on_fs_events(
    _stream: FSEventStreamRef,
    info: *mut c_void,
    num_events: usize,
    paths: *mut c_void,
    flags: *const u32,
    _ids: *const u64,
) {
    if num_events == 0 {
        return;
    }
    // SAFETY: without kFSEventStreamCreateFlagUseCFTypes, `paths` is an
    // array of `num_events` C strings and `flags` one of as many flags, valid
    // for this call. `info` is the `Box<OnEvents>` from `watch`, which `drop`
    // keeps alive until this call has returned.
    let (on_events, events) = unsafe {
        let paths = std::slice::from_raw_parts(paths as *const *const c_char, num_events);
        let flags = std::slice::from_raw_parts(flags, num_events);
        let events = paths
            .iter()
            .zip(flags)
            .map(|(&path, &flags)| {
                let path = OsStr::from_bytes(CStr::from_ptr(path).to_bytes());
                (PathBuf::from(path), flags)
            })
            .collect();
        (&*(info as *const OnEvents), events)
    };
    // A panic must not unwind into FSEvents, which would abort the process
    // and leave the terminal in raw mode. The panic hook has reported it.
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| on_events(events)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::TempDir;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
    use std::time::Instant;

    type Events = Receiver<Vec<(PathBuf, u32)>>;

    /// Waits up to five seconds for an event on `path` with `flag` set.
    fn wait_for(events: &Events, path: &Path, flag: u32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            let Ok(batch) = events.recv_timeout(left) else {
                return false;
            };
            if batch.iter().any(|(p, f)| p == path && f & flag != 0) {
                return true;
            }
        }
        false
    }

    #[test]
    fn reports_created_files() {
        let temp = TempDir::new("fsevents-created");
        let (tx, events) = mpsc::channel();
        let _stream = EventStream::watch(
            temp.path(),
            Duration::from_millis(50),
            Box::new(move |batch| {
                let _ = tx.send(batch);
            }),
        )
        .unwrap();

        std::fs::write(temp.path().join("a.txt"), b"a").unwrap();
        // FSEvents reports real paths, which the canonical temp dir is.
        assert!(wait_for(
            &events,
            &temp.path().join("a.txt"),
            flags::ITEM_CREATED
        ));
    }

    #[test]
    fn stops_on_drop() {
        let temp = TempDir::new("fsevents-drop");
        let (tx, events) = mpsc::channel();
        // Kept here so the channel stays open after the stream is gone.
        let sender = tx.clone();
        let alive = Arc::new(AtomicUsize::new(0));
        let in_callback = alive.clone();
        let stream = EventStream::watch(
            temp.path(),
            Duration::from_millis(50),
            Box::new(move |batch| {
                in_callback.fetch_add(1, Ordering::Relaxed);
                let _ = tx.send(batch);
            }),
        )
        .unwrap();

        // The stream works before it is dropped.
        std::fs::write(temp.path().join("before.txt"), b"b").unwrap();
        assert!(wait_for(
            &events,
            &temp.path().join("before.txt"),
            flags::ITEM_CREATED
        ));

        drop(stream);
        // The callback was freed along with the stream.
        assert_eq!(Arc::strong_count(&alive), 1);
        while events.try_recv().is_ok() {}
        std::fs::write(temp.path().join("after.txt"), b"a").unwrap();
        assert_eq!(
            events.recv_timeout(Duration::from_secs(1)),
            Err(RecvTimeoutError::Timeout)
        );
        drop(sender);
    }
}
