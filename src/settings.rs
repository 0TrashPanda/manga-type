//! User settings, kept in `$XDG_CONFIG_HOME/manga-type/settings.json`
//! (`~/.config/manga-type/settings.json`). Missing or unreadable file: defaults.
//! Unknown or missing fields fall back to their defaults, so new settings can be added
//! without breaking an old file.

use std::path::PathBuf;

use crate::typing::{CaseMode, Strictness};

/// What "Sync" copies from the phone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum SyncMode {
    /// Chapters Mihon hasn't marked read.
    #[default]
    MihonUnread,
    /// Every downloaded chapter that isn't on the PC yet.
    AllMissing,
    /// Only chapters after the last one finished in manga-type.
    AfterLastFinished,
}

impl SyncMode {
    pub const ALL: [SyncMode; 3] = [SyncMode::MihonUnread, SyncMode::AllMissing, SyncMode::AfterLastFinished];

    pub fn label(self) -> &'static str {
        match self {
            SyncMode::MihonUnread => "Chapters Mihon hasn't marked read",
            SyncMode::AllMissing => "Every downloaded chapter missing on the PC",
            SyncMode::AfterLastFinished => "Only chapters after the last one finished here",
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    pub strictness: Strictness,
    pub case: CaseMode,
    /// Where copied chapters live: `<library>/<Source>/<Series>/<chapter>.cbz`.
    pub library: PathBuf,
    pub sync_mode: SyncMode,
    /// OCR worker threads; 0 = automatic (one per core, capped).
    pub workers: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            strictness: Strictness::default(),
            case: CaseMode::default(),
            library: default_library(),
            sync_mode: SyncMode::default(),
            workers: 0,
        }
    }
}

/// `~/Manga`.
pub fn default_library() -> PathBuf {
    std::env::home_dir().unwrap_or_else(std::env::temp_dir).join("Manga")
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

    pub fn ocr_workers(&self) -> usize {
        if self.workers == 0 { crate::ocr::default_workers() } else { self.workers }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_file_gets_defaults() {
        let s: Settings = serde_json::from_str(r#"{"case":"Sensitive"}"#).unwrap();
        assert_eq!(s.case, CaseMode::Sensitive);
        assert_eq!(s.sync_mode, SyncMode::MihonUnread);
        assert_eq!(s.library, default_library());
    }
}
