//! User settings, kept in `$XDG_CONFIG_HOME/manga-type/settings.json`
//! (`~/.config/manga-type/settings.json`). Missing or unreadable file: defaults.
//! Unknown or missing fields fall back to their defaults, so new settings can be added
//! without breaking an old file.

use std::path::PathBuf;

use crate::typing::{CaseMode, Strictness};

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    pub strictness: Strictness,
    pub case: CaseMode,
}

fn path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::home_dir().map(|h| h.join(".config")))
        .map(|d| d.join("manga-type").join("settings.json"))
}

impl Settings {
    pub fn load() -> Self {
        path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Save atomically (write temp, rename).
    pub fn save(&self) -> Result<(), String> {
        let path = path().ok_or("no home directory")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
    }
}
