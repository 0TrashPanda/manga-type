//! The PC-side library: chapters copied from the phone, what Mihon says about them,
//! and how far you got in manga-type.
//!
//! On disk (`<library>` is a setting, `~/Manga` by default):
//!
//! ```text
//! <library>/.mihon-backup.tachibk                  newest Mihon backup seen on the phone
//! <library>/<Source>/<Series>/.manga-type-series.json  series settings (where to start copying)
//! <library>/<Source>/<Series>/<chapter>.cbz           chapter (or a folder of images)
//! <library>/<Source>/<Series>/<chapter>.cbz.manga-type.json           OCR cache (ocr.rs)
//! <library>/<Source>/<Series>/<chapter>.cbz.manga-type-progress.json  reading position, stats
//! ```
//!
//! Nothing here touches the phone; `sync.rs` does the copying.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::mihon::{Backup, BackupSeries};
use crate::phone;
use crate::settings::SyncMode;
use crate::typing::Stats;

pub const BACKUP_FILE: &str = ".mihon-backup.tachibk";
const SERIES_FILE: &str = ".manga-type-series.json";
const PROGRESS_SUFFIX: &str = ".manga-type-progress.json";

// ---------------------------------------------------------------------------
// Per-chapter progress
// ---------------------------------------------------------------------------

/// Where you are in a chapter, saved next to it.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ChapterProgress {
    /// Index of the bubble you're on.
    pub current: usize,
    /// One char per bubble before `current`: `d` done, `s` skipped.
    pub states: String,
    /// Bubbles in the chapter when last saved (0 = unknown).
    pub bubbles: usize,
    pub finished: bool,
    /// Totals over the whole chapter.
    pub stats: Stats,
    /// Time spent with the chapter open while active (gaps over a minute count as a
    /// minute), across every read of it.
    pub reading_seconds: f64,
    /// Unix seconds of the last save.
    pub updated: u64,
}

/// "4,120".
pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// "412 words", "~4,120 words" (estimated from characters for older chapters).
pub fn words_label(words: u64, approx: bool) -> String {
    format!("{}{} words", if approx { "~" } else { "" }, thousands(words))
}

/// "45s", "12m", "1h 05m".
pub fn format_duration(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        _ => format!("{}h {:02}m", s / 3600, s / 60 % 60),
    }
}

pub fn progress_path(chapter: &Path) -> PathBuf {
    let mut name = chapter.file_name().unwrap_or_default().to_os_string();
    name.push(PROGRESS_SUFFIX);
    chapter.with_file_name(name)
}

impl ChapterProgress {
    pub fn load(chapter: &Path) -> Option<Self> {
        serde_json::from_str(&std::fs::read_to_string(progress_path(chapter)).ok()?).ok()
    }

    pub fn save(&mut self, chapter: &Path) -> Result<(), String> {
        self.updated = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        write_json(&progress_path(chapter), self)
    }
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Series settings
// ---------------------------------------------------------------------------

/// Per-series settings, chosen in the add-series popup.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SeriesMeta {
    /// Phone file name of the first chapter to copy; earlier chapters are never synced.
    pub start_from: Option<String>,
}

impl SeriesMeta {
    pub fn load(dir: &Path) -> Self {
        std::fs::read_to_string(dir.join(SERIES_FILE))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) -> Result<(), String> {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        write_json(&dir.join(SERIES_FILE), self)
    }
}

// ---------------------------------------------------------------------------
// Mihon markers
// ---------------------------------------------------------------------------

/// What Mihon's backup says about a chapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Read,
    /// Started in Mihon (last page read > 0) but not finished.
    Partly,
    Unread,
    /// Not in the backup (newer than it, or the series isn't matched).
    Unknown,
}

pub fn mark(series: Option<&BackupSeries>, file_name: &str) -> Mark {
    match series.and_then(|s| s.match_chapter(file_name)) {
        Some(c) if c.read => Mark::Read,
        Some(c) if c.last_page_read > 0 => Mark::Partly,
        Some(_) => Mark::Unread,
        None => Mark::Unknown,
    }
}

/// Load the backup saved in the library by the last phone connection.
pub fn load_backup(library: &Path) -> Option<Backup> {
    let bytes = std::fs::read(library.join(BACKUP_FILE)).ok()?;
    crate::mihon::parse_backup(&bytes).ok()
}


// ---------------------------------------------------------------------------
// Scanning the library folder
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct LocalChapter {
    pub path: PathBuf,
    /// File/folder name, same as on the phone.
    pub file_name: String,
    pub display_name: String,
    pub mark: Mark,
    pub progress: Option<ChapterProgress>,
}

impl LocalChapter {
    pub fn finished(&self) -> bool {
        self.progress.as_ref().is_some_and(|p| p.finished)
    }

    pub fn started(&self) -> bool {
        self.progress.as_ref().is_some_and(|p| p.current > 0 || p.stats.correct > 0)
    }
}

#[derive(Debug, Clone)]
pub struct LocalSeries {
    pub source: String,
    pub title: String,
    pub meta: SeriesMeta,
    /// In reading order.
    pub chapters: Vec<LocalChapter>,
}

impl LocalSeries {
    pub fn phone_series(&self) -> phone::Series {
        phone::Series { source: self.source.clone(), title: self.title.clone() }
    }

    /// Words typed in this series, and whether any chapter's count is an estimate.
    pub fn words(&self) -> (u64, bool) {
        self.chapters.iter().filter_map(|c| c.progress.as_ref()).map(|p| p.stats.word_count()).fold((0, false), |a, (w, e)| (a.0 + w, a.1 || e))
    }

    /// Time spent reading this series (all chapters).
    pub fn reading_seconds(&self) -> f64 {
        self.chapters.iter().filter_map(|c| c.progress.as_ref()).map(|p| p.reading_seconds).sum()
    }

    pub fn finished_count(&self) -> usize {
        self.chapters.iter().filter(|c| c.finished()).count()
    }

    /// The chapter to open with "Continue": the one in progress, else the first unfinished
    /// one after the last finished, else the first unfinished.
    pub fn continue_index(&self) -> Option<usize> {
        if let Some(i) = self.chapters.iter().position(|c| c.started() && !c.finished()) {
            return Some(i);
        }
        let after = self.chapters.iter().rposition(LocalChapter::finished).map_or(0, |i| i + 1);
        (after..self.chapters.len())
            .chain(0..after)
            .find(|&i| !self.chapters[i].finished())
    }
}

/// A chapter entry in a series folder: a `.cbz` file or an image folder, not our own files.
fn is_chapter_entry(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else { return false };
    if name.starts_with('.') || name.ends_with(".part") || phone::is_unfinished(name) {
        return false;
    }
    if path.is_dir() { true } else { name.to_lowercase().ends_with(".cbz") }
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect())
        .unwrap_or_default();
    v.retain(|p| !p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.')));
    v.sort_by(|a, b| natord::compare_ignore_case(&a.to_string_lossy(), &b.to_string_lossy()));
    v
}

/// Sort chapters for reading. If Mihon's backup knows every chapter, use its order
/// (handles names whose scanlator prefix changed); otherwise natural sort by name.
pub fn reading_order<T>(items: &mut [T], file_name: impl Fn(&T) -> &str, display: impl Fn(&T) -> &str, backup: Option<&BackupSeries>) {
    if let Some(bs) = backup
        && items.iter().all(|c| bs.match_chapter(file_name(c)).is_some())
    {
        // Mihon's source order counts down from the newest chapter.
        items.sort_by_cached_key(|c| std::cmp::Reverse(bs.match_chapter(file_name(c)).map_or(0, |b| b.source_order)));
        return;
    }
    items.sort_by(|a, b| natord::compare_ignore_case(display(a), display(b)));
}

pub struct Library {
    pub series: Vec<LocalSeries>,
}

impl Library {
    pub fn load(root: &Path) -> Self {
        let backup = load_backup(root);
        let mut series = Vec::new();
        for source_dir in subdirs(root) {
            for dir in subdirs(&source_dir) {
                series.push(load_series(&source_dir, &dir, backup.as_ref()));
            }
        }
        Self { series }
    }

    pub fn contains(&self, s: &phone::Series) -> bool {
        self.series.iter().any(|l| l.source == s.source && l.title == s.title)
    }

    /// The chapter after `path` in its series, if it's on the PC.
    pub fn next_after(&self, path: &Path) -> Option<PathBuf> {
        for s in &self.series {
            if let Some(i) = s.chapters.iter().position(|c| c.path == path) {
                return s.chapters.get(i + 1).map(|c| c.path.clone());
            }
        }
        next_in_folder(path)
    }
}

fn load_series(source_dir: &Path, dir: &Path, backup: Option<&Backup>) -> LocalSeries {
    let source = source_dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let title = dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let bs = backup.and_then(|b| b.find_series(&source, &title));
    let mut chapters: Vec<LocalChapter> = std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| is_chapter_entry(p)).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .map(|path| {
            let file_name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            LocalChapter {
                display_name: phone::display_name(&file_name),
                mark: mark(bs, &file_name),
                progress: ChapterProgress::load(&path),
                file_name,
                path,
            }
        })
        .collect();
    reading_order(&mut chapters, |c| &c.file_name, |c| &c.display_name, bs);
    LocalSeries { source, title, meta: SeriesMeta::load(dir), chapters }
}

/// For a chapter opened outside the library: the next chapter in the same folder.
fn next_in_folder(path: &Path) -> Option<PathBuf> {
    let dir = path.parent()?;
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).filter(|p| is_chapter_entry(p)).collect();
    // A folder of page images isn't a series folder; only look at siblings of a chapter.
    entries.sort_by(|a, b| {
        let n = |p: &PathBuf| phone::display_name(&p.file_name().unwrap_or_default().to_string_lossy());
        natord::compare_ignore_case(&n(a), &n(b))
    });
    let i = entries.iter().position(|p| p == path)?;
    entries.get(i + 1).cloned()
}

// ---------------------------------------------------------------------------
// Sync planning
// ---------------------------------------------------------------------------

/// Which of the phone's chapters (already in reading order) to copy. Never chapters
/// already on the PC or before the series' start chapter.
///
/// - `local`: file names already on the PC.
/// - `finished`: file names finished in manga-type.
/// - `mode: None` copies everything from the start chapter on (used when adding a series).
pub fn plan_sync(
    phone_chapters: &[phone::Chapter],
    local: &HashSet<String>,
    finished: &HashSet<String>,
    start_from: Option<&str>,
    backup: Option<&BackupSeries>,
    mode: Option<SyncMode>,
) -> Vec<usize> {
    let start = start_from
        .and_then(|s| phone_chapters.iter().position(|c| c.file_name == s))
        .unwrap_or(0);
    let after_finished = phone_chapters.iter().rposition(|c| finished.contains(&c.file_name)).map_or(0, |i| i + 1);
    (start..phone_chapters.len())
        .filter(|&i| {
            let c = &phone_chapters[i];
            if local.contains(&c.file_name) {
                return false;
            }
            match mode {
                None | Some(SyncMode::AllMissing) => true,
                Some(SyncMode::MihonUnread) => mark(backup, &c.file_name) != Mark::Read,
                Some(SyncMode::AfterLastFinished) => i >= after_finished,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mihon::BackupChapter;

    fn pc(name: &str) -> phone::Chapter {
        phone::Chapter {
            file_name: format!("{name}.cbz"),
            display_name: name.into(),
            kind: phone::ChapterKind::Cbz,
            size: 10,
        }
    }

    fn set(v: &[&str]) -> HashSet<String> {
        v.iter().map(|s| format!("{s}.cbz")).collect()
    }

    #[test]
    fn plan_modes() {
        let ch: Vec<_> = ["Chapter 1", "Chapter 2", "Chapter 3", "Chapter 4", "Chapter 5"].map(pc).into();
        let read = |n: &str, r: bool| BackupChapter { name: n.into(), url: format!("/{n}"), read: r, ..Default::default() };
        let bs = BackupSeries {
            chapters: vec![read("Chapter 1", true), read("Chapter 2", true), read("Chapter 3", false)],
            ..Default::default()
        };
        let local = set(&["Chapter 1"]);
        let fin = set(&["Chapter 1", "Chapter 3"]);

        assert_eq!(plan_sync(&ch, &local, &fin, None, Some(&bs), None), [1, 2, 3, 4]);
        assert_eq!(plan_sync(&ch, &local, &fin, None, Some(&bs), Some(SyncMode::AllMissing)), [1, 2, 3, 4]);
        // Read in Mihon: 1, 2. Unknown chapters (4, 5) count as unread.
        assert_eq!(plan_sync(&ch, &local, &fin, None, Some(&bs), Some(SyncMode::MihonUnread)), [2, 3, 4]);
        // Last finished here is chapter 3 (index 2).
        assert_eq!(plan_sync(&ch, &local, &fin, None, Some(&bs), Some(SyncMode::AfterLastFinished)), [3, 4]);
        // Start chapter cuts off the earlier ones in every mode.
        assert_eq!(plan_sync(&ch, &HashSet::new(), &HashSet::new(), Some("Chapter 4.cbz"), None, None), [3, 4]);
    }

    #[test]
    fn reading_order_prefers_mihon() {
        let c = |n: &str, order: i64| BackupChapter { name: n.into(), url: format!("/{n}"), source_order: order, ..Default::default() };
        // Mihon lists newest first (source order 0 = newest).
        let bs = BackupSeries { chapters: vec![c("b_Chapter 2", 0), c("a_Chapter 10", 1)], ..Default::default() };
        let mut v = vec!["b_Chapter 2.cbz".to_string(), "a_Chapter 10.cbz".to_string()];
        reading_order(&mut v, |s| s, |s| s, Some(&bs));
        assert_eq!(v, ["a_Chapter 10.cbz", "b_Chapter 2.cbz"]);
        // Without a backup: natural sort.
        let mut v = vec!["Chapter 10".to_string(), "Chapter 2".to_string(), "Chapter 1".to_string()];
        reading_order(&mut v, |s| s, |s| s, None);
        assert_eq!(v, ["Chapter 1", "Chapter 2", "Chapter 10"]);
    }

    #[test]
    fn scans_folder_and_progress() {
        let root = std::env::temp_dir().join(format!("manga-type-test-{}", std::process::id()));
        let dir = root.join("Src (EN)").join("Series");
        std::fs::create_dir_all(&dir).unwrap();
        for n in ["Chapter 2_abcdef.cbz", "Chapter 10_123456.cbz", "Chapter 1_000000.cbz", "x.cbz.part", "Chapter 3_tmp"] {
            std::fs::write(dir.join(n), b"").unwrap();
        }
        std::fs::write(dir.join("Chapter 1_000000.cbz.manga-type.json"), b"{}").unwrap();
        let mut p = ChapterProgress { finished: true, ..Default::default() };
        p.save(&dir.join("Chapter 1_000000.cbz")).unwrap();

        let lib = Library::load(&root);
        let s = &lib.series[0];
        let names: Vec<&str> = s.chapters.iter().map(|c| c.display_name.as_str()).collect();
        assert_eq!(names, ["Chapter 1", "Chapter 2", "Chapter 10"]);
        assert!(s.chapters[0].finished());
        assert_eq!(s.continue_index(), Some(1));
        assert_eq!(lib.next_after(&s.chapters[1].path), Some(s.chapters[2].path.clone()));
        assert_eq!(lib.next_after(&s.chapters[2].path), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn word_labels() {
        assert_eq!(thousands(7), "7");
        assert_eq!(thousands(4120), "4,120");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(words_label(412, false), "412 words");
        assert_eq!(words_label(4120, true), "~4,120 words");
        let mut s = Stats::default();
        s.correct = 500;
        assert_eq!(s.word_count(), (100, true));
        s.words = 93;
        assert_eq!(s.word_count(), (93, false));
    }

}
