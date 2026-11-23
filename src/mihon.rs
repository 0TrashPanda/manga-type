//! Read-only access to Mihon's reading progress through its automatic backups
//! (`<mihon folder>/autobackup/*.tachibk`: gzip-compressed protobuf).
//!
//! Only the fields manga-type needs are decoded; everything else is skipped.

use std::collections::HashMap;
use std::io::Read;

/// A decoded Mihon backup.
#[derive(Debug, Default)]
pub struct Backup {
    /// Source id -> source name (e.g. `Asura Scans`).
    pub sources: HashMap<i64, String>,
    /// Every series in the backup: library entries plus ones only in history.
    pub series: Vec<BackupSeries>,
}

#[derive(Debug, Default, Clone)]
pub struct BackupSeries {
    pub source_id: i64,
    pub url: String,
    pub title: String,
    /// In the Mihon library (as opposed to only having history).
    pub favorite: bool,
    pub chapters: Vec<BackupChapter>,
}

#[derive(Debug, Default, Clone)]
pub struct BackupChapter {
    pub url: String,
    pub name: String,
    pub scanlator: String,
    pub read: bool,
    /// Page index Mihon was on (0-based) if partly read.
    pub last_page_read: i64,
    /// Number parsed by Mihon; -1 if unknown. Can be odd (e.g. 5478), don't rely on it for order.
    pub chapter_number: f32,
    /// Position in the source's chapter list: 0 = newest.
    pub source_order: i64,
}

impl BackupChapter {
    /// The 6-char suffix Mihon puts on a downloaded chapter's file name: md5(url)[..6].
    pub fn download_hash(&self) -> String {
        format!("{:x}", md5::compute(self.url.as_bytes()))[..6].to_string()
    }
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

/// Decode a `.tachibk` file's bytes.
pub fn parse_backup(gzipped: &[u8]) -> Result<Backup, String> {
    let mut raw = Vec::new();
    flate2::read::GzDecoder::new(gzipped)
        .read_to_end(&mut raw)
        .map_err(|e| format!("not a gzip backup: {e}"))?;

    let mut backup = Backup::default();
    for field in Fields::new(&raw) {
        match field? {
            // repeated BackupManga backupManga = 1
            (1, Value::Bytes(b)) => backup.series.push(parse_series(b)?),
            // repeated BackupSource backupSources = 101 { name = 1, sourceId = 2 }
            (101, Value::Bytes(b)) => {
                let (mut name, mut id) = (String::new(), 0);
                for f in Fields::new(b) {
                    match f? {
                        (1, Value::Bytes(v)) => name = text(v),
                        (2, Value::Varint(v)) => id = v as i64,
                        _ => {}
                    }
                }
                backup.sources.insert(id, name);
            }
            _ => {}
        }
    }
    Ok(backup)
}

fn parse_series(buf: &[u8]) -> Result<BackupSeries, String> {
    // `favorite` defaults to true and is only written when false.
    let mut s = BackupSeries { favorite: true, ..Default::default() };
    for f in Fields::new(buf) {
        match f? {
            (1, Value::Varint(v)) => s.source_id = v as i64,
            (2, Value::Bytes(v)) => s.url = text(v),
            (3, Value::Bytes(v)) => s.title = text(v),
            (100, Value::Varint(v)) => s.favorite = v != 0,
            (16, Value::Bytes(v)) => s.chapters.push(parse_chapter(v)?),
            _ => {}
        }
    }
    Ok(s)
}

fn parse_chapter(buf: &[u8]) -> Result<BackupChapter, String> {
    let mut c = BackupChapter { chapter_number: -1.0, ..Default::default() };
    for f in Fields::new(buf) {
        match f? {
            (1, Value::Bytes(v)) => c.url = text(v),
            (2, Value::Bytes(v)) => c.name = text(v),
            (3, Value::Bytes(v)) => c.scanlator = text(v),
            (4, Value::Varint(v)) => c.read = v != 0,
            (6, Value::Varint(v)) => c.last_page_read = v as i64,
            (9, Value::Fixed32(v)) => c.chapter_number = f32::from_bits(v),
            (10, Value::Varint(v)) => c.source_order = v as i64,
            _ => {}
        }
    }
    Ok(c)
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// A protobuf field value (only the wire types protobuf actually uses).
enum Value<'a> {
    Varint(u64),
    Fixed64(#[allow(dead_code)] u64),
    Bytes(&'a [u8]),
    Fixed32(u32),
}

/// Iterates the top-level fields of one protobuf message.
struct Fields<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Fields<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn varint(&mut self) -> Result<u64, String> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = *self.buf.get(self.pos).ok_or("truncated varint")?;
            self.pos += 1;
            v |= u64::from(b & 0x7f) << shift;
            if b < 0x80 {
                return Ok(v);
            }
        }
        Err("varint too long".into())
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.buf.len()).ok_or("truncated field")?;
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn next_field(&mut self) -> Result<(u32, Value<'a>), String> {
        let key = self.varint()?;
        let (num, wire) = ((key >> 3) as u32, key & 7);
        let val = match wire {
            0 => Value::Varint(self.varint()?),
            1 => Value::Fixed64(u64::from_le_bytes(self.take(8)?.try_into().unwrap())),
            2 => {
                let n = usize::try_from(self.varint()?).map_err(|_| "length too large")?;
                Value::Bytes(self.take(n)?)
            }
            5 => Value::Fixed32(u32::from_le_bytes(self.take(4)?.try_into().unwrap())),
            w => return Err(format!("unsupported wire type {w}")),
        };
        Ok((num, val))
    }
}

impl<'a> Iterator for Fields<'a> {
    type Item = Result<(u32, Value<'a>), String>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let r = self.next_field();
        if r.is_err() {
            self.pos = self.buf.len(); // stop after an error
        }
        Some(r)
    }
}

// ---------------------------------------------------------------------------
// Matching backup entries to the download folders on the phone
// ---------------------------------------------------------------------------

/// Mihon's `DiskUtil.buildValidFilename`: trim dots/spaces, replace characters FAT can't store.
pub fn mihon_filename(name: &str) -> String {
    let trimmed = name.trim_matches(|c| c == '.' || c == ' ');
    if trimmed.is_empty() {
        return "(invalid)".into();
    }
    trimmed
        .chars()
        .map(|c| match c {
            '\0'..='\u{1f}' | '\u{7f}' | '"' | '*' | '/' | ':' | '<' | '>' | '?' | '\\' | '|' => '_',
            c => c,
        })
        .collect()
}

/// `Asura Scans (EN)` -> `Asura Scans`.
fn strip_lang(source_folder: &str) -> &str {
    match source_folder.rsplit_once(" (") {
        Some((name, lang)) if lang.ends_with(')') && lang.len() <= 8 => name,
        _ => source_folder,
    }
}

impl Backup {
    /// The backup entry for a download folder `downloads/<source_folder>/<title_folder>/`.
    /// If the same series exists more than once for that source, prefer the library entry,
    /// then the one with the most chapters.
    pub fn find_series(&self, source_folder: &str, title_folder: &str) -> Option<&BackupSeries> {
        let source_name = strip_lang(source_folder);
        let ids: Vec<i64> = self
            .sources
            .iter()
            .filter(|(_, n)| n.eq_ignore_ascii_case(source_name))
            .map(|(id, _)| *id)
            .collect();
        self.series
            .iter()
            .filter(|s| ids.contains(&s.source_id))
            .filter(|s| mihon_filename(&s.title).eq_ignore_ascii_case(title_folder))
            .max_by_key(|s| (s.favorite, s.chapters.len()))
    }
}

impl BackupSeries {
    /// The backup chapter for a downloaded chapter file/folder name
    /// (e.g. `Chapter 334 - Special Forces (7)_a3b2e8.cbz`).
    ///
    /// 1. by the md5(url) suffix (exact, unless the source changed its URLs since download);
    /// 2. by name: `<scanlator>_<name>` or `<name>`, as Mihon builds it;
    /// 3. by name ignoring the scanlator prefix (it changes when sources move domains).
    pub fn match_chapter(&self, file_name: &str) -> Option<&BackupChapter> {
        let stem = file_name.strip_suffix(".cbz").unwrap_or(file_name);
        let (base, hash) = match stem.rsplit_once('_') {
            Some((b, h)) if h.len() == 6 && h.chars().all(|c| c.is_ascii_hexdigit()) => (b, Some(h)),
            _ => (stem, None),
        };

        if let Some(h) = hash
            && let Some(c) = self.chapters.iter().find(|c| c.download_hash() == h)
        {
            return Some(c);
        }

        let full = |c: &BackupChapter| {
            if c.scanlator.trim().is_empty() {
                mihon_filename(&c.name)
            } else {
                mihon_filename(&format!("{}_{}", c.scanlator, c.name))
            }
        };
        if let Some(c) = self.chapters.iter().find(|c| full(c) == base) {
            return Some(c);
        }

        let mut by_name = self.chapters.iter().filter(|c| {
            let n = mihon_filename(&c.name);
            base == n || base.ends_with(&format!("_{n}"))
        });
        // Only accept an unambiguous name match.
        match (by_name.next(), by_name.next()) {
            (Some(c), None) => Some(c),
            _ => None,
        }
    }

    /// Chapters in reading order (oldest first), using Mihon's source order.
    #[allow(dead_code)] // used by the library UI (step 6)
    pub fn reading_order(&self) -> Vec<&BackupChapter> {
        let mut v: Vec<&BackupChapter> = self.chapters.iter().collect();
        v.sort_by_key(|c| std::cmp::Reverse(c.source_order));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(url: &str, name: &str, scan: &str) -> BackupChapter {
        BackupChapter { url: url.into(), name: name.into(), scanlator: scan.into(), ..Default::default() }
    }

    #[test]
    fn filename_rules() {
        assert_eq!(mihon_filename("Kumo Desu Ga, Nani Ka?"), "Kumo Desu Ga, Nani Ka_");
        assert_eq!(mihon_filename("The Lord’s Coins Aren’t Decreasing?!"), "The Lord’s Coins Aren’t Decreasing_!");
        assert_eq!(mihon_filename(" .x. "), "x");
    }

    #[test]
    fn source_lang() {
        assert_eq!(strip_lang("Asura Scans (EN)"), "Asura Scans");
        assert_eq!(strip_lang("Weeb Central (EN)"), "Weeb Central");
        assert_eq!(strip_lang("Plain"), "Plain");
    }

    #[test]
    fn match_by_hash_then_name() {
        // Hash verified against the user's phone: md5("/series/nano-machine/chapter/334")... is
        // not asserted here; just check the mechanism with a computed hash.
        let a = ch("/series/x/chapter/1", "Chapter 1", "");
        let file = format!("Chapter 1_{}.cbz", a.download_hash());
        let s = BackupSeries { chapters: vec![a, ch("/u2", "Chapter 1.2", "www.natomanga.com")], ..Default::default() };
        assert_eq!(s.match_chapter(&file).unwrap().name, "Chapter 1");
        // URL/domain changed since download: hash and scanlator differ, name still matches.
        assert_eq!(s.match_chapter("www.manganato.gg_Chapter 1.2_a4eba9.cbz").unwrap().name, "Chapter 1.2");
        assert!(s.match_chapter("Chapter 9_ffffff.cbz").is_none());
    }

    #[test]
    fn protobuf_roundtrip_minimal() {
        // series { source=5, title="T", chapter { name="C", read=1, number=2.5 } }, gzipped
        let mut chap = vec![0x12, 1, b'C', 0x20, 1, 0x4d];
        chap.extend_from_slice(&2.5f32.to_le_bytes());
        let mut series = vec![0x08, 5, 0x1a, 1, b'T', 0x82, 0x01, chap.len() as u8];
        series.extend_from_slice(&chap);
        let mut top = vec![0x0a, series.len() as u8];
        top.extend_from_slice(&series);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &top).unwrap();
        let b = parse_backup(&gz.finish().unwrap()).unwrap();
        let s = &b.series[0];
        assert_eq!((s.source_id, s.title.as_str(), s.favorite), (5, "T", true));
        let c = &s.chapters[0];
        assert_eq!((c.name.as_str(), c.read, c.chapter_number), ("C", true, 2.5));
    }
}
