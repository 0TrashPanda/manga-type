//! Moving files to the desktop Trash (freedesktop.org spec, home trash only) and back.
//! `$XDG_DATA_HOME/Trash/files/<name>` holds the file, `Trash/info/<name>.trashinfo` its
//! original path and deletion time, so file managers can restore it too.

use std::path::{Path, PathBuf};

/// A file or folder moved to the Trash by [`trash`].
#[derive(Debug, Clone)]
pub struct Trashed {
    pub original: PathBuf,
    pub in_trash: PathBuf,
    pub info: PathBuf,
}

fn trash_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::home_dir().map(|h| h.join(".local").join("share")))
        .map(|d| d.join("Trash"))
}

/// Percent-encode a path for the `Path=` line (keep `/` and unreserved characters).
fn encode_path(p: &Path) -> String {
    let mut out = String::new();
    for b in p.to_string_lossy().as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => out.push(*b as char),
            b => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Local time as `YYYY-MM-DDThh:mm:ss`.
fn now_local() -> String {
    let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let t = unix + crate::history::utc_offset();
    let (y, m, d) = crate::history::civil_from_days(t.div_euclid(86_400));
    let s = t.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// Move `path` (a file or folder) to the Trash. Fails without touching it if it can't be
/// moved there (for example a library on another drive than the home folder).
pub fn trash(path: &Path) -> Result<Trashed, String> {
    trash_in(path, &trash_dir().ok_or("no home directory")?)
}

/// [`trash`] into a given Trash folder (tests use a private one).
pub(crate) fn trash_in(path: &Path, dir: &Path) -> Result<Trashed, String> {
    let (files, info) = (dir.join("files"), dir.join("info"));
    std::fs::create_dir_all(&files).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&info).map_err(|e| e.to_string())?;
    let original = std::path::absolute(path).map_err(|e| e.to_string())?;
    let base = original.file_name().ok_or("no file name")?.to_string_lossy().into_owned();
    // A free name in the Trash: "name", "name (2)", ...
    let mut name = base.clone();
    let mut n = 2;
    while files.join(&name).exists() || info.join(format!("{name}.trashinfo")).exists() {
        name = format!("{base} ({n})");
        n += 1;
    }
    let info_path = info.join(format!("{name}.trashinfo"));
    std::fs::write(&info_path, format!("[Trash Info]\nPath={}\nDeletionDate={}\n", encode_path(&original), now_local()))
        .map_err(|e| e.to_string())?;
    let in_trash = files.join(&name);
    if let Err(e) = std::fs::rename(&original, &in_trash) {
        let _ = std::fs::remove_file(&info_path);
        return Err(format!("couldn't move {} to the Trash: {e}", original.display()));
    }
    Ok(Trashed { original, in_trash, info: info_path })
}

/// Put a trashed file back where it was (unless something new is there now).
pub fn restore(t: &Trashed) -> Result<(), String> {
    if t.original.exists() {
        return Err(format!("{} exists again; left the Trash copy alone", t.original.display()));
    }
    if let Some(parent) = t.original.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::rename(&t.in_trash, &t.original).map_err(|e| format!("couldn't restore {}: {e}", t.original.display()))?;
    let _ = std::fs::remove_file(&t.info);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_like_the_spec() {
        assert_eq!(encode_path(Path::new("/home/a b/Chapter 1_ab.cbz")), "/home/a%20b/Chapter%201_ab.cbz");
        assert_eq!(encode_path(Path::new("/x/Star’s")), "/x/Star%E2%80%99s");
    }

    #[test]
    fn trash_and_restore() {
        let root = std::env::temp_dir().join(format!("manga-type-trash-{}", std::process::id()));
        let data = root.join("data");
        let lib = root.join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        let file = lib.join("Chapter 1.cbz");
        std::fs::write(&file, b"x").unwrap();
        // A private Trash for the test.
        let t = trash_in(&file, &data.join("Trash")).unwrap();
        assert!(!file.exists() && t.in_trash.exists());
        let info = std::fs::read_to_string(&t.info).unwrap();
        assert!(info.starts_with("[Trash Info]\nPath=/") && info.contains("Chapter%201.cbz") && info.contains("DeletionDate=20"));
        restore(&t).unwrap();
        assert!(file.exists() && !t.in_trash.exists() && !t.info.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
