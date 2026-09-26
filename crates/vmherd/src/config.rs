//! Persisted settings: cluster bookmarks (without secrets), per-cluster grid, UI preferences.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Where a bookmark's API token secret lives.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SecretSource {
    /// The OS credential store (macOS Keychain, Windows Credential Manager, Secret Service),
    /// entry "VMherd" / bookmark id. Written by this app.
    Keyring,
    /// An existing macOS Keychain item, read with `security find-generic-password -s <service> -w`.
    MacKeychain {
        service: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
    },
    /// A shell command whose first output line is the secret (`pass show pve/token`, `op read ...`).
    Command { command: String },
    /// A file only the user can read (mode 600) in the config directory; not encrypted. For Linux
    /// systems without a Secret Service keyring.
    File,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bookmark {
    pub id: Uuid,
    pub name: String,
    /// `https://host:8006`
    pub url: String,
    /// `user@realm!tokenname`
    pub token_id: String,
    pub secret: SecretSource,
    /// Trusted server certificate, `AB:CD:..` SHA-256 of the leaf.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_sha256: Option<String>,
    /// VMIDs in the grid, in order.
    #[serde(default)]
    pub grid: Vec<u32>,
    /// VMIDs whose tile does not receive broadcast input.
    #[serde(default)]
    pub nosync: Vec<u32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortKey {
    Vmid,
    #[default]
    Name,
    Node,
    Status,
    Tags,
    Uptime,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// Tiles per row, 0 = auto.
    pub cols: u32,
    /// Delay per typed character.
    pub delay_ms: u64,
    /// Press Enter after "Type".
    pub add_enter: bool,
    pub filter: String,
    pub only_running: bool,
    pub sort_key: SortKey,
    pub sort_desc: bool,
    pub last_cluster: Option<Uuid>,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            cols: 0,
            delay_ms: 20,
            add_enter: true,
            filter: String::new(),
            only_running: false,
            sort_key: SortKey::Name,
            sort_desc: false,
            last_cluster: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub bookmarks: Vec<Bookmark>,
    pub prefs: Prefs,
    /// Set when an existing file could not be read or moved aside: saving would destroy it.
    #[serde(skip)]
    pub save_blocked: Option<String>,
}

/// `path` for display, with the home directory as `~` (no user name in messages or screenshots).
pub fn tilde(path: &Path) -> String {
    let home = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf());
    match home.as_deref().and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) => Path::new("~").join(rest).display().to_string(),
        None => path.display().to_string(),
    }
}

impl Config {
    pub fn path() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "", "VMherd").map(|d| d.config_dir().join("config.json"))
    }

    /// Load, or defaults when there is no file yet. A broken file is reported, not overwritten
    /// silently: it is kept as `config.json.broken`.
    pub fn load() -> (Self, Option<String>) {
        let Some(path) = Self::path() else {
            return (Self::default(), Some("No config directory on this system, so settings are not saved".into()));
        };
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(cfg) => (cfg, None),
                Err(e) => {
                    let backup = path.with_extension("json.broken");
                    match std::fs::rename(&path, &backup) {
                        Ok(()) => (
                            Self::default(),
                            Some(format!("{} was unreadable ({e}); moved to {}", tilde(&path), tilde(&backup))),
                        ),
                        Err(re) => Self::blocked(format!(
                            "{} is unreadable ({e}) and could not be moved aside ({re}); settings are not saved",
                            tilde(&path)
                        )),
                    }
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Self::default(), None),
            Err(e) => Self::blocked(format!("Cannot read {} ({e}); settings are not saved", tilde(&path))),
        }
    }

    fn blocked(why: String) -> (Self, Option<String>) {
        (Self { save_blocked: Some(why.clone()), ..Self::default() }, Some(why))
    }

    /// Write atomically (temp file + rename), owner-only permissions on Unix.
    pub fn save(&self) -> anyhow::Result<()> {
        if let Some(why) = &self.save_blocked {
            anyhow::bail!("{why}");
        }
        let path = Self::path().context("no config directory")?;
        let dir = path.parent().context("config path has no parent")?;
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", tilde(dir)))?;
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_vec_pretty(self)?;
        write_private(&tmp, &json).with_context(|| format!("write {}", tilde(&tmp)))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("replace {}", tilde(&path)))?;
        Ok(())
    }

    pub fn bookmark(&self, id: Uuid) -> Option<&Bookmark> {
        self.bookmarks.iter().find(|b| b.id == id)
    }

    pub fn bookmark_mut(&mut self, id: Uuid) -> Option<&mut Bookmark> {
        self.bookmarks.iter_mut().find(|b| b.id == id)
    }
}

#[cfg(unix)]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

#[cfg(not(unix))]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_defaults() {
        let cfg = Config {
            bookmarks: vec![Bookmark {
                id: Uuid::nil(),
                name: "Lab".into(),
                url: "https://pve.example.com:8006".into(),
                token_id: "root@pam!vmherd".into(),
                secret: SecretSource::MacKeychain { service: "vmherd-token".into(), account: None },
                pinned_sha256: None,
                grid: vec![101, 102],
                nosync: vec![],
            }],
            prefs: Prefs::default(),
            save_blocked: None,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains(r#""kind":"mac_keychain""#));
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cfg);
        let empty: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.prefs.delay_ms, 20);
    }

    #[test]
    fn paths_show_the_home_directory_as_tilde() {
        let home = directories::BaseDirs::new().unwrap().home_dir().to_path_buf();
        let shown = tilde(&home.join("Library").join("VMherd").join("config.json"));
        assert_eq!(shown, Path::new("~").join("Library").join("VMherd").join("config.json").display().to_string());
        assert!(!shown.contains(&*home.to_string_lossy()));
        let elsewhere = Path::new("/etc/vmherd.json");
        assert_eq!(tilde(elsewhere), elsewhere.display().to_string());
    }
}
