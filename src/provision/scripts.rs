//! The custom provisioning scripts of an image. An image with custom scripts
//! has an image file, `~/.config/sendit/images/<name>.sh`, which provision.sh
//! runs. It calls the shared scripts in `~/.config/sendit/provision-scripts/`
//! with `sendit-provision NAME`, and they can call each other the same way;
//! each runs at most once per build. sendit finds the calls before a build
//! (see `CustomScript::calls`), so that it can refuse names it can't find and
//! cycles, put only the scripts that run on the seed ISO, and notice when one
//! of them changes. A script can ask for host directories to be shared with
//! the VMs made from the image, in comment lines at its top:
//! `# sendit-mount: HOST[:GUEST][:ro|rw]`.
//!
//! The mounts are recorded in the image's marker when it is built, and
//! project VMs keep a copy of it, so editing a script's mounts changes
//! only the VMs made from a later build.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::shell::{is_operator, shell_words};
use crate::config::MountSpec;
use crate::paths::{ImageName, Paths};
use crate::util;

/// Starts a comment line in which a custom script asks for a mount.
const MOUNT_COMMENT: &str = "# sendit-mount:";

/// The command with which custom scripts run a shared script.
pub(super) const RUN_COMMAND: &str = "sendit-provision";

/// A custom provisioning script the base image was built with.
#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct ScriptStamp {
    pub(super) name: String,
    /// Of the script below its header (see `CustomScript::split_header`),
    /// so that changing its description doesn't mark the image. Its mounts
    /// are compared on their own.
    sha256: String,
}

/// A mount that a custom script asks for, as written, and the script's
/// name.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ScriptMount {
    pub script: String,
    pub mount: MountSpec,
}

/// A custom provisioning script: the image file of the image being built,
/// or a shared script from `Paths::provision_scripts_dir` that it calls.
pub(super) struct CustomScript {
    /// For messages and the marker: `images/rust.sh` for an image file, the
    /// file name for a shared script.
    pub(super) name: String,
    /// The file name, e.g. `cargo.sh`, which `sendit-provision` takes for a
    /// shared script.
    pub(super) file: String,
    pub(super) content: Vec<u8>,
}

impl CustomScript {
    pub(super) fn stamp(&self) -> ScriptStamp {
        ScriptStamp {
            name: self.name.clone(),
            sha256: hex::encode(Sha256::digest(self.split_header().1)),
        }
    }

    /// The header, the blank and comment lines at the top of the script
    /// before its first command, and the rest. provision.sh and
    /// `sendit-provision` run scripts with `bash FILE`, so the header, shebang
    /// included, does nothing. Comments
    /// further down may not be comments, e.g. in a heredoc.
    fn split_header(&self) -> (&[u8], &[u8]) {
        let mut end = 0;
        for line in self.content.split_inclusive(|&b| b == b'\n') {
            let text = line.trim_ascii();
            if !text.is_empty() && !text.starts_with(b"#") {
                break;
            }
            end += line.len();
        }
        self.content.split_at(end)
    }

    /// The mounts asked for in the script's header. Their host paths must be
    /// absolute or start with `~`, as in the config file.
    fn mounts(&self) -> Result<Vec<MountSpec>> {
        let header = String::from_utf8_lossy(self.split_header().0);
        let mut mounts = Vec::new();
        for line in header.lines().map(str::trim) {
            let Some(spec) = line.strip_prefix(MOUNT_COMMENT) else {
                continue;
            };
            let spec: MountSpec = spec
                .trim()
                .parse()
                .with_context(|| format!("in the custom script {}", self.name))?;
            ensure!(
                spec.host.is_absolute() || spec.host.starts_with("~"),
                "in the custom script {}: mount {}: host path must be absolute or start with ~",
                self.name,
                spec.host.display()
            );
            mounts.push(spec);
        }
        Ok(mounts)
    }

    /// The names of the shared scripts that this one calls with
    /// `sendit-provision`, in the order they appear. Every `sendit-provision`
    /// word counts wherever it is, e.g. in an `if`, but not in a comment, in
    /// quotes or in the body of a here-document, except in a command
    /// substitution there. The name after it must be a literal file name,
    /// which can be resolved without running anything.
    fn calls(&self) -> Result<Vec<String>> {
        let text = String::from_utf8_lossy(&self.content);
        let lines: Vec<_> = text.lines().collect();
        let mut calls = Vec::new();
        let mut words = shell_words(&text).into_iter();
        while let Some((line, word)) = words.next() {
            if word != RUN_COMMAND {
                continue;
            }
            let name = words
                .next()
                .map(|(_, name)| name)
                .filter(|name| !is_operator(name))
                .unwrap_or_default();
            check_script_name(&name).with_context(|| {
                let line = lines.get(line).copied().unwrap_or_default();
                format!("in the custom script {}: `{}`", self.name, line.trim())
            })?;
            calls.push(name);
        }
        Ok(calls)
    }
}

/// Fails unless `name`, which a script passes to `sendit-provision`, is a plain
/// file name. It is only ever looked up in the listing of
/// `Paths::provision_scripts_dir`, never joined onto a path.
fn check_script_name(name: &str) -> Result<()> {
    ensure!(!name.is_empty(), "{RUN_COMMAND} names no script");
    ensure!(
        !name.contains('/'),
        "{RUN_COMMAND} {name}: the script must be named without a directory, \
         as a file in the provision-scripts directory"
    );
    ensure!(
        !name.starts_with('.'),
        "{RUN_COMMAND} {name}: script names can't start with a dot"
    );
    ensure!(
        !name.contains(|c: char| c.is_control()),
        "{RUN_COMMAND} {name:?}: script names can't have control characters"
    );
    ensure!(
        name.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c)),
        "{RUN_COMMAND} {name}: expected a literal file name, without quotes, \
         variables or other shell syntax"
    );
    Ok(())
}

/// Fails if `Paths::provision_scripts_dir` has subfolders: the scripts in
/// them, the old way to give an image scripts of its own, would be left out
/// without a word.
pub fn check_no_subfolders(paths: &Paths) -> Result<()> {
    for entry in util::read_dir(&paths.provision_scripts_dir())? {
        let path = entry?.path();
        let hidden = path
            .file_name()
            .is_some_and(|name| name.as_encoded_bytes().starts_with(b"."));
        ensure!(
            hidden || !path.is_dir(),
            "{}/: subfolders are no longer supported; images are defined by {} \
             (see the CHANGELOG)",
            paths.display(&path),
            paths.display(&paths.image_files_dir().join("NAME.sh"))
        );
    }
    Ok(())
}

/// Where the image file of `image` points if it is a symlink to nothing,
/// e.g. into a dotfiles repository that isn't there (yet). `custom_scripts`
/// finds no image file then, so `provision` refuses to build the image
/// without its scripts.
pub(super) fn dangling_image_file(paths: &Paths, image: &ImageName) -> Option<PathBuf> {
    let file = paths.image_file(image);
    let target = fs::read_link(&file).ok()?;
    match fs::metadata(&file) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(target),
        _ => None,
    }
}

/// Says that the image file of `image` points to `target`, which doesn't
/// exist.
pub(super) fn describe_dangling(paths: &Paths, image: &ImageName, target: &Path) -> String {
    format!(
        "the image file {} points to {}, which doesn't exist",
        paths.display(&paths.image_file(image)),
        paths.display(target)
    )
}

/// The custom scripts for `image`: its image file first, then the shared
/// scripts that it calls, directly or through other scripts, in the order
/// they are first found. None without an image file, or with one that is a
/// symlink to nothing (see `dangling_image_file`).
pub(super) fn custom_scripts(paths: &Paths, image: &ImageName) -> Result<Vec<CustomScript>> {
    let dir = paths.provision_scripts_dir();
    check_no_subfolders(paths)?;
    let image_file = paths.image_file(image);
    let content = match fs::read(&image_file) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", image_file.display())),
    };
    let main = CustomScript {
        name: format!("images/{image}.sh"),
        file: format!("{image}.sh"),
        content,
    };

    let mut shared: BTreeMap<_, _> = script_files(&dir)?.into_iter().collect();
    let mut scripts = vec![main];
    add_calls(paths, 0, &mut shared, &mut scripts, &mut Vec::new())?;
    Ok(scripts)
}

/// Appends the scripts that `scripts[caller]` calls to `scripts`, each
/// followed by those it calls in turn, taking them out of `shared`. Those
/// already in `scripts` are left out. `chain` names the shared scripts that
/// led to the caller: calling one of them again would never end.
fn add_calls(
    paths: &Paths,
    caller: usize,
    shared: &mut BTreeMap<String, Vec<u8>>,
    scripts: &mut Vec<CustomScript>,
    chain: &mut Vec<String>,
) -> Result<()> {
    for name in scripts[caller].calls()? {
        if let Some(start) = chain.iter().position(|n| *n == name) {
            bail!(
                "custom scripts call each other in a cycle: {} -> {name}",
                chain[start..].join(" -> ")
            );
        }
        if scripts[1..].iter().any(|s| s.file == name) {
            continue;
        }
        let Some(content) = shared.remove(&name) else {
            bail!(
                "the custom script {} calls `{RUN_COMMAND} {name}`, but there is no {}",
                scripts[caller].name,
                paths.display(&paths.provision_scripts_dir().join(&name))
            );
        };
        scripts.push(CustomScript {
            name: name.clone(),
            file: name.clone(),
            content,
        });
        chain.push(name);
        add_calls(paths, scripts.len() - 1, shared, scripts, chain)?;
        chain.pop();
    }
    Ok(())
}

/// The mounts that `scripts` ask for, in order.
pub(super) fn mounts_of(scripts: &[CustomScript]) -> Result<Vec<ScriptMount>> {
    let mut mounts = Vec::new();
    for script in scripts {
        for mount in script.mounts()? {
            mounts.push(ScriptMount {
                script: script.name.clone(),
                mount,
            });
        }
    }
    Ok(mounts)
}

/// Lists `mounts` one per line, as written, each with its script.
pub(super) fn describe_mounts(mounts: &[ScriptMount]) -> String {
    let mut text = "VMs made from the image share what the scripts ask for:".to_string();
    for ScriptMount { script, mount } in mounts {
        let _ = write!(text, "\n  {mount}  ({script})");
    }
    text
}

/// Names the image file of `image`, and lists the shared scripts it runs,
/// directly or through other scripts (`scripts` after the image file), one
/// per line.
pub(super) fn describe_scripts(
    paths: &Paths,
    image: &ImageName,
    scripts: &[CustomScript],
) -> String {
    let mut text = format!("Image file: {}", paths.display(&paths.image_file(image)));
    if scripts.len() > 1 {
        let _ = write!(
            text,
            "\nScripts it runs, from {}:",
            paths.display(&paths.provision_scripts_dir())
        );
        for script in &scripts[1..] {
            let _ = write!(text, "\n  {}", script.file);
        }
    }
    text
}

/// The names and contents of the `*.sh` files in `dir`.
fn script_files(dir: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    let mut scripts = Vec::new();
    for entry in util::read_dir(dir)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with('.') || !name.ends_with(".sh") || !path.is_file() {
            continue;
        }
        ensure!(
            !name.contains(|c: char| c.is_control()),
            "custom provisioning script {:?} has control characters in its name",
            path.display()
        );
        let content = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        scripts.push((name.to_string(), content));
    }
    Ok(scripts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provision::tests::{image, write_config};
    use crate::util::TempDir;

    /// The names of the custom scripts for `image`, in order.
    fn script_names(paths: &Paths, image: &ImageName) -> Vec<String> {
        custom_scripts(paths, image)
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect()
    }

    fn script_error(paths: &Paths, image: &ImageName) -> String {
        match custom_scripts(paths, image) {
            Ok(_) => panic!("no error"),
            Err(e) => format!("{e:#}"),
        }
    }

    #[test]
    fn resolves_the_scripts_an_image_file_calls() {
        let home = TempDir::new("scripts");
        let paths = Paths::new(home.path().to_path_buf());
        let rust = image("rust");
        // Without an image file, shared scripts don't run.
        write_config(&paths, "provision-scripts/unused.sh", "echo unused");
        assert!(custom_scripts(&paths, &rust).unwrap().is_empty());

        write_config(
            &paths,
            "images/rust.sh",
            "#!/bin/bash\n\
             # sendit-provision commented-out.sh\n\
             sendit-provision rust.sh  # the shared one, not this file\n\
             sudo apt-get install -y postgresql-client # sendit-provision no.sh\n\
             (sendit-provision helix.sh)\n\
             if [ -n \"$CI\" ]; then sendit-provision ci.sh; fi\n\
             sendit-provision rust.sh\n",
        );
        write_config(&paths, "provision-scripts/rust.sh", "curl | sh");
        write_config(
            &paths,
            "provision-scripts/helix.sh",
            "  sendit-provision rust.sh\nsendit-provision grammars.sh&&echo done\n",
        );
        write_config(
            &paths,
            "provision-scripts/grammars.sh",
            "hx --grammar fetch",
        );
        write_config(&paths, "provision-scripts/ci.sh", "true");
        // The image file first, then each script it calls, directly or not,
        // once, in the order they are first found.
        assert_eq!(
            script_names(&paths, &rust),
            [
                "images/rust.sh",
                "rust.sh",
                "helix.sh",
                "grammars.sh",
                "ci.sh"
            ]
        );
        let scripts = custom_scripts(&paths, &rust).unwrap();
        assert_eq!(scripts[0].file, "rust.sh");
        assert_eq!(scripts[1].content, b"curl | sh");
        // Other images don't get them.
        assert!(custom_scripts(&paths, &image("go")).unwrap().is_empty());
    }

    #[test]
    fn rejects_cycles_and_missing_scripts() {
        let home = TempDir::new("script-cycles");
        let paths = Paths::new(home.path().to_path_buf());
        let rust = image("rust");
        write_config(&paths, "images/rust.sh", "sendit-provision a.sh\n");
        write_config(&paths, "provision-scripts/a.sh", "sendit-provision b.sh\n");
        write_config(&paths, "provision-scripts/b.sh", "sendit-provision c.sh\n");
        write_config(&paths, "provision-scripts/c.sh", "sendit-provision a.sh\n");
        let error = script_error(&paths, &rust);
        assert!(
            error.contains("call each other in a cycle: a.sh -> b.sh -> c.sh -> a.sh"),
            "{error}"
        );
        write_config(&paths, "provision-scripts/c.sh", "sendit-provision c.sh\n");
        let error = script_error(&paths, &rust);
        assert!(error.contains("cycle: c.sh -> c.sh"), "{error}");

        write_config(
            &paths,
            "provision-scripts/c.sh",
            "sendit-provision missing.sh\n",
        );
        assert_eq!(
            script_error(&paths, &rust),
            "the custom script c.sh calls `sendit-provision missing.sh`, but there is no \
             ~/.config/sendit/provision-scripts/missing.sh"
        );
        // Only `*.sh` files that aren't hidden can be called.
        write_config(
            &paths,
            "provision-scripts/c.sh",
            "sendit-provision notes.txt\n",
        );
        write_config(&paths, "provision-scripts/notes.txt", "");
        assert!(script_error(&paths, &rust).contains("there is no"));
    }

    #[test]
    fn calls_scripts_only_by_literal_file_names() {
        let home = TempDir::new("script-names");
        let paths = Paths::new(home.path().to_path_buf());
        let rust = image("rust");
        // Files that a name with a path could reach.
        write_config(&paths, "secret.sh", "");
        write_config(&paths, "provision-scripts/sub.sh", "");
        write_config(&paths, "provision-scripts/.hidden.sh", "");
        for (call, reason) in [
            ("sendit-provision ../secret.sh", "without a directory"),
            ("sendit-provision ./sub.sh", "without a directory"),
            ("sendit-provision /etc/passwd", "without a directory"),
            ("sendit-provision ..", "can't start with a dot"),
            ("sendit-provision .hidden.sh", "can't start with a dot"),
            ("sendit-provision sub\u{1b}.sh", "control characters"),
            ("sendit-provision \"$x\"", "literal file name"),
            ("sendit-provision $x", "literal file name"),
            ("sendit-provision 'sub.sh'", "literal file name"),
            ("sendit-provision `echo sub.sh`", "literal file name"),
            ("sendit-provision sub*.sh", "literal file name"),
            ("sendit-provision", "names no script"),
            ("x && sendit-provision; true", "names no script"),
        ] {
            write_config(&paths, "images/rust.sh", &format!("echo\n{call}\n"));
            let error = script_error(&paths, &rust);
            assert!(
                error.contains(reason) && error.contains("images/rust.sh"),
                "{call}: {error}"
            );
        }
        // The same goes for the scripts it calls.
        write_config(&paths, "images/rust.sh", "sendit-provision sub.sh\n");
        write_config(
            &paths,
            "provision-scripts/sub.sh",
            "sendit-provision ../secret.sh",
        );
        let error = script_error(&paths, &rust);
        assert!(error.contains("in the custom script sub.sh"), "{error}");
    }

    #[test]
    fn finds_calls_outside_of_quotes_and_here_documents() {
        let calls = |text: &str| {
            CustomScript {
                name: "x.sh".into(),
                file: "x.sh".into(),
                content: text.into(),
            }
            .calls()
            .map_err(|e| format!("{e:#}"))
        };
        let found = |text: &str| calls(text).unwrap();
        // A `#` in quotes doesn't start a comment.
        assert_eq!(found("echo \"step #1\"; sendit-provision a.sh\n"), ["a.sh"]);
        assert_eq!(
            found("echo 'it''s #2' && sendit-provision a.sh\n"),
            ["a.sh"]
        );
        // Quoted, the word is text, also over several lines.
        assert!(found("echo \"Now run sendit-provision by hand\"\n").is_empty());
        assert!(found("echo 'first\nsendit-provision by hand'\n").is_empty());
        // But a command substitution in double quotes runs.
        assert_eq!(
            found("v=\"$(sendit-provision a.sh)\"\necho \"`sendit-provision b.sh`\"\n"),
            ["a.sh", "b.sh"]
        );
        // The body of a here-document is text, apostrophes included, also
        // with its lines indented by tabs (<<-), but a here-string isn't one.
        assert_eq!(
            found("cat > f <<'EOF'\nDon't sendit-provision this.\nEOF\nsendit-provision a.sh\n"),
            ["a.sh"]
        );
        assert_eq!(
            found(
                "cat <<-END; sendit-provision a.sh\n\tsendit-provision no.sh\n\tEND\nsendit-provision b.sh\n"
            ),
            ["a.sh", "b.sh"]
        );
        assert_eq!(found("cat <<<'text'\nsendit-provision a.sh\n"), ["a.sh"]);
        // A line continuation joins lines; a newline ends the command.
        assert_eq!(found("sendit-provision \\\n  a.sh\n"), ["a.sh"]);
        assert!(
            calls("sendit-provision\na.sh\n")
                .unwrap_err()
                .contains("names no script")
        );
        // Errors quote the line of the call, also after a string of several.
        let error = calls("echo \"\n\"\nsendit-provision $x\n").unwrap_err();
        assert!(error.contains("`sendit-provision $x`"), "{error}");
    }

    #[test]
    fn finds_the_calls_in_the_bundled_examples() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let calls = |path: &str| {
            CustomScript {
                name: path.into(),
                file: path.into(),
                content: fs::read(root.join(path)).unwrap(),
            }
            .calls()
            .unwrap()
        };
        assert_eq!(
            calls("images/example.sh"),
            ["rust.sh", "helix.sh", "claude-code.sh", "claude-md.sh"]
        );
        for (name, _) in script_files(&root.join("provision-scripts")).unwrap() {
            let expected: &[&str] = match name.as_str() {
                "helix.sh" => &["rust.sh"],
                _ => &[],
            };
            assert_eq!(
                calls(&format!("provision-scripts/{name}")),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn refuses_subfolders_of_shared_scripts() {
        let home = TempDir::new("script-subfolders");
        let paths = Paths::new(home.path().to_path_buf());
        // A repository's metadata is fine.
        fs::create_dir_all(paths.provision_scripts_dir().join(".git")).unwrap();
        assert!(custom_scripts(&paths, &image("rust")).unwrap().is_empty());
        // An image's own scripts in the old layout, even without image files.
        write_config(&paths, "provision-scripts/rust/10-cargo.sh", "cargo");
        assert_eq!(
            script_error(&paths, &ImageName::default()),
            "~/.config/sendit/provision-scripts/rust/: subfolders are no longer supported; \
             images are defined by ~/.config/sendit/images/NAME.sh (see the CHANGELOG)"
        );
    }

    #[test]
    fn describes_custom_scripts() {
        let home = TempDir::new("describe");
        let paths = Paths::new(home.path().to_path_buf());
        let rust = image("rust");
        let describe = || describe_scripts(&paths, &rust, &custom_scripts(&paths, &rust).unwrap());

        write_config(&paths, "images/rust.sh", "cargo install ripgrep\n");
        assert_eq!(describe(), "Image file: ~/.config/sendit/images/rust.sh");

        write_config(
            &paths,
            "images/rust.sh",
            "sendit-provision rust.sh\nsendit-provision helix.sh\n",
        );
        write_config(&paths, "provision-scripts/rust.sh", "rustup");
        write_config(
            &paths,
            "provision-scripts/helix.sh",
            "sendit-provision grammars.sh",
        );
        write_config(&paths, "provision-scripts/grammars.sh", "hx");
        assert_eq!(
            describe(),
            "Image file: ~/.config/sendit/images/rust.sh\n\
             Scripts it runs, from ~/.config/sendit/provision-scripts:\n  \
             rust.sh\n  \
             helix.sh\n  \
             grammars.sh"
        );
    }

    #[test]
    fn stamps_scripts_without_their_header() {
        let stamp = |content: &str| {
            CustomScript {
                name: "tool.sh".into(),
                file: "tool.sh".into(),
                content: content.into(),
            }
            .stamp()
            .sha256
        };
        let body = "set -eu\n# a comment, or maybe not:\ncat <<EOF\n# config\nEOF\n";
        let original = stamp(&format!("#!/bin/sh\n# Installs a tool.\n\n{body}"));
        assert_eq!(stamp(body), original);
        assert_eq!(
            stamp(&format!(
                "#!/bin/bash\n  # Installs a tool.\r\n# sendit-mount: ~/x\n{body}"
            )),
            original
        );
        // Below the header, every byte counts.
        assert_ne!(stamp(&body.replace("# config", "# other")), original);
        assert_ne!(stamp(&format!("{body}\n")), original);
        assert_ne!(stamp(&format!("echo\n{body}")), original);
        // A script of comments only does nothing.
        assert_eq!(stamp("# just notes\n"), stamp(""));
    }

    #[test]
    fn reads_mounts_from_the_top_of_scripts() {
        let home = TempDir::new("script-mounts");
        let paths = Paths::new(home.path().to_path_buf());
        let rust = image("rust");
        let mounts = || custom_scripts(&paths, &rust).and_then(|s| mounts_of(&s));
        let write = |path: &str, content: &str| write_config(&paths, path, content);

        write(
            "images/rust.sh",
            "#!/bin/bash\n\
             # Rust, with a shared registry.\n\
             # sendit-mount: ~/.cargo/registry:/home/dev/.cargo/registry\n\
             \n  # sendit-mount: /opt/data:rw  \n\
             sendit-provision notes.sh\n\
             # sendit-mount: ~/after-the-first-command\n",
        );
        write("provision-scripts/notes.sh", "# sendit-mount: ~/notes\n");
        write("provision-scripts/unused.sh", "# sendit-mount: ~/unused\n");
        write("images/go.sh", "# sendit-mount: ~/go\n");
        let described: Vec<_> = mounts()
            .unwrap()
            .iter()
            .map(|m| format!("{} {}", m.script, m.mount))
            .collect();
        assert_eq!(
            described,
            [
                "images/rust.sh ~/.cargo/registry:/home/dev/.cargo/registry:ro",
                "images/rust.sh /opt/data:rw",
                "notes.sh ~/notes:ro",
            ]
        );
        assert_eq!(
            describe_mounts(&mounts().unwrap()),
            "VMs made from the image share what the scripts ask for:\n  \
             ~/.cargo/registry:/home/dev/.cargo/registry:ro  (images/rust.sh)\n  \
             /opt/data:rw  (images/rust.sh)\n  \
             ~/notes:ro  (notes.sh)"
        );

        // Mistakes stop a build and name the script, and so does a relative
        // host path, which would depend on where `sendit run` happens to be
        // started.
        let error = || format!("{:#}", mounts().unwrap_err());
        write("provision-scripts/notes.sh", "# sendit-mount: a:b:c:d\n");
        let message = error();
        assert!(message.contains("custom script notes.sh"), "{message}");
        write("provision-scripts/notes.sh", "# sendit-mount: data:/data\n");
        let message = error();
        assert!(
            message.contains("custom script notes.sh") && message.contains("absolute"),
            "{message}"
        );
    }
}
