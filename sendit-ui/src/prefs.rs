//! The app's own preferences, in `~/Library/Application Support/sendit-ui`,
//! apart from sendit's configuration.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::launch::{Editor, Terminal};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Prefs {
    pub terminal: Terminal,
    /// For `Terminal::Custom`, with `{}` for the script to run.
    pub terminal_command: String,
    pub editor: Editor,
    /// For `Editor::Custom`, with `{}` for the file.
    pub editor_command: String,
    /// The sendit binary, if not found on its own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sendit: Option<PathBuf>,
    /// The VM folder, as `SENDIT_VM_DIR` sets it for sendit in a shell. An
    /// app started from Finder doesn't see what the shell's profile sets.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vm_dir: Option<PathBuf>,
}

impl Prefs {
    fn path() -> Option<PathBuf> {
        Some(std::env::home_dir()?.join("Library/Application Support/sendit-ui/settings.toml"))
    }

    /// The saved preferences, or the defaults with the reason they couldn't
    /// be read.
    pub fn load() -> (Self, Option<String>) {
        let Some(path) = Self::path() else {
            return (Self::default(), None);
        };
        match send_it_vm::read_toml::<Self>(&path) {
            Ok(prefs) => (prefs.unwrap_or_default(), None),
            Err(e) => (Self::default(), Some(format!("{e:#}"))),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path().context("cannot determine the home directory")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        send_it_vm::write_atomic(&path, toml::to_string(self)?.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let prefs = Prefs {
            terminal: Terminal::Custom,
            terminal_command: "wezterm start -- {}".into(),
            editor: Editor::TextApp,
            editor_command: String::new(),
            sendit: Some("/x/sendit".into()),
            vm_dir: None,
        };
        let text = toml::to_string(&prefs).unwrap();
        assert!(text.contains("terminal = \"custom\""), "{text}");
        assert_eq!(toml::from_str::<Prefs>(&text).unwrap(), prefs);
        assert_eq!(toml::from_str::<Prefs>("").unwrap(), Prefs::default());
    }
}
