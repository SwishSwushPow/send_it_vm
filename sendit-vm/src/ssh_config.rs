//! `sendit ssh-config`: lets ssh, and the programs that use it such as
//! Rex, reach each running VM as `sendit-<name>`, e.g. `ssh sendit-app`, or
//! `ssh://sendit-app` in Rex.
//!
//! sendit writes one `Host sendit-*` block to a file of its own, next to
//! the SSH keys, and offers to include that file at the top of
//! `~/.ssh/config`. The block stays the same as VMs come and go, boot with
//! other IP addresses and are renamed: its ProxyCommand, `sendit
//! ssh-proxy`, finds the VM by its name for each connection and connects to
//! it as `sendit ssh` does, checking its host key against the VM's own
//! known_hosts. ssh's connection inside that one reaches the same server,
//! so it skips a check of its own, which couldn't keep up anyway: a name
//! moves to a new VM with `sendit reset`.

use std::fs::{self, DirBuilder};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::commands::{Declined, PASS_YES, confirm};
use crate::config::GUEST_USER;
use crate::paths::{Paths, VmName};
use crate::project_vm::{self, quote};
use crate::util;

/// What the host names of VMs start with; their names follow.
pub const HOST_PREFIX: &str = "sendit-";

/// The name of the VM whose host name is `host`.
pub fn vm_name(host: &str) -> Result<VmName> {
    let name = host.strip_prefix(HOST_PREFIX).with_context(|| {
        format!("{host:?} is not the host name of a sendit VM, {HOST_PREFIX}<name>")
    })?;
    name.parse()
        .with_context(|| format!("{host:?} is not the host name of a sendit VM"))
}

/// Writes the Host block for the VMs, and adds the line that includes it
/// to the top of `~/.ssh/config` once the user agrees, or right away with
/// `yes`. Fails with `Declined` if the user doesn't.
pub fn setup(paths: &Paths, yes: bool) -> Result<()> {
    let (_, key) = project_vm::login(paths, false)?;
    let sendit = std::env::current_exe().context("finding the sendit binary")?;
    let file = paths.ssh_config();
    util::write_atomic(&file, host_block(&sendit, &key)?.as_bytes())?;

    let user_config = paths.home().join(".ssh/config");
    let shown = paths.display(&user_config);
    let target = resolve_link(&user_config)?;
    let old = util::if_exists(fs::read_to_string(&target))
        .with_context(|| format!("reading {}", target.display()))?;
    let include = format!("Include {}", config_word(&paths.display(&file))?);
    let forms = [paths.display(&file), file.display().to_string()];
    let old_text = old.as_deref().unwrap_or("");
    if includes(old_text, &forms) {
        eprintln!("{shown} includes {} already.", paths.display(&file));
    } else {
        eprintln!(
            "For this, {shown} needs these lines at its top, before any Host block:\n\n\
             {INCLUDE_COMMENT}\n{include}\n"
        );
        if !yes && !confirm(&format!("Add them to {shown}?"), PASS_YES)? {
            eprintln!("Without them, ssh doesn't know the VMs' host names.");
            return Err(Declined.into());
        }
        let mode = match &old {
            Some(_) => fs::metadata(&target)?.permissions().mode() & 0o7777,
            None => 0o600,
        };
        if let Some(dir) = target.parent() {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        let new = format!("{INCLUDE_COMMENT}\n{include}\n\n{old_text}");
        util::write_atomic(&target, new.as_bytes())?;
        fs::set_permissions(&target, fs::Permissions::from_mode(mode))
            .with_context(|| format!("setting the permissions of {}", target.display()))?;
        eprintln!("Added them.");
    }
    eprintln!(
        "ssh reaches each running VM as {HOST_PREFIX}<name>, e.g. `ssh {HOST_PREFIX}<name>`, \
         and so does Rex with the host ssh://{HOST_PREFIX}<name>. `sendit list` shows the names."
    );
    Ok(())
}

/// Whether `~/.ssh/config` includes the Host block of `setup`, which is
/// written too. Only a hint, e.g. for a desktop app: false if either can't
/// be read.
pub fn is_set_up(paths: &Paths) -> bool {
    let file = paths.ssh_config();
    let forms = [paths.display(&file), file.display().to_string()];
    file.exists()
        && resolve_link(&paths.home().join(".ssh/config"))
            .ok()
            .and_then(|target| fs::read_to_string(target).ok())
            .is_some_and(|text| includes(&text, &forms))
}

/// The comment above the Include line in `~/.ssh/config`.
const INCLUDE_COMMENT: &str = "# Added by `sendit ssh-config`: the host names of sendit's VMs.";

/// `path`, or if it is a symbolic link, e.g. into a repository of dotfiles,
/// the file it points to, so that writing it leaves the link in place.
fn resolve_link(path: &Path) -> Result<PathBuf> {
    let is_link = util::if_exists(fs::symlink_metadata(path))
        .with_context(|| format!("reading {}", path.display()))?
        .is_some_and(|metadata| metadata.file_type().is_symlink());
    if is_link {
        fs::canonicalize(path).with_context(|| format!("following the link {}", path.display()))
    } else {
        Ok(path.to_path_buf())
    }
}

/// Whether the ssh config `text` has an Include line for one of `forms`,
/// the ways of writing sendit's file.
fn includes(text: &str, forms: &[String]) -> bool {
    text.lines().any(|line| {
        let line = line.trim();
        let Some((keyword, rest)) = line.split_once(|c: char| c.is_whitespace() || c == '=') else {
            return false;
        };
        let rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '=');
        keyword.eq_ignore_ascii_case("include")
            && forms
                .iter()
                .any(|form| rest.trim().trim_matches('"') == form)
    })
}

/// The Host block for the VMs, which log in with the private key `key`
/// through the sendit binary `sendit`.
fn host_block(sendit: &Path, key: &Path) -> Result<String> {
    let sendit = sendit
        .to_str()
        .with_context(|| format!("the path of sendit, {}, isn't UTF-8", sendit.display()))?;
    // ssh runs it with the user's shell, after expanding its tokens.
    let proxy = format!("{} ssh-proxy %n", quote(sendit).replace('%', "%%"));
    let key = key
        .to_str()
        .with_context(|| format!("the path {} isn't UTF-8", key.display()))?;
    let key = config_word(key)?;
    Ok(format!(
        "\
# Written by `sendit ssh-config`, which writes it again each time it runs.
# ~/.ssh/config includes it, so that ssh reaches each running sendit VM as
# {HOST_PREFIX}<name>: `ssh {HOST_PREFIX}<name>`, or ssh://{HOST_PREFIX}<name> in Rex.
Host {HOST_PREFIX}*
    User {GUEST_USER}
    IdentityFile {key}
    IdentitiesOnly yes
    # sendit finds the VM by its name and connects to it as `sendit ssh`
    # does, checking its host key; this connection goes through that one.
    ProxyCommand {proxy}
    # So it needs no host key check of its own, which couldn't keep up
    # anyway: a name moves to a new VM with `sendit reset`.
    StrictHostKeyChecking no
    UserKnownHostsFile /dev/null
    GlobalKnownHostsFile /dev/null
    UpdateHostKeys no
    LogLevel ERROR
    # Settings for all hosts further down in ~/.ssh/config mustn't hand the
    # VM your SSH agent or display: for each option, the first value wins.
    ForwardAgent no
    ForwardX11 no
"
    ))
}

/// `path` as one word of an ssh config file: with `%` doubled, since ssh
/// expands tokens in paths, and in double quotes if it has spaces. Paths
/// with characters that ssh would expand otherwise, or that can't be
/// quoted, are refused.
fn config_word(path: &str) -> Result<String> {
    if path.contains(['"', '\\', '\n', '*', '?', '[']) || path.contains("${") {
        bail!(
            "sendit can't write {path} into an ssh config file: it has characters that ssh would read differently"
        );
    }
    let word = path.replace('%', "%%");
    Ok(if word.contains(char::is_whitespace) {
        format!("\"{word}\"")
    } else {
        word
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn maps_host_names_to_vm_names() {
        assert_eq!(vm_name("sendit-app").unwrap(), "app".parse().unwrap());
        assert_eq!(
            vm_name("sendit-sendit-2").unwrap(),
            "sendit-2".parse().unwrap()
        );
        for host in ["app", "sendit-", "sendit-App", "sendit-a_b", "x.sendit-a"] {
            assert!(vm_name(host).is_err(), "{host}");
        }
    }

    #[test]
    fn finds_include_lines() {
        let forms = [
            "~/.cache/sendit/ssh/config".to_string(),
            "/Users/me/.cache/sendit/ssh/config".to_string(),
        ];
        for text in [
            "Include ~/.cache/sendit/ssh/config\n",
            "Host a\n  HostName b\n\ninclude \"/Users/me/.cache/sendit/ssh/config\"\n",
            "  INCLUDE=~/.cache/sendit/ssh/config",
            "Include = ~/.cache/sendit/ssh/config  ",
        ] {
            assert!(includes(text, &forms), "{text}");
        }
        for text in [
            "",
            "# Include ~/.cache/sendit/ssh/config\n",
            "Include ~/.cache/sendit/ssh/config.d\n",
            "IdentityFile ~/.cache/sendit/ssh/config\n",
        ] {
            assert!(!includes(text, &forms), "{text}");
        }
    }

    #[test]
    fn tells_whether_set_up() {
        let temp = crate::util::TempDir::new("ssh-set-up");
        let paths = Paths::new(temp.path().to_path_buf());
        assert!(!is_set_up(&paths));
        fs::create_dir_all(paths.ssh_dir()).unwrap();
        fs::write(paths.ssh_config(), "Host sendit-*\n").unwrap();
        assert!(!is_set_up(&paths));
        fs::create_dir_all(temp.path().join(".ssh")).unwrap();
        fs::write(
            temp.path().join(".ssh/config"),
            "Include ~/.cache/sendit/ssh/config\n",
        )
        .unwrap();
        assert!(is_set_up(&paths));
    }

    #[test]
    fn quotes_paths_for_ssh() {
        assert_eq!(config_word("/a/b").unwrap(), "/a/b");
        assert_eq!(config_word("/My Drive/%h").unwrap(), "\"/My Drive/%%h\"");
        for path in ["/a\"b", "/a\\b", "/a/*", "/a/${HOME}", "/a\nb"] {
            assert!(config_word(path).is_err(), "{path:?}");
        }
    }

    /// The options ssh takes from `config` for `host`, as `ssh -G` shows.
    fn ssh_options(config: &Path, host: &str) -> String {
        let output = Command::new("ssh")
            .arg("-F")
            .arg(config)
            .args(["-G", host])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    #[test]
    fn ssh_reads_the_host_block_as_written() {
        let temp = util::TempDir::new("ssh-config-block");
        // Spaces split options and `%h` would be expanded, in both paths.
        let dir = temp.path().join("My  Drive/%h");
        fs::create_dir_all(&dir).unwrap();
        let key = dir.join("id_ed25519");
        let sendit = dir.join("it's sendit");
        let config = temp.path().join("config");
        fs::write(&config, host_block(&sendit, &key).unwrap()).unwrap();

        let options = ssh_options(&config, "sendit-app");
        let has = |line: &str| options.lines().any(|l| l == line);
        assert!(has("user dev"), "{options}");
        // Before ssh expands its tokens.
        let line = format!(
            "identityfile {}",
            key.display().to_string().replace('%', "%%")
        );
        assert!(has(&line), "{options}");
        assert!(has("identitiesonly yes"), "{options}");
        assert!(has("stricthostkeychecking false"), "{options}");
        assert!(has("userknownhostsfile /dev/null"), "{options}");
        assert!(has("forwardagent no"), "{options}");
        let proxy = options
            .lines()
            .find_map(|l| l.strip_prefix("proxycommand "))
            .unwrap_or_else(|| panic!("no proxycommand in {options}"));

        // The shell runs sendit with the host name.
        let words = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "printf '%s\\n' {}",
                proxy.replace("%%", "%").replace("%n", "sendit-app")
            ))
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(words.stdout).unwrap(),
            format!("{}\nssh-proxy\nsendit-app\n", sendit.display())
        );

        // Loading the key expands its path, before the connection to a
        // closed port fails, past the proxy.
        fs::write(&key, "").unwrap();
        let output = Command::new("ssh")
            .arg("-F")
            .arg(&config)
            .args(["-v", "-o", "BatchMode=yes", "-o", "ConnectTimeout=2"])
            .args([
                "-o",
                "ProxyCommand=none",
                "-o",
                "HostName=127.0.0.1",
                "-p",
                "1",
            ])
            .args(["sendit-app", "true"])
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&output.stderr);
        let line = format!("identity file {} type", key.display());
        assert!(log.contains(&line), "{log}");

        // Other hosts are left alone.
        let options = ssh_options(&config, "example.com");
        assert!(
            !options.lines().any(|l| l.starts_with("proxycommand ")),
            "{options}"
        );
        assert!(!options.lines().any(|l| l == "user dev"), "{options}");
    }
}
