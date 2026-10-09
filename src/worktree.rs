//! Recognizing linked git worktrees, whose VMs start as copies of the VM of
//! their main worktree.
//!
//! A linked worktree's `.git` is a file, `gitdir: <main>/.git/worktrees/<name>`,
//! and that directory's `gitdir` file points back at the worktree's `.git`.
//! Either may hold a path relative to its own directory
//! (`worktree.useRelativePaths`).
//!
//! The worktree's `.git` is shared with its VM, which can rewrite it when
//! `.git` is exposed, or create one where there is none. Trusted alone, it
//! would let a VM name any other project as its main worktree, and get a
//! copy of that project's VM on its next creation. The back link lives in
//! the main worktree's `.git`, out of the VM's reach, so both must agree.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

/// The most of a link file that is read; a VM could make `.git` huge.
const MAX_LINK_FILE: u64 = 4096;

/// The main worktree of `root`, a canonical path, if `root` is a linked
/// worktree of a non-bare repository and not the main worktree's ancestor.
pub fn main_worktree(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    let gitdir = read_link(&dot_git, "gitdir: ")?;
    let worktrees = gitdir.parent()?;
    let common = worktrees.parent()?;
    if worktrees.file_name()? != "worktrees" || common.file_name()? != ".git" {
        return None;
    }
    let main = common.parent()?;
    // A main worktree inside `root` would be the VM's own doing.
    if main.starts_with(root) || read_link(&gitdir.join("gitdir"), "")? != dot_git {
        return None;
    }
    Some(main.to_path_buf())
}

/// The canonical path in a one-line link file, after `prefix`. Relative
/// paths are relative to the file's directory.
fn read_link(file: &Path, prefix: &str) -> Option<PathBuf> {
    // Not a symlink, nor a FIFO, which would block the read.
    if !fs::symlink_metadata(file).ok()?.is_file() {
        return None;
    }
    let mut text = String::new();
    File::open(file)
        .ok()?
        .take(MAX_LINK_FILE)
        .read_to_string(&mut text)
        .ok()?;
    let target = text.strip_prefix(prefix)?.trim_end_matches(['\n', '\r']);
    if target.is_empty() || target.contains('\n') {
        return None;
    }
    file.parent()?.join(target).canonicalize().ok()
}

#[cfg(test)]
/// A main worktree at `<temp>/main` and the git metadata of a linked
/// worktree `wt` at `<temp>/wt`, as `git worktree add` leaves them.
pub fn layout(temp: &Path) -> (PathBuf, PathBuf) {
    let main = temp.join("main");
    let wt = temp.join("wt");
    let gitdir = main.join(".git/worktrees/wt");
    fs::create_dir_all(&gitdir).unwrap();
    fs::create_dir_all(&wt).unwrap();
    fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
    fs::write(
        gitdir.join("gitdir"),
        format!("{}\n", wt.join(".git").display()),
    )
    .unwrap();
    (main, wt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::TempDir;
    use std::process::Command;

    #[test]
    fn finds_the_main_worktree() {
        let temp = TempDir::new("worktree-main");
        let (main, wt) = layout(temp.path());
        assert_eq!(main_worktree(&wt), Some(main.clone()));
        assert_eq!(main_worktree(&main), None);
        assert_eq!(main_worktree(temp.path()), None);
    }

    #[test]
    fn follows_relative_links() {
        let temp = TempDir::new("worktree-relative");
        let (main, wt) = layout(temp.path());
        fs::write(wt.join(".git"), "gitdir: ../main/.git/worktrees/wt\n").unwrap();
        fs::write(
            main.join(".git/worktrees/wt/gitdir"),
            "../../../../wt/.git\n",
        )
        .unwrap();
        assert_eq!(main_worktree(&wt), Some(main));
    }

    #[test]
    fn needs_the_link_back() {
        let temp = TempDir::new("worktree-back-link");
        let (main, wt) = layout(temp.path());
        // Another directory claiming to be the worktree `wt`.
        let other = temp.path().join("other");
        fs::create_dir(&other).unwrap();
        fs::copy(wt.join(".git"), other.join(".git")).unwrap();
        assert_eq!(main_worktree(&other), None);

        fs::remove_file(main.join(".git/worktrees/wt/gitdir")).unwrap();
        assert_eq!(main_worktree(&wt), None);
    }

    #[test]
    fn ignores_what_only_the_project_vouches_for() {
        let temp = TempDir::new("worktree-inside");
        // A repository planted inside the project, linking back to it.
        let (_, wt) = layout(temp.path());
        let planted = wt.join("sub");
        let gitdir = planted.join(".git/worktrees/wt");
        fs::create_dir_all(&gitdir).unwrap();
        fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
        fs::write(
            gitdir.join("gitdir"),
            format!("{}\n", wt.join(".git").display()),
        )
        .unwrap();
        assert_eq!(main_worktree(&wt), None);
    }

    #[test]
    fn ignores_submodules() {
        let temp = TempDir::new("worktree-submodule");
        let modules = temp.path().join("super/.git/modules/sub");
        let sub = temp.path().join("super/sub");
        fs::create_dir_all(&modules).unwrap();
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join(".git"), format!("gitdir: {}\n", modules.display())).unwrap();
        assert_eq!(main_worktree(&sub), None);
    }

    #[test]
    fn understands_git() {
        let temp = TempDir::new("worktree-git");
        let main = temp.path().join("main");
        fs::create_dir(&main).unwrap();
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(&main)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap()
        };
        assert!(git(&["init", "-q"]).status.success());
        let commit = ["-c", "user.name=t", "-c", "user.email=t@t", "commit"];
        assert!(
            git(&[&commit[..], &["-q", "--allow-empty", "-m", "t"]].concat())
                .status
                .success()
        );
        assert!(
            git(&["worktree", "add", "-q", "--detach", "../wt"])
                .status
                .success()
        );
        let wt = temp.path().join("wt");
        assert_eq!(main_worktree(&wt), Some(main));
    }
}
