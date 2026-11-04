//! The forwarder's picture of the project tree: what it last told the guest
//! about each path. When FSEvents drops events, a rescan compares the tree
//! with it, so that what changed in the meantime arrives as what it is: a
//! creation, a change or a removal.
//!
//! Nothing here follows a symlink below the project root. The guest can
//! plant one there, e.g. swap a directory for a link to one elsewhere on the
//! Mac, and what is outside the project must neither show in what is sent
//! to the guest nor hang the forwarder on a slow network volume.

use std::collections::btree_map::Range;
use std::collections::{BTreeMap, HashMap};
use std::ffi::{CStr, CString, OsStr};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;

use super::console::{poll, pollfd};

/// The most entries the snapshot keeps, some 150 MB. Beyond that, a rescan
/// can't tell new files from old ones it never recorded, and reports them as
/// modified, as it did before there was a snapshot.
pub(super) const SNAPSHOT_LIMIT: usize = 1_000_000;

/// How many entries a walk reads between looks at the stop descriptor.
const STOP_CHECK: usize = 1000;

/// `AT_SYMLINK_NOFOLLOW_ANY` from <sys/fcntl.h> (macOS 11 and later), which
/// the libc crate doesn't declare: `fstatat` fails with ELOOP if a component
/// of the path is a symlink, and reports a final one as itself.
const AT_SYMLINK_NOFOLLOW_ANY: libc::c_int = 0x0800;

/// What a path is, as far as the guest's watchers care.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Dir,
    File,
    Symlink,
    Other,
}

/// A path's state when it was last looked at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Entry {
    pub kind: Kind,
    pub size: u64,
    pub mtime_ns: i64,
    /// The change time, which moves on with any change, a chmod's too.
    pub ctime_ns: i64,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }

    fn from_stat(stat: &libc::stat) -> Entry {
        let kind = match stat.st_mode & libc::S_IFMT {
            libc::S_IFDIR => Kind::Dir,
            libc::S_IFREG => Kind::File,
            libc::S_IFLNK => Kind::Symlink,
            _ => Kind::Other,
        };
        let ns = |secs: libc::time_t, nsecs: libc::c_long| {
            secs.saturating_mul(1_000_000_000).saturating_add(nsecs)
        };
        Entry {
            kind,
            size: stat.st_size.try_into().unwrap_or(0),
            mtime_ns: ns(stat.st_mtime, stat.st_mtime_nsec),
            ctime_ns: ns(stat.st_ctime, stat.st_ctime_nsec),
        }
    }
}

/// The stop descriptor closed during a walk.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Stopped;

/// How a path differs from the snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Change {
    Created(Entry),
    Modified(Entry),
    /// Only its change time moved: a chmod, chown or xattr.
    Attrib(Entry),
    /// Gone; what it was.
    Removed(Entry),
}

/// Like `lstat`, but fails with ELOOP if a component before the last one is
/// a symlink.
pub(super) fn stat(path: &Path) -> io::Result<Entry> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    fstatat(libc::AT_FDCWD, &path, AT_SYMLINK_NOFOLLOW_ANY)
}

fn fstatat(dir: libc::c_int, path: &CStr, flags: libc::c_int) -> io::Result<Entry> {
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `path` is a C string and `stat` is valid for writes of a stat.
    if unsafe { libc::fstatat(dir, path.as_ptr(), stat.as_mut_ptr(), flags) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstatat succeeded, so it filled in `stat`.
    Ok(Entry::from_stat(unsafe { stat.assume_init_ref() }))
}

/// `rel` below `root`.
pub(super) fn join(root: &Path, rel: &[u8]) -> PathBuf {
    if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(OsStr::from_bytes(rel))
    }
}

/// Whether `stop` has closed.
pub(super) fn stopped(stop: BorrowedFd) -> bool {
    let mut fds = [pollfd(stop.as_raw_fd())];
    poll(&mut fds, 0).is_err() || fds[0].revents != 0
}

/// An open directory.
struct Dir(NonNull<libc::DIR>);

impl Dir {
    /// Opens `path`, unless a component of it is a symlink.
    fn open(path: &Path) -> Option<Dir> {
        let path = CString::new(path.as_os_str().as_bytes()).ok()?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW_ANY | libc::O_CLOEXEC;
        // SAFETY: `path` is a C string.
        let fd = unsafe { libc::open(path.as_ptr(), flags) };
        if fd < 0 {
            return None;
        }
        // SAFETY: `fd` is an open directory; on success, the DIR owns it.
        match NonNull::new(unsafe { libc::fdopendir(fd) }) {
            Some(dir) => Some(Dir(dir)),
            None => {
                // SAFETY: `fd` is still ours.
                unsafe { libc::close(fd) };
                None
            }
        }
    }

    /// The next name in the directory, but `.` and `..`.
    fn next_name(&mut self) -> Option<Vec<u8>> {
        loop {
            // SAFETY: the DIR is open. The entry stays valid until the next
            // readdir on it, and the name is copied before that.
            let name = unsafe {
                let entry = libc::readdir(self.0.as_ptr()).as_ref()?;
                CStr::from_ptr(entry.d_name.as_ptr()).to_bytes().to_vec()
            };
            if name != b"." && name != b".." {
                return Some(name);
            }
        }
    }

    /// The state of `name` in the directory, a symlink as itself.
    fn stat(&self, name: &[u8]) -> io::Result<Entry> {
        let name = CString::new(name)?;
        // SAFETY: the DIR is open, and so is its descriptor.
        let fd = unsafe { libc::dirfd(self.0.as_ptr()) };
        fstatat(fd, &name, libc::AT_SYMLINK_NOFOLLOW)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        // SAFETY: the DIR is open, and isn't used again.
        unsafe { libc::closedir(self.0.as_ptr()) };
    }
}

/// Calls `each` with `sub` (relative to `root`; nothing for the root itself,
/// `sub` empty) and every path below it, and their state. Leaves out the
/// project's `.git` if `hide_git`. Directories are read through descriptors
/// opened without following a symlink anywhere in their path, so a
/// directory swapped for a symlink meanwhile isn't read either.
fn walk(
    root: &Path,
    hide_git: bool,
    sub: &[u8],
    stop: BorrowedFd,
    each: &mut dyn FnMut(&[u8], Entry),
) -> Result<(), Stopped> {
    let mut dirs = Vec::new();
    if sub.is_empty() {
        dirs.push(Vec::new());
    } else if let Ok(entry) = stat(&join(root, sub)) {
        each(sub, entry);
        if entry.is_dir() {
            dirs.push(sub.to_vec());
        }
    }
    let mut seen = 0;
    while let Some(dir) = dirs.pop() {
        let Some(mut listing) = Dir::open(&join(root, &dir)) else {
            continue;
        };
        while let Some(name) = listing.next_name() {
            if seen % STOP_CHECK == 0 && stopped(stop) {
                return Err(Stopped);
            }
            seen += 1;
            let rel = if dir.is_empty() {
                name.clone()
            } else {
                [&dir[..], b"/", &name].concat()
            };
            if hide_git && rel == b".git" {
                continue;
            }
            let Ok(entry) = listing.stat(&name) else {
                continue;
            };
            each(&rel, entry);
            if entry.is_dir() {
                dirs.push(rel);
            }
        }
    }
    Ok(())
}

/// How `new` differs from `old`, at the same path. A directory only changes
/// by being created or removed: its entries report their own changes.
fn compare(old: Entry, new: Entry) -> Option<Change> {
    if old.kind != new.kind {
        Some(Change::Created(new))
    } else if new.is_dir() {
        None
    } else if old.size != new.size || old.mtime_ns != new.mtime_ns {
        Some(Change::Modified(new))
    } else if old.ctime_ns != new.ctime_ns {
        Some(Change::Attrib(new))
    } else {
        None
    }
}

/// Each path below the project root (relative, as bytes) and its state.
pub(super) struct Snapshot {
    entries: BTreeMap<Box<[u8]>, Entry>,
    limit: usize,
    /// It reached `limit`: a path it doesn't have may be old or new.
    full: bool,
}

impl Snapshot {
    /// Walks the project at `root`, without a hidden `.git`.
    pub fn build(root: &Path, hide_git: bool, stop: BorrowedFd) -> Result<Snapshot, Stopped> {
        Self::build_limited(root, hide_git, SNAPSHOT_LIMIT, stop)
    }

    fn build_limited(
        root: &Path,
        hide_git: bool,
        limit: usize,
        stop: BorrowedFd,
    ) -> Result<Snapshot, Stopped> {
        let mut snapshot = Snapshot {
            entries: BTreeMap::new(),
            limit,
            full: false,
        };
        walk(root, hide_git, b"", stop, &mut |rel, entry| {
            snapshot.set(rel, entry);
        })?;
        Ok(snapshot)
    }

    pub fn get(&self, rel: &[u8]) -> Option<Entry> {
        self.entries.get(rel).copied()
    }

    /// Records `entry` for `rel`; returns false if the snapshot is full and
    /// didn't have `rel` before.
    pub fn set(&mut self, rel: &[u8], entry: Entry) -> bool {
        if let Some(old) = self.entries.get_mut(rel) {
            *old = entry;
            return true;
        }
        if self.full {
            return false;
        }
        if self.entries.len() >= self.limit {
            self.full = true;
            eprint!(
                "\r\nsendit: the project has more than {} files and folders; changes FSEvents \
                 loses past those arrive as modifications only\r\n",
                self.limit
            );
            return false;
        }
        self.entries.insert(rel.into(), entry);
        true
    }

    /// Forgets `rel` and what is below it; returns the latter.
    pub fn remove(&mut self, rel: &[u8]) -> Vec<(Vec<u8>, Entry)> {
        self.entries.remove(rel);
        self.remove_below(rel)
    }

    /// Forgets what is below `rel`, and returns it.
    pub fn remove_below(&mut self, rel: &[u8]) -> Vec<(Vec<u8>, Entry)> {
        let below: Vec<Box<[u8]>> = self.below(rel).map(|(path, _)| path.clone()).collect();
        (below.into_iter())
            .filter_map(|path| self.entries.remove_entry(&path))
            .map(|(path, entry)| (path.into_vec(), entry))
            .collect()
    }

    /// The entries strictly below `rel`, or all of them for the root (`rel`
    /// empty).
    fn below(&self, rel: &[u8]) -> Range<'_, Box<[u8]>, Entry> {
        use std::ops::Bound::{Excluded, Included, Unbounded};
        // '0' follows '/': everything from "rel/" up to "rel0" starts with
        // "rel/".
        let from = [rel, b"/"].concat();
        let to = [rel, b"0"].concat();
        let bounds = if rel.is_empty() {
            (Unbounded, Unbounded)
        } else {
            (Included(&from[..]), Excluded(&to[..]))
        };
        self.entries.range::<[u8], _>(bounds)
    }

    /// Walks `sub` (relative to `root`, empty for the whole project) and
    /// everything below it, and returns how that differs from the snapshot,
    /// which then matches it.
    pub fn rescan(
        &mut self,
        root: &Path,
        hide_git: bool,
        sub: &[u8],
        stop: BorrowedFd,
    ) -> Result<Vec<(Vec<u8>, Change)>, Stopped> {
        let mut now = HashMap::new();
        walk(root, hide_git, sub, stop, &mut |rel, entry| {
            now.insert(rel.to_vec(), entry);
        })?;

        let mut known: Vec<(Vec<u8>, Entry)> = (self.below(sub))
            .map(|(rel, entry)| (rel.to_vec(), *entry))
            .collect();
        if !sub.is_empty()
            && let Some(entry) = self.get(sub)
        {
            known.push((sub.to_vec(), entry));
        }
        let mut changes = Vec::new();
        for (rel, old) in known {
            match now.remove(&rel) {
                Some(new) => {
                    changes.extend(compare(old, new).map(|change| (rel.clone(), change)));
                    self.set(&rel, new);
                }
                None => {
                    self.entries.remove(&rel[..]);
                    changes.push((rel, Change::Removed(old)));
                }
            }
        }
        for (rel, new) in now {
            if self.set(&rel, new) {
                changes.push((rel, Change::Created(new)));
            } else if !new.is_dir() {
                // May be old: what was sent before there was a snapshot.
                changes.push((rel, Change::Modified(new)));
            }
        }
        Ok(changes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::TempDir;
    use std::fs;
    use std::os::fd::{AsFd, OwnedFd};
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn stop() -> (OwnedFd, OwnedFd) {
        crate::vm::console::pipe().unwrap()
    }

    /// The changes, by path and kind of change, sorted.
    fn summary(changes: Vec<(Vec<u8>, Change)>) -> Vec<String> {
        let mut out: Vec<String> = (changes.into_iter())
            .map(|(rel, change)| {
                let what = match change {
                    Change::Created(e) if e.is_dir() => "created dir",
                    Change::Created(_) => "created",
                    Change::Modified(_) => "modified",
                    Change::Attrib(_) => "attrib",
                    Change::Removed(e) if e.is_dir() => "removed dir",
                    Change::Removed(_) => "removed",
                };
                format!("{what} {}", String::from_utf8_lossy(&rel))
            })
            .collect();
        out.sort();
        out
    }

    fn paths(snapshot: &Snapshot) -> Vec<String> {
        (snapshot.entries.keys())
            .map(|key| String::from_utf8_lossy(key).into_owned())
            .collect()
    }

    #[test]
    fn rescans_against_the_snapshot() {
        let temp = TempDir::new("snapshot-diff");
        let root = temp.path();
        let (stop_read, _stop) = stop();
        for dir in ["olddir/sub", ".git", "kept"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        for file in [
            "keep.txt",
            "mod.txt",
            "attr.txt",
            "gone.txt",
            "olddir/x.txt",
            "olddir/sub/y.txt",
            ".git/HEAD",
            "kept/k.txt",
        ] {
            fs::write(root.join(file), "x\n").unwrap();
        }
        let mut snapshot = Snapshot::build(root, true, stop_read.as_fd()).unwrap();
        assert_eq!(
            paths(&snapshot),
            [
                "attr.txt",
                "gone.txt",
                "keep.txt",
                "kept",
                "kept/k.txt",
                "mod.txt",
                "olddir",
                "olddir/sub",
                "olddir/sub/y.txt",
                "olddir/x.txt"
            ]
        );

        fs::write(root.join("mod.txt"), "longer\n").unwrap();
        fs::set_permissions(root.join("attr.txt"), fs::Permissions::from_mode(0o600)).unwrap();
        fs::remove_file(root.join("gone.txt")).unwrap();
        fs::remove_dir_all(root.join("olddir")).unwrap();
        fs::write(root.join("new.txt"), "n\n").unwrap();
        fs::create_dir_all(root.join("newdir/deeper")).unwrap();
        fs::write(root.join("newdir/f.txt"), "f\n").unwrap();
        fs::write(root.join("newdir/deeper/g.txt"), "g\n").unwrap();
        fs::write(root.join(".git/index"), "i\n").unwrap();

        let changes = snapshot.rescan(root, true, b"", stop_read.as_fd()).unwrap();
        assert_eq!(
            summary(changes),
            [
                "attrib attr.txt",
                "created dir newdir",
                "created dir newdir/deeper",
                "created new.txt",
                "created newdir/deeper/g.txt",
                "created newdir/f.txt",
                "modified mod.txt",
                "removed dir olddir",
                "removed dir olddir/sub",
                "removed gone.txt",
                "removed olddir/sub/y.txt",
                "removed olddir/x.txt",
            ]
        );
        // The snapshot is up to date now.
        assert_eq!(
            snapshot.rescan(root, true, b"", stop_read.as_fd()),
            Ok(Vec::new())
        );

        // Only the subtree is compared, and a path next to it with the
        // same start isn't part of it.
        fs::write(root.join("kept/k.txt"), "changed\n").unwrap();
        fs::write(root.join("kept.txt"), "next to it\n").unwrap();
        fs::remove_dir_all(root.join("newdir")).unwrap();
        let changes = snapshot
            .rescan(root, true, b"kept", stop_read.as_fd())
            .unwrap();
        assert_eq!(summary(changes), ["modified kept/k.txt"]);
        let changes = snapshot
            .rescan(root, true, b"newdir", stop_read.as_fd())
            .unwrap();
        assert_eq!(
            summary(changes),
            [
                "removed dir newdir",
                "removed dir newdir/deeper",
                "removed newdir/deeper/g.txt",
                "removed newdir/f.txt"
            ]
        );
        assert_eq!(snapshot.get(b"kept.txt"), None);
    }

    #[test]
    fn falls_back_once_full() {
        let temp = TempDir::new("snapshot-full");
        let root = temp.path();
        let (stop_read, _stop) = stop();
        fs::write(root.join("a.txt"), "a\n").unwrap();
        fs::write(root.join("b.txt"), "b\n").unwrap();
        let mut snapshot = Snapshot::build_limited(root, true, 1, stop_read.as_fd()).unwrap();
        assert_eq!(paths(&snapshot).len(), 1);

        // What the snapshot doesn't have may be old: reported as modified,
        // and a new directory not at all, as before there was a snapshot.
        fs::create_dir(root.join("d")).unwrap();
        fs::write(root.join("c.txt"), "c\n").unwrap();
        let changes = snapshot.rescan(root, true, b"", stop_read.as_fd()).unwrap();
        let known = paths(&snapshot).remove(0);
        let mut wanted = vec!["modified c.txt".to_string()];
        wanted.push(format!(
            "modified {}",
            if known == "a.txt" { "b.txt" } else { "a.txt" }
        ));
        wanted.sort();
        assert_eq!(summary(changes), wanted);
    }

    #[test]
    fn stops_walking_when_asked() {
        let temp = TempDir::new("snapshot-stop");
        fs::write(temp.path().join("a.txt"), "a\n").unwrap();
        let (stop_read, stop) = stop();
        drop(stop);
        assert!(Snapshot::build(temp.path(), true, stop_read.as_fd()).is_err());
    }

    #[test]
    fn never_follows_symlinks() {
        let temp = TempDir::new("snapshot-symlinks");
        let root = temp.path();
        let outside = TempDir::new("snapshot-symlinks-outside");
        fs::write(outside.path().join("secret.txt"), "s\n").unwrap();
        fs::create_dir(root.join("real")).unwrap();
        fs::write(root.join("real/a.txt"), "a\n").unwrap();
        symlink("real", root.join("link")).unwrap();
        symlink(outside.path(), root.join("out")).unwrap();
        symlink("..", root.join("real/up")).unwrap();
        let (stop_read, _stop) = stop();

        let mut snapshot = Snapshot::build(root, true, stop_read.as_fd()).unwrap();
        assert_eq!(
            paths(&snapshot),
            ["link", "out", "real", "real/a.txt", "real/up"]
        );
        assert_eq!(snapshot.get(b"out").unwrap().kind, Kind::Symlink);

        // A symlink is a path of its own, but nothing is looked at through
        // it, even when it is the subtree to rescan.
        assert_eq!(stat(&root.join("link")).unwrap().kind, Kind::Symlink);
        let through = stat(&root.join("out/secret.txt")).unwrap_err();
        assert_eq!(through.raw_os_error(), Some(libc::ELOOP));
        assert!(stat(&root.join("link/a.txt")).is_err());
        assert_eq!(
            snapshot.rescan(root, true, b"out", stop_read.as_fd()),
            Ok(Vec::new())
        );

        // A directory swapped for a symlink: what was below it is gone.
        fs::rename(root.join("real"), root.join("moved")).unwrap();
        symlink(outside.path(), root.join("real")).unwrap();
        let changes = snapshot
            .rescan(root, true, b"real", stop_read.as_fd())
            .unwrap();
        assert_eq!(
            summary(changes),
            ["created real", "removed real/a.txt", "removed real/up"]
        );
    }
}
