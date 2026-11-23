//! Opening a copied chapter (`.cbz` or a folder of images) and laying its pages
//! out as one vertical strip.
//!
//! Strip coordinates: every page is scaled to a common reference width
//! ([`Chapter::strip_width`], the most common page width) and stacked top to bottom.
//! Bubble boxes are stored in these coordinates, so they scale linearly with any
//! display width, and a bubble cut across two images (Mihon splits tall pages into
//! `016__001.jpg`, `016__002.jpg`, ...) is still one box.

use std::fs::{self, File};
use std::io::{BufReader, Cursor, Read};
use std::path::{Path, PathBuf};

use image::{DynamicImage, GrayImage, ImageReader, imageops};

const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif"];

/// One image file in the chapter.
#[derive(Debug, Clone)]
pub struct PageImage {
    /// Entry name inside the cbz, or file name in the folder.
    pub name: String,
    /// Page number from the file name (`016` in `016__002.jpg`), if any.
    pub page_no: Option<u32>,
    /// Piece number of a page Mihon split (`002` in `016__002.jpg`), if any.
    pub part: Option<u32>,
    /// Original pixel size.
    pub width: u32,
    pub height: u32,
    /// Top edge in strip coordinates.
    pub strip_y: f32,
    /// Height in strip coordinates.
    pub strip_h: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Store {
    Cbz,
    Folder,
}

/// An opened chapter: page list in reading order plus the strip layout.
#[derive(Debug)]
pub struct Chapter {
    pub path: PathBuf,
    store: Store,
    pub pages: Vec<PageImage>,
    /// Reference width of the strip (most common page width).
    pub strip_width: u32,
    /// Total strip height at `strip_width`.
    pub strip_height: f32,
}

/// A horizontal band of the strip to OCR, in strip coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Band {
    pub y0: f32,
    pub y1: f32,
}

fn is_image(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    !lower.starts_with("__macosx/")
        && !lower.rsplit('/').next().unwrap_or("").starts_with('.')
        && IMAGE_EXTS.iter().any(|e| lower.ends_with(&format!(".{e}")))
}

/// `016__002.webp` -> (Some(16), Some(2)); `003.jpg` -> (Some(3), None); `cover.jpg` -> (None, None).
fn page_numbers(name: &str) -> (Option<u32>, Option<u32>) {
    let file = name.rsplit('/').next().unwrap_or(name);
    let stem = file.rsplit_once('.').map_or(file, |(s, _)| s);
    let lead = |s: &str| -> Option<u32> {
        let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    };
    match stem.split_once("__") {
        Some((page, part)) => (lead(page), lead(part)),
        None => (lead(stem), None),
    }
}

impl Chapter {
    /// Open a chapter and read every page's size (headers only, no full decode).
    pub fn open(path: &Path) -> Result<Self, String> {
        let store = if path.is_dir() { Store::Folder } else { Store::Cbz };
        let mut pages = Vec::new();

        match store {
            Store::Cbz => {
                let file = File::open(path).map_err(|e| format!("can't open {}: {e}", path.display()))?;
                let mut zip = zip::ZipArchive::new(BufReader::new(file))
                    .map_err(|e| format!("not a valid cbz ({}): {e}", path.display()))?;
                for i in 0..zip.len() {
                    let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
                    let name = entry.name().map_err(|e| e.to_string())?.into_owned();
                    if entry.is_dir() || !is_image(&name) {
                        continue;
                    }
                    let mut bytes = Vec::with_capacity(entry.size() as usize);
                    entry.read_to_end(&mut bytes).map_err(|e| format!("{name}: {e}"))?;
                    let (w, h) = dimensions(Cursor::new(bytes)).map_err(|e| format!("{name}: {e}"))?;
                    pages.push(new_page(name, w, h));
                }
            }
            Store::Folder => {
                let entries = fs::read_dir(path).map_err(|e| format!("can't read {}: {e}", path.display()))?;
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if !entry.path().is_file() || !is_image(&name) {
                        continue;
                    }
                    let file = File::open(entry.path()).map_err(|e| format!("{name}: {e}"))?;
                    let (w, h) = dimensions(BufReader::new(file)).map_err(|e| format!("{name}: {e}"))?;
                    pages.push(new_page(name, w, h));
                }
            }
        }

        if pages.is_empty() {
            return Err(format!("no images in {}", path.display()));
        }
        // Zip entries come in any order; natural sort gives 2 < 10 and 016 < 016__001 < 017.
        pages.sort_by(|a, b| natord::compare_ignore_case(&a.name, &b.name));

        let mut ch = Self { path: path.to_path_buf(), store, pages, strip_width: 0, strip_height: 0.0 };
        ch.layout();
        Ok(ch)
    }

    /// Pick the reference width and stack pages.
    fn layout(&mut self) {
        // Most common width (ties -> wider). Odd one-off pages (a wide cover) get scaled.
        let mut counts: Vec<(u32, usize)> = Vec::new();
        for p in &self.pages {
            match counts.iter_mut().find(|(w, _)| *w == p.width) {
                Some((_, n)) => *n += 1,
                None => counts.push((p.width, 1)),
            }
        }
        self.strip_width = counts.iter().max_by_key(|(w, n)| (*n, *w)).map_or(1, |(w, _)| *w);

        let mut y = 0.0;
        for p in &mut self.pages {
            p.strip_y = y;
            p.strip_h = p.height as f32 * self.strip_width as f32 / p.width as f32;
            y += p.strip_h;
        }
        self.strip_height = y;
    }

    /// Decode one page at full resolution.
    pub fn load_page(&self, index: usize) -> Result<DynamicImage, String> {
        let page = self.pages.get(index).ok_or("page index out of range")?;
        let bytes = match self.store {
            Store::Cbz => {
                let file = File::open(&self.path).map_err(|e| e.to_string())?;
                let mut zip = zip::ZipArchive::new(BufReader::new(file)).map_err(|e| e.to_string())?;
                let mut entry = zip.by_name(&page.name).map_err(|e| format!("{}: {e}", page.name))?;
                let mut bytes = Vec::with_capacity(entry.size() as usize);
                entry.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
                bytes
            }
            Store::Folder => fs::read(self.path.join(&page.name)).map_err(|e| e.to_string())?,
        };
        ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|e| e.to_string())?
            .decode()
            .map_err(|e| format!("{}: {e}", page.name))
    }

    /// Indices of pages overlapping strip rows `y0..y1`.
    pub fn pages_in(&self, y0: f32, y1: f32) -> impl Iterator<Item = usize> + '_ {
        self.pages
            .iter()
            .enumerate()
            .filter(move |(_, p)| p.strip_y < y1 && p.strip_y + p.strip_h > y0)
            .map(|(i, _)| i)
    }

    /// Split the strip into bands of about `target_h` strip pixels for OCR, each overlapping
    /// the next by `overlap` so a bubble on a cut is whole in at least one band.
    /// Bands ignore image boundaries on purpose: split pieces are OCR'd as one picture.
    pub fn ocr_bands(&self, target_h: f32, overlap: f32) -> Vec<Band> {
        assert!(target_h > overlap);
        let total = self.strip_height;
        if total <= target_h {
            return vec![Band { y0: 0.0, y1: total }];
        }
        // Even out band heights instead of leaving a tiny last band.
        let step = target_h - overlap;
        let n = ((total - overlap) / step).ceil();
        let step = (total - overlap) / n;
        let h = step + overlap;
        (0..n as usize)
            .map(|i| {
                let y0 = i as f32 * step;
                Band { y0, y1: (y0 + h).min(total) }
            })
            .collect()
    }

    /// Render a band of the strip as one grayscale image at `strip_width`
    /// (pages decoded, scaled to the reference width if needed, and stitched).
    /// `cache` keeps recently decoded pages: consecutive bands share pages, so render
    /// bands in order with one cache per worker.
    pub fn render_band(&self, band: Band, cache: &mut PageCache) -> Result<GrayImage, String> {
        let w = self.strip_width;
        let top = band.y0.floor() as i64;
        let h = (band.y1.ceil() as i64 - top).max(1) as u32;
        let mut out = GrayImage::from_pixel(w, h, image::Luma([255]));
        for i in self.pages_in(band.y0, band.y1) {
            let y = self.pages[i].strip_y.round() as i64 - top;
            let img = cache.get(self, i)?;
            imageops::replace(&mut out, img, 0, y);
        }
        Ok(out)
    }

    /// A page decoded to grayscale at strip width.
    fn strip_page(&self, index: usize) -> Result<GrayImage, String> {
        let p = &self.pages[index];
        let img = self.load_page(index)?.to_luma8();
        Ok(if p.width == self.strip_width {
            img
        } else {
            let h = p.strip_h.round().max(1.0) as u32;
            imageops::resize(&img, self.strip_width, h, imageops::FilterType::Triangle)
        })
    }
}

/// The last few pages decoded for [`Chapter::render_band`].
#[derive(Default)]
pub struct PageCache {
    pages: Vec<(usize, GrayImage)>,
}

impl PageCache {
    /// Pages a band can touch at once is small; 4 covers a band spanning short pages.
    const SIZE: usize = 4;

    fn get(&mut self, ch: &Chapter, index: usize) -> Result<&GrayImage, String> {
        if let Some(pos) = self.pages.iter().position(|(i, _)| *i == index) {
            let entry = self.pages.remove(pos);
            self.pages.push(entry);
        } else {
            if self.pages.len() >= Self::SIZE {
                self.pages.remove(0);
            }
            self.pages.push((index, ch.strip_page(index)?));
        }
        Ok(&self.pages.last().unwrap().1)
    }
}

fn new_page(name: String, width: u32, height: u32) -> PageImage {
    let (page_no, part) = page_numbers(&name);
    PageImage { name, page_no, part, width, height, strip_y: 0.0, strip_h: 0.0 }
}

fn dimensions<R: std::io::BufRead + std::io::Seek>(r: R) -> Result<(u32, u32), String> {
    ImageReader::new(r)
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .into_dimensions()
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(page_numbers("016__002.jpg"), (Some(16), Some(2)));
        assert_eq!(page_numbers("003.webp"), (Some(3), None));
        assert_eq!(page_numbers("sub/012.png"), (Some(12), None));
        assert_eq!(page_numbers("cover.jpg"), (None, None));
        assert!(is_image("001.WEBP"));
        assert!(!is_image("ComicInfo.xml"));
        assert!(!is_image("__MACOSX/001.jpg"));
    }

    #[test]
    fn sort_order() {
        let mut v = vec!["017.webp", "016__002.jpg", "002.webp", "016__001.jpg", "010.webp", "016.webp"];
        v.sort_by(|a, b| natord::compare_ignore_case(a, b));
        assert_eq!(v, ["002.webp", "010.webp", "016.webp", "016__001.jpg", "016__002.jpg", "017.webp"]);
    }

    fn fake(heights: &[u32], width: u32) -> Chapter {
        let mut ch = Chapter {
            path: PathBuf::new(),
            store: Store::Folder,
            pages: heights.iter().map(|&h| new_page(String::new(), width, h)).collect(),
            strip_width: 0,
            strip_height: 0.0,
        };
        ch.layout();
        ch
    }

    #[test]
    fn bands_cover_strip_with_overlap() {
        let ch = fake(&[4000, 4000, 1500], 900);
        let bands = ch.ocr_bands(2000.0, 200.0);
        assert_eq!(bands.first().unwrap().y0, 0.0);
        assert!((bands.last().unwrap().y1 - 9500.0).abs() < 0.01);
        for w in bands.windows(2) {
            assert!(w[0].y1 - w[1].y0 >= 199.0, "overlap kept");
            assert!(w[0].y1 - w[0].y0 <= 2000.01, "no band too tall");
        }
        assert_eq!(ch.pages_in(3900.0, 4100.0).collect::<Vec<_>>(), vec![0, 1]);
    }
}
