//! Finding the sendit binary, and opening commands in a terminal and files
//! in an editor.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// The sendit binary: `configured` if given, else the first of next to
/// this app, `~/.cargo/bin`, `PATH`, and Homebrew's folders. An app
/// started from Finder has little of the shell's `PATH`.
pub fn find_sendit(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = configured {
        return is_executable(path).then(|| path.to_path_buf());
    }
    let mut candidates = Vec::new();
    if let Some(dir) = std::env::current_exe().ok().and_then(|exe| {
        let dir = exe.parent()?.to_path_buf();
        Some(dir)
    }) {
        candidates.push(dir.join("sendit"));
    }
    if let Some(home) = std::env::home_dir() {
        candidates.push(home.join(".cargo/bin/sendit"));
    }
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|dir| dir.join("sendit")));
    }
    candidates.extend(["/opt/homebrew/bin/sendit", "/usr/local/bin/sendit"].map(PathBuf::from));
    candidates.into_iter().find(|path| is_executable(path))
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// `arg` quoted for a POSIX shell, unless it needs no quotes.
pub fn shell_quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c));
    if plain {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

/// `argv` as one shell command line.
pub fn command_line(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Which terminal app opens interactive sessions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Terminal {
    #[default]
    Ghostty,
    /// Terminal.app.
    #[serde(rename = "terminal")]
    Apple,
    /// A command line with `{}` where the script to run goes.
    Custom,
}

/// How files are opened for editing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Editor {
    /// `$VISUAL`, else `$EDITOR`, else vi, in the terminal.
    #[default]
    Terminal,
    /// The default app for text files (`open -t`).
    TextApp,
    /// A command line with `{}` where the file goes.
    Custom,
}

/// A command to run in a new terminal window.
#[derive(Clone, Debug)]
pub struct Launch {
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub title: String,
    /// Keep the window open after the command ends, to read its output.
    /// Without, it stays open only if the command fails.
    pub hold: bool,
}

/// The script that a terminal runs for `launch`. It deletes itself first.
pub fn script(launch: &Launch) -> String {
    let mut script = String::from("#!/bin/sh\nrm -f \"$0\"\n");
    match &launch.cwd {
        Some(dir) => {
            script += &format!(
                "cd {} 2>/dev/null || cd\n",
                shell_quote(&dir.display().to_string())
            )
        }
        None => script += "cd\n",
    }
    script += &format!("printf '\\033]0;%s\\007' {}\n", shell_quote(&launch.title));
    let command = command_line(&launch.argv);
    // Without `hold`, the window still stays open after a failure, so that
    // its message can be read.
    let condition = if launch.hold {
        "true"
    } else {
        "[ \"$status\" -ne 0 ]"
    };
    script += &format!(
        "{command}\nstatus=$?\nif {condition}; then\n  \
         printf '\\n[exited with %d; press Return to close]' \"$status\"\n  read _\nfi\n\
         exit \"$status\"\n"
    );
    script
}

/// Writes `script` to a new private file to run, in the app's cache.
fn write_script(script: &str) -> Result<PathBuf> {
    let home = std::env::home_dir().context("cannot determine the home directory")?;
    let dir = home.join("Library/Caches/sendit-ui/launch");
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let path = dir.join(format!("{nanos}-{}.command", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(&path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(script.as_bytes())?;
    Ok(path)
}

/// Opens a new window of `terminal` running `launch`. `custom` is the
/// command line for `Terminal::Custom`.
pub fn open_in_terminal(terminal: Terminal, custom: &str, launch: &Launch) -> Result<()> {
    let path = write_script(&script(launch))?;
    let path_str = path.display().to_string();
    let mut command = match terminal {
        Terminal::Ghostty => {
            let mut command = Command::new("/usr/bin/open");
            // As Ghostty's help says to pass arguments on macOS: a new
            // instance, which quits again when its window closes.
            command.args([
                "-na",
                "Ghostty",
                "--args",
                "--quit-after-last-window-closed=true",
                "-e",
                &path_str,
            ]);
            command
        }
        Terminal::Apple => {
            let mut command = Command::new("/usr/bin/open");
            command.args(["-a", "Terminal", &path_str]);
            command
        }
        Terminal::Custom => {
            let line = fill_template(custom, &path_str)?;
            return spawn_detached(&line);
        }
    };
    let result = run_quietly(&mut command);
    if result.is_err() {
        let _ = fs::remove_file(&path);
    }
    result
}

/// Opens `file` for editing as `editor` says. `custom` is the command line
/// for `Editor::Custom`.
pub fn open_in_editor(
    editor: Editor,
    custom: &str,
    terminal: Terminal,
    custom_terminal: &str,
    file: &Path,
) -> Result<()> {
    let file_str = file.display().to_string();
    match editor {
        Editor::Terminal => {
            // An interactive login shell, which sets $EDITOR in most
            // setups; an app started from Finder doesn't have it. eval
            // splits it into words, e.g. `code --wait`, also in zsh, which
            // doesn't on its own. fish and other shells that aren't like sh
            // can't run that, so zsh, macOS's own, stands in for them.
            let shell = std::env::var("SHELL")
                .ok()
                .filter(|shell| {
                    let name = Path::new(shell).file_name().and_then(|n| n.to_str());
                    matches!(name, Some("zsh" | "bash" | "sh" | "ksh" | "dash"))
                })
                .unwrap_or_else(|| "/bin/zsh".to_string());
            let launch = Launch {
                argv: vec![
                    shell.clone(),
                    "-lic".to_string(),
                    r#"eval "exec ${VISUAL:-${EDITOR:-vi}} \"\$1\"""#.to_string(),
                    shell,
                    file_str,
                ],
                cwd: file.parent().map(Path::to_path_buf),
                title: file
                    .file_name()
                    .map_or_else(String::new, |name| name.display().to_string()),
                hold: false,
            };
            open_in_terminal(terminal, custom_terminal, &launch)
        }
        Editor::TextApp => run_quietly(Command::new("/usr/bin/open").args(["-t", &file_str])),
        Editor::Custom => spawn_detached(&fill_template(custom, &file_str)?),
    }
}

/// `template` with each `{}` replaced by `arg`, quoted, or with `arg`
/// appended if it has none.
pub fn fill_template(template: &str, arg: &str) -> Result<String> {
    let template = template.trim();
    if template.is_empty() {
        bail!("no command is set in the preferences");
    }
    let arg = shell_quote(arg);
    Ok(if template.contains("{}") {
        template.replace("{}", &arg)
    } else {
        format!("{template} {arg}")
    })
}

/// Starts the shell command `line` without waiting for it: a terminal or
/// editor may run until the user quits it. It is waited for in the
/// background, so that it doesn't linger as a zombie.
fn spawn_detached(line: &str) -> Result<()> {
    let mut child = Command::new("/bin/sh")
        .args(["-c", line])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("running {line}"))?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// Runs `command`, which returns at once, as `open` does, and fails with
/// its output unless it succeeds.
fn run_quietly(command: &mut Command) -> Result<()> {
    let output = command
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {command:?}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{command:?} failed ({}): {}", output.status, stderr.trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_for_the_shell() {
        assert_eq!(shell_quote("sendit"), "sendit");
        assert_eq!(shell_quote("/a/b-c_d.e"), "/a/b-c_d.e");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("$HOME"), "'$HOME'");
        assert_eq!(
            command_line(&["sendit".into(), "-n".into(), "my app".into()]),
            "sendit -n 'my app'"
        );
    }

    #[test]
    fn writes_scripts_that_run_the_command() {
        let launch = Launch {
            argv: vec!["/x/sendit".into(), "-n".into(), "app".into(), "ssh".into()],
            cwd: Some(PathBuf::from("/p q")),
            title: "app".into(),
            hold: false,
        };
        let text = script(&launch);
        assert!(text.starts_with("#!/bin/sh\nrm -f \"$0\"\ncd '/p q' 2>/dev/null || cd\n"));
        assert!(
            text.contains("\n/x/sendit -n app ssh\nstatus=$?\nif [ \"$status\" -ne 0 ]; then\n")
        );
        assert!(text.ends_with("exit \"$status\"\n"));

        let held = script(&Launch {
            hold: true,
            ..launch
        });
        assert!(held.contains("/x/sendit -n app ssh\nstatus=$?\nif true; then\n"));
    }

    #[test]
    fn scripts_run_and_hold_only_on_failure() {
        // The script, run without a terminal: it ends at once on success
        // and waits for Return after a failure.
        for (argv, status, waits) in [
            (vec!["true".to_string()], 0, false),
            (
                vec!["sh".to_string(), "-c".to_string(), "exit 3".to_string()],
                3,
                true,
            ),
        ] {
            let launch = Launch {
                argv,
                cwd: None,
                title: "t".into(),
                hold: false,
            };
            // From a file of its own, which it deletes.
            let file = std::env::temp_dir()
                .join(format!("sendit-ui-script-{}-{status}", std::process::id()));
            fs::write(&file, script(&launch)).unwrap();
            let output = Command::new("/bin/sh")
                .arg(&file)
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert!(!file.exists());
            assert_eq!(output.status.code(), Some(status));
            let text = String::from_utf8_lossy(&output.stdout);
            assert_eq!(text.contains("press Return"), waits, "{text}");
        }
    }

    #[test]
    fn splits_the_editor_into_words() {
        // As the editor's login shell runs it, here without the login part.
        let output = Command::new("/bin/zsh")
            .args([
                "-fc",
                r#"eval "exec ${VISUAL:-${EDITOR:-vi}} \"\$1\"""#,
                "zsh",
                "/a b",
            ])
            .env_remove("VISUAL")
            .env("EDITOR", "echo opened")
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout), "opened /a b\n");
    }

    #[test]
    fn fills_templates() {
        assert_eq!(fill_template("zed {}", "/a b").unwrap(), "zed '/a b'");
        assert_eq!(fill_template("code", "/a").unwrap(), "code /a");
        assert!(fill_template("  ", "/a").is_err());
    }
}
