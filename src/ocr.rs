//! OCR: find speech bubbles in a chapter.
//!
//! Pipeline: strip bands (see `pages.rs`) -> OCR engine -> text lines -> merged into
//! bubbles -> de-duplicated across band overlaps -> reading order. Runs on background
//! workers, top of the chapter first, and caches the result as JSON next to the chapter.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;

use image::GrayImage;
use serde::{Deserialize, Serialize};

use super::pages::{Band, Chapter, PageCache};

/// Band height and overlap in strip pixels. The overlap must be taller than a bubble's
/// text block so every bubble is whole in at least one band.
pub const BAND_HEIGHT: f32 = 2400.0;
pub const BAND_OVERLAP: f32 = 400.0;
/// Words below this Tesseract confidence are dropped.
const MIN_WORD_CONF: f32 = 45.0;
/// Bump when the merge/filter rules change, so old caches get re-scanned.
// 10: second, thresholded OCR pass on dark bands (glowing light-on-dark lettering).
// 11: every bubble re-read on its own as a text block (psm 6), better reading kept.
// 12: caches record their engine (PaddleOCR replaces Tesseract when its models are there).
// 13: bubbles record how plain the background around their lines is (`framed`).
const CACHE_VERSION: u32 = 13;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// An axis-aligned box. In band pixels inside the engine, strip coordinates elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl Rect {
    pub fn w(&self) -> f32 {
        self.x1 - self.x0
    }
    pub fn h(&self) -> f32 {
        self.y1 - self.y0
    }
    fn union(&self, o: &Rect) -> Rect {
        Rect { x0: self.x0.min(o.x0), y0: self.y0.min(o.y0), x1: self.x1.max(o.x1), y1: self.y1.max(o.y1) }
    }
    fn area(&self) -> f32 {
        self.w().max(0.0) * self.h().max(0.0)
    }
    fn intersection(&self, o: &Rect) -> f32 {
        let w = self.x1.min(o.x1) - self.x0.max(o.x0);
        let h = self.y1.min(o.y1) - self.y0.max(o.y0);
        if w <= 0.0 || h <= 0.0 { 0.0 } else { w * h }
    }
    /// Horizontal overlap as a fraction of the narrower box.
    fn h_overlap(&self, o: &Rect) -> f32 {
        let ov = self.x1.min(o.x1) - self.x0.max(o.x0);
        ov.max(0.0) / self.w().min(o.w()).max(1.0)
    }
    /// Vertical overlap as a fraction of the shorter box.
    fn v_overlap(&self, o: &Rect) -> f32 {
        let ov = self.y1.min(o.y1) - self.y0.max(o.y0);
        ov.max(0.0) / self.h().min(o.h()).max(1.0)
    }
    fn shifted(&self, dy: f32) -> Rect {
        Rect { y0: self.y0 + dy, y1: self.y1 + dy, ..*self }
    }
}

/// One line of text found by the engine.
#[derive(Debug, Clone)]
pub struct Line {
    pub rect: Rect,
    pub text: String,
    pub conf: f32,
    /// The words with their boxes (left to right); `text` is them joined by spaces.
    pub words: Vec<(Rect, String)>,
}

/// A speech bubble: merged lines.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bubble {
    /// Text box in strip coordinates.
    pub rect: Rect,
    /// Text as read by OCR.
    pub text: String,
    /// Mean word confidence.
    pub conf: f32,
    /// User's correction ("Edit text"), used instead of `text` when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited: Option<String>,
    /// OCR text before automatic word correction (`fix.rs`), if it changed anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    /// How plain the background right around each text line is (0-1, mean over lines):
    /// ~1 inside speech bubbles and boxes, lower for text on art or textured game windows.
    /// `None` when not measured (Tesseract path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub framed: Option<f32>,
}

impl Bubble {
    /// The text to type.
    pub fn target(&self) -> &str {
        self.edited.as_deref().unwrap_or(&self.text)
    }
}

/// An OCR backend. Tesseract today; `ocrs` could be dropped in later.
pub trait OcrEngine: Send + Sync {
    /// Find text anywhere in a band (sparse text).
    fn lines(&self, img: &GrayImage) -> Result<Vec<Line>, String>;
    /// Read a crop around one bubble as a single block of text.
    fn block(&self, img: &GrayImage) -> Result<Vec<Line>, String> {
        self.lines(img)
    }
    /// Stored in the OCR cache: a cache from another engine is rescanned.
    fn name(&self) -> &'static str {
        "tesseract"
    }
    /// Tesseract needs help: upscaling, a thresholded pass on dark bands, and a block
    /// re-read of every bubble. PaddleOCR reads the band as it is.
    fn workarounds(&self) -> bool {
        true
    }
}

/// An engine plus an optional note for the user (why it isn't PaddleOCR), or why there's none.
pub type EngineChoice = Result<(Arc<dyn OcrEngine>, Option<String>), String>;

/// The OCR engine to use, loaded once per run: PaddleOCR when its models are installed
/// (`scripts/install.sh`), else Tesseract. The second value is a note for the user when
/// falling back.
pub fn engine() -> EngineChoice {
    static ENGINE: std::sync::OnceLock<EngineChoice> = std::sync::OnceLock::new();
    ENGINE
        .get_or_init(|| match crate::paddle::Paddle::load_default() {
            Ok(p) => Ok((Arc::new(p) as Arc<dyn OcrEngine>, None)),
            Err(paddle_err) => {
                let tess = Tesseract::default();
                match tess.check() {
                    Ok(()) => Ok((
                        Arc::new(tess) as Arc<dyn OcrEngine>,
                        Some(format!("Using Tesseract (much less accurate): {paddle_err}. Run scripts/install.sh to get PaddleOCR.")),
                    )),
                    Err(e) => Err(format!("No OCR engine: {paddle_err}; {e}")),
                }
            }
        })
        .clone()
}

// ---------------------------------------------------------------------------
// Tesseract CLI
// ---------------------------------------------------------------------------

/// Runs the `tesseract` command (one process per band, single-threaded, so workers scale).
pub struct Tesseract {
    pub binary: PathBuf,
}

impl Default for Tesseract {
    fn default() -> Self {
        Self { binary: PathBuf::from("tesseract") }
    }
}

impl Tesseract {
    /// Check the binary runs and has English data.
    pub fn check(&self) -> Result<(), String> {
        let out = Command::new(&self.binary)
            .arg("--list-langs")
            .output()
            .map_err(|e| format!("can't run {}: {e} (pacman -S tesseract tesseract-data-eng)", self.binary.display()))?;
        let langs = String::from_utf8_lossy(&out.stdout);
        if langs.lines().any(|l| l.trim() == "eng") {
            Ok(())
        } else {
            Err("tesseract has no English data (pacman -S tesseract-data-eng)".into())
        }
    }
}

impl OcrEngine for Tesseract {
    fn lines(&self, img: &GrayImage) -> Result<Vec<Line>, String> {
        self.run(img, "11")
    }

    fn block(&self, img: &GrayImage) -> Result<Vec<Line>, String> {
        self.run(img, "6")
    }
}

impl Tesseract {
    /// Run tesseract with a page segmentation mode: 11 = sparse text, 6 = one text block.
    fn run(&self, img: &GrayImage, psm: &str) -> Result<Vec<Line>, String> {
        let mut png = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .map_err(|e| e.to_string())?;

        let mut child = Command::new(&self.binary)
            .args(["-", "-", "--psm", psm, "-l", "eng", "tsv"])
            .env("OMP_THREAD_LIMIT", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("can't run tesseract: {e}"))?;
        // Write on another thread: tesseract may start writing output before reading all input.
        let mut stdin = child.stdin.take().unwrap();
        let writer = thread::spawn(move || stdin.write_all(&png));
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        let _ = writer.join();
        if !out.status.success() {
            return Err(format!("tesseract failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
        Ok(parse_tsv(&String::from_utf8_lossy(&out.stdout)))
    }
}

/// Tesseract TSV -> lines. Words below [`MIN_WORD_CONF`] are dropped before building lines.
fn parse_tsv(tsv: &str) -> Vec<Line> {
    // (block, par, line) -> words (left, top, w, h, conf, text)
    let mut groups: BTreeMap<(u32, u32, u32), Vec<(f32, f32, f32, f32, f32, String)>> = BTreeMap::new();
    for row in tsv.lines().skip(1) {
        let c: Vec<&str> = row.split('\t').collect();
        if c.len() < 12 || c[0] != "5" {
            continue;
        }
        let num = |i: usize| c[i].trim().parse::<f32>().unwrap_or(-1.0);
        let conf = num(10);
        let Some(text) = clean_word(c[11]) else { continue };
        if conf < MIN_WORD_CONF {
            continue;
        }
        let key = (num(2) as u32, num(3) as u32, num(4) as u32);
        groups.entry(key).or_default().push((num(6), num(7), num(8), num(9), conf, text));
    }
    groups
        .into_values()
        .map(|mut words| {
            words.sort_by(|a, b| a.0.total_cmp(&b.0));
            let rect = words.iter().fold(None::<Rect>, |r, w| {
                let wr = Rect { x0: w.0, y0: w.1, x1: w.0 + w.2, y1: w.1 + w.3 };
                Some(r.map_or(wr, |r| r.union(&wr)))
            });
            Line {
                rect: rect.unwrap(),
                conf: words.iter().map(|w| w.4).sum::<f32>() / words.len() as f32,
                text: words.iter().map(|w| w.5.as_str()).collect::<Vec<_>>().join(" "),
                words: words.into_iter().map(|w| (Rect { x0: w.0, y0: w.1, x1: w.0 + w.2, y1: w.1 + w.3 }, w.5)).collect(),
            }
        })
        .collect()
}

/// Characters that are almost always artwork/speed lines read as text.
const STRAY: &[char] = &['|', '\\', '/', '_', '~', '=', '<', '>', '{', '}', '[', ']', '®', '©', '«', '»', '¢', '§'];

/// Trim stray marks off a word's ends; drop it if it's mostly marks
/// (`/////W`, `\\`). Returns `None` for words to drop.
fn clean_word(word: &str) -> Option<String> {
    // Judge the word as read (before trimming), so `/////W` doesn't become a plausible `W`.
    let raw = word.trim();
    let total = raw.chars().count();
    let stray = raw.chars().filter(|c| STRAY.contains(c)).count();
    let alnum = raw.chars().filter(|c| c.is_alphanumeric()).count();
    // Pure punctuation like "..." or "!!" is fine; mostly marks is not.
    if total == 0 || stray * 2 > total || (alnum == 0 && stray > 0) {
        return None;
    }
    let w = raw.trim_matches(STRAY);
    (!w.is_empty()).then(|| w.to_string())
}

// ---------------------------------------------------------------------------
// Lines -> bubbles
// ---------------------------------------------------------------------------

/// Merge lines into bubbles: sorted top to bottom, a line joins a group when the gap to
/// the group's bottom is under 1.3x the line height and they overlap horizontally >= 30%.
#[cfg(test)] // the app uses group_lines + a re-read; tests check the grouping rules
pub fn merge_lines(lines: Vec<Line>) -> Vec<Bubble> {
    group_lines(lines)
        .into_iter()
        .map(|g| group_bubble(&g))
        .filter(|b| looks_like_text(&b.text, b.conf))
        .collect()
}

fn group_bubble(g: &[Line]) -> Bubble {
    let rect = g.iter().skip(1).fold(g[0].rect, |r, l| r.union(&l.rect));
    let conf = g.iter().map(|l| l.conf).sum::<f32>() / g.len() as f32;
    Bubble { rect, conf, text: join_lines(g.iter().map(|l| l.text.as_str())), edited: None, raw: None, framed: None }
}

/// Group lines into bubbles (see [`merge_lines`]), without the junk filter.
fn group_lines(mut lines: Vec<Line>) -> Vec<Vec<Line>> {
    lines.sort_by(|a, b| a.rect.y0.total_cmp(&b.rect.y0));
    let mut groups: Vec<Vec<Line>> = Vec::new();
    for line in lines {
        let h = line.rect.h().max(1.0);
        let target = groups.iter_mut().rev().find(|g| {
            let last = g.last().unwrap();
            let gap = line.rect.y0 - last.rect.y1;
            let bbox = g.iter().skip(1).fold(g[0].rect, |r, l| r.union(&l.rect));
            gap < 1.3 * h && gap > -0.5 * h && line.rect.h_overlap(&bbox) >= 0.3
        });
        match target {
            Some(g) => g.push(line),
            None => groups.push(vec![line]),
        }
    }
    groups
}

/// Reject OCR read from artwork. Tuned on Eleceed ch 1.2 (2026-11-23): of 271 candidates
/// it kept 121 (nearly all real dialogue) and dropped mostly junk like `l‘l`, `N A`,
/// `/ ’\%p ’HII“`. Real text it drops is mostly sound effects (`yawn’`, `HAAH...`).
pub fn looks_like_text(text: &str, conf: f32) -> bool {
    const NEUTRAL: &str = ".,!?'-…:;’\"";
    let letters = text.chars().filter(|c| c.is_alphabetic()).count();
    let digits = text.chars().filter(|c| c.is_ascii_digit()).count();
    let junk = text
        .chars()
        .filter(|c| !c.is_alphanumeric() && !c.is_whitespace() && !NEUTRAL.contains(*c))
        .count();
    if letters < 2 || (letters as f32) < 0.75 * (letters + junk + digits) as f32 {
        return false;
    }
    // Words of 2+ letters (apostrophes allowed inside).
    let words: Vec<&str> = text
        .split(|c: char| !(c.is_alphabetic() || c == '\'' || c == '’'))
        .filter(|w| w.trim_matches(|c| c == '\'' || c == '’').chars().count() >= 2)
        .collect();
    let long = words.iter().filter(|w| w.chars().count() >= 3).count();
    if conf >= 60.0 && long >= 2 {
        return true;
    }
    if conf >= 80.0 && (words.len() >= 2 || long >= 1) {
        return true;
    }
    // A single shouted word: "HUH?", "HAM..."
    let exclaim = text.contains(['!', '?']) || text.contains("...") || text.contains('…');
    conf >= 60.0
        && words.len() == 1
        && words[0].chars().count() >= 3
        && words[0].chars().all(|c| !c.is_lowercase())
        && exclaim
}

/// Join a bubble's lines; a word hyphenated at a line end is joined back ("BE-" + "CAUSE").
fn join_lines<'a>(lines: impl Iterator<Item = &'a str>) -> String {
    let mut out = String::new();
    for l in lines {
        let l = l.trim();
        if out.is_empty() {
            out.push_str(l);
        } else if out.ends_with('-')
            && !out.ends_with("--")
            && out[..out.len() - 1].ends_with(char::is_alphabetic)
            && l.starts_with(char::is_alphabetic)
        {
            out.pop();
            out.push_str(l);
        } else {
            out.push(' ');
            out.push_str(l);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Bands -> chapter bubbles
// ---------------------------------------------------------------------------

/// OCR result for one band, already in strip coordinates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BandResult {
    pub band: usize,
    pub bubbles: Vec<Bubble>,
    /// Bubbles touching a band cut (likely truncated there).
    pub clipped: Vec<bool>,
}

/// Run the engine on one band and convert to strip coordinates.
pub fn ocr_band(
    ch: &Chapter,
    engine: &dyn OcrEngine,
    index: usize,
    band: Band,
    cache: &mut PageCache,
) -> Result<BandResult, String> {
    let img = ch.render_band(band, cache)?;
    if !engine.workarounds() {
        // PaddleOCR: detect and read text lines on the band as it is, group into bubbles.
        let top = band.y0.floor();
        let bubbles: Vec<Bubble> = group_lines(engine.lines(&img)?)
            .into_iter()
            .map(|g| {
                let mut b = group_bubble(&g);
                let rings: Vec<f32> = g.iter().filter_map(|l| plain_ring(&img, &l.rect)).collect();
                b.framed = (!rings.is_empty()).then(|| rings.iter().sum::<f32>() / rings.len() as f32);
                b
            })
            .filter(|b| looks_like_text(&b.text, b.conf))
            .map(|mut b| {
                b.rect = b.rect.shifted(top);
                b
            })
            .collect();
        return Ok(band_result(ch, index, band, bubbles));
    }
    let mut img = img;
    // Tesseract does much better on larger lettering: on a 690 px Eleceed page, 2x fixed
    // dropped words ("ABLE TO GIVE": TO went from confidence 41 to 87) and misreads.
    let scale = ocr_scale(ch.strip_width);
    if scale > 1.05 {
        let (w, h) = ((img.width() as f32 * scale) as u32, (img.height() as f32 * scale) as u32);
        img = image::imageops::resize(&img, w, h, image::imageops::FilterType::CatmullRom);
    }
    let mut lines = engine.lines(&img)?;
    // Glowing light-on-dark lettering ("IF IT WISHES TO SHINE" over a starry sky) reads as
    // junk; a hard black/white version of the band reads it fine. Only dark bands get the
    // second pass (about 1 in 15 on Star-Embracing Swordmaster), so it costs little.
    if mean_luma(&img) < DARK_BAND_MEAN {
        let bw = threshold_dark_text(&img);
        lines = merge_passes(lines, engine.lines(&bw)?);
    }
    // The sparse pass finds text well but reads some clean bubbles badly, depending on
    // scale ("LET'S GET OUT" -> "LET;:S:bEEE ouT"). Re-read each bubble on its own as a text
    // block and keep the better reading.
    let mut bubbles: Vec<Bubble> = Vec::new();
    for g in group_lines(lines) {
        let sparse = group_bubble(&g);
        let reread = reread_block(engine, &img, &g)?;
        let pick = match reread {
            Some(b) if better_reading(&b, &sparse) => b,
            _ => sparse,
        };
        if looks_like_text(&pick.text, pick.conf) {
            bubbles.push(pick);
        }
    }
    let bubbles = dedupe_overlapping(bubbles);
    let top = band.y0.floor();
    let bubbles: Vec<Bubble> = bubbles
        .into_iter()
        .map(|mut b| {
            b.rect = Rect { x0: b.rect.x0 / scale, y0: b.rect.y0 / scale, x1: b.rect.x1 / scale, y1: b.rect.y1 / scale }
                .shifted(top);
            b
        })
        .collect();
    Ok(band_result(ch, index, band, bubbles))
}

fn band_result(ch: &Chapter, index: usize, band: Band, bubbles: Vec<Bubble>) -> BandResult {
    // Text within this margin of a cut (not the strip's own ends) may be cut off.
    let margin = 12.0;
    let clipped = bubbles
        .iter()
        .map(|b| {
            (band.y0 > 0.0 && b.rect.y0 < band.y0 + margin)
                || (band.y1 < ch.strip_height - 0.5 && b.rect.y1 > band.y1 - margin)
        })
        .collect();
    BandResult { band: index, bubbles, clipped }
}

/// Re-read a group of lines as one text block, from a crop padded by about a line height
/// (so a line the sparse pass missed, like "AH, YES." above, is read too). `None` if the
/// crop is too small or nothing was read. Coordinates stay in the band image's pixels.
fn reread_block(engine: &dyn OcrEngine, img: &GrayImage, group: &[Line]) -> Result<Option<Bubble>, String> {
    let rect = group.iter().skip(1).fold(group[0].rect, |r, l| r.union(&l.rect));
    let lh = group.iter().map(|l| l.rect.h()).sum::<f32>() / group.len() as f32;
    if rect.w() < 8.0 || lh < 6.0 {
        return Ok(None);
    }
    let (pad_x, pad_y) = (0.8 * lh, 1.3 * lh);
    let x0 = (rect.x0 - pad_x).max(0.0) as u32;
    let y0 = (rect.y0 - pad_y).max(0.0) as u32;
    let x1 = ((rect.x1 + pad_x) as u32).min(img.width());
    let y1 = ((rect.y1 + pad_y) as u32).min(img.height());
    if x1 <= x0 + 4 || y1 <= y0 + 4 {
        return Ok(None);
    }
    let crop = image::imageops::crop_imm(img, x0, y0, x1 - x0, y1 - y0).to_image();
    let lines = engine.block(&crop)?;
    if lines.is_empty() {
        return Ok(None);
    }
    let shift = |r: Rect| Rect { x0: r.x0 + x0 as f32, y0: r.y0 + y0 as f32, x1: r.x1 + x0 as f32, y1: r.y1 + y0 as f32 };
    // The padding also picks up the bubble's outline, read as stray letters ("4 N W-WHAT
    // ... Y"): drop 1-2 character words that lie outside the text the sparse pass found.
    let core = Rect { x0: rect.x0 - 0.3 * lh, y0: rect.y0 - 0.2 * lh, x1: rect.x1 + 0.3 * lh, y1: rect.y1 + 0.2 * lh };
    let lines: Vec<Line> = lines
        .into_iter()
        .filter_map(|mut l| {
            let words: Vec<(Rect, String)> = l
                .words
                .iter()
                .map(|(r, w)| (shift(*r), w.clone()))
                .filter(|(r, w)| w.chars().count() > 2 || r.intersection(&core) > 0.5 * r.area())
                .collect();
            let first = words.first()?;
            l.rect = words.iter().skip(1).fold(first.0, |acc, (r, _)| acc.union(r));
            l.text = words.iter().map(|(_, w)| w.as_str()).collect::<Vec<_>>().join(" ");
            l.words = words;
            Some(l)
        })
        .collect();
    if lines.is_empty() {
        return Ok(None);
    }
    let mut lines = lines;
    lines.sort_by(|a, b| a.rect.y0.total_cmp(&b.rect.y0));
    Ok(Some(group_bubble(&lines)))
}

/// Letters weighted by confidence, minus a penalty for junk characters.
fn reading_score(b: &Bubble) -> f32 {
    const NEUTRAL: &str = ".,!?'-…:;’\"";
    let letters = b.text.chars().filter(|c| c.is_alphabetic()).count() as f32;
    let junk = b.text.chars().filter(|c| !c.is_alphanumeric() && !c.is_whitespace() && !NEUTRAL.contains(*c)).count() as f32;
    (letters - 3.0 * junk) * b.conf
}

/// Whether the block re-read should replace the sparse reading: when the sparse one is
/// junk, when the re-read keeps every letter at about the same confidence, or when it's
/// clearly better. A re-read that drops a word ("DID [YOU] COMMIT") loses.
fn better_reading(reread: &Bubble, sparse: &Bubble) -> bool {
    if !looks_like_text(&reread.text, reread.conf) {
        return false;
    }
    let letters = |b: &Bubble| b.text.chars().filter(|c| c.is_alphabetic()).count();
    let (new, old) = (reading_score(reread), reading_score(sparse));
    !looks_like_text(&sparse.text, sparse.conf)
        || (letters(reread) >= letters(sparse) && new >= 0.9 * old)
        || new >= 1.25 * old
}

/// Two re-reads of neighbouring groups can return the same bubble ("LET'S GO GET THE" and
/// "GET THE TEMS!" both re-read as the whole bubble): keep the better of overlapping ones.
fn dedupe_overlapping(mut bubbles: Vec<Bubble>) -> Vec<Bubble> {
    bubbles.sort_by(|a, b| reading_score(b).total_cmp(&reading_score(a)));
    let mut kept: Vec<Bubble> = Vec::new();
    for b in bubbles {
        let dup = kept.iter().any(|k| k.rect.intersection(&b.rect) > 0.5 * k.rect.area().min(b.rect.area()));
        if !dup {
            kept.push(b);
        }
    }
    kept.sort_by(|a, b| a.rect.y0.total_cmp(&b.rect.y0));
    kept
}

/// Share of pixels in a thin ring around a text line that are within 25 grey levels of
/// the ring's median: ~1 on a plain bubble (white, black or grey), lower on art.
fn plain_ring(img: &GrayImage, r: &Rect) -> Option<f32> {
    let pad = (0.25 * r.h()).max(3.0);
    let (w, h) = (img.width() as f32, img.height() as f32);
    let (x0, y0) = ((r.x0 - pad).max(0.0) as u32, (r.y0 - pad).max(0.0) as u32);
    let (x1, y1) = ((r.x1 + pad).min(w) as u32, (r.y1 + pad).min(h) as u32);
    let mut px: Vec<u8> = Vec::new();
    for y in y0..y1 {
        for x in x0..x1 {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let inside = fx >= r.x0 && fx < r.x1 && fy >= r.y0 && fy < r.y1;
            if !inside {
                px.push(img.get_pixel(x, y).0[0]);
            }
        }
    }
    if px.len() < 20 {
        return None;
    }
    let mut sorted = px.clone();
    sorted.sort_unstable();
    let med = sorted[sorted.len() / 2] as i16;
    Some(px.iter().filter(|&&p| (p as i16 - med).abs() <= 25).count() as f32 / px.len() as f32)
}

/// Bands darker than this (mean grey, 0-255) also get the thresholded OCR pass.
const DARK_BAND_MEAN: f32 = 100.0;

fn mean_luma(img: &GrayImage) -> f32 {
    let n = (img.width() as u64 * img.height() as u64).max(1);
    img.as_raw().iter().map(|&p| p as u64).sum::<u64>() as f32 / n as f32
}

/// Bright pixels (>= 60% grey) become black text on white, everything else white.
fn threshold_dark_text(img: &GrayImage) -> GrayImage {
    let mut out = img.clone();
    for p in out.pixels_mut() {
        p.0[0] = if p.0[0] >= 153 { 0 } else { 255 };
    }
    out
}

/// How much readable text a line holds: letters/digits weighted by confidence.
fn line_score(l: &Line) -> f32 {
    l.text.chars().filter(|c| c.is_alphanumeric()).count() as f32 * l.conf
}

/// Combine the normal and thresholded passes: where they overlap, keep whichever reads
/// more text with more confidence; lines only one pass found are kept.
fn merge_passes(mut base: Vec<Line>, extra: Vec<Line>) -> Vec<Line> {
    for e in extra {
        let overlaps = |b: &Line| b.rect.intersection(&e.rect) > 0.3 * b.rect.area().min(e.rect.area());
        let theirs: f32 = base.iter().filter(|b| overlaps(b)).map(line_score).sum();
        if line_score(&e) > theirs {
            base.retain(|b| !overlaps(b));
            base.push(e);
        }
    }
    base
}

/// Upscale factor before OCR: bring the strip toward ~1600 px wide, at most 2.5x.
pub fn ocr_scale(strip_width: u32) -> f32 {
    (1600.0 / strip_width.max(1) as f32).clamp(1.0, 2.5)
}

/// Combine band results: whole bubbles win over clipped ones, duplicates from the
/// overlaps are dropped, and the rest is put in reading order.
pub fn combine(results: &[BandResult]) -> Vec<Bubble> {
    let mut candidates: Vec<(&Bubble, bool)> = results
        .iter()
        .flat_map(|r| r.bubbles.iter().zip(r.clipped.iter().copied()))
        .collect();
    // Whole bubbles first, then the one with more text.
    candidates.sort_by(|a, b| a.1.cmp(&b.1).then(b.0.text.len().cmp(&a.0.text.len())));
    let mut kept: Vec<Bubble> = Vec::new();
    for (b, _) in candidates {
        let dup = kept.iter().any(|k| {
            let inter = k.rect.intersection(&b.rect);
            inter > 0.0 && inter / b.rect.area().min(k.rect.area()).max(1.0) > 0.5
        });
        if !dup {
            kept.push(b.clone());
        }
    }
    reading_order(&mut kept);
    kept
}

/// Top to bottom; bubbles side by side (overlapping vertically by half) go left to right.
pub fn reading_order(bubbles: &mut [Bubble]) {
    bubbles.sort_by(|a, b| a.rect.y0.total_cmp(&b.rect.y0));
    // Insertion pass for side-by-side neighbours.
    for i in 1..bubbles.len() {
        let mut j = i;
        while j > 0
            && bubbles[j].rect.v_overlap(&bubbles[j - 1].rect) >= 0.5
            && bubbles[j].rect.x0 < bubbles[j - 1].rect.x0
        {
            bubbles.swap(j, j - 1);
            j -= 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------

/// Text that marks a scanlator ad or credits page rather than story.
const AD_MARKERS: &[&str] = &[
    "MANHUATOP", "DISCORD", "PATREON", "KO-FI", "KOFI", "SCANS", "BOOKMARK", "WWW.", ".COM", ".GG",
    "TRANSLATOR", "PROOFREADER", "TYPESETTER", "QUALITY CHECK", "REDRAWER", "CLEANER", "STUDIO",
];

/// Remove scanlator ads, credits and watermarks:
/// - a bubble with ad text is always dropped (watermarks like `FLAMESCANS.ORG` sit
///   inside story pages, so only that bubble goes);
/// - on the first or last two page images, ad text that isn't just a watermark
///   (`READ ON MANHUATOP?`, `TRANSLATOR`, `JOIN OUR DISCORD`) drops the rest of that
///   image toward the chapter's edge: everything above it (plus [`AD_MARGIN`] below) at
///   the start, everything below it (plus the margin above) at the end. Mihon images can
///   be very tall, and the last one often ends the story right above the "READ AT
///   ASURASCANS.COM" banner (Star-Embracing Swordmaster ch 1 lost its last 4 lines when
///   the whole image went).
///
/// Checked on the user's chapters: ManhuaTop's ~20-bubble banner and end page,
/// ZeroScans/Asura credit pages and Asura's novel plug are removed; FlameScans and
/// Asura watermarks in mid-chapter story pages no longer take the dialogue with them.
pub fn drop_ad_pages(ch: &Chapter, bubbles: Vec<Bubble>) -> Vec<Bubble> {
    let page_of = |b: &Bubble| {
        let y = (b.rect.y0 + b.rect.y1) / 2.0;
        ch.pages.iter().position(|p| y >= p.strip_y && y < p.strip_y + p.strip_h)
    };
    let is_ad = |b: &Bubble| {
        let t = b.text.to_uppercase();
        // Also with spaces removed: logos letter-spaced like "S T U D I O".
        let squeezed: String = t.chars().filter(|c| !c.is_whitespace()).collect();
        AD_MARKERS.iter().any(|m| t.contains(m) || squeezed.contains(m))
    };
    // Just a site name: one token once stray punctuation is trimmed.
    let is_watermark = |b: &Bubble| b.text.trim_matches(|c: char| !c.is_alphanumeric()).split_whitespace().count() <= 1;
    let n = ch.pages.len();
    // Per page: drop above this y (start of chapter) / below this y (end of chapter).
    let mut cut_above: Vec<Option<f32>> = vec![None; n];
    let mut cut_below: Vec<Option<f32>> = vec![None; n];
    for b in bubbles.iter().filter(|b| is_ad(b) && !is_watermark(b)) {
        let Some(p) = page_of(b) else { continue };
        if p < 2 {
            let y = b.rect.y1 + AD_MARGIN;
            cut_above[p] = Some(cut_above[p].map_or(y, |c: f32| c.max(y)));
        }
        if p + 2 >= n {
            let y = b.rect.y0 - AD_MARGIN;
            cut_below[p] = Some(cut_below[p].map_or(y, |c: f32| c.min(y)));
        }
    }
    bubbles
        .into_iter()
        .filter(|b| {
            if is_ad(b) {
                return false;
            }
            let Some(p) = page_of(b) else { return true };
            let cy = (b.rect.y0 + b.rect.y1) / 2.0;
            !(cut_above[p].is_some_and(|c| cy < c) || cut_below[p].is_some_and(|c| cy > c))
        })
        .collect()
}

/// How far around an ad on the first/last images credits and banner text still go.
const AD_MARGIN: f32 = 300.0;

/// Raw OCR bubbles -> what the reader shows: ad pages dropped, words corrected.
/// Cheap (milliseconds), so it runs on every load and rule changes need no rescan.
/// `engine` is the [`OcrEngine::name`] that read them: Tesseract's text gets the full
/// dictionary correction; PaddleOCR's is near-perfect already, and those rules mostly
/// broke it ("LV. 1" -> "LV. I", "ZEMINA" -> "ZEMIN A"), so it only gets light cleanup.
pub fn finish(ch: &Chapter, raw: &[Bubble], dict: Option<&super::fix::Dictionary>, engine: &str) -> Vec<Bubble> {
    // Only text the user types: speech and narration (no SFX, game windows, side chatter).
    let mut out = crate::kinds::keep_typeable(drop_ad_pages(ch, raw.to_vec()), dict);
    if engine == "tesseract" {
        if let Some(d) = dict {
            super::fix::correct_bubbles(&mut out, d);
        }
    } else {
        super::fix::light_fix_bubbles(&mut out);
    }
    out
}

/// What's stored next to a chapter: `<chapter>.manga-type.json`. Holds the raw OCR
/// bubbles (plus user edits); [`finish`] is applied after loading.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrCache {
    pub version: u32,
    /// [`OcrEngine::name`] of the engine that made it.
    #[serde(default)]
    pub engine: String,
    pub strip_width: u32,
    pub strip_height: f32,
    pub bubbles: Vec<Bubble>,
}

pub fn cache_path(chapter_path: &Path) -> PathBuf {
    let mut name = chapter_path.file_name().unwrap_or_default().to_os_string();
    name.push(".manga-type.json");
    chapter_path.with_file_name(name)
}

/// Load a cache if it exists and matches this chapter's layout and the current rules.
pub fn load_cache(ch: &Chapter, engine: &str) -> Option<OcrCache> {
    let text = std::fs::read_to_string(cache_path(&ch.path)).ok()?;
    let c: OcrCache = serde_json::from_str(&text).ok()?;
    (c.version == CACHE_VERSION
        && c.engine == engine
        && c.strip_width == ch.strip_width
        && (c.strip_height - ch.strip_height).abs() < 1.0)
        .then_some(c)
}

/// Save atomically (write temp, rename) so a crash never leaves a half-written cache.
pub fn save_cache(ch: &Chapter, bubbles: &[Bubble], engine: &str) -> Result<(), String> {
    let c = OcrCache {
        version: CACHE_VERSION,
        engine: engine.to_string(),
        strip_width: ch.strip_width,
        strip_height: ch.strip_height,
        bubbles: bubbles.to_vec(),
    };
    let path = cache_path(&ch.path);
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(&c).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Background scan
// ---------------------------------------------------------------------------

/// Messages from a running scan.
#[derive(Debug)]
pub enum ScanEvent {
    Band(BandResult),
    Error { band: usize, error: String },
    /// All workers finished.
    Done,
}

/// A background scan of a chapter. Workers take bands top to bottom, so the top of
/// the chapter is ready first and the scan stays ahead of the reader.
pub struct Scan {
    pub bands: Vec<Band>,
    pub events: mpsc::Receiver<ScanEvent>,
    cancel: Arc<AtomicBool>,
}

impl Scan {
    pub fn start(ch: Arc<Chapter>, engine: Arc<dyn OcrEngine>, workers: usize) -> Self {
        let bands = ch.ocr_bands(BAND_HEIGHT, BAND_OVERLAP);
        let (tx, rx) = mpsc::channel();
        let next = Arc::new(AtomicUsize::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let workers = workers.clamp(1, bands.len().max(1));
        let mut handles = Vec::new();
        for _ in 0..workers {
            let (ch, engine, tx, next, cancel, bands) =
                (ch.clone(), engine.clone(), tx.clone(), next.clone(), cancel.clone(), bands.clone());
            handles.push(thread::spawn(move || {
                let mut cache = PageCache::default();
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    if i >= bands.len() || cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let ev = match ocr_band(&ch, engine.as_ref(), i, bands[i], &mut cache) {
                        Ok(r) => ScanEvent::Band(r),
                        Err(error) => ScanEvent::Error { band: i, error },
                    };
                    if tx.send(ev).is_err() {
                        break;
                    }
                }
            }));
        }
        thread::spawn(move || {
            for h in handles {
                let _ = h.join();
            }
            let _ = tx.send(ScanEvent::Done);
        });
        Self { bands, events: rx, cancel }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for Scan {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Default worker count: the machine's physical cores (at most 16, and never more than
/// the threads this process may use). Past the physical cores, workers share cores and
/// each band costs more CPU (Regressor ch 2 on a 12-core / 24-thread Ryzen AI 9 HX 370:
/// 6 workers 27.0 s / 1.4 GB, 12 workers 21.5 s / 2.5 GB, 22 workers 20.0 s / 4.1 GB
/// using 4x the CPU time). `MANGA_TYPE_WORKERS` overrides it; so does the setting.
pub fn default_workers() -> usize {
    if let Some(n) = std::env::var("MANGA_TYPE_WORKERS").ok().and_then(|v| v.parse::<usize>().ok()) {
        return n.max(1);
    }
    let threads = thread::available_parallelism().map_or(2, |n| n.get());
    // Unknown topology (not Linux): assume two threads per core.
    let cores = physical_cores().unwrap_or(threads / 2);
    cores.min(threads).clamp(1, 16)
}

/// Physical cores, from Linux's CPU topology: distinct (package, core) pairs over the
/// CPUs in `/sys/devices/system/cpu`. `None` when that isn't available.
pub fn physical_cores() -> Option<usize> {
    let mut cores = std::collections::HashSet::new();
    for entry in std::fs::read_dir("/sys/devices/system/cpu").ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.strip_prefix("cpu").is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())) {
            continue;
        }
        let topo = entry.path().join("topology");
        let read = |f: &str| std::fs::read_to_string(topo.join(f)).ok().map(|s| s.trim().to_string());
        if let (Some(pkg), Some(core)) = (read("physical_package_id"), read("core_id")) {
            cores.insert((pkg, core));
        }
    }
    (!cores.is_empty()).then_some(cores.len())
}

/// Bubbles that are final given which bands are done: everything above the first
/// band still missing (minus the overlap, where that band might still add a bubble).
pub fn ready_bubbles(bands: &[Band], results: &[BandResult]) -> (Vec<Bubble>, f32) {
    let done: Vec<bool> = (0..bands.len()).map(|i| results.iter().any(|r| r.band == i)).collect();
    let ready_y = match done.iter().position(|d| !d) {
        None => f32::INFINITY,
        Some(first_missing) => bands[first_missing].y0,
    };
    let bubbles = combine(results).into_iter().filter(|b| b.rect.y1 <= ready_y).collect();
    (bubbles, ready_y)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(x0: f32, y0: f32, x1: f32, y1: f32, t: &str) -> Line {
        Line { rect: Rect { x0, y0, x1, y1 }, text: t.into(), conf: 90.0, words: Vec::new() }
    }

    #[test]
    fn tsv_lines_filter_low_conf() {
        let tsv = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n\
                   5\t1\t1\t1\t1\t1\t10\t20\t30\t15\t90\tHELLO\n\
                   5\t1\t1\t1\t1\t2\t45\t20\t30\t15\t20\t|\n\
                   5\t1\t1\t1\t1\t3\t80\t21\t40\t15\t88\tTHERE\n\
                   5\t1\t2\t1\t1\t1\t300\t400\t20\t15\t30\tM‘V§\n";
        let lines = parse_tsv(tsv);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "HELLO THERE");
        assert_eq!(lines[0].rect, Rect { x0: 10.0, y0: 20.0, x1: 120.0, y1: 36.0 });
    }

    #[test]
    fn merges_bubble_lines_and_hyphens() {
        let b = merge_lines(vec![
            line(100.0, 100.0, 300.0, 120.0, "I WASN'T ABLE TO GIVE"),
            line(110.0, 124.0, 290.0, 144.0, "THE CATS THAT MANY BE-"),
            line(120.0, 148.0, 280.0, 168.0, "CAUSE OF YOU."),
            // Far below: another bubble.
            line(100.0, 400.0, 250.0, 420.0, "OH? THESE TREATS"),
            // Same height, other side of the page: another bubble.
            line(600.0, 101.0, 800.0, 121.0, "WHAT?"),
            line(700.0, 700.0, 705.0, 720.0, "|"),
        ]);
        let texts: Vec<_> = b.iter().map(|b| b.text.as_str()).collect();
        assert!(texts.contains(&"I WASN'T ABLE TO GIVE THE CATS THAT MANY BECAUSE OF YOU."));
        assert!(texts.contains(&"OH? THESE TREATS"));
        assert!(texts.contains(&"WHAT?"));
        assert_eq!(b.len(), 3, "the lone | is filtered");
    }

    #[test]
    fn word_cleanup() {
        assert_eq!(clean_word("MONSTER!!/").as_deref(), Some("MONSTER!!"));
        assert_eq!(clean_word("/////W"), None);
        assert_eq!(clean_word("\\\\"), None);
        assert_eq!(clean_word("...").as_deref(), Some("..."));
        assert_eq!(clean_word("|"), None);
        assert_eq!(clean_word("DON'T").as_deref(), Some("DON'T"));
    }

    #[test]
    fn junk_filter() {
        for (t, c) in [("UH OH...", 90.0), ("HUH?", 64.0), ("...A CAT? /", 84.0), ("THE KID!", 80.0), ("I'M HERE.", 70.0)] {
            assert!(looks_like_text(t, c), "should keep {t}");
        }
        for (t, c) in [("l‘l", 64.0), ("N A", 57.0), ("/ ’\\%p ’HII“ | | |", 69.0), ("DAL", 50.0), ("7174", 57.0), ("o 0", 62.0)] {
            assert!(!looks_like_text(t, c), "should drop {t}");
        }
    }

    #[test]
    fn combine_prefers_whole_and_orders() {
        let bub = |y: f32, x: f32, t: &str| Bubble {
            rect: Rect { x0: x, y0: y, x1: x + 100.0, y1: y + 60.0 },
            text: t.into(),
            conf: 90.0,
            edited: None,
            raw: None,
            framed: None,
        };
        let results = vec![
            BandResult { band: 0, bubbles: vec![bub(100.0, 50.0, "A"), bub(1990.0, 50.0, "CUT")], clipped: vec![false, true] },
            BandResult { band: 1, bubbles: vec![bub(1985.0, 40.0, "WHOLE TEXT"), bub(2500.0, 600.0, "RIGHT"), bub(2510.0, 50.0, "LEFT")], clipped: vec![false, false, false] },
        ];
        let out: Vec<_> = combine(&results).into_iter().map(|b| b.text).collect();
        assert_eq!(out, ["A", "WHOLE TEXT", "LEFT", "RIGHT"]);
    }

    #[test]
    fn thresholded_pass_wins_where_it_reads_more() {
        let base = vec![line(0.0, 0.0, 100.0, 20.0, "IFIT"), line(0.0, 40.0, 100.0, 60.0, "'HM SHINE")];
        let extra = vec![
            line(0.0, 0.0, 120.0, 20.0, "IF IT WISHES"),
            line(0.0, 40.0, 100.0, 60.0, "TO"),
            line(0.0, 90.0, 50.0, 100.0, "NEW"),
        ];
        let mut texts: Vec<String> = merge_passes(base, extra).into_iter().map(|l| l.text).collect();
        texts.sort();
        assert_eq!(texts, ["'HM SHINE", "IF IT WISHES", "NEW"]);
    }


    #[test]
    fn worker_default_is_sane() {
        let threads = thread::available_parallelism().map_or(2, |n| n.get());
        if let Some(c) = physical_cores() {
            assert!(c >= 1 && c <= threads, "{c} cores, {threads} threads");
        }
        let w = default_workers();
        assert!((1..=16).contains(&w));
    }

}
