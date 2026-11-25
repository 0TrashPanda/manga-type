//! Reading sessions, one JSON line each in `$XDG_DATA_HOME/manga-type/sessions.jsonl`
//! (`~/.local/share/manga-type/sessions.jsonl`), appended when you leave a chapter.
//! The stats page uses them for activity per day and progress over time; per-chapter
//! totals live in the chapters' progress files.

use std::io::Write;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Session {
    /// Unix seconds when the session ended.
    pub ended: u64,
    pub series: String,
    pub chapter: String,
    /// Time spent (see `Reader::track_time`).
    pub seconds: f64,
    /// Typing during the session.
    pub correct: u64,
    pub wrong: u64,
    pub uncorrected: u64,
    pub typing_seconds: f64,
    /// Bubbles moved past (typed or skipped).
    pub bubbles: usize,
    /// The chapter was finished in this session.
    pub finished: bool,
}

fn path() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::home_dir().map(|h| h.join(".local").join("share")))
        .map(|d| d.join("manga-type").join("sessions.jsonl"))
}

pub fn append(s: &Session) -> Result<(), String> {
    let path = path().ok_or("no home directory")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let line = serde_json::to_string(s).map_err(|e| e.to_string())?;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path).map_err(|e| e.to_string())?;
    writeln!(f, "{line}").map_err(|e| e.to_string())
}

/// All sessions, oldest first. Unreadable lines are skipped.
pub fn load() -> Vec<Session> {
    let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) else { return Vec::new() };
    let mut v: Vec<Session> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    v.sort_by_key(|s| s.ended);
    v
}

/// The local timezone's offset from UTC in seconds (from `date +%z`; 0 if unknown).
pub fn utc_offset() -> i64 {
    let out = std::process::Command::new("date").arg("+%z").output().ok();
    let s = out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    if s.len() != 5 {
        return 0;
    }
    let sign = if s.starts_with('-') { -1 } else { 1 };
    let h: i64 = s[1..3].parse().unwrap_or(0);
    let m: i64 = s[3..5].parse().unwrap_or(0);
    sign * (h * 3600 + m * 60)
}

/// Local day number (days since 1970-01-01) of a unix time.
pub fn local_day(unix: u64, offset: i64) -> i64 {
    (unix as i64 + offset).div_euclid(86_400)
}

/// "Mon 24" style label for a local day number.
pub fn day_label(day: i64) -> String {
    const NAMES: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"]; // 1970-01-01 was a Thursday
    let (_, _, d) = civil_from_days(day);
    format!("{} {d}", NAMES[day.rem_euclid(7) as usize])
}

/// Days since 1970-01-01 -> (year, month, day). Howard Hinnant's algorithm.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2026-11-25 is day 20_782 and a Wednesday.
        assert_eq!(civil_from_days(20_782), (2026, 11, 25));
        assert_eq!(day_label(20_782), "Wed 25");
        assert_eq!(local_day(86_399, 3600), 1);
    }
}
