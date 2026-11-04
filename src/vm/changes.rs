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
//! host's clock. `c`, `m` and `a`, followed by the path's change time
//! (ctime) in ns and the path, report a path created, modified, or changed
//! in its metadata only; `r 0 <path>` reports one removed, and `x 0 <path>`
//! a removed directory. The module compares the change time and the clock
//! with the guest's own recent changes, to drop the Mac's echo of those:
//! unlike the mtime, the change time moves on with every change on the Mac,
//! also with a chmod or a copy that keeps the mtime.
//!
//! When FSEvents drops events, the thread walks the tree and compares it
//! with its snapshot of what it sent before (see `snapshot`), so that what
//! changed meanwhile still arrives as created, modified or removed.
//!
//! Nothing is sent until the guest writes `?`, which it does once the module
//! accepts records: an older image has nobody reading the port. A guest
//! that stops reading gets what it missed once it reads again, or once its
//! service has started again and written another `?`.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use objc2::rc::Retained;
use objc2_virtualization::{VZFileHandleSerialPortAttachment, VZSerialPortAttachment};

use super::console::{file_handle, pipe, poll, pollfd, read, serial_port, write};
use super::fsevents::{EventStream, flags};
use super::snapshot::{self, Change, Entry, Snapshot, join, stopped};

/// How long FSEvents collects events before it delivers them.
const LATENCY: Duration = Duration::from_millis(50);

/// How long after the first unsent event a batch is sent, so that a burst
/// (a branch switch, a build) goes out in few batches.
const DEBOUNCE: Duration = Duration::from_millis(100);

/// How long a write may make no progress before the guest counts as
/// stalled. What it hasn't been sent waits until it reads again.
const STALL_TIMEOUT: Duration = Duration::from_secs(5);

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

/// The most paths that wait to be sent. Past that, a rescan of the whole
/// project takes their place, which finds their changes in the snapshot.
const PENDING_LIMIT: usize = 1_000_000;

/// How many bytes of the records sent last are kept by path. A service that
/// starts again discards what reached the guest's port before, and what was
/// still in the pipe and the virtio queue on the way there: those paths go
/// again. While the service was stopped, about 1.1 MiB piled up there.
const RESEND_BYTES: usize = 4 << 20;

/// How many paths `records` looks at between checks of the stop descriptor.
const STOP_CHECK: usize = 1000;

/// Flags that only report a change of metadata.
const METADATA_FLAGS: u32 = flags::ITEM_INODE_META_MOD
    | flags::ITEM_XATTR_MOD
    | flags::ITEM_FINDER_INFO_MOD
    | flags::ITEM_CHANGE_OWNER;

/// Flags that say FSEvents lost events under a path, which is then rescanned.
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

fn lock(pending: &Pending) -> MutexGuard<'_, HashMap<PathBuf, u32>> {
    pending.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Adds `flags` for `path` to `pending`. Past `limit` paths, they give way
/// to a rescan of the project at `root`.
fn add(pending: &mut HashMap<PathBuf, u32>, root: &Path, path: PathBuf, flags: u32, limit: usize) {
    if pending.len() >= limit && !pending.contains_key(&path) {
        pending.clear();
        pending.insert(root.to_path_buf(), flags::MUST_SCAN_SUB_DIRS);
    }
    *pending.entry(path).or_default() |= flags;
}

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
        let root = forward.host_root.clone();
        // Kept short: dropping the stream waits for it to return.
        let events = EventStream::watch(
            &forward.host_root,
            LATENCY,
            Box::new(move |events| {
                {
                    let mut collected = lock(&collected);
                    for (path, flags) in events {
                        add(&mut collected, &root, path, flags, PENDING_LIMIT);
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
            .spawn(move || forward_changes(&forward, &pending, channel, STALL_TIMEOUT))
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

/// Where the guest is with reading records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Guest {
    /// It hasn't written `?` yet. Batches are dropped meanwhile; if any
    /// were, the project is rescanned once it has.
    Waiting {
        dropped: bool,
    },
    Ready,
    /// It made no room for a batch for the stall timeout. Changes wait
    /// until it reads again, or writes `?` after its service restarted.
    Stalled,
}

/// The paths of the records sent last, to send again when the guest's
/// service has started again.
#[derive(Default)]
struct Sent {
    /// Each chunk's paths, with the flags that send them again, and size.
    chunks: VecDeque<(Vec<(PathBuf, u32)>, usize)>,
    bytes: usize,
}

impl Sent {
    fn push(&mut self, paths: Vec<(PathBuf, u32)>, bytes: usize) {
        self.chunks.push_back((paths, bytes));
        self.bytes += bytes;
        while (self.chunks.front()).is_some_and(|(_, oldest)| self.bytes - oldest >= RESEND_BYTES) {
            let (_, oldest) = self.chunks.pop_front().expect("there is a front");
            self.bytes -= oldest;
        }
    }

    fn take_into(&mut self, paths: &mut HashMap<PathBuf, u32>) {
        for (path, flags) in self.chunks.drain(..).flat_map(|(chunk, _)| chunk) {
            *paths.entry(path).or_default() |= flags;
        }
        self.bytes = 0;
    }
}

/// Sends the changes in `pending` to the guest, `DEBOUNCE` after the first
/// one, once the guest has written `?`. Until then they are dropped, and
/// found again by a rescan when it has. While the guest makes no room for
/// them, they wait, and so do the paths of what wasn't sent. First takes a
/// snapshot of the project. Ends when `channel.stop` closes.
fn forward_changes(
    forward: &Forward,
    pending: &Pending,
    channel: Channel,
    stall_timeout: Duration,
) {
    let stop = channel.stop.as_fd();
    let Ok(mut snapshot) = Snapshot::build(&forward.host_root, forward.hide_git, stop) else {
        return;
    };
    let mut fds = [
        pollfd(channel.stop.as_raw_fd()),
        pollfd(channel.wake.as_raw_fd()),
        pollfd(channel.from_guest.as_raw_fd()),
        // While the guest is stalled: it has read again.
        libc::pollfd {
            fd: -1,
            events: libc::POLLOUT,
            revents: 0,
        },
    ];
    let mut buf = [0u8; 64];
    let mut guest = Guest::Waiting { dropped: false };
    // Paths to send once the guest reads (again), with their flags.
    let mut held = HashMap::new();
    let mut sent = Sent::default();
    // The guest's end of the pipe is gone, so it won't make room again.
    let mut broken = false;
    let mut first_unsent: Option<Instant> = None;
    loop {
        fds[3].fd = if guest == Guest::Stalled && !broken {
            channel.to_guest.as_raw_fd()
        } else {
            -1
        };
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
        if fds[1].revents != 0 {
            match read(fds[1].fd, &mut buf) {
                // poll() skips negative descriptors.
                Ok(0) | Err(_) => fds[1].fd = -1,
                Ok(_) => {
                    first_unsent.get_or_insert_with(Instant::now);
                }
            }
        }
        if fds[2].revents != 0 {
            match read(fds[2].fd, &mut buf) {
                Ok(0) | Err(_) => fds[2].fd = -1,
                Ok(n) if buf[..n].contains(&b'?') => {
                    match guest {
                        Guest::Waiting { dropped: true } => {
                            held.insert(forward.host_root.clone(), flags::MUST_SCAN_SUB_DIRS);
                        }
                        Guest::Waiting { dropped: false } => {}
                        // The service started again, and discarded what
                        // was on the way to it.
                        Guest::Ready | Guest::Stalled => sent.take_into(&mut held),
                    }
                    guest = Guest::Ready;
                    first_unsent.get_or_insert_with(Instant::now);
                }
                Ok(_) => {}
            }
        }
        if fds[3].fd >= 0 && fds[3].revents != 0 {
            if fds[3].revents & libc::POLLOUT != 0 {
                guest = Guest::Ready;
                first_unsent.get_or_insert_with(Instant::now);
            } else {
                broken = true;
            }
        }

        let Some(first) = first_unsent else {
            continue;
        };
        if first.elapsed() < DEBOUNCE {
            continue;
        }
        first_unsent = None;
        match &mut guest {
            Guest::Waiting { dropped } => {
                *dropped |= !std::mem::take(&mut *lock(pending)).is_empty();
                continue;
            }
            Guest::Stalled => continue,
            Guest::Ready => {}
        }
        let mut changes = std::mem::take(&mut *lock(pending));
        for (path, flags) in held.drain() {
            *changes.entry(path).or_default() |= flags;
        }
        let Some(chunks) = records(forward, &mut snapshot, changes, stop) else {
            return;
        };
        let mut chunks = chunks.into_iter();
        while let Some(chunk) = chunks.next() {
            if chunk.wait && wait(stop, NEW_DIRECTORY_PAUSE) {
                return;
            }
            // An empty record first, which the module skips: it ends what
            // is left of a record cut off when the guest stalled, so that
            // it doesn't swallow the clock. The clock comes as close to the
            // guest reading it as possible.
            let mut data = Vec::with_capacity(chunk.data.len() + 32);
            data.push(0);
            encode_clock(realtime_ns(), &mut data);
            data.extend_from_slice(&chunk.data);
            match write_batch(channel.to_guest.as_fd(), stop, &data, stall_timeout) {
                WriteOutcome::Written => sent.push(chunk.paths, data.len()),
                WriteOutcome::Stalled => {
                    // This chunk may have been cut off: it goes again too.
                    guest = Guest::Stalled;
                    for (path, flags) in std::iter::once(chunk).chain(chunks).flat_map(|c| c.paths)
                    {
                        *held.entry(path).or_default() |= flags;
                    }
                    break;
                }
                WriteOutcome::Stopped => return,
            }
        }
    }
}

/// The records for `changes`, in chunks of up to `RECORDS_PER_CLOCK`, or
/// `None` if `stop` closed meanwhile. Each path is classified by what it is
/// now, not by the flags alone, which FSEvents coalesces; rescans compare
/// with the snapshot, which is updated with everything sent. Removals go
/// first, each before the directory it was in. The rest follow sorted, so a
/// new directory comes before what was created in it, and what is in a new
/// directory goes in later chunks, one level of new directories at a time.
fn records(
    forward: &Forward,
    snapshot: &mut Snapshot,
    changes: HashMap<PathBuf, u32>,
    stop: BorrowedFd,
) -> Option<Vec<Chunk>> {
    let mut found: HashMap<Vec<u8>, Found> = HashMap::new();

    // Rescans first, which compare with the snapshot from before the paths
    // below update it.
    let mut scans: Vec<Vec<u8>> = (changes.iter())
        .filter(|(_, flags)| *flags & SCAN_FLAGS != 0)
        .filter_map(|(path, _)| {
            // FSEvents may name an ancestor of the root.
            if forward.host_root.starts_with(path) {
                Some(Vec::new())
            } else {
                relative(forward, path).map(<[u8]>::to_vec)
            }
        })
        .collect();
    // Shorter first, so that a subtree comes after the tree it is in.
    scans.sort_by_key(Vec::len);
    let mut scanned: Vec<Vec<u8>> = Vec::new();
    for sub in scans {
        if scanned.iter().any(|tree| is_in(&sub, tree)) {
            continue;
        }
        let changes = (snapshot.rescan(&forward.host_root, forward.hide_git, &sub, stop)).ok()?;
        found.extend(
            changes
                .into_iter()
                .map(|(rel, change)| (rel, Found::from(change))),
        );
        scanned.push(sub);
    }

    for (i, (path, flags)) in changes.iter().enumerate() {
        if i % STOP_CHECK == 0 && stopped(stop) {
            return None;
        }
        let Some(rel) = relative(forward, path) else {
            continue;
        };
        if found.contains_key(rel) {
            continue;
        }
        let old = snapshot.get(rel);
        let Ok(entry) = snapshot::stat(path) else {
            // Gone, or only reachable through a symlink now. So is what was
            // in it, which FSEvents doesn't report when the directory was
            // moved away.
            let dir = flags & flags::ITEM_IS_DIR != 0 || old.as_ref().is_some_and(Entry::is_dir);
            for (below, was) in snapshot.remove(rel) {
                found.entry(below).or_insert(Found::removed(was.is_dir()));
            }
            found.insert(rel.to_vec(), Found::removed(dir));
            continue;
        };
        if old.is_some_and(|old| old.is_dir() && !entry.is_dir()) {
            for (below, was) in snapshot.remove_below(rel) {
                found.entry(below).or_insert(Found::removed(was.is_dir()));
            }
        }
        snapshot.set(rel, entry);
        if let Some(op) = classify(*flags, Some(&entry)) {
            found.insert(
                rel.to_vec(),
                Found {
                    op,
                    time_ns: entry.ctime_ns,
                    new_dir: op == Op::Created && entry.is_dir(),
                },
            );
        }
    }

    let new_dirs: HashSet<&[u8]> = (found.iter())
        .filter(|(_, found)| found.new_dir)
        .map(|(rel, _)| rel.as_slice())
        .collect();
    let mut levelled: Vec<_> = (found.iter())
        .map(|(rel, found)| {
            let level = (rel.iter().enumerate())
                .filter(|&(i, &byte)| byte == b'/' && new_dirs.contains(&rel[..i]))
                .count();
            (level, rel.as_slice(), found)
        })
        .collect();
    levelled.sort_by(|(level_a, a, found_a), (level_b, b, found_b)| {
        let order = match (found_a.op.removes(), found_b.op.removes()) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            // A path sorts before what is in it: reversed, after.
            (true, true) => b.cmp(a),
            (false, false) => a.cmp(b),
        };
        level_a.cmp(level_b).then(order)
    });
    let mut records = Records::default();
    for (level, rel, found) in levelled {
        records.push(forward, level, found, rel);
    }
    Some(records.chunks)
}

/// Whether `path` is `tree` or below it; the root is empty.
fn is_in(path: &[u8], tree: &[u8]) -> bool {
    tree.is_empty()
        || (path.strip_prefix(tree)).is_some_and(|rest| rest.is_empty() || rest[0] == b'/')
}

/// A record to send, for a path relative to the root.
struct Found {
    op: Op,
    /// The change time.
    time_ns: i64,
    /// A directory created since the last batch.
    new_dir: bool,
}

impl Found {
    fn removed(dir: bool) -> Found {
        Found {
            op: if dir { Op::RemovedDir } else { Op::Removed },
            time_ns: 0,
            new_dir: false,
        }
    }

    /// FSEvents flags that make `records` send this again.
    fn flags(&self) -> u32 {
        match self.op {
            Op::Created if self.new_dir => flags::ITEM_CREATED | flags::ITEM_IS_DIR,
            Op::Created => flags::ITEM_CREATED,
            Op::Modified => flags::ITEM_MODIFIED,
            Op::Attrib => flags::ITEM_INODE_META_MOD,
            Op::Removed => flags::ITEM_REMOVED,
            Op::RemovedDir => flags::ITEM_REMOVED | flags::ITEM_IS_DIR,
        }
    }
}

impl From<Change> for Found {
    fn from(change: Change) -> Found {
        let (op, entry) = match change {
            Change::Created(entry) => (Op::Created, entry),
            Change::Modified(entry) => (Op::Modified, entry),
            Change::Attrib(entry) => (Op::Attrib, entry),
            Change::Removed(was) => return Found::removed(was.is_dir()),
        };
        Found {
            op,
            time_ns: entry.ctime_ns,
            new_dir: op == Op::Created && entry.is_dir(),
        }
    }
}

/// Records sent together, after the clock.
struct Chunk {
    /// Sent `NEW_DIRECTORY_PAUSE` after the chunk before it.
    wait: bool,
    data: Vec<u8>,
    /// The host paths of the records, with FSEvents flags that send them
    /// again.
    paths: Vec<(PathBuf, u32)>,
}

/// Encoded records, in chunks of up to `RECORDS_PER_CLOCK`, and a new one
/// for each level of new directories.
#[derive(Default)]
struct Records {
    chunks: Vec<Chunk>,
    level: usize,
}

impl Records {
    fn push(&mut self, forward: &Forward, level: usize, found: &Found, rel: &[u8]) {
        let deeper = level > self.level;
        let full = (self.chunks.last()).is_some_and(|last| last.paths.len() == RECORDS_PER_CLOCK);
        if self.chunks.is_empty() || full || deeper {
            self.chunks.push(Chunk {
                wait: deeper,
                data: Vec::new(),
                paths: Vec::new(),
            });
            self.level = level;
        }
        let last = self.chunks.last_mut().expect("a chunk was just added");
        let guest = join(&forward.guest_root, rel);
        encode(found.op, found.time_ns, &guest, &mut last.data);
        last.paths
            .push((join(&forward.host_root, rel), found.flags()));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Created,
    Modified,
    Attrib,
    Removed,
    RemovedDir,
}

impl Op {
    fn removes(self) -> bool {
        matches!(self, Op::Removed | Op::RemovedDir)
    }
}

/// What to report for a path with FSEvents `flags` that is now as `entry`
/// says, or gone. Nothing for a directory that only changed (its entries
/// report their own changes) or for flags that report nothing.
fn classify(flags: u32, entry: Option<&Entry>) -> Option<Op> {
    let Some(entry) = entry else {
        return Some(if flags & flags::ITEM_IS_DIR != 0 {
            Op::RemovedDir
        } else {
            Op::Removed
        });
    };
    // Reported as removed but there: replaced, as when an editor saves by
    // renaming a new file over the old one.
    if flags & (flags::ITEM_CREATED | flags::ITEM_RENAMED | flags::ITEM_REMOVED) != 0 {
        Some(Op::Created)
    } else if entry.is_dir() {
        None
    } else if flags & flags::ITEM_MODIFIED != 0 {
        Some(Op::Modified)
    } else if flags & METADATA_FLAGS != 0 {
        Some(Op::Attrib)
    } else {
        None
    }
}

/// `host` relative to the project root, if it is below it and not left
/// out.
fn relative<'a>(forward: &Forward, host: &'a Path) -> Option<&'a [u8]> {
    // Bytes, not components: FSEvents' paths are normalized already, and
    // Path::strip_prefix would also take "/private/tmp/p/" for the root.
    let rest = (host.as_os_str().as_bytes())
        .strip_prefix(forward.host_root.as_os_str().as_bytes())?
        .strip_prefix(b"/")?;
    let git = rest == b".git" || rest.starts_with(b".git/");
    if rest.is_empty() || (forward.hide_git && git) {
        return None;
    }
    Some(rest)
}

/// Appends the record for `op` on `guest`.
fn encode(op: Op, time_ns: i64, guest: &Path, out: &mut Vec<u8>) {
    let (code, time_ns) = match op {
        Op::Created => ('c', time_ns),
        Op::Modified => ('m', time_ns),
        Op::Attrib => ('a', time_ns),
        Op::Removed => ('r', 0),
        Op::RemovedDir => ('x', 0),
    };
    let _ = write!(out, "{code} {time_ns} ");
    out.extend_from_slice(guest.as_os_str().as_bytes());
    out.push(0);
}

/// Appends the record that gives the host's clock.
fn encode_clock(host_now_ns: i64, out: &mut Vec<u8>) {
    let _ = write!(out, "T {host_now_ns}\0");
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
    /// The guest made no room for the stall timeout, or its end is gone;
    /// the rest wasn't sent.
    Stalled,
    /// `stop` closed.
    Stopped,
}

/// Writes `data` to the non-blocking `to_guest`, unless the guest makes no
/// room for `stall_timeout` or `stop` closes first.
fn write_batch(
    to_guest: BorrowedFd,
    stop: BorrowedFd,
    mut data: &[u8],
    stall_timeout: Duration,
) -> WriteOutcome {
    let mut progress = Instant::now();
    while !data.is_empty() {
        match write(to_guest.as_raw_fd(), data) {
            Ok(n) => {
                data = &data[n..];
                progress = Instant::now();
                continue;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => return WriteOutcome::Stalled,
        }
        let Some(left) = stall_timeout.checked_sub(progress.elapsed()) else {
            return WriteOutcome::Stalled;
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
    use std::ffi::OsStr;
    use std::fs;
    use std::os::unix::fs::symlink;

    fn forward(hide_git: bool) -> Forward {
        Forward {
            host_root: "/private/tmp/p".into(),
            guest_root: "/home/dev/p".into(),
            hide_git,
        }
    }

    fn forward_in(temp: &TempDir) -> Forward {
        Forward {
            host_root: temp.path().to_path_buf(),
            guest_root: "/home/dev/p".into(),
            hide_git: true,
        }
    }

    fn entry(path: &Path) -> Entry {
        snapshot::stat(path).unwrap()
    }

    #[test]
    fn classifies_flags() {
        let temp = TempDir::new("changes-classify");
        let file = temp.path().join("a.txt");
        fs::write(&file, b"a").unwrap();
        let file = entry(&file);
        let dir = entry(temp.path());
        let (file, dir) = (Some(&file), Some(&dir));

        assert_eq!(classify(flags::ITEM_REMOVED, None), Some(Op::Removed));
        assert_eq!(classify(flags::ITEM_CREATED, None), Some(Op::Removed));
        let removed_dir = flags::ITEM_REMOVED | flags::ITEM_IS_DIR;
        assert_eq!(classify(removed_dir, None), Some(Op::RemovedDir));
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
        let rel = |forward: &Forward, host: &[u8]| {
            relative(forward, Path::new(OsStr::from_bytes(host))).map(<[u8]>::to_vec)
        };
        assert_eq!(
            rel(&hidden, b"/private/tmp/p/src/a.rs"),
            Some(b"src/a.rs".to_vec())
        );
        assert_eq!(rel(&hidden, b"/private/tmp/p"), None);
        assert_eq!(rel(&hidden, b"/private/tmp/pq/a"), None);
        assert_eq!(rel(&hidden, b"/private/tmp"), None);
        assert_eq!(rel(&hidden, b"/private/tmp/p/.git"), None);
        assert_eq!(rel(&hidden, b"/private/tmp/p/.git/HEAD"), None);
        // Only the project's own .git is hidden.
        assert_eq!(
            rel(&hidden, b"/private/tmp/p/.github/ci.yml"),
            Some(b".github/ci.yml".to_vec())
        );
        assert_eq!(
            rel(&forward(false), b"/private/tmp/p/.git/HEAD"),
            Some(b".git/HEAD".to_vec())
        );
        assert_eq!(
            rel(&hidden, b"/private/tmp/p/\xff.rs"),
            Some(b"\xff.rs".to_vec())
        );
        assert_eq!(
            join(&hidden.guest_root, b"\xff.rs"),
            Path::new(OsStr::from_bytes(b"/home/dev/p/\xff.rs"))
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
        encode(Op::RemovedDir, 6, Path::new("/home/dev/p/d"), &mut out);
        assert_eq!(
            out,
            b"m 3 /home/dev/p/a\0a 4 /home/dev/p/b\0r 0 /home/dev/p/c\0x 0 /home/dev/p/d\0"
        );

        let mut out = Vec::new();
        encode_clock(5, &mut out);
        assert_eq!(out, b"T 5\0");
    }

    #[test]
    fn collapses_too_many_pending_paths() {
        let root = Path::new("/private/tmp/p");
        let mut pending = HashMap::new();
        for name in ["a", "b", "a"] {
            add(&mut pending, root, root.join(name), flags::ITEM_MODIFIED, 2);
        }
        assert_eq!(pending.len(), 2);
        // The rescan of the root finds what changed among them.
        add(&mut pending, root, root.join("c"), flags::ITEM_CREATED, 2);
        assert_eq!(
            pending,
            HashMap::from([
                (root.to_path_buf(), flags::MUST_SCAN_SUB_DIRS),
                (root.join("c"), flags::ITEM_CREATED),
            ])
        );
    }

    #[test]
    fn gives_up_on_batches_the_guest_does_not_read() {
        let (guest_reads, to_guest) = pipe().unwrap();
        set_nonblocking(&to_guest).unwrap();
        let (stop_read, stop) = pipe().unwrap();
        let data = vec![b'x'; 1 << 20];

        let start = Instant::now();
        let stall = Duration::from_millis(300);
        assert_eq!(
            write_batch(to_guest.as_fd(), stop_read.as_fd(), &data, stall),
            WriteOutcome::Stalled
        );
        assert!(start.elapsed() < Duration::from_secs(2));

        // The pipe is full now: only stopping ends the next batch early.
        let thread = std::thread::spawn(move || {
            let start = Instant::now();
            let outcome = write_batch(to_guest.as_fd(), stop_read.as_fd(), &data, STALL_TIMEOUT);
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
            write_batch(to_guest.as_fd(), stop_read.as_fd(), &data, STALL_TIMEOUT),
            WriteOutcome::Written
        );
        drop(to_guest);
        assert_eq!(reader.join().unwrap(), data.len());
    }

    /// Reads what arrives on `from` until nothing has for `quiet`.
    fn read_for(from: &OwnedFd, quiet: Duration) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 65536];
        let mut fds = [pollfd(from.as_raw_fd())];
        while poll(&mut fds, quiet.as_millis() as libc::c_int).unwrap() > 0 {
            match read(from.as_raw_fd(), &mut buf).unwrap() {
                0 => break,
                n => out.extend_from_slice(&buf[..n]),
            }
        }
        out
    }

    /// The records in `sent`, without the clock and the empty ones.
    fn records_in(sent: &[u8]) -> Vec<String> {
        (sent.split(|&b| b == 0))
            .filter(|record| !record.is_empty() && !record.starts_with(b"T "))
            .map(|record| String::from_utf8_lossy(record).into_owned())
            .collect()
    }

    /// The records of `chunks`, as op and path.
    fn ops_and_paths(chunk: &Chunk) -> Vec<String> {
        (records_in(&chunk.data).iter())
            .map(|record| {
                let (op, rest) = record.split_once(' ').unwrap();
                let (_, path) = rest.split_once(' ').unwrap();
                format!("{op} {path}")
            })
            .collect()
    }

    /// A forwarder thread with its ends for the test.
    struct Running {
        pending: Pending,
        guest_reads: OwnedFd,
        guest_writes: OwnedFd,
        wake_write: OwnedFd,
        stop: Option<OwnedFd>,
        thread: Option<JoinHandle<()>>,
    }

    impl Running {
        fn start(forward: Forward, stall: Duration) -> Running {
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
                std::thread::spawn(move || forward_changes(&forward, &pending, channel, stall))
            };
            Running {
                pending,
                guest_reads,
                guest_writes,
                wake_write,
                stop: Some(stop),
                thread: Some(thread),
            }
        }

        fn change(&self, path: &Path, flags: u32) {
            lock(&self.pending).insert(path.to_path_buf(), flags);
            write(self.wake_write.as_raw_fd(), &[0]).unwrap();
        }

        fn ready(&self) {
            write(self.guest_writes.as_raw_fd(), b"?").unwrap();
        }
    }

    impl Drop for Running {
        fn drop(&mut self) {
            drop(self.stop.take());
            self.thread.take().unwrap().join().unwrap();
        }
    }

    #[test]
    fn sends_changes_once_the_guest_is_ready() {
        let temp = TempDir::new("changes-forward");
        let file = temp.path().join("a b.txt");
        fs::write(&file, b"a").unwrap();
        fs::create_dir(temp.path().join(".git")).unwrap();
        let ctime = entry(&file).ctime_ns;
        let running = Running::start(forward_in(&temp), STALL_TIMEOUT);

        // Not ready: dropped.
        running.change(&file, flags::ITEM_CREATED);
        assert_eq!(
            read_for(&running.guest_reads, Duration::from_millis(400)),
            b""
        );
        assert!(lock(&running.pending).is_empty());

        running.ready();
        running.change(&file, flags::ITEM_MODIFIED);
        running.change(&temp.path().join(".git/HEAD"), flags::ITEM_CREATED);
        running.change(&temp.path().join("gone"), flags::ITEM_REMOVED);
        let sent = read_for(&running.guest_reads, Duration::from_millis(500));
        let records: Vec<&[u8]> = sent.split_inclusive(|&b| b == 0).collect();
        assert_eq!(records.len(), 4, "{:?}", String::from_utf8_lossy(&sent));
        assert_eq!(records[0], b"\0");
        assert!(records[1].starts_with(b"T "));
        // Removals first.
        assert_eq!(records[2], b"r 0 /home/dev/p/gone\0");
        let modified = format!("m {ctime} /home/dev/p/a b.txt\0");
        assert_eq!(records[3], modified.as_bytes());
    }

    #[test]
    fn sends_what_changed_while_it_waited_for_the_guest() {
        let temp = TempDir::new("changes-waited");
        fs::write(temp.path().join("old.txt"), b"a").unwrap();
        let running = Running::start(forward_in(&temp), STALL_TIMEOUT);
        std::thread::sleep(Duration::from_millis(200));

        // Dropped while the guest boots, but the snapshot from before tells.
        fs::write(temp.path().join("new.txt"), b"n").unwrap();
        running.change(&temp.path().join("new.txt"), flags::ITEM_CREATED);
        std::thread::sleep(Duration::from_millis(300));
        running.ready();
        let sent = read_for(&running.guest_reads, Duration::from_millis(500));
        let records = records_in(&sent);
        assert_eq!(records.len(), 1, "{records:?}");
        assert!(records[0].starts_with("c ") && records[0].ends_with(" /home/dev/p/new.txt"));
    }

    #[test]
    fn holds_back_changes_while_the_guest_stalls() {
        let temp = TempDir::new("changes-stall");
        let running = Running::start(forward_in(&temp), Duration::from_millis(300));
        running.ready();
        std::thread::sleep(Duration::from_millis(200));
        // Far more than the pipe holds.
        let names: Vec<String> = (0..3000)
            .map(|i| format!("{i}-{}", "x".repeat(60)))
            .collect();
        for name in &names {
            fs::write(temp.path().join(name), b"").unwrap();
            running.change(&temp.path().join(name), flags::ITEM_CREATED);
        }
        // The guest stalls, and reads again later.
        std::thread::sleep(Duration::from_secs(1));
        let sent = read_for(&running.guest_reads, Duration::from_millis(1000));

        let got: HashSet<String> = (records_in(&sent).iter())
            .filter_map(|record| record.rsplit_once('/').map(|(_, name)| name.to_string()))
            .collect();
        let missing: Vec<_> = names.iter().filter(|name| !got.contains(*name)).collect();
        assert!(missing.is_empty(), "{} missing", missing.len());
    }

    #[test]
    fn sends_again_what_a_restarted_guest_discarded() {
        let temp = TempDir::new("changes-restart");
        let running = Running::start(forward_in(&temp), STALL_TIMEOUT);
        running.ready();
        std::thread::sleep(Duration::from_millis(200));
        for name in ["a", "b"] {
            fs::write(temp.path().join(name), b"").unwrap();
            running.change(&temp.path().join(name), flags::ITEM_CREATED);
        }
        // Sent, but the guest's service restarts, discards them and asks
        // again.
        let sent = read_for(&running.guest_reads, Duration::from_millis(500));
        assert_eq!(records_in(&sent).len(), 2);
        running.ready();
        let sent = read_for(&running.guest_reads, Duration::from_millis(500));
        let mut paths: Vec<String> = (records_in(&sent).iter())
            .map(|record| record.rsplit_once('/').unwrap().1.to_string())
            .collect();
        paths.sort();
        assert_eq!(paths, ["a", "b"]);
    }

    #[test]
    fn rescans_after_dropped_events() {
        let temp = TempDir::new("changes-rescan");
        let forward = forward_in(&temp);
        let root = temp.path();
        fs::create_dir_all(root.join("src/deep")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("src/deep/a.rs"), b"a").unwrap();
        fs::write(root.join("src/old.rs"), b"o").unwrap();
        fs::write(root.join(".git/HEAD"), b"h").unwrap();
        let (stop_read, _stop) = pipe().unwrap();
        let mut snapshot = Snapshot::build(root, true, stop_read.as_fd()).unwrap();

        fs::write(root.join("src/deep/a.rs"), b"changed").unwrap();
        fs::write(root.join(".git/index"), b"i").unwrap();
        fs::remove_file(root.join("src/old.rs")).unwrap();
        fs::create_dir(root.join("src/new")).unwrap();
        fs::write(root.join("src/new/b.rs"), b"b").unwrap();
        // FSEvents names the watched root's ancestor when it drops events.
        let changes = HashMap::from([(PathBuf::from("/"), flags::USER_DROPPED)]);
        let chunks = records(&forward, &mut snapshot, changes, stop_read.as_fd()).unwrap();
        let ops: Vec<_> = chunks.iter().map(ops_and_paths).collect();
        assert_eq!(
            ops,
            [
                vec![
                    "r /home/dev/p/src/old.rs",
                    "m /home/dev/p/src/deep/a.rs",
                    "c /home/dev/p/src/new",
                ],
                vec!["c /home/dev/p/src/new/b.rs"],
            ]
        );
        assert!(chunks[1].wait);

        // Nothing changed since.
        let changes = HashMap::from([(root.join("src"), flags::MUST_SCAN_SUB_DIRS)]);
        let chunks = records(&forward, &mut snapshot, changes, stop_read.as_fd()).unwrap();
        assert!(chunks.is_empty());

        // Large batches get the clock again every `RECORDS_PER_CLOCK`.
        for i in 0..=RECORDS_PER_CLOCK {
            fs::write(root.join(format!("src/{i}")), b"").unwrap();
        }
        let changes = HashMap::from([(root.join("src"), flags::MUST_SCAN_SUB_DIRS)]);
        let chunks = records(&forward, &mut snapshot, changes, stop_read.as_fd()).unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[1].data.iter().filter(|&&b| b == 0).count(), 1);
        assert!(!chunks[1].wait);
    }

    #[test]
    fn removes_what_is_in_a_directory_first() {
        let temp = TempDir::new("changes-removals");
        let forward = forward_in(&temp);
        let root = temp.path();
        fs::create_dir_all(root.join("d/sub")).unwrap();
        for file in ["d/a", "d/sub/c", "d.txt", "e"] {
            fs::write(root.join(file), b"").unwrap();
        }
        let (stop_read, _stop) = pipe().unwrap();
        let mut snapshot = Snapshot::build(root, true, stop_read.as_fd()).unwrap();

        // FSEvents reports some of it; the snapshot knows the rest.
        fs::remove_dir_all(root.join("d")).unwrap();
        fs::remove_file(root.join("d.txt")).unwrap();
        fs::write(root.join("e"), b"changed").unwrap();
        let changes = HashMap::from([
            (root.join("d"), flags::ITEM_REMOVED | flags::ITEM_IS_DIR),
            (root.join("d/a"), flags::ITEM_REMOVED),
            (root.join("d.txt"), flags::ITEM_REMOVED),
            (root.join("e"), flags::ITEM_MODIFIED),
        ]);
        let chunks = records(&forward, &mut snapshot, changes, stop_read.as_fd()).unwrap();
        let ops: Vec<_> = chunks.iter().map(ops_and_paths).collect();
        assert_eq!(
            ops,
            [vec![
                "r /home/dev/p/d/sub/c",
                "x /home/dev/p/d/sub",
                "r /home/dev/p/d/a",
                "r /home/dev/p/d.txt",
                "x /home/dev/p/d",
                "m /home/dev/p/e",
            ]]
        );
        assert_eq!(snapshot.get(b"d/sub/c"), None);
    }

    #[test]
    fn looks_at_nothing_through_symlinks() {
        let temp = TempDir::new("changes-symlinks");
        let outside = TempDir::new("changes-symlinks-outside");
        let forward = forward_in(&temp);
        let root = temp.path();
        fs::write(outside.path().join("secret.txt"), b"s").unwrap();
        fs::create_dir(root.join("A")).unwrap();
        let (stop_read, _stop) = pipe().unwrap();
        let mut snapshot = Snapshot::build(root, true, stop_read.as_fd()).unwrap();

        // The guest swaps A for a link to elsewhere: a path through it is
        // gone, whether its target has the name or not.
        fs::remove_dir(root.join("A")).unwrap();
        symlink(outside.path(), root.join("A")).unwrap();
        let changes = HashMap::from([
            (root.join("A"), flags::ITEM_CREATED | flags::ITEM_REMOVED),
            (root.join("A/secret.txt"), flags::ITEM_CREATED),
            (root.join("A/missing.txt"), flags::ITEM_CREATED),
        ]);
        let chunks = records(&forward, &mut snapshot, changes, stop_read.as_fd()).unwrap();
        let ops: Vec<_> = chunks.iter().map(ops_and_paths).collect();
        assert_eq!(
            ops,
            [vec![
                "r /home/dev/p/A/secret.txt",
                "r /home/dev/p/A/missing.txt",
                "c /home/dev/p/A",
            ]]
        );
    }

    #[test]
    fn waits_before_what_is_in_new_directories() {
        let temp = TempDir::new("changes-new-dirs");
        let forward = forward_in(&temp);
        let (stop_read, _stop) = pipe().unwrap();
        let mut snapshot = Snapshot::build(temp.path(), true, stop_read.as_fd()).unwrap();
        fs::create_dir_all(temp.path().join("new/sub")).unwrap();
        for name in ["new/sub/f", "new/g", "old.txt", "z.txt"] {
            fs::write(temp.path().join(name), b"").unwrap();
        }
        let changes = HashMap::from([
            (temp.path().join("new"), flags::ITEM_CREATED),
            (temp.path().join("new/sub"), flags::ITEM_CREATED),
            (temp.path().join("new/sub/f"), flags::ITEM_CREATED),
            (temp.path().join("new/g"), flags::ITEM_CREATED),
            (temp.path().join("old.txt"), flags::ITEM_MODIFIED),
            (temp.path().join("z.txt"), flags::ITEM_MODIFIED),
        ]);
        let chunks = records(&forward, &mut snapshot, changes, stop_read.as_fd()).unwrap();
        // A watcher adds a watch on a new directory only once it has heard
        // of it: what is in it comes after a pause, level by level.
        assert_eq!(chunks.len(), 3);
        assert!(!chunks[0].wait);
        assert_eq!(
            ops_and_paths(&chunks[0]),
            [
                "c /home/dev/p/new",
                "m /home/dev/p/old.txt",
                "m /home/dev/p/z.txt"
            ]
        );
        assert!(chunks[1].wait);
        assert_eq!(
            ops_and_paths(&chunks[1]),
            ["c /home/dev/p/new/g", "c /home/dev/p/new/sub"]
        );
        assert!(chunks[2].wait);
        assert_eq!(ops_and_paths(&chunks[2]), ["c /home/dev/p/new/sub/f"]);
    }
}
