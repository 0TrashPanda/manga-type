//! manga-type. For now a small command-line harness to test the phone layer
//! against the real phone; the egui app replaces this later.
//!
//!   manga-type series                      list series in Mihon's downloads
//!   manga-type chapters <series#>          list chapters of a series
//!   manga-type pull <series#> <chapter#> <dest-dir>
//!   manga-type stop-adb                    stop a running adb server
//!   manga-type progress                    match phone chapters to Mihon's backup
//!   manga-type pages <chapter.cbz|dir>     page order and strip layout
//!   manga-type bands <chapter> <out-dir>   write the OCR bands as PNGs
//!   manga-type ocr <chapter> [annot-dir]   scan a chapter, print bubbles
//!   manga-type read <chapter>              open the reader window

mod fix;
mod mihon;
mod ocr;
mod pages;
mod phone;
mod reader;
mod typing;

/// Debug: write each OCR band with bubble boxes drawn on it (red) and their index.
fn annotate(ch: &pages::Chapter, bubbles: &[ocr::Bubble], dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut cache = pages::PageCache::default();
    // Non-overlapping slices of the strip so every box is drawn once.
    let step = 2000.0;
    let mut y = 0.0;
    let mut n = 0;
    while y < ch.strip_height {
        let band = pages::Band { y0: y, y1: (y + step).min(ch.strip_height) };
        let gray = ch.render_band(band, &mut cache)?;
        let mut img = image::DynamicImage::ImageLuma8(gray).to_rgb8();
        for b in bubbles.iter().filter(|b| b.rect.y1 > band.y0 && b.rect.y0 < band.y1) {
            let r = &b.rect;
            for t in 0..3 {
                let (x0, x1) = (r.x0 as i64 - 4 - t, r.x1 as i64 + 4 + t);
                let (y0, y1) = (r.y0 as i64 - band.y0 as i64 - 4 - t, r.y1 as i64 - band.y0 as i64 + 4 + t);
                for x in x0..=x1 {
                    for yy in [y0, y1] {
                        if x >= 0 && yy >= 0 && (x as u32) < img.width() && (yy as u32) < img.height() {
                            img.put_pixel(x as u32, yy as u32, image::Rgb([230, 30, 30]));
                        }
                    }
                }
                for yy in y0..=y1 {
                    for x in [x0, x1] {
                        if x >= 0 && yy >= 0 && (x as u32) < img.width() && (yy as u32) < img.height() {
                            img.put_pixel(x as u32, yy as u32, image::Rgb([230, 30, 30]));
                        }
                    }
                }
            }
        }
        img.save(dir.join(format!("strip{n:03}_{y:.0}.jpg"))).map_err(|e| e.to_string())?;
        y += step;
        n += 1;
    }
    Ok(())
}

use std::io::Write;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use phone::{Phone, PhoneError};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = run(&args) {
        // Start on a new line: a progress counter may be on the current one.
        eprintln!("\nerror: {e}");
        std::process::exit(1);
    }
}

fn connect() -> Result<Phone, PhoneError> {
    let key = phone::default_key_path().ok_or(PhoneError::Other("no home directory".into()))?;
    let phone = Phone::connect(&key)?;
    eprintln!("connected, Mihon folder: {}", phone.root());
    Ok(phone)
}

fn arg_index(args: &[String], i: usize, what: &str) -> Result<usize, PhoneError> {
    args.get(i)
        .and_then(|s| s.parse::<usize>().ok())
        .ok_or_else(|| PhoneError::Other(format!("expected {what} number")))
}

fn run(args: &[String]) -> Result<(), PhoneError> {
    match args.first().map(String::as_str) {
        Some("pages") => {
            let path = args.get(1).ok_or(PhoneError::Other("expected chapter path".into()))?;
            let t = Instant::now();
            let ch = pages::Chapter::open(Path::new(path)).map_err(PhoneError::Other)?;
            println!(
                "{} pages, strip {} x {:.0}, read in {:.0} ms",
                ch.pages.len(),
                ch.strip_width,
                ch.strip_height,
                t.elapsed().as_secs_f64() * 1e3
            );
            for p in &ch.pages {
                println!(
                    "  {:<16} page {:>4} part {:>4}  {:>5}x{:<5} strip y {:>7.0} h {:>6.0}",
                    p.name,
                    p.page_no.map_or("-".into(), |n| n.to_string()),
                    p.part.map_or("-".into(), |n| n.to_string()),
                    p.width,
                    p.height,
                    p.strip_y,
                    p.strip_h
                );
            }
            let bands = ch.ocr_bands(2000.0, 200.0);
            println!("{} OCR bands of ~2000 px with 200 px overlap", bands.len());
        }
        Some("bands") => {
            let path = args.get(1).ok_or(PhoneError::Other("expected chapter path".into()))?;
            let out = Path::new(args.get(2).ok_or(PhoneError::Other("expected out dir".into()))?);
            std::fs::create_dir_all(out).map_err(PhoneError::LocalIo)?;
            let ch = pages::Chapter::open(Path::new(path)).map_err(PhoneError::Other)?;
            let mut cache = pages::PageCache::default();
            let (mut render, mut save) = (0.0, 0.0);
            for (i, b) in ch.ocr_bands(2000.0, 200.0).into_iter().enumerate() {
                let t = Instant::now();
                let img = ch.render_band(b, &mut cache).map_err(PhoneError::Other)?;
                render += t.elapsed().as_secs_f64();
                let t = Instant::now();
                img.save(out.join(format!("band{i:03}_{:.0}-{:.0}.png", b.y0, b.y1)))
                    .map_err(|e| PhoneError::Other(e.to_string()))?;
                save += t.elapsed().as_secs_f64();
            }
            println!("rendered in {render:.1}s, saving PNGs took {save:.1}s");
        }
        Some("read") => {
            let path = args.get(1).ok_or(PhoneError::Other("expected chapter path".into()))?;
            reader::run(Path::new(path)).map_err(PhoneError::Other)?;
        }
        Some("ocr") => {
            // ocr <chapter> [annotated-out-dir]
            let path = args.get(1).ok_or(PhoneError::Other("expected chapter path".into()))?;
            let ch = std::sync::Arc::new(pages::Chapter::open(Path::new(path)).map_err(PhoneError::Other)?);
            let t = Instant::now();
            let raw = if let Some(c) = ocr::load_cache(&ch) {
                println!("loaded cache {}", ocr::cache_path(&ch.path).display());
                c.bubbles
            } else {
                let tess = ocr::Tesseract::default();
                tess.check().map_err(PhoneError::Other)?;
                let workers = ocr::default_workers();
                let scan = ocr::Scan::start(ch.clone(), std::sync::Arc::new(tess), workers);
                let total = scan.bands.len();
                let mut results = Vec::new();
                let mut first_ready = None;
                for ev in scan.events.iter() {
                    match ev {
                        ocr::ScanEvent::Band(r) => {
                            results.push(r);
                            let (ready, _) = ocr::ready_bubbles(&scan.bands, &results);
                            if first_ready.is_none() && !ready.is_empty() {
                                first_ready = Some(t.elapsed().as_secs_f64());
                            }
                            eprint!("\r  {}/{total} bands, {} bubbles ready  ", results.len(), ready.len());
                        }
                        ocr::ScanEvent::Error { band, error } => eprintln!("\n  band {band}: {error}"),
                        ocr::ScanEvent::Done => break,
                    }
                }
                eprintln!();
                println!(
                    "{total} bands on {workers} workers in {:.1}s (first bubble ready after {:.1}s)",
                    t.elapsed().as_secs_f64(),
                    first_ready.unwrap_or(0.0)
                );
                let raw = ocr::combine(&results);
                ocr::save_cache(&ch, &raw).map_err(PhoneError::Other)?;
                raw
            };
            let dict = fix::Dictionary::load_or_build().inspect_err(|e| eprintln!("word correction off: {e}")).ok();
            let bubbles = ocr::finish(&ch, &raw, dict.as_ref());
            let corrected = bubbles.iter().filter(|b| b.raw.is_some()).count();
            println!(
                "{} raw bubbles -> {} after dropping ad/credit pages; {corrected} bubbles corrected",
                raw.len(),
                bubbles.len()
            );
            for (i, b) in bubbles.iter().enumerate() {
                println!("{i:3} y{:>7.0} conf{:>3.0}  {}", b.rect.y0, b.conf, b.target());
            }
            if let Some(dir) = args.get(2) {
                annotate(&ch, &bubbles, Path::new(dir)).map_err(PhoneError::Other)?;
            }
        }
        Some("progress") => {
            // Match every downloaded chapter on the phone to Mihon's backup.
            let p = connect()?;
            let (name, time, bytes) =
                p.read_newest_backup()?.ok_or(PhoneError::Other("no Mihon backup on the phone".into()))?;
            let backup = mihon::parse_backup(&bytes).map_err(PhoneError::Other)?;
            println!(
                "backup {name} ({} KB, mtime {time}): {} series, {} sources",
                bytes.len() / 1024,
                backup.series.len(),
                backup.sources.len()
            );
            let (mut total, mut matched) = (0, 0);
            for s in p.list_series()? {
                let chapters = p.list_chapters(&s)?;
                let Some(bs) = backup.find_series(&s.source, &s.title) else {
                    println!("  NO SERIES MATCH  {} / {}", s.source, s.title);
                    total += chapters.len();
                    continue;
                };
                let mut line = String::new();
                for c in &chapters {
                    total += 1;
                    match bs.match_chapter(&c.file_name) {
                        Some(bc) => {
                            matched += 1;
                            line.push(if bc.read { 'R' } else if bc.last_page_read > 0 { 'p' } else { '.' });
                        }
                        None => line.push('?'),
                    }
                }
                let lib = if bs.favorite { "lib" } else { "   " };
                println!("  {lib} {:<45} {line}", format!("{} / {}", s.source, s.title).chars().take(45).collect::<String>());
            }
            println!("matched {matched}/{total} chapters   (R read, p partly read, . unread, ? no match)");
        }
        Some("stop-adb") => {
            phone::stop_adb_server()?;
            println!("adb server stopped (or wasn't running)");
        }
        Some("series") => {
            let p = connect()?;
            for (i, s) in p.list_series()?.iter().enumerate() {
                println!("{i:3}  {:<22} {}", s.source, s.title);
            }
        }
        Some("chapters") => {
            let p = connect()?;
            let series = p.list_series()?;
            let s = series
                .get(arg_index(args, 1, "series")?)
                .ok_or(PhoneError::Other("no such series".into()))?;
            for (i, c) in p.list_chapters(s)?.iter().enumerate() {
                println!("{i:3}  {:?}  {:>6.1} MB  {}", c.kind, c.size as f64 / 1e6, c.display_name);
            }
        }
        Some("pull") => {
            let p = connect()?;
            let series = p.list_series()?;
            let s = series
                .get(arg_index(args, 1, "series")?)
                .ok_or(PhoneError::Other("no such series".into()))?;
            let chapters = p.list_chapters(s)?;
            let c = chapters
                .get(arg_index(args, 2, "chapter")?)
                .ok_or(PhoneError::Other("no such chapter".into()))?;
            let dest = args.get(3).ok_or(PhoneError::Other("expected dest dir".into()))?;
            let dest = Path::new(dest).join(&s.source).join(&s.title);
            let cancel = AtomicBool::new(false);
            let start = Instant::now();
            let mut last = 0u64;
            let out = p.pull_chapter(s, c, &dest, &cancel, &mut |pr| {
                if pr.bytes_done - last >= 1 << 20 || pr.bytes_done == pr.bytes_total {
                    last = pr.bytes_done;
                    eprint!("\r  {:.1} / {:.1} MB", pr.bytes_done as f64 / 1e6, pr.bytes_total as f64 / 1e6);
                    let _ = std::io::stderr().flush();
                }
            })?;
            let secs = start.elapsed().as_secs_f64();
            eprintln!();
            println!("copied to {} in {secs:.1}s ({:.1} MB/s)", out.display(), last as f64 / 1e6 / secs);
        }
        _ => {
            eprintln!("usage: manga-type series | chapters <n> | pull <series#> <chapter#> <dest> | stop-adb");
        }
    }
    Ok(())
}
