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
use crate::ocr::{self, BandResult, Bubble, Scan, ScanEvent};
use crate::pages::Chapter;
use crate::settings::{ScrollMode, Settings};
use crate::typing::{CaseMode, Stats, Typing};

/// Rows per GPU texture; pages taller than this are split into tiles.
const TILE: u32 = 4096;
/// Fraction of the remaining distance the view moves per 60 Hz frame.
const EASE: f32 = 0.14;
/// Where the current bubble's centre ends up, as a fraction of viewport height.
const FOCUS: f32 = 0.38;
/// A new bubble is first brought at least this far up (its bottom edge, as a fraction of
/// viewport height), so you never type text that's still off screen.
const REVEAL: f32 = 0.85;
/// Page zoom limits and the step per Ctrl +/- press.
const ZOOM_MIN: f32 = 0.3;
const ZOOM_MAX: f32 = 4.0;
const ZOOM_STEP: f32 = 1.15;
/// Longest gap between inputs that still counts as time spent reading.
const IDLE_CAP: Duration = Duration::from_secs(60);
/// j / k / arrow keys scroll this fraction of the window height.
const LINE_STEP: f32 = 0.12;
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
    /// Open the settings dialog.
    Settings,
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
    /// A dialog of the app is open on top: don't take keys.
    pub paused: bool,
    /// OCR engine of this chapter's scan (and its cache).
    engine_name: String,
    /// The chapter after this one, offered when this one is finished.
    next: Option<PathBuf>,
    /// A progress file existed when opened (so an empty position is still worth saving).
    had_progress: bool,
    finished: bool,
    /// Scroll to the restored bubble once its position is known.
    restore_pending: bool,

    /// Strip y at the top of the viewport.
    scroll: f32,
    /// Where manual scrolling (keys, wheel) is heading; the view eases toward it.
    manual_target: f32,
    /// You've typed the first letter of the current bubble: from now on j / k / Space type.
    typing_started: bool,
    /// Time spent on this chapter (saved with the progress).
    reading_seconds: f64,
    /// Last input (key, mouse, scroll) while the window had focus.
    last_activity: Option<std::time::Instant>,
    /// State when the chapter was opened, for the session log.
    session_start: (f64, Stats, usize),
    /// Screen points per strip pixel (from the last frame; 0 before the first).
    scale: f32,
    /// Sideways offset of the page in screen points, when zoomed wider than the window.
    pan_x: f32,
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
            paused: false,
            engine_name: String::new(),
            next,
            had_progress: false,
            finished: false,
            restore_pending: false,
            scroll: 0.0,
            manual_target: 0.0,
            typing_started: false,
            reading_seconds: 0.0,
            last_activity: None,
            session_start: (0.0, Stats::default(), 0),
            scale: 0.0,
            pan_x: 0.0,
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
        let saved = ChapterProgress::load(&ch.path);
        // Time spent carries over every read, also "Read again" on a finished chapter.
        r.reading_seconds = saved.as_ref().map_or(0.0, |p| p.reading_seconds);
        if let Some(p) = saved.filter(|p| !p.finished) {
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

        let engine = ocr::engine();
        if let Ok((e, note)) = &engine {
            r.engine_name = e.name().to_string();
            r.notice = note.clone();
        }
        r.session_start = (r.reading_seconds, r.chapter_stats.clone(), r.current);
        if let Some(cache) = ocr::load_cache(&ch, &r.engine_name) {
            for b in &cache.bubbles {
                if let Some(e) = &b.edited {
                    r.edits.insert(rect_key(b), e.clone());
                }
            }
            r.raw_done = Some(cache.bubbles);
        } else {
            match engine {
                Ok((e, _)) => {
                    let scan = Scan::start(ch, e, r.settings.ocr_workers());
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
        self.bubbles = ocr::finish(&self.ch, &raw, self.dict.as_deref(), &self.engine_name);
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
            && let Err(e) = ocr::save_cache(&self.ch, &self.with_edits(raw.clone()), &self.engine_name)
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

    /// Smallest scroll position that shows bubble `i` fully, its bottom at [`REVEAL`].
    fn reveal_for(&self, i: usize) -> f32 {
        let b = &self.bubbles[i];
        (b.rect.y1 + PAD - REVEAL * self.view_h).clamp(0.0, self.max_scroll())
    }

    fn target_scroll(&self) -> f32 {
        if self.settings.scroll == ScrollMode::Manual {
            return self.manual_target;
        }
        match (&self.typing, self.current < self.bubbles.len()) {
            (Some(t), true) => {
                let end = self.end_for(self.current);
                // Where the previous bubble left off, but never with this bubble still below
                // the screen (long art stretches between bubbles): jump there first.
                let start = self.start_scroll.max(self.reveal_for(self.current)).min(end);
                start + (end - start) * t.progress()
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
        if let Some(t) = &self.typing {
            self.bubble_stats.uncorrected += t.uncorrected();
            if state == State::Done {
                let text: String = t.chars.iter().collect();
                self.bubble_stats.words += text.split_whitespace().filter(|w| w.chars().any(char::is_alphanumeric)).count() as u64;
            }
        }
        self.chapter_stats.merge(&self.bubble_stats);
        self.bubble_stats = Stats::default();
        self.current += 1;
        self.typing_started = false;
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
        if !self.had_progress && self.current == 0 && stats.correct + stats.wrong == 0 && self.reading_seconds < 10.0 {
            return; // opened and closed without doing anything: leave no file behind
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
            reading_seconds: self.reading_seconds,
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

    /// Furthest manual scrolling may go: the bubble you're on sticks to the top of the
    /// screen until it's typed or skipped (Tab).
    fn scroll_limit(&self) -> f32 {
        let max = self.max_scroll();
        match self.bubbles.get(self.current) {
            Some(b) if self.settings.scroll == ScrollMode::Manual => (b.rect.y0 - PAD - 8.0).clamp(0.0, max),
            _ => max,
        }
    }

    /// Scroll by `amount` strip pixels (manual mode).
    fn scroll_by(&mut self, amount: f32) {
        self.manual_target = (self.manual_target + amount).clamp(0.0, self.scroll_limit());
    }

    /// Where the current bubble is relative to the (target) view: `Ordering::Equal` when
    /// it's on screen, `Less` above, `Greater` below. `None` when there's no current bubble.
    fn current_placement(&self) -> Option<std::cmp::Ordering> {
        use std::cmp::Ordering;
        let b = self.bubbles.get(self.current)?;
        if self.settings.scroll != ScrollMode::Manual {
            return Some(Ordering::Equal);
        }
        let (top, bottom) = (self.manual_target, self.manual_target + self.view_h);
        let (y0, y1) = (b.rect.y0 - PAD, b.rect.y1 + PAD);
        // On screen: fully visible, or (taller than the window) mostly visible.
        let visible = (y1 - y0) * 0.6;
        let shown = y1.min(bottom) - y0.max(top);
        Some(if (y0 >= top - 1.0 && y1 <= bottom + 1.0) || shown >= visible.min(self.view_h * 0.6) {
            Ordering::Equal
        } else if y1 <= top + 1.0 || (y0 < top && shown < visible) && b.rect.y0 < top {
            Ordering::Less
        } else {
            Ordering::Greater
        })
    }

    /// Ctrl + / Ctrl - / Ctrl 0 and Ctrl + mouse wheel zoom the page; Left / Right pan
    /// when it's wider than the window. Saved as a setting.
    fn handle_zoom_keys(&mut self, ctx: &egui::Context) {
        use egui::KeyboardShortcut as K;
        let locked = self.scroll_locked();
        let before = self.settings.zoom;
        let mut zoom = before;
        ctx.input_mut(|i| {
            if i.consume_shortcut(&K::new(Modifiers::COMMAND, Key::Num0)) {
                zoom = 1.0;
            }
            if i.consume_shortcut(&K::new(Modifiers::COMMAND, Key::Plus))
                || i.consume_shortcut(&K::new(Modifiers::COMMAND, Key::Equals))
            {
                zoom *= ZOOM_STEP;
            }
            if i.consume_shortcut(&K::new(Modifiers::COMMAND, Key::Minus)) {
                zoom /= ZOOM_STEP;
            }
            // Ctrl + wheel (and touchpad pinch).
            zoom *= i.zoom_delta();
            let pan = !locked || i.modifiers.command;
            if pan && i.key_pressed(Key::ArrowLeft) {
                self.pan_x += 80.0;
            }
            if pan && i.key_pressed(Key::ArrowRight) {
                self.pan_x -= 80.0;
            }
        });
        let zoom = zoom.clamp(ZOOM_MIN, ZOOM_MAX);
        if (zoom - before).abs() > 1e-4 {
            self.settings.zoom = zoom;
            if (zoom - 1.0).abs() < 0.02 {
                self.settings.zoom = 1.0;
            }
            if let Err(e) = self.settings.save() {
                self.notice = Some(format!("Couldn't save settings: {e}"));
            }
        }
    }

    /// Scroll for a j / k / Space typed as text (manual mode).
    fn scroll_char(&mut self, c: char) {
        match c {
            'j' => self.scroll_by(LINE_STEP * self.view_h),
            'k' => self.scroll_by(-LINE_STEP * self.view_h),
            ' ' => self.scroll_by(self.settings.space_step.fraction() * self.view_h),
            _ => {}
        }
    }

    /// Whether j / k / Space type (true) or scroll (false). In manual mode they scroll
    /// until you've typed the first letter of the bubble you're on (it must be on screen),
    /// and again from the next bubble on.
    fn keys_type(&self) -> bool {
        self.settings.scroll != ScrollMode::Manual
            || (self.typing_started && !self.at_end() && self.current_placement() == Some(std::cmp::Ordering::Equal))
    }

    /// Scrolling that works in every state (manual mode): wheel, arrows, Page Up/Down,
    /// Shift+Space, and at the end of the chapter also j / k / Space.
    /// While typing a bubble (manual scrolling), the scroll keys only work with Ctrl held, so
    /// Shift+Space or an arrow key in the middle of a sentence can't make the page jump.
    /// The mouse wheel and touchpad always scroll (the user's choice).
    fn scroll_locked(&self) -> bool {
        self.settings.scroll == ScrollMode::Manual && self.keys_type()
    }

    fn handle_scroll_keys(&mut self, ctx: &egui::Context) {
        if self.settings.scroll != ScrollMode::Manual {
            return;
        }
        let line = LINE_STEP * self.view_h;
        let page = 0.9 * self.view_h;
        let space = self.settings.space_step.fraction() * self.view_h;
        let locked = self.scroll_locked();
        let wheel = ctx.input(|i| i.smooth_scroll_delta.y);
        if wheel != 0.0 {
            self.scroll_by(-wheel / self.scale.max(0.01));
        }
        let keys_type = self.keys_type();
        let at_end = self.at_end();
        let shift = ctx.input(|i| i.modifiers.shift);
        let events = ctx.input(|i| i.events.clone());
        for ev in &events {
            match ev {
                egui::Event::Key { key, pressed: true, modifiers, .. } if locked => {
                    if modifiers.command {
                        match key {
                            Key::ArrowDown | Key::J => self.scroll_by(line),
                            Key::ArrowUp | Key::K => self.scroll_by(-line),
                            Key::PageDown => self.scroll_by(page),
                            Key::PageUp => self.scroll_by(-page),
                            _ => {}
                        }
                    }
                }
                egui::Event::Key { key, pressed: true, modifiers, .. } => match key {
                    Key::ArrowDown => self.scroll_by(line),
                    Key::ArrowUp => self.scroll_by(-line),
                    Key::PageDown => self.scroll_by(page),
                    Key::PageUp => self.scroll_by(-page),
                    Key::Home if !keys_type => self.manual_target = 0.0,
                    Key::End if !keys_type => self.manual_target = self.scroll_limit(),
                    Key::Space if modifiers.shift => self.scroll_by(-space),
                    _ => {}
                },
                // Typing is over: the keys only scroll (handle_keys isn't called).
                egui::Event::Text(t) if at_end && !(shift && t == " ") => t.chars().for_each(|c| self.scroll_char(c)),
                _ => {}
            }
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        let (tab, events) = ctx.input_mut(|i| (i.consume_key(Modifiers::NONE, Key::Tab), i.events.clone()));
        if tab {
            self.advance(State::Skipped);
            return;
        }
        let manual = self.settings.scroll == ScrollMode::Manual;
        let shift = ctx.input(|i| i.modifiers.shift);
        let on_screen = self.current_placement().is_none_or(|p| p == std::cmp::Ordering::Equal);
        for ev in events {
            match ev {
                // Shift+Space scrolls up in manual mode (handle_scroll_keys) until you start
                // typing the bubble; from then on it's just a space.
                egui::Event::Text(text) if !(manual && shift && text == " " && !self.typing_started) => {
                    for c in text.chars() {
                        let scroll_key = matches!(c, 'j' | 'k' | ' ');
                        if manual && !self.typing_started {
                            // Before the first letter: j / k / Space scroll, unless the bubble
                            // really starts with that letter. Off screen nothing types.
                            let expected = self.typing.as_ref().is_some_and(|t| t.expects(c));
                            if !on_screen || (scroll_key && !expected) {
                                if scroll_key {
                                    self.scroll_char(c);
                                }
                                continue;
                            }
                            self.typing_started = true;
                        }
                        if let Some(t) = &mut self.typing {
                            let expected = t.chars.get(t.pos).copied();
                            if let Some(ok) = t.type_char(c) {
                                self.bubble_stats.key_char(expected.unwrap_or(c), c, ok);
                            }
                        }
                    }
                }
                egui::Event::Key { key: Key::Backspace, pressed: true, .. } if on_screen => {
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
        Typing::new(text, self.settings.strictness, self.settings.case).errors(self.settings.errors)
    }

    /// Settings changed in the app's dialog (already saved there).
    pub fn apply_settings(&mut self, settings: Settings) {
        self.settings = settings;
        if let (Some(t), Some(b)) = (&self.typing, self.bubbles.get(self.current)) {
            self.typing = Some(t.with_settings(b.target(), self.settings.strictness, self.settings.case, self.settings.errors));
        }
        self.manual_target = self.scroll;
    }

    /// A setting changed: retarget the current bubble (keeping its progress) and save.
    fn settings_changed(&mut self) {
        if let (Some(t), Some(b)) = (&self.typing, self.bubbles.get(self.current)) {
            self.typing = Some(t.with_settings(b.target(), self.settings.strictness, self.settings.case, self.settings.errors));
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
        self.typing_started = false;
        self.refresh();
        self.save_cache();
    }

    // -- drawing ----------------------------------------------------------------------------

    fn draw_strip(&mut self, ui: &mut egui::Ui) {
        let rect = ui.max_rect();
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::from_gray(18));

        let fit = (rect.width() - 24.0).clamp(100.0, MAX_WIDTH);
        let width = (fit * self.settings.zoom).max(60.0);
        let scale = width / self.ch.strip_width as f32;
        let view_h = rect.height() / scale;
        // Zoom (or a window resize) changed the scale: keep the middle of the view in place.
        if self.scale > 0.0 && (scale - self.scale).abs() > 1e-6 {
            let keep = |y: f32| y + (self.view_h - view_h) / 2.0;
            self.scroll = keep(self.scroll);
            self.manual_target = keep(self.manual_target);
        }
        self.scale = scale;
        self.view_h = view_h;
        self.scroll = self.scroll.clamp(0.0, self.max_scroll());
        self.manual_target = self.manual_target.clamp(0.0, self.scroll_limit());
        // Wider than the window: Left / Right pan; otherwise centred.
        let max_pan = ((width - rect.width()) / 2.0 + 12.0).max(0.0);
        self.pan_x = self.pan_x.clamp(-max_pan, max_pan);
        let x0 = rect.center().x - width / 2.0 + self.pan_x;
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
        // Off screen (manual scrolling): dim the text until you scroll to the bubble.
        let offscreen = self.current_placement().is_some_and(|p| p != std::cmp::Ordering::Equal);
        for (i, c) in t.chars.iter().enumerate() {
            let color = if t.is_wrong(i) {
                WRONG
            } else if i < t.pos {
                TYPED
            } else if offscreen {
                DIM_TEXT
            } else {
                PENDING_TEXT
            };
            let mut fmt = TextFormat::simple(font.clone(), color);
            if t.is_wrong(i) {
                fmt.underline = Stroke::new(2.0, WRONG);
            }
            if i == t.pos && !offscreen {
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
            ui.horizontal_wrapped(|ui| {
                match self.current_placement() {
                    Some(std::cmp::Ordering::Greater) => {
                        ui.label(egui::RichText::new("▼ scroll down to this bubble (j, Space, ↓, wheel)").color(ACCENT));
                    }
                    Some(std::cmp::Ordering::Less) => {
                        ui.label(egui::RichText::new("▲ scroll up to this bubble (k, Shift+Space, ↑, wheel)").color(ACCENT));
                    }
                    Some(_) if self.settings.scroll == ScrollMode::Manual && !self.typing_started => {
                        ui.label(
                            egui::RichText::new("Type the first letter to start · until then j / k / Space scroll").color(DIM_TEXT),
                        );
                    }
                    Some(_) if self.scroll_locked() => {
                        ui.label(egui::RichText::new("Typing: keys scroll with Ctrl (Ctrl + ↑ / ↓, J / K, Page Up / Down) · wheel always").color(DIM_TEXT));
                    }
                    _ => {}
                }
                if self.typing.as_ref().is_some_and(Typing::needs_fixing) {
                    ui.label(egui::RichText::new("Fix the red letters (Backspace) to finish this bubble").color(WRONG));
                }
            });
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
        // Wraps onto a second line in narrow windows instead of cutting buttons off.
        ui.horizontal_wrapped(|ui| {
            if ui.button("← Library").on_hover_text("Back to the library (Esc). Your place is saved.").clicked() {
                action = Some(Action::Back);
            }
            if ui.button("Settings").clicked() {
                action = Some(Action::Settings);
            }
            if (self.settings.zoom - 1.0).abs() > 1e-3 {
                ui.label(egui::RichText::new(format!("zoom {:.0}%", self.settings.zoom * 100.0)).color(DIM_TEXT))
                    .on_hover_text("Ctrl + / Ctrl - / Ctrl 0 (reset), or Ctrl + mouse wheel");
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
            ui.label(egui::RichText::new(crate::library::format_duration(self.reading_seconds)).color(DIM_TEXT))
                .on_hover_text("Time spent on this chapter");
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

    /// Count time between inputs while the window has focus; a gap over [`IDLE_CAP`]
    /// counts as that much (looking at a page, not away from the computer).
    fn track_time(&mut self, ctx: &egui::Context) {
        let (active, focused) = ctx.input(|i| {
            (!i.events.is_empty() || i.pointer.is_moving() || i.smooth_scroll_delta != egui::Vec2::ZERO, i.focused)
        });
        if !focused {
            self.last_activity = None;
            return;
        }
        if active {
            let now = std::time::Instant::now();
            if let Some(t) = self.last_activity {
                self.reading_seconds += now.duration_since(t).min(IDLE_CAP).as_secs_f64();
            }
            self.last_activity = Some(now);
        }
    }

    /// Draw one frame. Returns what the app should do next, if anything.
    pub fn show(&mut self, ui: &mut egui::Ui) -> Option<Action> {
        let ctx = ui.ctx().clone();
        self.track_time(&ctx);
        self.poll_background();
        let mut action = None;

        if self.editing.is_none() && !self.paused {
            self.handle_zoom_keys(&ctx);
            // Keys go to typing, not to whatever button was clicked last (Space would press it).
            ctx.memory_mut(|m| {
                if let Some(id) = m.focused() {
                    m.surrender_focus(id);
                }
            });
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
                action = Some(Action::Back);
            }
            self.handle_scroll_keys(&ctx);
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

        if let Some(a) = egui::Panel::bottom("bar").exact_size(150.0).show(ui, |ui| self.draw_bar(ui)).inner {
            action = Some(a);
        }
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| self.draw_strip(ui));
        self.draw_edit_dialog(&ctx);

        // Jump straight to a restored position once the bubbles before it are known.
        if self.restore_pending && self.bubbles.len() >= self.current {
            self.restore_pending = false;
            self.start_scroll = self.end_for(self.current - 1);
            self.scroll = self.start_scroll;
            self.manual_target = self.start_scroll;
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

impl Reader {
    /// Append this visit to the session log (if anything happened).
    fn log_session(&self) {
        let (read0, stats0, current0) = &self.session_start;
        let total = self.total_stats();
        let seconds = self.reading_seconds - read0;
        let correct = total.correct.saturating_sub(stats0.correct);
        if seconds < 5.0 && correct == 0 {
            return;
        }
        let name = |p: Option<&std::path::Path>| {
            p.and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        };
        let s = crate::history::Session {
            ended: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()),
            series: name(self.ch.path.parent()),
            chapter: crate::phone::display_name(&name(Some(&self.ch.path))),
            seconds,
            correct,
            wrong: total.wrong.saturating_sub(stats0.wrong),
            uncorrected: total.uncorrected.saturating_sub(stats0.uncorrected),
            typing_seconds: (total.seconds - stats0.seconds).max(0.0),
            bubbles: self.current.saturating_sub(*current0),
            finished: self.finished,
        };
        if let Err(e) = crate::history::append(&s) {
            eprintln!("couldn't log the session: {e}");
        }
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.save_progress();
        self.log_session();
    }
}
