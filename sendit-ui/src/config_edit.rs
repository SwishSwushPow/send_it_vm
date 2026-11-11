//! Editing sendit's config file, `~/.config/sendit/config.toml`, keeping
//! its comments and layout: only the settings that change are rewritten.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use send_it_vm::ImageName;
use send_it_vm::{Config, Resources};
use toml_edit::{Array, DocumentMut, Item, Table, TableLike, Value};

/// A part of the config file.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Section {
    /// The top-level settings, for all projects.
    Global,
    /// `[images.<name>]`: CPUs and memory for an image and its VMs.
    Image(String),
    /// `[projects."<path>"]`: settings for one project.
    Project(String),
}

impl Section {
    pub fn title(&self) -> String {
        match self {
            Self::Global => "All projects".to_string(),
            Self::Image(name) => format!("Image {name}"),
            Self::Project(path) => path.clone(),
        }
    }

    /// Whether the section takes only CPUs and memory.
    pub fn is_image(&self) -> bool {
        matches!(self, Self::Image(_))
    }
}

/// The settings of a section, as text for the form. Empty, or `None`,
/// means not set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Fields {
    pub cpus: String,
    pub memory: String,
    pub disk_size: String,
    pub image: String,
    /// `internet`, `none` or `open`.
    pub network: String,
    /// One per line.
    pub network_allow: String,
    /// One per line.
    pub mounts: String,
    pub read_only: Option<bool>,
    pub expose_git: Option<bool>,
    pub banner: Option<bool>,
}

pub struct ConfigFile {
    pub path: PathBuf,
    doc: DocumentMut,
}

impl ConfigFile {
    /// The file at `path`, or an empty one if it doesn't exist.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Self::parse(path, &text)
    }

    pub fn parse(path: &Path, text: &str) -> Result<Self> {
        let doc = text
            .parse::<DocumentMut>()
            .with_context(|| format!("in {}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            doc,
        })
    }

    pub fn text(&self) -> String {
        self.doc.to_string()
    }

    /// Every section, the global one first.
    pub fn sections(&self) -> Vec<Section> {
        let mut sections = vec![Section::Global];
        for (key, kind) in [("images", 0), ("projects", 1)] {
            if let Some(table) = self.doc.get(key).and_then(Item::as_table_like) {
                for (name, item) in table.iter() {
                    if item.is_table_like() {
                        sections.push(match kind {
                            0 => Section::Image(name.to_string()),
                            _ => Section::Project(name.to_string()),
                        });
                    }
                }
            }
        }
        sections
    }

    fn table(&self, section: &Section) -> Option<&dyn TableLike> {
        match section {
            Section::Global => Some(self.doc.as_table() as &dyn TableLike),
            Section::Image(name) => self.child("images", name),
            Section::Project(path) => self.child("projects", path),
        }
    }

    fn child(&self, parent: &str, name: &str) -> Option<&dyn TableLike> {
        self.doc
            .get(parent)?
            .as_table_like()?
            .get(name)?
            .as_table_like()
    }

    fn table_mut(&mut self, section: &Section) -> Option<&mut dyn TableLike> {
        let (parent, name) = match section {
            Section::Global => return Some(self.doc.as_table_mut() as &mut dyn TableLike),
            Section::Image(name) => ("images", name),
            Section::Project(path) => ("projects", path),
        };
        self.doc
            .get_mut(parent)?
            .as_table_like_mut()?
            .get_mut(name)?
            .as_table_like_mut()
    }

    pub fn fields(&self, section: &Section) -> Fields {
        let Some(table) = self.table(section) else {
            return Fields::default();
        };
        let value = |key: &str| table.get(key).and_then(Item::as_value);
        let text = |key: &str| match value(key) {
            Some(Value::String(s)) => s.value().clone(),
            Some(Value::Integer(i)) => i.value().to_string(),
            Some(other) => other.to_string().trim().to_string(),
            None => String::new(),
        };
        let list = |key: &str| match value(key) {
            Some(Value::Array(array)) => array
                .iter()
                .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        let flag = |key: &str| value(key).and_then(Value::as_bool);
        Fields {
            cpus: text("cpus"),
            memory: text("memory"),
            disk_size: text("disk-size"),
            image: text("image"),
            network: text("network"),
            network_allow: list("network-allow"),
            mounts: list("mounts"),
            read_only: flag("read-only"),
            expose_git: flag("expose-git"),
            banner: flag("banner"),
        }
    }

    /// Sets the section's settings to `fields`. Those that stay the same
    /// are left as they are written, comments included.
    pub fn set_fields(&mut self, section: &Section, fields: &Fields) -> Result<()> {
        let image = section.is_image();
        let Some(table) = self.table_mut(section) else {
            bail!("{} is not in the config file", section.title());
        };
        let cpus = fields.cpus.trim();
        let cpus = match cpus {
            "" => None,
            _ => {
                Some(Value::from(cpus.parse::<i64>().with_context(|| {
                    format!("CPUs: {cpus:?} is not a number")
                })?))
            }
        };
        set(table, "cpus", cpus);
        let texts = [
            ("memory", &fields.memory),
            ("disk-size", &fields.disk_size),
            ("image", &fields.image),
            ("network", &fields.network),
        ];
        for (key, text) in texts {
            if image && key != "memory" {
                continue;
            }
            let text = text.trim();
            set(table, key, (!text.is_empty()).then(|| Value::from(text)));
        }
        if image {
            return Ok(());
        }
        for (key, text) in [
            ("network-allow", &fields.network_allow),
            ("mounts", &fields.mounts),
        ] {
            let items: Vec<&str> = text
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect();
            // An empty list is the same as none, and may hold comments, e.g.
            // a share commented out.
            let empty = table
                .get(key)
                .and_then(Item::as_array)
                .is_some_and(Array::is_empty);
            if items.is_empty() && empty {
                continue;
            }
            let value =
                (!items.is_empty()).then(|| Value::Array(items.into_iter().collect::<Array>()));
            set(table, key, value);
        }
        for (key, flag) in [
            ("read-only", fields.read_only),
            ("expose-git", fields.expose_git),
            ("banner", fields.banner),
        ] {
            set(table, key, flag.map(Value::from));
        }
        Ok(())
    }

    /// Adds an empty `[images.<name>]` or `[projects."<path>"]` table.
    pub fn add_section(&mut self, section: &Section) -> Result<()> {
        let (parent, name) = match section {
            Section::Global => return Ok(()),
            Section::Image(name) => {
                name.parse::<ImageName>()?;
                ("images", name)
            }
            Section::Project(path) => ("projects", path),
        };
        if self.table(section).is_some() {
            bail!("{} is in the config file already", section.title());
        }
        let root = self.doc.as_table_mut();
        if !root.contains_key(parent) {
            let mut table = Table::new();
            table.set_implicit(true);
            root.insert(parent, Item::Table(table));
        }
        let parent = root
            .get_mut(parent)
            .and_then(Item::as_table_like_mut)
            .with_context(|| format!("`{parent}` in the config file is not a table"))?;
        parent.insert(name, Item::Table(Table::new()));
        Ok(())
    }

    pub fn remove_section(&mut self, section: &Section) {
        let (parent, name) = match section {
            Section::Global => return,
            Section::Image(name) => ("images", name),
            Section::Project(path) => ("projects", path),
        };
        let root = self.doc.as_table_mut();
        if let Some(table) = root.get_mut(parent).and_then(Item::as_table_like_mut) {
            table.remove(name);
            if table.is_empty() {
                root.remove(parent);
            }
        }
    }

    /// The config as sendit reads it, or why it can't.
    pub fn check(&self) -> Result<Config> {
        let config = Config::parse(&self.text())?;
        // CPUs and memory are checked when they are used; check them for
        // every image now, so that a typo fails here rather than in `run`.
        let images: Vec<_> = config
            .images()
            .cloned()
            .chain([ImageName::default()])
            .collect();
        for image in images {
            config
                .resolve_provision(&image, &Resources::default())
                .with_context(|| format!("for the {image} image"))?;
        }
        Ok(config)
    }

    /// Writes the file, after checking that sendit can read it, and that
    /// it is still `loaded`, the text it was loaded from, rather than
    /// changed meanwhile, e.g. in an editor. A file that is a link, e.g.
    /// into a repository of dotfiles, stays one: the file it points to is
    /// written. Its permissions stay too.
    pub fn save(&self, loaded: &str) -> Result<()> {
        self.check()?;
        let target = match fs::symlink_metadata(&self.path) {
            Ok(m) if m.file_type().is_symlink() => fs::canonicalize(&self.path)
                .with_context(|| format!("following the link {}", self.path.display()))?,
            _ => self.path.clone(),
        };
        let (now, mode) = match fs::read_to_string(&target) {
            Ok(text) => (text, Some(fs::metadata(&target)?.permissions().mode())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (String::new(), None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", target.display())),
        };
        if now != loaded {
            bail!(
                "{} changed since it was loaded here; Reload loads it, without the changes \
                 made here",
                self.path.display()
            );
        }
        if let Some(dir) = target.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        send_it_vm::write_atomic(&target, self.text().as_bytes())?;
        if let Some(mode) = mode {
            fs::set_permissions(&target, fs::Permissions::from_mode(mode))
                .with_context(|| format!("setting the permissions of {}", target.display()))?;
        }
        Ok(())
    }
}

/// Sets `key` in `table` to `value`, or removes it for `None`. A value
/// that stays the same keeps how it is written; a changed one keeps the
/// comment after it.
fn set(table: &mut dyn TableLike, key: &str, value: Option<Value>) {
    let Some(mut value) = value else {
        table.remove(key);
        return;
    };
    if let Some(old) = table.get(key).and_then(Item::as_value) {
        if same(old, &value) {
            return;
        }
        *value.decor_mut() = old.decor().clone();
    }
    // Replacing keeps the key, with the comments above it.
    match table.get_mut(key) {
        Some(item) => *item = Item::Value(value),
        None => {
            table.insert(key, Item::Value(value));
        }
    }
}

/// Whether two values are the same, however they are written.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(a), Value::String(b)) => a.value() == b.value(),
        (Value::Integer(a), Value::Integer(b)) => a.value() == b.value(),
        (Value::Boolean(a), Value::Boolean(b)) => a.value() == b.value(),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b.iter()).all(|(a, b)| same(a, b))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"# My sendit settings
cpus = 4                  # default 2
memory = "8G"
mounts = [
    "~/.cargo/registry:/home/dev/.cargo/registry",  # crates
]

# Images
[images.rust]             # for rust
cpus = 6

[projects."~/Code/app"]
# keep git visible
expose-git = true
"#;

    fn file(text: &str) -> ConfigFile {
        ConfigFile::parse(Path::new("/c/config.toml"), text).unwrap()
    }

    #[test]
    fn reads_sections_and_fields() {
        let config = file(EXAMPLE);
        assert_eq!(
            config.sections(),
            [
                Section::Global,
                Section::Image("rust".into()),
                Section::Project("~/Code/app".into())
            ]
        );
        let global = config.fields(&Section::Global);
        assert_eq!(global.cpus, "4");
        assert_eq!(global.memory, "8G");
        assert_eq!(global.mounts, "~/.cargo/registry:/home/dev/.cargo/registry");
        assert_eq!(global.banner, None);
        assert_eq!(config.fields(&Section::Image("rust".into())).cpus, "6");
        let project = config.fields(&Section::Project("~/Code/app".into()));
        assert_eq!(project.expose_git, Some(true));
    }

    #[test]
    fn leaves_what_stays_the_same_as_written() {
        let mut config = file(EXAMPLE);
        for section in config.sections() {
            let fields = config.fields(&section);
            config.set_fields(&section, &fields).unwrap();
        }
        assert_eq!(config.text(), EXAMPLE);
    }

    #[test]
    fn keeps_empty_lists_with_comments() {
        let text = "mounts = [\n  # \"~/.cargo/registry:/home/dev/.cargo/registry:rw\"\n]\n\nbanner = true\n";
        let mut config = file(text);
        let fields = config.fields(&Section::Global);
        assert_eq!(fields.mounts, "");
        assert_eq!(fields.banner, Some(true));
        config.set_fields(&Section::Global, &fields).unwrap();
        assert_eq!(config.text(), text);
    }

    #[test]
    fn keeps_comments_when_changing() {
        let mut config = file(EXAMPLE);
        let mut global = config.fields(&Section::Global);
        global.cpus = "8".into();
        global.network = "none".into();
        global.memory = String::new();
        global.banner = Some(false);
        config.set_fields(&Section::Global, &global).unwrap();
        let project = Section::Project("~/Code/app".into());
        let mut fields = config.fields(&project);
        fields.expose_git = None;
        fields.read_only = Some(true);
        config.set_fields(&project, &fields).unwrap();
        let text = config.text();
        assert!(
            text.starts_with("# My sendit settings\ncpus = 8                  # default 2\n"),
            "{text}"
        );
        assert!(!text.contains("memory"), "{text}");
        assert!(text.contains("network = \"none\""), "{text}");
        assert!(text.contains("banner = false"), "{text}");
        assert!(text.contains("# crates"), "{text}");
        assert!(
            text.contains("[images.rust]             # for rust\ncpus = 6"),
            "{text}"
        );
        assert!(
            text.contains("[projects.\"~/Code/app\"]\nread-only = true"),
            "{text}"
        );
        assert!(!text.contains("expose-git"), "{text}");
        // The new settings land in the top-level table, before the others.
        let parsed = config.check().unwrap();
        assert_eq!(parsed.defaults.cpus, Some(8));
        assert_eq!(parsed.defaults.banner, Some(false));
        assert!(parsed.defaults.read_only.is_none());
    }

    #[test]
    fn adds_and_removes_sections() {
        let mut config = file("");
        config.add_section(&Section::Image("go".into())).unwrap();
        config
            .add_section(&Section::Project("/p/q r".into()))
            .unwrap();
        assert!(config.add_section(&Section::Image("go".into())).is_err());
        assert!(config.add_section(&Section::Image("Go".into())).is_err());
        let mut fields = Fields {
            memory: "2G".into(),
            ..Fields::default()
        };
        config
            .set_fields(&Section::Image("go".into()), &fields)
            .unwrap();
        fields.mounts = "/a\n\n/b:rw\n".into();
        config
            .set_fields(&Section::Project("/p/q r".into()), &fields)
            .unwrap();
        let text = config.text();
        assert_eq!(
            text,
            "[images.go]\nmemory = \"2G\"\n\n[projects.\"/p/q r\"]\nmemory = \"2G\"\nmounts = [\"/a\", \"/b:rw\"]\n"
        );
        config.check().unwrap();
        config.remove_section(&Section::Image("go".into()));
        assert!(!config.text().contains("images"));
        assert_eq!(config.sections().len(), 2);
    }

    #[test]
    fn refuses_what_sendit_would() {
        let mut config = file("");
        let mut fields = Fields {
            cpus: "two".into(),
            ..Fields::default()
        };
        assert!(config.set_fields(&Section::Global, &fields).is_err());
        fields.cpus = "0".into();
        config.set_fields(&Section::Global, &fields).unwrap();
        assert!(config.check().is_err());
        fields.cpus = String::new();
        fields.memory = "lots".into();
        config.set_fields(&Section::Global, &fields).unwrap();
        assert!(config.check().is_err());
        fields.memory = String::new();
        fields.network = "offline".into();
        config.set_fields(&Section::Global, &fields).unwrap();
        assert!(config.check().is_err());
    }

    #[test]
    fn saves_through_links() {
        let temp = std::env::temp_dir().join(format!("sendit-ui-link-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);
        let real = temp.join("dotfiles/config.toml");
        fs::create_dir_all(real.parent().unwrap()).unwrap();
        fs::write(&real, "cpus = 2\n").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o600)).unwrap();
        let link = temp.join("config.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mut config = ConfigFile::load(&link).unwrap();
        let mut fields = config.fields(&Section::Global);
        fields.cpus = "3".into();
        config.set_fields(&Section::Global, &fields).unwrap();
        // Not over a change made meanwhile.
        assert!(config.save("cpus = 1\n").is_err());
        assert_eq!(fs::read_to_string(&real).unwrap(), "cpus = 2\n");
        config.save("cpus = 2\n").unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&real).unwrap(), "cpus = 3\n");
        let mode = fs::metadata(&real).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        fs::remove_dir_all(&temp).unwrap();
    }
}
