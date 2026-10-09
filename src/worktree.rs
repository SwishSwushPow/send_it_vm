//! Linked git worktrees, whose VMs start as copies of the VM of their main
//! worktree: recognizing them, and making them for `sendit run --worktree`.
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

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};

use crate::util;

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

/// The worktree of `branch` in the git repository at `dir`, made if
/// needed: the one `branch` is checked out in, else a new one at `path`,
/// by default `<main>.worktrees/<branch>` next to the main worktree. A new
/// branch starts at `from`, else HEAD; one that only a single remote has
/// tracks it, as with `git checkout`.
pub fn open(dir: &Path, branch: &str, from: Option<&str>, path: Option<&Path>) -> Result<PathBuf> {
    let top = git(dir, &["rev-parse", "--show-toplevel"])
        .with_context(|| format!("finding the git repository of {}", dir.display()))?;
    let top = PathBuf::from(OsString::from_vec(top)).canonicalize()?;
    let main = if top.join(".git").is_dir() {
        top
    } else {
        main_worktree(&top)
            .with_context(|| format!("can't find the main worktree of {}", top.display()))?
    };
    git(&main, &["check-ref-format", "--branch", branch])
        .with_context(|| format!("{branch:?} is not a valid branch name"))?;
    let exists = |what: &str| format!("{branch} {what}; --from only applies to new branches");

    let list = git(&main, &["worktree", "list", "--porcelain"])?;
    if let Some(existing) = worktree_of(&String::from_utf8_lossy(&list), branch) {
        ensure!(from.is_none(), "{}", exists("is checked out already"));
        ensure!(
            existing.exists(),
            "the worktree of {branch}, {}, is gone; `git worktree prune` forgets it",
            existing.display()
        );
        let existing = existing.canonicalize()?;
        if let Some(path) = path {
            ensure!(
                path.canonicalize().ok().as_ref() == Some(&existing),
                "{branch} is checked out in {} already",
                existing.display()
            );
        }
        eprintln!("Using the worktree of {branch} in {}", existing.display());
        return Ok(existing);
    }

    let path = match path {
        Some(path) => std::path::absolute(path)?,
        None => default_path(&main, branch),
    };
    let local = git(
        &main,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_ok();
    let remotes = git(
        &main,
        &[
            "for-each-ref",
            "--format=%(refname)",
            &format!("refs/remotes/*/{branch}"),
        ],
    )?;
    let remote = remotes
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .count()
        == 1;
    let mut add = Command::new("git");
    add.arg("-C").arg(&main).args(["worktree", "add"]);
    if local {
        ensure!(from.is_none(), "{}", exists("exists already"));
        add.arg(&path).arg(branch);
    } else if remote && from.is_none() {
        add.arg(&path).arg(branch);
    } else {
        add.args(["-b", branch]).arg(&path).args(from);
    }
    util::run(&mut add)?;
    Ok(path.canonicalize()?)
}

/// `<main>.worktrees/<branch>` next to the main worktree, with each `/` of
/// the branch name replaced by `-`: the folder name also names the VM.
fn default_path(main: &Path, branch: &str) -> PathBuf {
    let name = main
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or("root".into());
    let parent = main.parent().unwrap_or(main);
    parent
        .join(format!("{name}.worktrees"))
        .join(branch.replace('/', "-"))
}

/// The worktree that `branch` is checked out in, from the output of
/// `git worktree list --porcelain`.
fn worktree_of(list: &str, branch: &str) -> Option<PathBuf> {
    let wanted = format!("refs/heads/{branch}");
    let mut path = None;
    for line in list.lines() {
        if let Some(worktree) = line.strip_prefix("worktree ") {
            path = Some(worktree);
        } else if line.strip_prefix("branch ") == Some(&wanted) {
            return path.map(PathBuf::from);
        }
    }
    None
}

/// Runs git in `dir` and returns its output without the final newline.
fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut output = util::run_output(Command::new("git").arg("-C").arg(dir).args(args))?;
    if output.last() == Some(&b'\n') {
        output.pop();
    }
    Ok(output)
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

    /// Runs git in `dir` without the user's config, and checks it succeeds.
    fn git_in(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    /// A repository at `<temp>/repo` with one commit.
    fn repo(temp: &Path) -> PathBuf {
        let repo = temp.join("repo");
        fs::create_dir(&repo).unwrap();
        git_in(&repo, &["init", "-q"]);
        let commit = ["-c", "user.name=t", "-c", "user.email=t@t", "commit"];
        git_in(
            &repo,
            &[&commit[..], &["-q", "--allow-empty", "-m", "t"]].concat(),
        );
        repo
    }

    #[test]
    fn understands_git() {
        let temp = TempDir::new("worktree-git");
        let repo = repo(temp.path());
        git_in(&repo, &["worktree", "add", "-q", "--detach", "../wt"]);
        assert_eq!(main_worktree(&temp.path().join("wt")), Some(repo));
    }

    #[test]
    fn opens_worktrees_for_branches() {
        let temp = TempDir::new("worktree-open");
        let repo = repo(temp.path());
        let branch = |dir: &Path| git_in(dir, &["branch", "--show-current"]);

        // A new branch, next to the main worktree.
        let wt = open(&repo, "feature/x", None, None).unwrap();
        assert_eq!(wt, temp.path().join("repo.worktrees/feature-x"));
        assert_eq!(branch(&wt), "feature/x");
        // Its worktree, from the main worktree or the worktree itself.
        assert_eq!(open(&repo, "feature/x", None, None).unwrap(), wt);
        assert_eq!(open(&wt, "feature/x", None, Some(&wt)).unwrap(), wt);
        assert!(open(&repo, "feature/x", Some("HEAD"), None).is_err());
        assert!(open(&repo, "feature/x", None, Some(&temp.path().join("y"))).is_err());

        // An existing branch without a worktree.
        git_in(&repo, &["branch", "old"]);
        assert!(open(&repo, "old", Some("HEAD"), None).is_err());
        let old = open(&repo, "old", None, None).unwrap();
        assert_eq!(branch(&old), "old");

        // Elsewhere, from a given commit.
        let elsewhere = temp.path().join("elsewhere");
        assert_eq!(
            open(&repo, "new", Some("old"), Some(&elsewhere)).unwrap(),
            elsewhere
        );
        assert_eq!(branch(&elsewhere), "new");

        assert!(open(&repo, "a..b", None, None).is_err());
        assert!(open(temp.path(), "x", None, None).is_err());
    }

    #[test]
    fn finds_the_worktree_of_a_branch() {
        let list = "worktree /r\nHEAD 1\nbranch refs/heads/main\n\n\
                    worktree /r.worktrees/a-b\nHEAD 2\nbranch refs/heads/a/b\n\n\
                    worktree /d\nHEAD 3\ndetached\n";
        assert_eq!(
            worktree_of(list, "a/b"),
            Some(PathBuf::from("/r.worktrees/a-b"))
        );
        assert_eq!(worktree_of(list, "main"), Some(PathBuf::from("/r")));
        assert_eq!(worktree_of(list, "a"), None);
    }
}
