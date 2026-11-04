//! Forwarding the Mac's file changes to the project into the guest.
//!
//! Virtiofs doesn't turn the Mac's writes into inotify events in the guest,
//! so watchers there would never see them. FSEvents reports them here; a
//! thread turns them into records and sends them over a fourth virtio
//! console port, /dev/hvc3 in the guest. There a service pipes them into the
//! sendit-notify kernel module, which emits them as fsnotify events.
//!
//! Each record ends with a NUL byte: an ASCII header, then the guest path as
//! raw bytes, so any file name passes. `T <host realtime ns>` gives the
//! host's clock; `c`, `m`, `a` and `r` followed by the mtime in ns and the
//! path report a file created, modified, changed in its metadata only, or
//! removed. The module compares mtimes and the clock with the guest's own
//! recent writes, to drop the Mac's echo of those.
//!
//! Nothing is sent until the guest writes `?`, which it does once the module
//! accepts records: an older image has nobody reading the port.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::Metadata;
use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use objc2::rc::Retained;
use objc2_virtualization::{VZFileHandleSerialPortAttachment, VZSerialPortAttachment};

use super::console::{file_handle, pipe, poll, pollfd, read, serial_port, write};
use super::fsevents::{EventStream, flags};

/// How long FSEvents collects events before it delivers them.
const LATENCY: Duration = Duration::from_millis(50);

/// How long after the first unsent event a batch is sent, so that a burst
/// (a branch switch, a build) goes out in few batches.
const DEBOUNCE: Duration = Duration::from_millis(100);

/// How long a write may make no progress before the rest of the batch is
/// dropped: the guest isn't reading.
const WRITE_TIMEOUT: Duration = Duration::from_secs(1);

/// The most directory entries one batch walks after FSEvents dropped
/// events, so a huge tree doesn't stall forwarding.
const WALK_LIMIT: usize = 20_000;

/// How many records follow one clock record. The module's echo suppression
/// compares against the clock, and the guest takes a while for a large
/// batch, so the clock is sent again in between.
const RECORDS_PER_CLOCK: usize = 1000;

/// How long to wait before sending what is in a directory just created. A
/// recursive watcher (`inotifywait -r`, Rust's `notify`) adds a watch on a
/// new directory only once it has heard of it, and doesn't report what was
/// in it by then: records that came right behind the directory's would be
/// lost to it.
const NEW_DIRECTORY_PAUSE: Duration = Duration::from_millis(100);

/// Flags that only report a change of metadata.
const METADATA_FLAGS: u32 = flags::ITEM_INODE_META_MOD
    | flags::ITEM_XATTR_MOD
    | flags::ITEM_FINDER_INFO_MOD
    | flags::ITEM_CHANGE_OWNER;

/// Flags that say FSEvents lost events under a path, which is then walked.
const SCAN_FLAGS: u32 = flags::MUST_SCAN_SUB_DIRS | flags::USER_DROPPED | flags::KERNEL_DROPPED;

/// The project directory to forward changes from.
#[derive(Clone, Debug)]
pub struct Forward {
    /// Canonical, as FSEvents reports real paths.
    pub host_root: PathBuf,
    pub guest_root: PathBuf,
    /// Leave out `<project>/.git` and everything under it.
    pub hide_git: bool,
}

/// Paths with changes not sent yet, and their flags OR-ed together.
type Pending = Arc<Mutex<HashMap<PathBuf, u32>>>;

/// The serial port that carries changes, and what feeds it.
pub struct Forwarder {
    port: Retained<VZFileHandleSerialPortAttachment>,
    /// Closing it ends the thread.
    stop: Option<OwnedFd>,
    thread: Option<JoinHandle<()>>,
    events: Option<EventStream>,
}

impl Forwarder {
    /// Watches `forward.host_root` and starts the thread that sends its
    /// changes to the port once the guest is ready.
    pub fn start(forward: &Forward) -> Result<Forwarder> {
        let (guest_reads, to_guest) = pipe()?;
        let (from_guest, guest_writes) = pipe()?;
        // A guest that stops reading must not block the thread.
        set_nonblocking(&to_guest)?;
        let (wake, wake_write) = pipe()?;
        // Nor the FSEvents callback: one pending wakeup is as good as many.
        set_nonblocking(&wake_write)?;
        let (stop_read, stop) = pipe()?;

        let pending = Pending::default();
        let collected = pending.clone();
        // Kept short: dropping the stream waits for it to return.
        let events = EventStream::watch(
            &forward.host_root,
            LATENCY,
            Box::new(move |events| {
                {
                    let mut collected = collected.lock().unwrap_or_else(PoisonError::into_inner);
                    for (path, flags) in events {
                        *collected.entry(path).or_default() |= flags;
                    }
                }
                let _ = write(wake_write.as_raw_fd(), &[0]);
            }),
        )
        .with_context(|| format!("watching {}", forward.host_root.display()))?;

        let channel = Channel {
            to_guest,
            from_guest,
            wake,
            stop: stop_read,
        };
        let forward = forward.clone();
        let thread = std::thread::Builder::new()
            .name("sendit-changes".into())
            .spawn(move || forward_changes(&forward, &pending, channel))
            .context("starting the change forwarder")?;
        Ok(Forwarder {
            port: serial_port(Some(&file_handle(guest_reads)), &file_handle(guest_writes)),
            stop: Some(stop),
            thread: Some(thread),
            events: Some(events),
        })
    }

    /// The attachment for the guest's /dev/hvc3.
    pub fn port(&self) -> &VZSerialPortAttachment {
        &self.port
    }
}

impl Drop for Forwarder {
    /// Ends the thread, which never blocks on the guest for long, then
    /// stops watching. Until then, the FSEvents callback may write to the
    /// wake pipe the thread has closed, which fails harmlessly: Rust
    /// programs ignore SIGPIPE.
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        drop(self.events.take());
    }
}

fn set_nonblocking(fd: &OwnedFd) -> io::Result<()> {
    // SAFETY: plain fcntl calls on a descriptor we own.
    unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// The forwarder thread's descriptors.
struct Channel {
    /// Non-blocking.
    to_guest: OwnedFd,
    from_guest: OwnedFd,
    /// Readable once the FSEvents callback has added to the pending paths.
    wake: OwnedFd,
    /// Readable (closed) when the thread is to end.
    stop: OwnedFd,
}

/// Sends the changes in `pending` to the guest, `DEBOUNCE` after the first
/// one, once the guest has written `?`; until then they are dropped. Ends
/// when `channel.stop` closes.
fn forward_changes(forward: &Forward, pending: &Pending, channel: Channel) {
    let mut fds = [
        pollfd(channel.stop.as_raw_fd()),
        pollfd(channel.wake.as_raw_fd()),
        pollfd(channel.from_guest.as_raw_fd()),
    ];
    let mut buf = [0u8; 64];
    let mut ready = false;
    let mut first_unsent: Option<Instant> = None;
    loop {
        let timeout = first_unsent.map_or(-1, |first| {
            let left = (first + DEBOUNCE).saturating_duration_since(Instant::now());
            // Rounded up, so it doesn't wake up just before the time.
            left.as_micros()
                .div_ceil(1000)
                .try_into()
                .unwrap_or(libc::c_int::MAX)
        });
        // Stopping first: the VM is going away.
        if poll(&mut fds, timeout).is_err() || fds[0].revents != 0 {
            return;
        }
        for pollfd in &mut fds[1..] {
            if pollfd.revents == 0 {
                continue;
            }
            match read(pollfd.fd, &mut buf) {
                // poll() skips negative descriptors.
                Ok(0) | Err(_) => pollfd.fd = -1,
                Ok(_) if pollfd.fd == channel.wake.as_raw_fd() => {
                    first_unsent.get_or_insert_with(Instant::now);
                }
                // A later `?` comes from a restarted service, which reads
                // records again.
                Ok(n) => ready |= buf[..n].contains(&b'?'),
            }
        }
        let Some(first) = first_unsent else {
            continue;
        };
        if first.elapsed() < DEBOUNCE {
            continue;
        }
        first_unsent = None;
        let changes = std::mem::take(&mut *pending.lock().unwrap_or_else(PoisonError::into_inner));
        if !ready {
            continue;
        }
        for chunk in records(forward, changes) {
            if chunk.wait && wait(channel.stop.as_fd(), NEW_DIRECTORY_PAUSE) {
                return;
            }
            // An empty record first, which the module skips: it ends what
            // is left of a record cut off when an earlier batch was
            // dropped, or when the guest's reader restarted, so that it
            // doesn't swallow the clock. The clock comes as close to the
            // guest reading it as possible.
            let mut data = Vec::with_capacity(chunk.data.len() + 32);
            data.push(0);
            encode_clock(realtime_ns(), &mut data);
            data.extend_from_slice(&chunk.data);
            match write_batch(channel.to_guest.as_fd(), channel.stop.as_fd(), &data) {
                WriteOutcome::Written => {}
                WriteOutcome::Dropped => break,
                WriteOutcome::Stopped => return,
            }
        }
    }
}

/// The records for `changes`, in chunks of up to `RECORDS_PER_CLOCK`. Each
/// path is classified by what it is now, not by the flags alone, which
/// FSEvents coalesces. Paths are sorted, so a new directory comes before
/// what was created in it, and what is in a new directory goes in later
/// chunks, one level of new directories at a time.
fn records(forward: &Forward, changes: HashMap<PathBuf, u32>) -> Vec<Chunk> {
    let mut changes: Vec<_> = changes.into_iter().collect();
    changes
        .sort_unstable_by(|(a, _), (b, _)| a.as_os_str().as_bytes().cmp(b.as_os_str().as_bytes()));
    let mut found = Vec::new();
    let mut walk_budget = WALK_LIMIT;
    for (path, flags) in changes {
        if flags & SCAN_FLAGS != 0 {
            // FSEvents may name an ancestor of the root.
            let under = if forward.host_root.starts_with(&path) {
                Some(forward.host_root.clone())
            } else {
                guest_path(forward, &path).map(|_| path.clone())
            };
            if let Some(dir) = under {
                walk(forward, &dir, &mut walk_budget, &mut found);
            }
        }
        let Some(guest) = guest_path(forward, &path) else {
            continue;
        };
        let meta = std::fs::symlink_metadata(&path).ok();
        if let Some(op) = classify(flags, meta.as_ref()) {
            let new_dir = op == Op::Created && meta.as_ref().is_some_and(Metadata::is_dir);
            found.push(Found {
                op,
                mtime_ns: meta.as_ref().map_or(0, mtime_ns),
                guest,
                new_dir,
            });
        }
    }

    let new_dirs: HashSet<PathBuf> = (found.iter())
        .filter(|found| found.new_dir)
        .map(|found| found.guest.clone())
        .collect();
    let mut levelled: Vec<_> = found
        .into_iter()
        .map(|found| {
            let level = (found.guest.ancestors().skip(1))
                .filter(|dir| new_dirs.contains(*dir))
                .count();
            (level, found)
        })
        .collect();
    // Stable: paths stay sorted within a level.
    levelled.sort_by_key(|(level, _)| *level);
    let mut records = Records::default();
    for (level, found) in levelled {
        records.push(level, found.op, found.mtime_ns, &found.guest);
    }
    records.chunks
}

/// A record to send.
struct Found {
    op: Op,
    mtime_ns: i64,
    guest: PathBuf,
    /// A directory created since the last batch.
    new_dir: bool,
}

/// Adds a `Modified` record for each regular file under `dir`, without
/// following symlinks, until `budget` entries have been seen.
fn walk(forward: &Forward, dir: &Path, budget: &mut usize, found: &mut Vec<Found>) {
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if *budget == 0 {
                return;
            }
            *budget -= 1;
            let path = entry.path();
            // Also skips a hidden .git.
            let Some(guest) = guest_path(forward, &path) else {
                continue;
            };
            // Neither of these follows a symlink.
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                dirs.push(path);
            } else if file_type.is_file()
                && let Ok(meta) = entry.metadata()
            {
                found.push(Found {
                    op: Op::Modified,
                    mtime_ns: mtime_ns(&meta),
                    guest,
                    new_dir: false,
                });
            }
        }
    }
}

/// Records sent together, after the clock.
struct Chunk {
    /// Sent `NEW_DIRECTORY_PAUSE` after the chunk before it.
    wait: bool,
    data: Vec<u8>,
}

/// Encoded records, in chunks of up to `RECORDS_PER_CLOCK`, and a new one
/// for each level of new directories.
#[derive(Default)]
struct Records {
    chunks: Vec<Chunk>,
    /// How many records the last chunk holds.
    in_last: usize,
    level: usize,
}

impl Records {
    fn push(&mut self, level: usize, op: Op, mtime_ns: i64, guest: &Path) {
        let deeper = level > self.level;
        if self.chunks.is_empty() || self.in_last == RECORDS_PER_CLOCK || deeper {
            self.chunks.push(Chunk {
                wait: deeper,
                data: Vec::new(),
            });
            self.in_last = 0;
            self.level = level;
        }
        let last = self.chunks.last_mut().expect("a chunk was just added");
        encode(op, mtime_ns, guest, &mut last.data);
        self.in_last += 1;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Created,
    Modified,
    Attrib,
    Removed,
}

/// What to report for a path with FSEvents `flags` that is now as `meta`
/// says, or gone. Nothing for a directory that only changed (its entries
/// report their own changes) or for flags that report nothing.
fn classify(flags: u32, meta: Option<&Metadata>) -> Option<Op> {
    let Some(meta) = meta else {
        return Some(Op::Removed);
    };
    // Reported as removed but there: replaced, as when an editor saves by
    // renaming a new file over the old one.
    if flags & (flags::ITEM_CREATED | flags::ITEM_RENAMED | flags::ITEM_REMOVED) != 0 {
        Some(Op::Created)
    } else if meta.is_dir() {
        None
    } else if flags & flags::ITEM_MODIFIED != 0 {
        Some(Op::Modified)
    } else if flags & METADATA_FLAGS != 0 {
        Some(Op::Attrib)
    } else {
        None
    }
}

/// Where `host` is in the guest, if it is below the project root and not
/// left out.
fn guest_path(forward: &Forward, host: &Path) -> Option<PathBuf> {
    // Bytes, not components: FSEvents' paths are normalized already, and
    // Path::strip_prefix would also take "/private/tmp/p/" for the root.
    let rest = (host.as_os_str().as_bytes())
        .strip_prefix(forward.host_root.as_os_str().as_bytes())?
        .strip_prefix(b"/")?;
    let git = rest == b".git" || rest.starts_with(b".git/");
    if rest.is_empty() || (forward.hide_git && git) {
        return None;
    }
    Some(forward.guest_root.join(OsStr::from_bytes(rest)))
}

/// Appends the record for `op` on `guest`.
fn encode(op: Op, mtime_ns: i64, guest: &Path, out: &mut Vec<u8>) {
    let (code, mtime_ns) = match op {
        Op::Created => ('c', mtime_ns),
        Op::Modified => ('m', mtime_ns),
        Op::Attrib => ('a', mtime_ns),
        Op::Removed => ('r', 0),
    };
    let _ = write!(out, "{code} {mtime_ns} ");
    out.extend_from_slice(guest.as_os_str().as_bytes());
    out.push(0);
}

/// Appends the record that gives the host's clock.
fn encode_clock(host_now_ns: i64, out: &mut Vec<u8>) {
    let _ = write!(out, "T {host_now_ns}\0");
}

fn mtime_ns(meta: &Metadata) -> i64 {
    meta.mtime()
        .saturating_mul(1_000_000_000)
        .saturating_add(meta.mtime_nsec())
}

fn realtime_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos().try_into().unwrap_or(i64::MAX))
}

/// Waits for `pause`; returns whether `stop` closed meanwhile.
fn wait(stop: BorrowedFd, pause: Duration) -> bool {
    let mut fds = [pollfd(stop.as_raw_fd())];
    let timeout = pause.as_millis().try_into().unwrap_or(libc::c_int::MAX);
    poll(&mut fds, timeout).is_err() || fds[0].revents != 0
}

#[derive(Debug, PartialEq, Eq)]
enum WriteOutcome {
    Written,
    /// The guest made no room for `WRITE_TIMEOUT`; the rest wasn't sent.
    Dropped,
    /// `stop` closed.
    Stopped,
}

/// Writes `data` to the non-blocking `to_guest`, unless the guest stops
/// reading or `stop` closes first.
fn write_batch(to_guest: BorrowedFd, stop: BorrowedFd, mut data: &[u8]) -> WriteOutcome {
    let mut progress = Instant::now();
    while !data.is_empty() {
        match write(to_guest.as_raw_fd(), data) {
            Ok(n) => {
                data = &data[n..];
                progress = Instant::now();
                continue;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => return WriteOutcome::Dropped,
        }
        let Some(left) = WRITE_TIMEOUT.checked_sub(progress.elapsed()) else {
            return WriteOutcome::Dropped;
        };
        let mut fds = [pollfd(stop.as_raw_fd()), pollfd(to_guest.as_raw_fd())];
        fds[1].events = libc::POLLOUT;
        let timeout = left
            .as_micros()
            .div_ceil(1000)
            .try_into()
            .unwrap_or(libc::c_int::MAX);
        if poll(&mut fds, timeout).is_err() || fds[0].revents != 0 {
            return WriteOutcome::Stopped;
        }
    }
    WriteOutcome::Written
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::TempDir;

    fn forward(hide_git: bool) -> Forward {
        Forward {
            host_root: "/private/tmp/p".into(),
            guest_root: "/home/dev/p".into(),
            hide_git,
        }
    }

    #[test]
    fn classifies_flags() {
        let temp = TempDir::new("changes-classify");
        let file = temp.path().join("a.txt");
        std::fs::write(&file, b"a").unwrap();
        let file = std::fs::symlink_metadata(&file).unwrap();
        let dir = std::fs::symlink_metadata(temp.path()).unwrap();
        let (file, dir) = (Some(&file), Some(&dir));

        assert_eq!(classify(flags::ITEM_REMOVED, None), Some(Op::Removed));
        assert_eq!(classify(flags::ITEM_CREATED, None), Some(Op::Removed));
        assert_eq!(classify(flags::ITEM_CREATED, file), Some(Op::Created));
        assert_eq!(classify(flags::ITEM_RENAMED, file), Some(Op::Created));
        // Replaced, e.g. by an editor's rename over it.
        assert_eq!(classify(flags::ITEM_REMOVED, file), Some(Op::Created));
        // Removed and created again within one batch.
        let replaced = flags::ITEM_REMOVED | flags::ITEM_CREATED | flags::ITEM_MODIFIED;
        assert_eq!(classify(replaced, file), Some(Op::Created));
        assert_eq!(classify(flags::ITEM_MODIFIED, file), Some(Op::Modified));
        let both = flags::ITEM_MODIFIED | flags::ITEM_INODE_META_MOD;
        assert_eq!(classify(both, file), Some(Op::Modified));
        for flag in [
            flags::ITEM_INODE_META_MOD,
            flags::ITEM_XATTR_MOD,
            flags::ITEM_FINDER_INFO_MOD,
            flags::ITEM_CHANGE_OWNER,
        ] {
            assert_eq!(classify(flag, file), Some(Op::Attrib), "{flag:#x}");
        }
        assert_eq!(classify(flags::ITEM_CREATED, dir), Some(Op::Created));
        let dir_flags = flags::ITEM_MODIFIED | flags::ITEM_INODE_META_MOD;
        assert_eq!(classify(dir_flags, dir), None);
        assert_eq!(classify(flags::MUST_SCAN_SUB_DIRS, dir), None);
    }

    #[test]
    fn translates_canonical_paths() {
        let hidden = forward(true);
        assert_eq!(
            guest_path(&hidden, Path::new("/private/tmp/p/src/a.rs")),
            Some(PathBuf::from("/home/dev/p/src/a.rs"))
        );
        assert_eq!(guest_path(&hidden, Path::new("/private/tmp/p")), None);
        assert_eq!(guest_path(&hidden, Path::new("/private/tmp/pq/a")), None);
        assert_eq!(guest_path(&hidden, Path::new("/private/tmp")), None);
        assert_eq!(guest_path(&hidden, Path::new("/private/tmp/p/.git")), None);
        assert_eq!(
            guest_path(&hidden, Path::new("/private/tmp/p/.git/HEAD")),
            None
        );
        // Only the project's own .git is hidden.
        assert_eq!(
            guest_path(&hidden, Path::new("/private/tmp/p/.github/ci.yml")),
            Some(PathBuf::from("/home/dev/p/.github/ci.yml"))
        );
        assert_eq!(
            guest_path(&forward(false), Path::new("/private/tmp/p/.git/HEAD")),
            Some(PathBuf::from("/home/dev/p/.git/HEAD"))
        );
        let odd = OsStr::from_bytes(b"/private/tmp/p/\xff.rs");
        assert_eq!(
            guest_path(&hidden, Path::new(odd)),
            Some(PathBuf::from(OsStr::from_bytes(b"/home/dev/p/\xff.rs")))
        );
    }

    #[test]
    fn encodes_records() {
        let mut out = Vec::new();
        encode(
            Op::Created,
            12,
            Path::new("/home/dev/p/odd name\n.txt"),
            &mut out,
        );
        assert_eq!(out, b"c 12 /home/dev/p/odd name\n.txt\0");

        let mut out = Vec::new();
        encode(Op::Modified, 3, Path::new("/home/dev/p/a"), &mut out);
        encode(Op::Attrib, 4, Path::new("/home/dev/p/b"), &mut out);
        encode(Op::Removed, 5, Path::new("/home/dev/p/c"), &mut out);
        assert_eq!(
            out,
            b"m 3 /home/dev/p/a\0a 4 /home/dev/p/b\0r 0 /home/dev/p/c\0"
        );

        let mut out = Vec::new();
        encode_clock(5, &mut out);
        assert_eq!(out, b"T 5\0");
    }

    #[test]
    fn drops_batches_the_guest_does_not_read() {
        let (guest_reads, to_guest) = pipe().unwrap();
        set_nonblocking(&to_guest).unwrap();
        let (stop_read, stop) = pipe().unwrap();
        let data = vec![b'x'; 1 << 20];

        let start = Instant::now();
        assert_eq!(
            write_batch(to_guest.as_fd(), stop_read.as_fd(), &data),
            WriteOutcome::Dropped
        );
        assert!(start.elapsed() < Duration::from_secs(3));

        // The pipe is full now: only stopping ends the next batch early.
        let thread = std::thread::spawn(move || {
            let start = Instant::now();
            let outcome = write_batch(to_guest.as_fd(), stop_read.as_fd(), &data);
            (outcome, start.elapsed())
        });
        std::thread::sleep(Duration::from_millis(100));
        drop(stop);
        let (outcome, elapsed) = thread.join().unwrap();
        assert_eq!(outcome, WriteOutcome::Stopped);
        assert!(elapsed < Duration::from_millis(600), "{elapsed:?}");
        drop(guest_reads);
    }

    #[test]
    fn writes_batches_the_guest_reads() {
        let (guest_reads, to_guest) = pipe().unwrap();
        set_nonblocking(&to_guest).unwrap();
        let (stop_read, _stop) = pipe().unwrap();
        let data = vec![b'x'; 1 << 20];
        let reader = std::thread::spawn(move || {
            let mut read = Vec::new();
            io::Read::read_to_end(&mut std::fs::File::from(guest_reads), &mut read).unwrap();
            read.len()
        });
        assert_eq!(
            write_batch(to_guest.as_fd(), stop_read.as_fd(), &data),
            WriteOutcome::Written
        );
        drop(to_guest);
        assert_eq!(reader.join().unwrap(), data.len());
    }

    /// Reads what arrives on `from` within `wait`.
    fn read_for(from: &OwnedFd, wait: Duration) -> Vec<u8> {
        let deadline = Instant::now() + wait;
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        let mut fds = [pollfd(from.as_raw_fd())];
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            if poll(&mut fds, left.as_millis() as libc::c_int).unwrap() == 0 {
                break;
            }
            match read(from.as_raw_fd(), &mut buf).unwrap() {
                0 => break,
                n => out.extend_from_slice(&buf[..n]),
            }
        }
        out
    }

    #[test]
    fn sends_changes_once_the_guest_is_ready() {
        let temp = TempDir::new("changes-forward");
        let forward = Forward {
            host_root: temp.path().to_path_buf(),
            guest_root: "/home/dev/p".into(),
            hide_git: true,
        };
        let file = temp.path().join("a b.txt");
        std::fs::write(&file, b"a").unwrap();
        std::fs::create_dir(temp.path().join(".git")).unwrap();
        let mtime = mtime_ns(&std::fs::symlink_metadata(&file).unwrap());

        let (guest_reads, to_guest) = pipe().unwrap();
        set_nonblocking(&to_guest).unwrap();
        let (from_guest, guest_writes) = pipe().unwrap();
        let (wake, wake_write) = pipe().unwrap();
        let (stop_read, stop) = pipe().unwrap();
        let pending = Pending::default();
        let channel = Channel {
            to_guest,
            from_guest,
            wake,
            stop: stop_read,
        };
        let thread = {
            let pending = pending.clone();
            std::thread::spawn(move || forward_changes(&forward, &pending, channel))
        };
        let change = |path: &Path, flags: u32| {
            pending.lock().unwrap().insert(path.to_path_buf(), flags);
            write(wake_write.as_raw_fd(), &[0]).unwrap();
        };

        // Not ready: dropped.
        change(&file, flags::ITEM_CREATED);
        assert_eq!(read_for(&guest_reads, Duration::from_millis(400)), b"");
        assert!(pending.lock().unwrap().is_empty());

        write(guest_writes.as_raw_fd(), b"?").unwrap();
        change(&file, flags::ITEM_MODIFIED);
        change(&temp.path().join(".git/HEAD"), flags::ITEM_CREATED);
        change(&temp.path().join("gone"), flags::ITEM_REMOVED);
        let sent = read_for(&guest_reads, Duration::from_millis(500));
        let records: Vec<&[u8]> = sent.split_inclusive(|&b| b == 0).collect();
        assert_eq!(records.len(), 4, "{:?}", String::from_utf8_lossy(&sent));
        assert_eq!(records[0], b"\0");
        assert!(records[1].starts_with(b"T "));
        let modified = format!("m {mtime} /home/dev/p/a b.txt\0");
        assert_eq!(records[2], modified.as_bytes());
        assert_eq!(records[3], b"r 0 /home/dev/p/gone\0");

        drop(stop);
        thread.join().unwrap();
    }

    #[test]
    fn walks_after_dropped_events() {
        let temp = TempDir::new("changes-walk");
        let forward = Forward {
            host_root: temp.path().to_path_buf(),
            guest_root: "/home/dev/p".into(),
            hide_git: true,
        };
        std::fs::create_dir_all(temp.path().join("src/deep")).unwrap();
        std::fs::create_dir_all(temp.path().join(".git")).unwrap();
        std::fs::write(temp.path().join("src/deep/a.rs"), b"a").unwrap();
        std::fs::write(temp.path().join(".git/HEAD"), b"h").unwrap();
        std::os::unix::fs::symlink("/", temp.path().join("src/root")).unwrap();

        // FSEvents names the watched root's ancestor when it drops events.
        let changes = HashMap::from([(PathBuf::from("/"), flags::USER_DROPPED)]);
        let chunks = records(&forward, changes);
        assert_eq!(chunks.len(), 1);
        let sent: Vec<&[u8]> = chunks[0].data.split_inclusive(|&b| b == 0).collect();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].starts_with(b"m "));
        assert!(sent[0].ends_with(b" /home/dev/p/src/deep/a.rs\0"));

        // Large batches get the clock again every `RECORDS_PER_CLOCK`.
        for i in 0..RECORDS_PER_CLOCK {
            std::fs::write(temp.path().join(format!("src/{i}")), b"").unwrap();
        }
        let changes = HashMap::from([(temp.path().join("src"), flags::MUST_SCAN_SUB_DIRS)]);
        let chunks = records(&forward, changes);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[1].data.iter().filter(|&&b| b == 0).count(), 1);
        assert!(!chunks[1].wait);
    }

    #[test]
    fn waits_before_what_is_in_new_directories() {
        let temp = TempDir::new("changes-new-dirs");
        let forward = Forward {
            host_root: temp.path().to_path_buf(),
            guest_root: "/home/dev/p".into(),
            hide_git: true,
        };
        std::fs::create_dir_all(temp.path().join("new/sub")).unwrap();
        for name in ["new/sub/f", "new/g", "old.txt", "z.txt"] {
            std::fs::write(temp.path().join(name), b"").unwrap();
        }
        let changes = HashMap::from([
            (temp.path().join("new"), flags::ITEM_CREATED),
            (temp.path().join("new/sub"), flags::ITEM_CREATED),
            (temp.path().join("new/sub/f"), flags::ITEM_CREATED),
            (temp.path().join("new/g"), flags::ITEM_CREATED),
            (temp.path().join("old.txt"), flags::ITEM_MODIFIED),
            (temp.path().join("z.txt"), flags::ITEM_MODIFIED),
        ]);
        let chunks = records(&forward, changes);
        let paths = |chunk: &Chunk| -> Vec<String> {
            (chunk.data.split(|&b| b == 0))
                .filter(|record| !record.is_empty())
                .map(|record| {
                    let record = String::from_utf8_lossy(record);
                    let (op, rest) = record.split_once(' ').unwrap();
                    let (_, path) = rest.split_once(' ').unwrap();
                    format!("{op} {path}")
                })
                .collect()
        };
        // A watcher adds a watch on a new directory only once it has heard
        // of it: what is in it comes after a pause, level by level.
        assert_eq!(chunks.len(), 3);
        assert!(!chunks[0].wait);
        assert_eq!(
            paths(&chunks[0]),
            [
                "c /home/dev/p/new",
                "m /home/dev/p/old.txt",
                "m /home/dev/p/z.txt"
            ]
        );
        assert!(chunks[1].wait);
        assert_eq!(
            paths(&chunks[1]),
            ["c /home/dev/p/new/g", "c /home/dev/p/new/sub"]
        );
        assert!(chunks[2].wait);
        assert_eq!(paths(&chunks[2]), ["c /home/dev/p/new/sub/f"]);
    }
}
