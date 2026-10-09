//! The pristine Debian cloud image and raw disk file helpers.

use std::ffi::CString;
use std::fs::{self, File};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha512};

use crate::config::ByteSize;
use crate::paths::Paths;
use crate::status::Status;
use crate::util::{fresh_dir, run, run_output};

const IMAGE_BASE_URL: &str = "https://cloud.debian.org/images/cloud/trixie/latest";
const IMAGE_NAME: &str = "debian-13-genericcloud-arm64";

/// The blocks in which disk images are checked for zeros, which are then
/// left unallocated.
const SPARSE_BLOCK: usize = 64 << 10;

/// Returns the path of the unpacked, checksum-verified Debian raw disk image, downloading
/// it first if the cached copy is missing or outdated. Falls back to the
/// cached image when the checksum list can't be fetched (e.g. offline).
pub fn debian_image(paths: &Paths) -> Result<PathBuf> {
    let dir = paths.downloads_dir();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let tarball = dir.join(format!("{IMAGE_NAME}.tar.xz"));
    let raw = dir.join(format!("{IMAGE_NAME}.raw"));
    // Records the checksum of the tarball `raw` was unpacked from.
    let stamp = dir.join(format!("{IMAGE_NAME}.raw.sha512"));

    let cached = fs::read_to_string(&stamp).ok().filter(|_| raw.exists());

    let expected = match fetch_checksum() {
        Ok(sum) => sum,
        Err(e) if cached.is_none() => return Err(e),
        Err(_) => {
            eprintln!("warning: could not check for a newer Debian image; using the cached one");
            return Ok(raw);
        }
    };
    if cached.is_some_and(|sum| sum.trim() == expected) {
        return Ok(raw);
    }

    // A tarball is only left over if unpacking it failed.
    if !tarball.exists() || sha512_file(&tarball)? != expected {
        download(&format!("{IMAGE_BASE_URL}/{IMAGE_NAME}.tar.xz"), &tarball)?;
        let actual = sha512_file(&tarball)?;
        if actual != expected {
            let _ = fs::remove_file(&tarball);
            bail!("checksum mismatch for {IMAGE_NAME}.tar.xz: expected {expected}, got {actual}");
        }
    }

    Status::Working("Unpacking the Debian image", None).report();
    eprintln!("Unpacking {IMAGE_NAME}.tar.xz");
    let unpack_dir = dir.join("unpack");
    fresh_dir(&unpack_dir)?;
    run(Command::new("tar")
        .arg("-xJf")
        .arg(&tarball)
        .arg("-C")
        .arg(&unpack_dir))?;
    let unpacked = unpack_dir.join("disk.raw");
    ensure!(
        unpacked.exists(),
        "the image archive did not contain disk.raw"
    );
    // The tarball doesn't record holes, so the unpacked image is fully
    // allocated; punching out the zero blocks saves about 2 GB.
    punch_zero_blocks(&unpacked)?;
    fs::rename(&unpacked, &raw)?;
    fs::remove_dir_all(&unpack_dir)?;
    fs::write(&stamp, &expected)?;
    // Only `raw` is needed from now on.
    let _ = fs::remove_file(&tarball);
    Ok(raw)
}

fn fetch_checksum() -> Result<String> {
    let url = format!("{IMAGE_BASE_URL}/SHA512SUMS");
    let sums = run_output(
        Command::new("curl")
            // Without a timeout, a stalled connection would hang here instead
            // of falling back to the cached image.
            .args(["-fsSL", "--connect-timeout", "10", "--max-time", "30", &url]),
    )
    .with_context(|| format!("fetching {url}"))?;
    let wanted = format!("{IMAGE_NAME}.tar.xz");
    String::from_utf8_lossy(&sums)
        .lines()
        .find_map(|line| {
            let (sum, name) = line.split_once(char::is_whitespace)?;
            (name.trim() == wanted).then(|| sum.to_string())
        })
        .with_context(|| format!("{wanted} is not listed in {url}"))
}

fn download(url: &str, dest: &Path) -> Result<()> {
    Status::Working("Downloading the Debian image", None).report();
    eprintln!("Downloading {url}");
    let part = dest.with_extension("part");
    run(Command::new("curl")
        .args(["-fL", "--progress-bar", "-o"])
        .arg(&part)
        .arg(url))?;
    fs::rename(&part, dest)?;
    Ok(())
}

fn sha512_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha512::new();
    let mut buf = vec![0; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Deallocates all 64 KiB blocks of `path` that contain only zeros.
fn punch_zero_blocks(path: &Path) -> Result<()> {
    let mut file = File::options()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut buf = vec![0; SPARSE_BLOCK];
    let mut offset = 0;
    // Start of the current run of zero blocks.
    let mut zero_run: Option<u64> = None;
    loop {
        let n = read_full(&mut file, &mut buf)?;
        // Only whole blocks can be punched; a partial tail stays allocated.
        let zero = n == SPARSE_BLOCK && buf.iter().all(|&b| b == 0);
        match (zero, zero_run) {
            (true, None) => zero_run = Some(offset),
            (false, Some(start)) => {
                punch_hole(&file, start, offset - start)?;
                zero_run = None;
            }
            _ => {}
        }
        if n < SPARSE_BLOCK {
            return Ok(());
        }
        offset += n as u64;
    }
}

fn punch_hole(file: &File, offset: u64, len: u64) -> Result<()> {
    let args = libc::fpunchhole_t {
        fp_flags: 0,
        reserved: 0,
        fp_offset: offset as libc::off_t,
        fp_length: len as libc::off_t,
    };
    // SAFETY: F_PUNCHHOLE takes a pointer to a valid fpunchhole_t.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PUNCHHOLE, &args) } != 0 {
        return Err(std::io::Error::last_os_error()).context("punching a hole in the disk image");
    }
    Ok(())
}

/// Reads until `buf` is full or the end of the file is reached.
fn read_full(file: &mut File, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

/// Copies `src` to `dst` as an APFS copy-on-write clone: instant, and no
/// extra space is used until either file changes. Across volumes, e.g.
/// with `SENDIT_VM_DIR` on an external drive, or on other file systems,
/// it falls back to a sparse copy.
pub fn clone_file(src: &Path, dst: &Path) -> Result<()> {
    let c_src = CString::new(src.as_os_str().as_bytes())?;
    let c_dst = CString::new(dst.as_os_str().as_bytes())?;
    // SAFETY: both arguments are valid NUL-terminated paths.
    if unsafe { libc::clonefile(c_src.as_ptr(), c_dst.as_ptr(), 0) } != 0 {
        let error = std::io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(libc::EXDEV | libc::ENOTSUP)) {
            return copy_sparse(src, dst)
                .with_context(|| format!("copying {} to {}", src.display(), dst.display()));
        }
        return Err(error)
            .with_context(|| format!("cloning {} to {}", src.display(), dst.display()));
    }
    Ok(())
}

/// Copies `src` to a new file `dst`, leaving out the 64 KiB blocks that
/// contain only zeros, so that they stay unallocated where the file system
/// supports it.
fn copy_sparse(src: &Path, dst: &Path) -> Result<()> {
    let mut from = File::open(src)?;
    let to = File::options().write(true).create_new(true).open(dst)?;
    // Sized first: APFS allocates the gap when a write goes past the end,
    // but not the holes a truncate leaves.
    to.set_len(from.metadata()?.len())?;
    let mut buf = vec![0; SPARSE_BLOCK];
    let mut offset = 0;
    loop {
        let n = read_full(&mut from, &mut buf)?;
        if buf[..n].iter().any(|&b| b != 0) {
            to.write_all_at(&buf[..n], offset)?;
        }
        offset += n as u64;
        if n < SPARSE_BLOCK {
            break;
        }
    }
    to.sync_all()?;
    Ok(())
}

/// Grows a raw disk image to `size` without allocating the new space.
pub fn grow_disk(path: &Path, size: ByteSize) -> Result<()> {
    let file = File::options().write(true).open(path)?;
    if file.metadata()?.len() < size.0 {
        file.set_len(size.0)
            .with_context(|| format!("resizing {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::TempDir;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn copies_sparsely() {
        let temp = TempDir::new("copy-sparse");
        let src = temp.path().join("src.raw");
        // APFS fully allocates smaller files that a truncate grew.
        let mut data = vec![0u8; 64 << 20];
        data[100] = 1;
        data.extend_from_slice(&[3; 1000]); // partial tail block
        fs::write(&src, &data).unwrap();

        let dst = temp.path().join("dst.raw");
        copy_sparse(&src, &dst).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), data);
        let allocated = fs::metadata(&dst).unwrap().blocks() * 512;
        assert!(allocated <= 256 << 10, "allocated {allocated} bytes");
        assert!(
            copy_sparse(&src, &dst).is_err(),
            "overwrote {}",
            dst.display()
        );
    }

    #[test]
    fn punches_zero_blocks() {
        let temp = TempDir::new("sparse");
        let path = temp.path().join("disk.raw");
        let mut data = vec![0u8; 4 << 20];
        data[100] = 1;
        data[3 << 20] = 2;
        data.extend_from_slice(&[0; 1000]); // partial tail block
        fs::write(&path, &data).unwrap();

        punch_zero_blocks(&path).unwrap();
        assert_eq!(fs::read(&path).unwrap(), data);
        let allocated = fs::metadata(&path).unwrap().blocks() * 512;
        assert!(allocated <= 256 << 10, "allocated {allocated} bytes");
    }
}
