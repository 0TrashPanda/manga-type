//! The reading screen: the chapter as one vertical strip that only moves as you type.
//!
//! - Scrolling is locked; typing drives it. For each bubble, the scroll target goes from
//!   where the previous bubble left off to "bubble centre at 38% of the viewport",
//!   in proportion to how much of the bubble is typed, and the view eases toward it.
//! - Current bubble: accent outline (red while a wrong key flashes). Done: faint green.
//!   Skipped (Tab): dashed grey.
//! - OCR runs in the background; bubbles appear as bands finish, top first.
//! - Position (bubble index, done/skipped marks) and stats are saved next to the chapter
//!   (`library::ChapterProgress`) and restored when it's opened again.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use eframe::egui::{
    self, Color32, ColorImage, FontId, Key, Modifiers, Pos2, Rect, Stroke, StrokeKind, TextFormat,
    TextureHandle, TextureOptions, text::LayoutJob,
};

use crate::fix::Dictionary;
use crate::library::ChapterProgress;
use crate::ocr::{self, BandResult, Bubble, Scan, ScanEvent, Tesseract};
use crate::pages::Chapter;
use crate::settings::Settings;
use crate::typing::{CaseMode, Stats, Typing};

/// Rows per GPU texture; pages taller than this are split into tiles.
const TILE: u32 = 4096;
/// Fraction of the remaining distance the view moves per 60 Hz frame.
const EASE: f32 = 0.14;
/// Where the current bubble's centre ends up, as a fraction of viewport height.
const FOCUS: f32 = 0.38;
/// Padding around a bubble's text box when drawing its highlight (strip px).
const PAD: f32 = 8.0;
/// How long a wrong key flashes red.
const FLASH: Duration = Duration::from_millis(180);
/// Widest the strip is drawn, in screen points.
const MAX_WIDTH: f32 = 1000.0;

const ACCENT: Color32 = Color32::from_rgb(255, 170, 40);
const WRONG: Color32 = Color32::from_rgb(235, 60, 60);
const TYPED: Color32 = Color32::from_rgb(120, 215, 140);
const PENDING_TEXT: Color32 = Color32::from_gray(225);
const DIM_TEXT: Color32 = Color32::from_gray(140);

/// What the reader asks the app to do.
pub enum Action {
    /// Back to the library.
    Back,
    /// Open this chapter next.
    Open(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Todo,
    Done,
    Skipped,
}

struct Tile {
    tex: TextureHandle,
    /// Top of the tile in the page's own pixels.
    y: f32,
    h: f32,
}

type LoadedPage = (usize, Result<Vec<(ColorImage, f32)>, String>);

pub struct Reader {
    ch: Arc<Chapter>,
    dict: Option<Arc<Dictionary>>,
    dict_rx: Option<mpsc::Receiver<Option<Dictionary>>>,

    scan: Option<Scan>,
    results: Vec<BandResult>,
    bands_total: usize,
    /// Raw OCR bubbles once the scan is complete (or loaded from cache).
    raw_done: Option<Vec<Bubble>>,
    /// User corrections, keyed by the bubble's box, so they survive re-processing.
    edits: HashMap<[u32; 4], String>,

    /// What's shown and typed: raw OCR after ad removal and word correction.
    bubbles: Vec<Bubble>,
    states: Vec<State>,
    current: usize,
    typing: Option<Typing>,
    bubble_stats: Stats,
    chapter_stats: Stats,
    /// Read by the app after each frame (the case dropdown can change it).
    pub settings: Settings,
    /// The chapter after this one, offered when this one is finished.
    next: Option<PathBuf>,
    /// A progress file existed when opened (so an empty position is still worth saving).
    had_progress: bool,
    finished: bool,
    /// Scroll to the restored bubble once its position is known.
    restore_pending: bool,

    /// Strip y at the top of the viewport.
    scroll: f32,
    /// Where the current bubble started from (the previous bubble's end).
    start_scroll: f32,
    /// Viewport height in strip pixels (from the last frame).
    view_h: f32,

    tiles: HashMap<usize, Vec<Tile>>,
    requested: HashSet<usize>,
    load_tx: mpsc::Sender<usize>,
    loaded_rx: mpsc::Receiver<LoadedPage>,

    editing: Option<String>,
    notice: Option<String>,
    /// For creating textures and waking the UI from background threads.
    ctx: egui::Context,
}

fn rect_key(b: &Bubble) -> [u32; 4] {
    [b.rect.x0.to_bits(), b.rect.y0.to_bits(), b.rect.x1.to_bits(), b.rect.y1.to_bits()]
}

/// Decode pages on a background thread and cut them into texture-sized tiles.
fn spawn_loader(ch: Arc<Chapter>, ctx: egui::Context) -> (mpsc::Sender<usize>, mpsc::Receiver<LoadedPage>) {
    let (req_tx, req_rx) = mpsc::channel::<usize>();
    let (out_tx, out_rx) = mpsc::channel();
    thread::spawn(move || {
        for i in req_rx {
            let tiles = ch.load_page(i).map(|img| {
                let rgba = img.to_rgba8();
                let (w, h) = rgba.dimensions();
                let mut tiles = Vec::new();
                let mut y = 0;
                while y < h {
                    let th = TILE.min(h - y);
                    let sub = image::imageops::crop_imm(&rgba, 0, y, w, th).to_image();
                    tiles.push((ColorImage::from_rgba_unmultiplied([w as usize, th as usize], sub.as_raw()), y as f32));
                    y += th;
                }
                tiles
            });
            if out_tx.send((i, tiles)).is_err() {
                break;
            }
            ctx.request_repaint();
        }
    });
    (req_tx, out_rx)
}

impl Reader {
    pub fn new(ch: Arc<Chapter>, ctx: egui::Context, settings: Settings, next: Option<PathBuf>) -> Self {
        let (load_tx, loaded_rx) = spawn_loader(ch.clone(), ctx.clone());

        // The correction dictionary takes a moment to load; typing waits for it so the
        // text doesn't change under your fingers.
        let (dict_tx, dict_rx) = mpsc::channel();
        let dctx = ctx.clone();
        thread::spawn(move || {
            let _ = dict_tx.send(Dictionary::load_or_build().ok());
            dctx.request_repaint();
        });

        let mut r = Self {
            ch: ch.clone(),
            dict: None,
            dict_rx: Some(dict_rx),
            scan: None,
            results: Vec::new(),
            bands_total: 0,
            raw_done: None,
            edits: HashMap::new(),
            bubbles: Vec::new(),
            states: Vec::new(),
            current: 0,
            typing: None,
            bubble_stats: Stats::default(),
            chapter_stats: Stats::default(),
            settings,
            next,
            had_progress: false,
            finished: false,
            restore_pending: false,
            scroll: 0.0,
            start_scroll: 0.0,
            view_h: 1000.0,
            tiles: HashMap::new(),
            requested: HashSet::new(),
            load_tx,
            loaded_rx,
            editing: None,
            notice: None,
            ctx: ctx.clone(),
        };

        // A finished chapter opens from the start again ("Read again"); otherwise resume.
        if let Some(p) = ChapterProgress::load(&ch.path).filter(|p| !p.finished) {
            r.had_progress = true;
            r.current = p.current;
            r.states = p
                .states
                .chars()
                .map(|c| if c == 's' { State::Skipped } else { State::Done })
                .collect();
            r.states.resize(r.current, State::Done);
            r.chapter_stats = p.stats;
            r.restore_pending = r.current > 0;
        }

        if let Some(cache) = ocr::load_cache(&ch) {
            for b in &cache.bubbles {
                if let Some(e) = &b.edited {
                    r.edits.insert(rect_key(b), e.clone());
                }
            }
            r.raw_done = Some(cache.bubbles);
        } else {
            let tess = Tesseract::default();
            match tess.check() {
                Ok(()) => {
                    let scan = Scan::start(ch, Arc::new(tess), r.settings.ocr_workers());
                    r.bands_total = scan.bands.len();
                    r.scan = Some(scan);
                }
                Err(e) => r.notice = Some(e),
            }
        }
        r.refresh();
        r
    }

    // -- data -------------------------------------------------------------------------------

    fn with_edits(&self, mut raw: Vec<Bubble>) -> Vec<Bubble> {
        for b in &mut raw {
            b.edited = self.edits.get(&rect_key(b)).cloned();
        }
        raw
    }

    /// Rebuild the bubble list from the OCR results so far.
    fn refresh(&mut self) {
        let raw = match &self.raw_done {
            Some(raw) => raw.clone(),
            None => match &self.scan {
                Some(scan) => ocr::ready_bubbles(&scan.bands, &self.results).0,
                None => Vec::new(),
            },
        };
        let raw = self.with_edits(raw);
        self.bubbles = ocr::finish(&self.ch, &raw, self.dict.as_deref());
        // Never shrink: restored marks may run ahead of a scan that's still going.
        if self.states.len() < self.bubbles.len() {
            self.states.resize(self.bubbles.len(), State::Todo);
        }

        // Keep the current bubble's typing unless nothing is typed yet and its text changed.
        let target = self.bubbles.get(self.current).map(|b| b.target().to_string());
        let stale = match (&self.typing, &target) {
            (Some(t), Some(text)) => t.pos == 0 && t.chars != crate::typing::target(text, self.settings.case),
            (None, Some(_)) => true,
            _ => false,
        };
        if stale && self.dict_rx.is_none() {
            self.typing = target.map(|t| self.new_typing(&t));
        }
    }

    fn save_cache(&self) {
        if let Some(raw) = &self.raw_done
            && let Err(e) = ocr::save_cache(&self.ch, &self.with_edits(raw.clone()))
        {
            eprintln!("couldn't save OCR cache: {e}");
        }
    }

    fn poll_background(&mut self) {
        let mut changed = false;
        if let Some(rx) = &self.dict_rx
            && let Ok(d) = rx.try_recv()
        {
            self.dict = d.map(Arc::new);
            self.dict_rx = None;
            changed = true;
        }
        if let Some(scan) = &self.scan {
            while let Ok(ev) = scan.events.try_recv() {
                match ev {
                    ScanEvent::Band(r) => {
                        self.results.push(r);
                        changed = true;
                    }
                    ScanEvent::Error { band, error } => self.notice = Some(format!("OCR failed on band {band}: {error}")),
                    ScanEvent::Done => {
                        self.raw_done = Some(ocr::combine(&self.results));
                        self.save_cache();
                        changed = true;
                    }
                }
            }
            if self.raw_done.is_some() {
                self.scan = None;
            }
        }
        while let Ok((i, res)) = self.loaded_rx.try_recv() {
            match res {
                Ok(images) => {
                    let p = &self.ch.pages[i];
                    let ctx = self.ctx.clone();
                    let tiles = images
                        .into_iter()
                        .enumerate()
                        .map(|(k, (img, y))| {
                            let h = img.size[1] as f32;
                            Tile { tex: ctx.load_texture(format!("{}#{k}", p.name), img, TextureOptions::LINEAR), y, h }
                        })
                        .collect();
                    if self.requested.contains(&i) {
                        self.tiles.insert(i, tiles);
                    }
                }
                Err(e) => self.notice = Some(format!("Couldn't load page {}: {e}", self.ch.pages[i].name)),
            }
        }
        if changed {
            self.refresh();
        }
    }

    // -- typing -----------------------------------------------------------------------------

    fn max_scroll(&self) -> f32 {
        (self.ch.strip_height - self.view_h).max(0.0)
    }

    /// Scroll position that puts bubble `i` at the focus line.
    fn end_for(&self, i: usize) -> f32 {
        let b = &self.bubbles[i];
        let cy = (b.rect.y0 + b.rect.y1) / 2.0;
        (cy - FOCUS * self.view_h).clamp(0.0, self.max_scroll())
    }

    fn target_scroll(&self) -> f32 {
        match (&self.typing, self.current < self.bubbles.len()) {
            (Some(t), true) => {
                let end = self.end_for(self.current);
                self.start_scroll + (end - self.start_scroll) * t.progress()
            }
            // Waiting for OCR or the dictionary: hold still.
            _ => self.start_scroll,
        }
    }

    fn advance(&mut self, state: State) {
        if self.current >= self.bubbles.len() {
            return;
        }
        self.states[self.current] = state;
        self.start_scroll = self.end_for(self.current);
        self.chapter_stats.merge(&self.bubble_stats);
        self.bubble_stats = Stats::default();
        self.current += 1;
        self.typing = self.bubbles.get(self.current).map(|b| self.new_typing(b.target()));
        self.save_progress();
    }

    fn total_stats(&self) -> Stats {
        let mut total = self.chapter_stats.clone();
        total.merge(&self.bubble_stats);
        total
    }

    /// Save position and stats next to the chapter.
    pub fn save_progress(&mut self) {
        let stats = self.total_stats();
        if !self.had_progress && self.current == 0 && stats.correct + stats.wrong == 0 {
            return; // opened and closed without typing: leave no file behind
        }
        let states = self.states[..self.current.min(self.states.len())]
            .iter()
            .map(|s| if *s == State::Skipped { 's' } else { 'd' })
            .collect();
        let mut p = ChapterProgress {
            current: self.current,
            states,
            bubbles: if self.raw_done.is_some() { self.bubbles.len() } else { 0 },
            finished: self.finished,
            stats,
            updated: 0,
        };
        match p.save(&self.ch.path) {
            Ok(()) => self.had_progress = true,
            Err(e) => self.notice = Some(format!("Couldn't save your position: {e}")),
        }
    }

    /// All bubbles typed or skipped and the scan is complete.
    fn at_end(&self) -> bool {
        self.raw_done.is_some() && self.dict_rx.is_none() && self.current >= self.bubbles.len()
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        let (tab, events) = ctx.input_mut(|i| (i.consume_key(Modifiers::NONE, Key::Tab), i.events.clone()));
        if tab {
            self.advance(State::Skipped);
            return;
        }
        for ev in events {
            match ev {
                egui::Event::Text(text) => {
                    for c in text.chars() {
                        if let Some(t) = &mut self.typing
                            && let Some(ok) = t.type_char(c)
                        {
                            self.bubble_stats.key(ok);
                        }
                    }
                }
                egui::Event::Key { key: Key::Backspace, pressed: true, .. } => {
                    if let Some(t) = &mut self.typing {
                        t.backspace();
                    }
                }
                _ => {}
            }
            if self.typing.as_ref().is_some_and(Typing::done) {
                self.advance(State::Done);
            }
        }
    }

    fn new_typing(&self, text: &str) -> Typing {
        Typing::new(text, self.settings.strictness, self.settings.case)
    }

    /// A setting changed: retarget the current bubble (keeping its progress) and save.
    fn settings_changed(&mut self) {
        if let (Some(t), Some(b)) = (&self.typing, self.bubbles.get(self.current)) {
            self.typing = Some(t.with_settings(b.target(), self.settings.strictness, self.settings.case));
        }
        if let Err(e) = self.settings.save() {
            self.notice = Some(format!("Couldn't save settings: {e}"));
        }
    }

    fn apply_edit(&mut self, text: String) {
        let Some(b) = self.bubbles.get(self.current) else { return };
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        self.edits.insert(rect_key(b), text.clone());
        self.typing = Some(self.new_typing(&text));
        self.refresh();
        self.save_cache();
    }

    // -- drawing ----------------------------------------------------------------------------

    fn draw_strip(&mut self, ui: &mut egui::Ui) {
        let rect = ui.max_rect();
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::from_gray(18));

        let width = (rect.width() - 24.0).clamp(100.0, MAX_WIDTH);
        let scale = width / self.ch.strip_width as f32;
        self.view_h = rect.height() / scale;
        let x0 = rect.center().x - width / 2.0;
        let to_screen = |x: f32, y: f32| Pos2::new(x0 + x * scale, rect.top() + (y - self.scroll) * scale);

        // Load pages around the view, drop far ones.
        let (near0, near1) = (self.scroll - self.view_h, self.scroll + 2.0 * self.view_h);
        let (keep0, keep1) = (self.scroll - 3.0 * self.view_h, self.scroll + 4.0 * self.view_h);
        for (i, p) in self.ch.pages.iter().enumerate() {
            let (top, bottom) = (p.strip_y, p.strip_y + p.strip_h);
            if bottom > near0 && top < near1 && self.requested.insert(i) {
                let _ = self.load_tx.send(i);
            }
            if bottom < keep0 || top > keep1 {
                self.requested.remove(&i);
                self.tiles.remove(&i);
            }
        }

        // Pages.
        let (view0, view1) = (self.scroll, self.scroll + self.view_h);
        for (i, p) in self.ch.pages.iter().enumerate() {
            if p.strip_y + p.strip_h < view0 || p.strip_y > view1 {
                continue;
            }
            let page_scale = p.strip_h / p.height as f32;
            match self.tiles.get(&i) {
                Some(tiles) => {
                    for t in tiles {
                        let y0 = p.strip_y + t.y * page_scale;
                        let r = Rect::from_min_max(
                            to_screen(0.0, y0),
                            to_screen(self.ch.strip_width as f32, y0 + t.h * page_scale),
                        );
                        painter.image(t.tex.id(), r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                    }
                }
                None => {
                    let r = Rect::from_min_max(to_screen(0.0, p.strip_y), to_screen(self.ch.strip_width as f32, p.strip_y + p.strip_h));
                    painter.rect_filled(r, 0.0, Color32::from_gray(40));
                }
            }
        }

        // Bubble highlights.
        let flashing = self.typing.as_ref().is_some_and(|t| t.flashing(FLASH));
        for (i, b) in self.bubbles.iter().enumerate() {
            if b.rect.y1 + PAD < view0 || b.rect.y0 - PAD > view1 {
                continue;
            }
            let r = Rect::from_min_max(
                to_screen(b.rect.x0 - PAD, b.rect.y0 - PAD),
                to_screen(b.rect.x1 + PAD, b.rect.y1 + PAD),
            );
            if i == self.current {
                let color = if flashing { WRONG } else { ACCENT };
                painter.rect_stroke(r, 6.0, Stroke::new(3.0, color), StrokeKind::Outside);
            } else {
                match self.states[i] {
                    State::Done => {
                        painter.rect_filled(r, 6.0, Color32::from_rgba_unmultiplied(80, 200, 110, 40));
                        painter.rect_stroke(r, 6.0, Stroke::new(1.0, Color32::from_rgba_unmultiplied(80, 200, 110, 120)), StrokeKind::Outside);
                    }
                    State::Skipped => {
                        let pts = [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom(), r.left_top()];
                        painter.extend(egui::Shape::dashed_line(&pts, Stroke::new(1.5, Color32::from_gray(150)), 6.0, 5.0));
                    }
                    State::Todo => {}
                }
            }
        }
    }

    fn bubble_text_job(&self, width: f32) -> LayoutJob {
        let mut job = LayoutJob::default();
        job.wrap.max_width = width;
        let font = FontId::monospace(20.0);
        let Some(t) = &self.typing else { return job };
        let flashing = t.flashing(FLASH);
        for (i, c) in t.chars.iter().enumerate() {
            let mut fmt = TextFormat::simple(font.clone(), if i < t.pos { TYPED } else { PENDING_TEXT });
            if i == t.pos {
                fmt.background = if flashing { WRONG } else { Color32::from_rgba_unmultiplied(255, 170, 40, 90) };
                fmt.underline = Stroke::new(2.0, ACCENT);
            }
            job.append(&c.to_string(), 0.0, fmt);
        }
        job
    }

    fn draw_bar(&mut self, ui: &mut egui::Ui) -> Option<Action> {
        let mut action = None;
        ui.add_space(8.0);
        let done = self.current >= self.bubbles.len();
        let scanning = self.raw_done.is_none() && self.scan.is_some();

        if self.dict_rx.is_some() {
            ui.label(egui::RichText::new("Loading dictionary…").color(DIM_TEXT));
        } else if !done {
            let job = self.bubble_text_job(ui.available_width());
            ui.label(job);
        } else if scanning {
            ui.label(
                egui::RichText::new(format!("Scanning ahead… {}/{} bands", self.results.len(), self.bands_total))
                    .size(18.0)
                    .color(DIM_TEXT),
            );
        } else {
            let total = self.total_stats();
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "Chapter finished — {:.0} WPM, {:.1}% accuracy",
                        total.wpm(),
                        total.accuracy()
                    ))
                    .size(20.0)
                    .color(TYPED),
                );
                if let Some(next) = &self.next {
                    let name = next.file_name().map(|n| crate::phone::display_name(&n.to_string_lossy())).unwrap_or_default();
                    if ui
                        .add(egui::Button::new(egui::RichText::new(format!("Next: {name}  (Enter)")).size(16.0)).fill(Color32::from_rgb(70, 110, 60)))
                        .clicked()
                    {
                        action = Some(Action::Open(next.clone()));
                    }
                } else {
                    ui.label(egui::RichText::new("No next chapter on this PC yet.").color(DIM_TEXT));
                }
            });
        }

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button("← Library").on_hover_text("Back to the library (Esc). Your place is saved.").clicked() {
                action = Some(Action::Back);
            }
            ui.separator();
            let total = self.total_stats();
            let n = self.bubbles.len();
            let more = if scanning { "+" } else { "" };
            ui.label(format!("Bubble {} / {n}{more}", (self.current + 1).min(n.max(1))));
            ui.separator();
            ui.label(format!("{:.0} WPM", total.wpm()));
            ui.label(egui::RichText::new(format!("(bubble {:.0})", self.bubble_stats.wpm())).color(DIM_TEXT));
            ui.separator();
            ui.label(format!("{:.1}% accuracy", total.accuracy()));
            ui.separator();
            if ui.add_enabled(!done, egui::Button::new("Skip  (Tab)")).clicked() {
                self.advance(State::Skipped);
            }
            if ui.add_enabled(!done, egui::Button::new("Edit text")).clicked()
                && let Some(b) = self.bubbles.get(self.current)
            {
                self.editing = Some(b.target().to_string());
            }
            ui.separator();
            let before = self.settings.case;
            egui::ComboBox::from_id_salt("case-mode")
                .selected_text(self.settings.case.label())
                .show_ui(ui, |ui| {
                    for m in CaseMode::ALL {
                        ui.selectable_value(&mut self.settings.case, m, m.label());
                    }
                });
            if self.settings.case != before {
                self.settings_changed();
            }
            if scanning {
                ui.separator();
                ui.label(egui::RichText::new(format!("OCR {}/{}", self.results.len(), self.bands_total)).color(DIM_TEXT));
            }
        });
        if let Some(n) = &self.notice {
            ui.label(egui::RichText::new(n).color(WRONG));
        }
        action
    }

    fn draw_edit_dialog(&mut self, ctx: &egui::Context) {
        let Some(text) = &mut self.editing else { return };
        let (mut save, mut cancel) = (false, false);
        egui::Modal::new(egui::Id::unique("edit-bubble")).show(ctx, |ui| {
            ui.heading("Edit bubble text");
            ui.label(egui::RichText::new("Fix what OCR got wrong, then type it.").color(DIM_TEXT));
            let edit = ui.add(egui::TextEdit::multiline(text).desired_width(520.0).desired_rows(4).font(FontId::monospace(18.0)));
            edit.request_focus();
            ui.horizontal(|ui| {
                save = ui.button("Save").clicked();
                cancel = ui.button("Cancel").clicked();
            });
            if ui.input(|i| i.key_pressed(Key::Escape)) {
                cancel = true;
            }
        });
        if save {
            let t = text.clone();
            self.editing = None;
            self.apply_edit(t);
        } else if cancel {
            self.editing = None;
        }
    }
}

impl Reader {
    pub fn chapter_path(&self) -> &std::path::Path {
        &self.ch.path
    }

    /// Draw one frame. Returns what the app should do next, if anything.
    pub fn show(&mut self, ui: &mut egui::Ui) -> Option<Action> {
        let ctx = ui.ctx().clone();
        self.poll_background();
        let mut action = None;

        if self.editing.is_none() {
            // Keys go to typing, not to whatever button was clicked last (Space would press it).
            ctx.memory_mut(|m| {
                if let Some(id) = m.focused() {
                    m.surrender_focus(id);
                }
            });
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
                action = Some(Action::Back);
            }
            if self.at_end() {
                if let Some(next) = &self.next
                    && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter))
                {
                    action = Some(Action::Open(next.clone()));
                }
            } else if self.dict_rx.is_none() {
                self.handle_keys(&ctx);
            }
        }

        if self.at_end() && !self.finished {
            self.finished = true;
            self.save_progress();
        }

        if let Some(a) = egui::Panel::bottom("bar").exact_size(124.0).show(ui, |ui| self.draw_bar(ui)).inner {
            action = Some(a);
        }
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| self.draw_strip(ui));
        self.draw_edit_dialog(&ctx);

        // Jump straight to a restored position once the bubbles before it are known.
        if self.restore_pending && self.bubbles.len() >= self.current {
            self.restore_pending = false;
            self.start_scroll = self.end_for(self.current - 1);
            self.scroll = self.start_scroll;
        }

        // Ease toward the target, frame-rate independent.
        let dt = ctx.input(|i| i.stable_dt).min(0.1);
        let target = self.target_scroll().clamp(0.0, self.max_scroll());
        let k = 1.0 - (1.0 - EASE).powf(dt * 60.0);
        self.scroll += (target - self.scroll) * k;
        if (target - self.scroll).abs() > 0.3 {
            ctx.request_repaint();
        } else {
            self.scroll = target;
        }
        if self.typing.as_ref().is_some_and(|t| t.flashing(FLASH)) {
            ctx.request_repaint_after(FLASH);
        }
        // OCR workers don't wake the UI themselves: keep polling while a scan runs,
        // otherwise new bubbles only show up on the next mouse move or key press.
        if self.scan.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        action
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.save_progress();
    }
}
