//! PaddleOCR (PP-OCRv6 small) text detection + recognition, run with `tract` (pure Rust,
//! no system libraries). Mirrors RapidOCR's pre/post-processing, minus the angle
//! classifier: comic text is upright, and the classifier flips short words upside down
//! ("NO" -> "¿NO").
//!
//! Benchmark (61 bubble crops from Regressor Instruction Manual ch 2 and Star-Embracing
//! Swordmaster ch 1, 2026-11-25): word error rate 0.5-0.9% vs 33% for Tesseract.
//!
//! Model files (see `scripts/install.sh`), in `$XDG_DATA_HOME/manga-type/models/`:
//! `PP-OCRv6_det_small.onnx`, `PP-OCRv6_rec_small.onnx` (character list in its metadata).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use image::GrayImage;
use tract_onnx::prelude::*;

use crate::ocr::{Line, OcrEngine, Rect};

pub const DET_MODEL: &str = "PP-OCRv6_det_small.onnx";
pub const REC_MODEL: &str = "PP-OCRv6_rec_small.onnx";

/// DB post-processing, as RapidOCR's defaults.
const THRESH: f32 = 0.3;
const BOX_THRESH: f32 = 0.5;
const UNCLIP_RATIO: f32 = 1.6;
const MIN_SIZE: f32 = 3.0;
/// Detection input: the shorter side is brought up to at least this (RapidOCR `min` mode).
const LIMIT_SIDE: f32 = 736.0;
/// Recognition input height, and the line confidence below which a line is dropped.
const REC_H: u32 = 48;
const MIN_LINE_SCORE: f32 = 0.5;

type Plan = Arc<TypedRunnableModel>;

pub struct Paddle {
    det_proto: tract_onnx::pb::ModelProto,
    rec_proto: tract_onnx::pb::ModelProto,
    /// Index 0 is the CTC blank; the last entry is a space.
    chars: Vec<String>,
    /// Optimised plans per input size (bands are mostly the same size; line widths are
    /// rounded up to a few buckets).
    det_plans: Mutex<HashMap<(usize, usize), Plan>>,
    rec_plans: Mutex<HashMap<usize, Plan>>,
}

/// `$XDG_DATA_HOME/manga-type/models` (`~/.local/share/manga-type/models`).
pub fn model_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::home_dir().map(|h| h.join(".local").join("share")))
        .map(|d| d.join("manga-type").join("models"))
}

fn onnx() -> tract_onnx::Onnx {
    // The models carry shape hints with a symbolic batch size that clash with a fixed
    // input shape; let tract work the shapes out itself.
    tract_onnx::onnx().with_ignore_output_shapes(true).with_ignore_value_info(true)
}

impl Paddle {
    /// Load both models from `dir`. Fails if either file is missing.
    pub fn load(dir: &Path) -> Result<Self, String> {
        let read = |name: &str| {
            let p = dir.join(name);
            onnx().proto_model_for_path(&p).map_err(|e| format!("can't read {}: {e}", p.display()))
        };
        let det_proto = read(DET_MODEL)?;
        let rec_proto = read(REC_MODEL)?;
        let charlist = rec_proto
            .metadata_props
            .iter()
            .find(|p| p.key == "character")
            .map(|p| p.value.clone())
            .ok_or("recognition model has no character list")?;
        let mut chars: Vec<String> = vec![String::new()]; // CTC blank
        chars.extend(charlist.lines().map(str::to_string));
        chars.push(" ".into());
        Ok(Self { det_proto, rec_proto, chars, det_plans: Mutex::default(), rec_plans: Mutex::default() })
    }

    /// The default model folder, if both models are there.
    pub fn load_default() -> Result<Self, String> {
        let dir = model_dir().ok_or("no home directory")?;
        Self::load(&dir)
    }

    fn plan(proto: &tract_onnx::pb::ModelProto, shape: [usize; 4]) -> Result<Plan, String> {
        onnx()
            .model_for_proto_model(proto)
            .and_then(|m| m.with_input_fact(0, f32::fact(shape).into()))
            .and_then(|m| m.into_optimized())
            .and_then(|m| m.into_runnable())
            .map_err(|e| format!("loading OCR model: {e}"))
    }

    fn det_plan(&self, h: usize, w: usize) -> Result<Plan, String> {
        let mut cache = self.det_plans.lock().unwrap();
        if let Some(p) = cache.get(&(h, w)) {
            return Ok(p.clone());
        }
        let p = Self::plan(&self.det_proto, [1, 3, h, w])?;
        cache.insert((h, w), p.clone());
        Ok(p)
    }

    fn rec_plan(&self, w: usize) -> Result<Plan, String> {
        let mut cache = self.rec_plans.lock().unwrap();
        if let Some(p) = cache.get(&w) {
            return Ok(p.clone());
        }
        let p = Self::plan(&self.rec_proto, [1, 3, REC_H as usize, w])?;
        cache.insert(w, p.clone());
        Ok(p)
    }

    /// Text line boxes in `img` pixels.
    fn detect(&self, img: &GrayImage) -> Result<Vec<Rect>, String> {
        let (w0, h0) = (img.width() as f32, img.height() as f32);
        let ratio = if w0.min(h0) < LIMIT_SIDE { LIMIT_SIDE / w0.min(h0) } else { 1.0 };
        let round32 = |v: f32| (((v * ratio) / 32.0).round() as u32).max(1) * 32;
        let (w, h) = (round32(w0), round32(h0));
        let resized = image::imageops::resize(img, w, h, image::imageops::FilterType::Triangle);
        let input = to_tensor(&resized, w as usize, 0.0);
        let plan = self.det_plan(h as usize, w as usize)?;
        let out = plan.run(tvec!(input.into())).map_err(|e| format!("OCR detection failed: {e}"))?;
        let pred = out[0].to_plain_array_view::<f32>().map_err(|e| e.to_string())?;
        let pred = pred.as_slice().ok_or("detection output isn't contiguous")?;
        let (mw, mh) = (w as usize, h as usize);
        let boxes = db_boxes(pred, mw, mh);
        // Back to the image's own pixels.
        let (sx, sy) = (w0 / mw as f32, h0 / mh as f32);
        Ok(boxes
            .into_iter()
            .map(|r| Rect {
                x0: (r.x0 * sx).clamp(0.0, w0),
                y0: (r.y0 * sy).clamp(0.0, h0),
                x1: (r.x1 * sx).clamp(0.0, w0),
                y1: (r.y1 * sy).clamp(0.0, h0),
            })
            .filter(|r| r.w() >= 2.0 && r.h() >= 2.0)
            .collect())
    }

    /// Read one line crop: text and mean confidence (0-1).
    fn recognize(&self, crop: &GrayImage) -> Result<(String, f32), String> {
        let (cw, ch) = (crop.width().max(1) as f32, crop.height().max(1) as f32);
        let rw = ((REC_H as f32 * cw / ch).ceil() as u32).clamp(8, 3200);
        // Pad the width up to a bucket so only a few model variants get optimised.
        let bucket = (rw.max(320) as usize).div_ceil(160) * 160;
        let resized = image::imageops::resize(crop, rw, REC_H, image::imageops::FilterType::Triangle);
        // RapidOCR pads with zeros *after* normalising, i.e. mid-grey, not black.
        let input = to_tensor(&resized, bucket, 0.0);
        let plan = self.rec_plan(bucket)?;
        let out = plan.run(tvec!(input.into())).map_err(|e| format!("OCR recognition failed: {e}"))?;
        let probs = out[0].to_plain_array_view::<f32>().map_err(|e| e.to_string())?;
        let shape = probs.shape().to_vec(); // [1, T, classes]
        let (t, classes) = (shape[1], shape[2]);
        let flat = probs.as_slice().ok_or("recognition output isn't contiguous")?;
        Ok(ctc_decode(flat, t, classes, &self.chars))
    }
}

/// Grey image -> `[1, 3, h, w]` tensor, normalised to -1..1 like RapidOCR; columns past
/// the image's width get `pad` (already normalised).
fn to_tensor(img: &GrayImage, w: usize, pad: f32) -> Tensor {
    let h = img.height() as usize;
    let arr = tract_ndarray::Array4::from_shape_fn((1, 3, h, w), |(_, _, y, x)| {
        if x < img.width() as usize {
            (img.get_pixel(x as u32, y as u32).0[0] as f32 / 255.0 - 0.5) / 0.5
        } else {
            pad
        }
    });
    arr.into()
}

/// Greedy CTC decode: best class per step, repeats collapsed, blanks dropped.
/// Confidence = mean probability of the kept characters.
fn ctc_decode(flat: &[f32], t: usize, classes: usize, chars: &[String]) -> (String, f32) {
    let mut text = String::new();
    let (mut sum, mut n) = (0.0, 0);
    let mut prev = usize::MAX;
    for step in 0..t {
        let row = &flat[step * classes..(step + 1) * classes];
        let (best, p) = row.iter().enumerate().fold((0, f32::MIN), |acc, (i, &v)| if v > acc.1 { (i, v) } else { acc });
        if best != 0 && best != prev && best < chars.len() {
            text.push_str(&chars[best]);
            sum += p;
            n += 1;
        }
        prev = best;
    }
    (text.trim().to_string(), if n == 0 { 0.0 } else { sum / n as f32 })
}

/// DB post-processing on the probability map: threshold, 2x2 dilation, connected
/// components, box score >= [`BOX_THRESH`], then expand ("unclip") each box. Comic text is
/// upright, so boxes are axis-aligned (RapidOCR fits rotated rectangles).
fn db_boxes(pred: &[f32], w: usize, h: usize) -> Vec<Rect> {
    let mut mask = vec![false; w * h];
    for y in 0..h {
        for x in 0..w {
            // Dilation with a 2x2 kernel: on if this pixel or one up/left of it is.
            let on = |yy: usize, xx: usize| pred[yy * w + xx] > THRESH;
            mask[y * w + x] = on(y, x) || (x > 0 && on(y, x - 1)) || (y > 0 && on(y - 1, x)) || (x > 0 && y > 0 && on(y - 1, x - 1));
        }
    }
    let mut seen = vec![false; w * h];
    let mut boxes = Vec::new();
    let mut stack = Vec::new();
    for start in 0..w * h {
        if !mask[start] || seen[start] {
            continue;
        }
        let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
        seen[start] = true;
        stack.push(start);
        while let Some(i) = stack.pop() {
            let (x, y) = (i % w, i / w);
            x0 = x0.min(x);
            x1 = x1.max(x);
            y0 = y0.min(y);
            y1 = y1.max(y);
            let mut visit = |j: usize| {
                if mask[j] && !seen[j] {
                    seen[j] = true;
                    stack.push(j);
                }
            };
            if x > 0 {
                visit(i - 1);
            }
            if x + 1 < w {
                visit(i + 1);
            }
            if y > 0 {
                visit(i - w);
            }
            if y + 1 < h {
                visit(i + w);
            }
        }
        let (bw, bh) = ((x1 - x0 + 1) as f32, (y1 - y0 + 1) as f32);
        if bw.min(bh) < MIN_SIZE {
            continue;
        }
        let mut score = 0.0;
        for y in y0..=y1 {
            score += pred[y * w + x0..=y * w + x1].iter().sum::<f32>();
        }
        score /= bw * bh;
        if score < BOX_THRESH {
            continue;
        }
        // Unclip: grow by area * ratio / perimeter on every side.
        let d = bw * bh * UNCLIP_RATIO / (2.0 * (bw + bh));
        let r = Rect { x0: x0 as f32 - d, y0: y0 as f32 - d, x1: x1 as f32 + 1.0 + d, y1: y1 as f32 + 1.0 + d };
        if r.w().min(r.h()) < MIN_SIZE + 2.0 {
            continue;
        }
        boxes.push(r);
    }
    boxes
}

impl OcrEngine for Paddle {
    fn lines(&self, img: &GrayImage) -> Result<Vec<Line>, String> {
        let mut lines = Vec::new();
        for r in self.detect(img)? {
            let (x, y) = (r.x0.floor() as u32, r.y0.floor() as u32);
            let (w, h) = ((r.x1.ceil() as u32).saturating_sub(x), (r.y1.ceil() as u32).saturating_sub(y));
            if w < 2 || h < 2 {
                continue;
            }
            let crop = image::imageops::crop_imm(img, x, y, w, h).to_image();
            let (text, conf) = self.recognize(&crop)?;
            if text.is_empty() || conf < MIN_LINE_SCORE {
                continue;
            }
            lines.push(Line { rect: r, text, conf: conf * 100.0, words: Vec::new() });
        }
        Ok(lines)
    }

    fn name(&self) -> &'static str {
        "paddle-v6"
    }

    fn workarounds(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctc_collapses_repeats_and_blanks() {
        let chars: Vec<String> = ["", "A", "B"].iter().map(|s| s.to_string()).collect();
        // steps: A A blank A B B  -> "AAB"
        let rows = [[0.1, 0.8, 0.1], [0.1, 0.9, 0.0], [0.9, 0.05, 0.05], [0.0, 0.7, 0.3], [0.0, 0.2, 0.8], [0.1, 0.1, 0.8]];
        let flat: Vec<f32> = rows.iter().flatten().copied().collect();
        let (t, conf) = ctc_decode(&flat, 6, 3, &chars);
        assert_eq!(t, "AAB");
        assert!((conf - (0.8 + 0.7 + 0.8) / 3.0).abs() < 1e-5);
    }

    #[test]
    fn db_finds_two_lines() {
        let (w, h) = (40, 30);
        let mut pred = vec![0.0f32; w * h];
        for y in 5..10 {
            for x in 4..30 {
                pred[y * w + x] = 0.9;
            }
        }
        for y in 18..23 {
            for x in 8..20 {
                pred[y * w + x] = 0.9;
            }
        }
        let b = db_boxes(&pred, w, h);
        assert_eq!(b.len(), 2);
        assert!(b[0].x0 < 4.0 && b[0].x1 > 30.0, "unclipped: {:?}", b[0]);
    }
}
