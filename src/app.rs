//! The app window: the library (series list, chapter list, phone sync, settings) and the
//! reader, one at a time.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use eframe::egui::{self, Color32, RichText};

use crate::library::{Library, LocalChapter, Mark, format_duration, words_label};
use crate::pages::Chapter;
use crate::phone::{self, Series};
use crate::reader::{self, Reader};
use crate::settings::{ScrollMode, ScrollStep, Settings, SyncMode, default_library};
use crate::sync::{Cmd, Ev, Worker};
use crate::typing::{CaseMode, ErrorMode, Strictness};

const DIM: Color32 = Color32::from_gray(140);
const GOOD: Color32 = Color32::from_rgb(120, 215, 140);
const BAD: Color32 = Color32::from_rgb(235, 90, 90);
const ACCENT: Color32 = Color32::from_rgb(255, 170, 40);

struct PhoneInfo {
    series: Vec<Series>,
    backup: Option<String>,
}

struct CopyState {
    label: String,
    index: usize,
    count: usize,
    done: u64,
    total: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Start {
    All,
    From,
    MihonUnread,
}

struct AddDialog {
    filter: String,
    picked: Option<Series>,
    chapters: Option<Vec<(phone::Chapter, Mark)>>,
    first_unread: Option<usize>,
    start: Start,
    from: usize,
}

pub struct App {
    settings: Settings,
    lib: Library,
    /// Selected series, by (source, title) so it survives reloads.
    selected: Option<(String, String)>,
    filter: String,

    worker: Worker,
    phone: Option<PhoneInfo>,
    error: Option<(String, bool)>,
    status: Option<String>,
    copy: Option<CopyState>,

    add: Option<AddDialog>,
    settings_draft: Option<(Settings, String)>,
    reader: Option<Reader>,
    stats: Option<crate::stats_page::StatsPage>,
    title: String,
}

impl App {
    fn new(ctx: &egui::Context, open: Option<PathBuf>, stats: bool) -> Self {
        let settings = Settings::load();
        let lib = Library::load(&settings.library);
        let selected = lib.series.first().map(|s| (s.source.clone(), s.title.clone()));
        let mut app = Self {
            settings,
            lib,
            selected,
            filter: String::new(),
            worker: Worker::start(ctx.clone()),
            phone: None,
            error: None,
            status: None,
            copy: None,
            add: None,
            settings_draft: None,
            reader: None,
            stats: None,
            title: String::new(),
        };
        if let Some(p) = open {
            app.open(ctx, &p);
        }
        if stats {
            app.stats = Some(crate::stats_page::StatsPage::new(&app.lib));
        }
        app
    }

    fn reload(&mut self) {
        self.lib = Library::load(&self.settings.library);
        if self.selected.as_ref().is_none_or(|(src, t)| !self.lib.series.iter().any(|s| &s.source == src && &s.title == t)) {
            self.selected = self.lib.series.first().map(|s| (s.source.clone(), s.title.clone()));
        }
    }

    fn selected_index(&self) -> Option<usize> {
        let (src, t) = self.selected.as_ref()?;
        self.lib.series.iter().position(|s| &s.source == src && &s.title == t)
    }

    fn open(&mut self, ctx: &egui::Context, path: &Path) {
        match Chapter::open(path) {
            Ok(ch) => {
                let next = self.lib.next_after(path);
                self.reader = Some(Reader::new(Arc::new(ch), ctx.clone(), self.settings.clone(), next));
                // Select its series so Back lands there.
                if let Some(s) = self.lib.series.iter().find(|s| s.chapters.iter().any(|c| c.path == path)) {
                    self.selected = Some((s.source.clone(), s.title.clone()));
                }
            }
            Err(e) => self.error = Some((format!("Couldn't open {}: {e}", path.display()), false)),
        }
    }

    fn library_path(&self) -> PathBuf {
        self.settings.library.clone()
    }

    fn connect(&mut self) {
        self.error = None;
        self.worker.send(Cmd::Connect { library: self.library_path() });
    }

    fn sync(&mut self, series: Vec<Series>) {
        self.error = None;
        self.status = None;
        self.worker.send(Cmd::Sync { library: self.library_path(), series, mode: self.settings.sync_mode });
    }

    fn poll(&mut self) {
        for ev in self.worker.poll() {
            match ev {
                Ev::Connected { series, backup, .. } => {
                    self.phone = Some(PhoneInfo { series, backup });
                    self.reload();
                }
                Ev::Chapters { series, chapters, first_unread } => {
                    if let Some(d) = &mut self.add
                        && d.picked.as_ref().is_some_and(|p| p.source == series.source && p.title == series.title)
                    {
                        d.start = if first_unread.is_some() { Start::MihonUnread } else { Start::All };
                        d.from = first_unread.unwrap_or(0);
                        d.first_unread = first_unread;
                        d.chapters = Some(chapters);
                    }
                }
                Ev::Copy { label, index, count, done, total } => {
                    self.copy = Some(CopyState { label, index, count, done, total });
                }
                Ev::Copied => self.reload(),
                Ev::Finished(msg) => {
                    self.copy = None;
                    self.status = Some(msg);
                    self.reload();
                }
                Ev::Error { message, adb_busy } => {
                    self.copy = None;
                    self.error = Some((message, adb_busy));
                    if self.add.as_ref().is_some_and(|d| d.picked.is_some() && d.chapters.is_none()) {
                        // Let the user pick again.
                        if let Some(d) = &mut self.add {
                            d.picked = None;
                        }
                    }
                }
            }
        }
    }

    // -- library screen -------------------------------------------------------------------

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.heading(RichText::new("manga-type").strong());
            ui.separator();
            match &self.phone {
                Some(p) => {
                    ui.label(RichText::new("● Phone connected").color(GOOD));
                    if let Some(b) = &p.backup {
                        ui.label(RichText::new(format!("Mihon backup: {}", backup_label(b))).color(DIM));
                    } else {
                        ui.label(RichText::new("No Mihon backup found").color(DIM));
                    }
                }
                None => {
                    ui.label(RichText::new("○ Phone not connected").color(DIM));
                }
            }
            let busy = self.worker.busy();
            if ui.add_enabled(!busy, egui::Button::new(if self.phone.is_some() { "Reconnect" } else { "Connect phone" })).clicked() {
                self.connect();
            }
            if busy {
                ui.spinner();
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Settings").clicked() {
                    self.settings_draft = Some((self.settings.clone(), self.settings.library.display().to_string()));
                }
                if ui.button("Stats").clicked() {
                    self.reload();
                    self.stats = Some(crate::stats_page::StatsPage::new(&self.lib));
                }
                let can_sync = !busy && !self.lib.series.is_empty();
                if ui
                    .add_enabled(can_sync, egui::Button::new("Sync all"))
                    .on_hover_text(format!("Copy {} for every series", sync_mode_short(self.settings.sync_mode)))
                    .clicked()
                {
                    let all = self.lib.series.iter().map(|s| s.phone_series()).collect();
                    self.sync(all);
                }
                if ui.add_enabled(!busy, egui::Button::new("+ Add series")).clicked() {
                    self.add = Some(AddDialog {
                        filter: String::new(),
                        picked: None,
                        chapters: None,
                        first_unread: None,
                        start: Start::All,
                        from: 0,
                    });
                    if self.phone.is_none() {
                        self.connect();
                    }
                }
            });
        });
        if let Some((msg, adb_busy)) = self.error.clone() {
            ui.horizontal(|ui| {
                ui.label(RichText::new(msg).color(BAD));
                if adb_busy && ui.button("Stop adb server").clicked() {
                    self.error = None;
                    self.worker.send(Cmd::StopAdb { library: self.library_path() });
                }
                if ui.small_button("✕").clicked() {
                    self.error = None;
                }
            });
        }
        if let Some(s) = &self.status {
            ui.label(RichText::new(s).color(DIM));
        }
        ui.add_space(4.0);
    }

    fn copy_bar(&mut self, ui: &mut egui::Ui) {
        let Some(c) = &self.copy else { return };
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let frac = if c.total > 0 { c.done as f32 / c.total as f32 } else { 0.0 };
            let overall = (c.index as f32 + frac) / c.count.max(1) as f32;
            let text = format!(
                "Copying {} of {}: {}   {:.1} / {:.1} MB",
                c.index + 1,
                c.count,
                c.label,
                c.done as f64 / 1e6,
                c.total as f64 / 1e6
            );
            ui.add(egui::ProgressBar::new(overall).text(text).desired_width(ui.available_width() - 80.0));
            if ui.button("Cancel").clicked() {
                self.worker.cancel();
            }
        });
        ui.add_space(6.0);
    }

    fn series_list(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Filter series").desired_width(f32::INFINITY));
        ui.add_space(4.0);
        let filter = self.filter.to_lowercase();
        let mut clicked = None;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for s in &self.lib.series {
                if !filter.is_empty() && !s.title.to_lowercase().contains(&filter) {
                    continue;
                }
                let sel = self.selected.as_ref().is_some_and(|(src, t)| src == &s.source && t == &s.title);
                let text = format!("{}\n", s.title);
                let mut job = egui::text::LayoutJob::default();
                job.append(&text, 0.0, egui::TextFormat::simple(egui::FontId::proportional(15.0), ui.visuals().strong_text_color()));
                job.append(
                    &{
                        let t = s.reading_seconds();
                        let time = if t >= 1.0 { format!(" · {}", format_duration(t)) } else { String::new() };
                        let (w, approx) = s.words();
                        let words = if w > 0 { format!(" · {}", words_label(w, approx)) } else { String::new() };
                        format!("{} · {} chapters · {} finished{time}{words}", s.source, s.chapters.len(), s.finished_count())
                    },
                    0.0,
                    egui::TextFormat::simple(egui::FontId::proportional(12.0), DIM),
                );
                let r = ui.add(egui::Button::selectable(sel, job).min_size(egui::vec2(ui.available_width(), 0.0)));
                if r.clicked() {
                    clicked = Some((s.source.clone(), s.title.clone()));
                }
                ui.add_space(2.0);
            }
        });
        if clicked.is_some() {
            self.selected = clicked;
        }
    }

    fn series_view(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(i) = self.selected_index() else {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.heading("Your library is empty");
                ui.add_space(8.0);
                ui.label("Plug in your phone (USB debugging on) and click \"+ Add series\" to copy chapters from Mihon.");
                ui.add_space(4.0);
                ui.label(RichText::new(format!("Library folder: {}", self.settings.library.display())).color(DIM));
            });
            return;
        };
        let s = self.lib.series[i].clone();
        let mut open = None;
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.heading(RichText::new(&s.title).strong());
                let t = s.reading_seconds();
                let spent = if t >= 1.0 { format!(" · {} spent", format_duration(t)) } else { String::new() };
                let (w, approx) = s.words();
                let spent = if w > 0 { format!("{spent} · {} typed", words_label(w, approx)) } else { spent };
                ui.label(RichText::new(format!("{}{spent}", s.source)).color(DIM));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let busy = self.worker.busy();
                if ui
                    .add_enabled(!busy, egui::Button::new("Sync"))
                    .on_hover_text(format!("Copy {} from the phone", sync_mode_short(self.settings.sync_mode)))
                    .clicked()
                {
                    self.sync(vec![s.phone_series()]);
                }
                if let Some(ci) = s.continue_index() {
                    let c = &s.chapters[ci];
                    let verb = if c.started() { "Continue" } else { "Read" };
                    if ui
                        .add(egui::Button::new(RichText::new(format!("▶ {verb}: {}", c.display_name)).size(16.0)).fill(Color32::from_rgb(70, 110, 60)))
                        .clicked()
                    {
                        open = Some(c.path.clone());
                    }
                }
            });
        });
        ui.add_space(6.0);
        ui.separator();

        if s.chapters.is_empty() {
            ui.label(RichText::new("No chapters on the PC yet. Click Sync with the phone connected.").color(DIM));
        }
        let row_h = 26.0;
        egui::ScrollArea::vertical().auto_shrink(false).show_rows(ui, row_h, s.chapters.len(), |ui, range| {
            for c in &s.chapters[range] {
                ui.horizontal(|ui| {
                    ui.set_min_height(row_h);
                    let (sym, color, tip) = mark_style(c.mark);
                    ui.label(RichText::new(sym).color(color).monospace()).on_hover_text(tip);
                    let name = RichText::new(&c.display_name).size(15.0);
                    ui.label(if c.finished() { name.color(DIM) } else { name });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let verb = if c.finished() { "Read again" } else if c.started() { "Continue" } else { "Read" };
                        if ui.button(verb).clicked() {
                            open = Some(c.path.clone());
                        }
                        ui.label(RichText::new(progress_text(c)).color(if c.finished() { GOOD } else { DIM }));
                    });
                });
            }
        });

        if let Some(p) = open {
            self.open(ctx, &p);
        }
    }

    // -- dialogs ----------------------------------------------------------------------------

    fn add_dialog(&mut self, ctx: &egui::Context) {
        let Some(d) = &mut self.add else { return };
        let mut close = false;
        let mut pick: Option<Series> = None;
        let mut confirm: Option<(Series, Option<String>)> = None;
        let busy = self.worker.busy();

        egui::Modal::new(egui::Id::unique("add-series")).show(ctx, |ui| {
            ui.set_width(560.0);
            ui.heading("Add a series from your phone");
            ui.add_space(6.0);
            match (&self.phone, &d.picked) {
                (None, _) => {
                    if busy {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Connecting to the phone… (accept \"Allow USB debugging?\" on it if asked)");
                        });
                    } else {
                        ui.label(RichText::new("Not connected. Check the cable and USB debugging, then retry.").color(BAD));
                        if ui.button("Retry").clicked() {
                            self.worker.send(Cmd::Connect { library: self.settings.library.clone() });
                        }
                    }
                }
                (Some(p), None) => {
                    ui.add(egui::TextEdit::singleline(&mut d.filter).hint_text("Search").desired_width(f32::INFINITY));
                    let f = d.filter.to_lowercase();
                    egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                        let mut last_source = "";
                        for s in &p.series {
                            if !f.is_empty() && !s.title.to_lowercase().contains(&f) {
                                continue;
                            }
                            if s.source != last_source {
                                ui.add_space(4.0);
                                ui.label(RichText::new(&s.source).color(DIM).small());
                                last_source = &s.source;
                            }
                            if self.lib.contains(s) {
                                ui.add_enabled(false, egui::Button::new(format!("{}  (in library)", s.title)).frame(false));
                            } else if ui.add(egui::Button::new(&s.title).frame(false)).clicked() {
                                pick = Some(s.clone());
                            }
                        }
                    });
                }
                (Some(_), Some(series)) => {
                    ui.label(RichText::new(&series.title).size(17.0).strong());
                    ui.label(RichText::new(&series.source).color(DIM));
                    ui.add_space(6.0);
                    match &d.chapters {
                        None => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("Reading chapters…");
                            });
                        }
                        Some(chs) if chs.is_empty() => {
                            ui.label("No downloaded chapters for this series on the phone.");
                        }
                        Some(chs) => {
                            let size = |from: usize| chs[from..].iter().map(|(c, _)| c.size).sum::<u64>() as f64 / 1e6;
                            ui.label("Which chapters should be copied?");
                            ui.radio_value(&mut d.start, Start::All, format!("All {} chapters ({:.0} MB)", chs.len(), size(0)));
                            ui.horizontal(|ui| {
                                ui.radio_value(&mut d.start, Start::From, "Starting from");
                                egui::ComboBox::from_id_salt("add-from")
                                    .width(300.0)
                                    .selected_text(&chs[d.from.min(chs.len() - 1)].0.display_name)
                                    .show_ui(ui, |ui| {
                                        for (i, (c, m)) in chs.iter().enumerate() {
                                            let (sym, _, _) = mark_style(*m);
                                            if ui.selectable_label(d.from == i, format!("{sym} {}", c.display_name)).clicked() {
                                                d.from = i;
                                                d.start = Start::From;
                                            }
                                        }
                                    });
                            });
                            let unread_text = match d.first_unread {
                                Some(i) => format!("From Mihon's first unread: {} ({} chapters)", chs[i].0.display_name, chs.len() - i),
                                None => "From Mihon's first unread (everything is read, or no backup)".into(),
                            };
                            ui.add_enabled_ui(d.first_unread.is_some(), |ui| {
                                ui.radio_value(&mut d.start, Start::MihonUnread, unread_text);
                            });

                            let from = match d.start {
                                Start::All => 0,
                                Start::From => d.from,
                                Start::MihonUnread => d.first_unread.unwrap_or(0),
                            };
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                let label = format!("Add and copy {} chapters ({:.0} MB)", chs.len() - from, size(from));
                                if ui.add_enabled(!busy, egui::Button::new(RichText::new(label).strong()).fill(Color32::from_rgb(70, 110, 60))).clicked() {
                                    let start_from = (from > 0).then(|| chs[from].0.file_name.clone());
                                    confirm = Some((series.clone(), start_from));
                                }
                                ui.label(RichText::new("Later syncs never copy chapters before this one.").color(DIM).small());
                            });
                        }
                    }
                }
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if d.picked.is_some() && ui.button("← Back").clicked() {
                    d.picked = None;
                    d.chapters = None;
                }
                if ui.button("Close").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close = true;
                }
            });
        });

        if let Some(s) = pick {
            d.picked = Some(s.clone());
            d.chapters = None;
            self.worker.send(Cmd::Chapters(s));
        }
        if let Some((series, start_from)) = confirm {
            self.selected = Some((series.source.clone(), series.title.clone()));
            self.status = None;
            self.worker.send(Cmd::Add { library: self.settings.library.clone(), series, start_from });
            close = true;
        }
        if close {
            self.add = None;
        }
    }

    fn settings_dialog(&mut self, ctx: &egui::Context) {
        let Some((draft, lib_text)) = &mut self.settings_draft else { return };
        let (mut save, mut cancel) = (false, false);
        egui::Modal::new(egui::Id::unique("settings")).show(ctx, |ui| {
            ui.set_width(520.0);
            ui.heading("Settings");
            ui.add_space(8.0);

            ui.label(RichText::new("Typing").strong());
            ui.horizontal(|ui| {
                ui.label("Case:");
                egui::ComboBox::from_id_salt("settings-case").selected_text(draft.case.label()).show_ui(ui, |ui| {
                    for m in CaseMode::ALL {
                        ui.selectable_value(&mut draft.case, m, m.label());
                    }
                });
            });
            ui.horizontal(|ui| {
                ui.label("Punctuation:");
                ui.radio_value(&mut draft.strictness, Strictness::Strict, "Type spaces and punctuation");
                ui.radio_value(&mut draft.strictness, Strictness::Lenient, "Letters and digits only");
            });
            ui.horizontal(|ui| {
                ui.label("Wrong letters:");
                egui::ComboBox::from_id_salt("settings-errors").selected_text(draft.errors.label()).show_ui(ui, |ui| {
                    for m in ErrorMode::ALL {
                        ui.selectable_value(&mut draft.errors, m, m.label());
                    }
                });
            });

            ui.add_space(10.0);
            ui.label(RichText::new("Scrolling").strong());
            for m in ScrollMode::ALL {
                ui.radio_value(&mut draft.scroll, m, m.label());
            }
            ui.add_enabled_ui(draft.scroll == ScrollMode::Manual, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Space scrolls:");
                    for st in ScrollStep::ALL {
                        ui.radio_value(&mut draft.space_step, st, st.label());
                    }
                });
                ui.label(
                    RichText::new("j / k / Space scroll while the bubble you're on is off screen; once it's in view they type. Arrows, Page Up/Down and the mouse wheel always scroll.")
                        .color(DIM)
                        .small(),
                );
            });

            ui.add_space(10.0);
            ui.label(RichText::new("Sync copies").strong());
            for m in SyncMode::ALL {
                ui.radio_value(&mut draft.sync_mode, m, m.label());
            }

            ui.add_space(10.0);
            ui.label(RichText::new("Library folder").strong());
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(lib_text).desired_width(380.0));
                if ui.button("Default").clicked() {
                    *lib_text = default_library().display().to_string();
                }
            });

            ui.add_space(10.0);
            ui.label(RichText::new("OCR").strong());
            ui.horizontal(|ui| {
                let max = std::thread::available_parallelism().map_or(8, |n| n.get());
                ui.add(egui::Slider::new(&mut draft.workers, 0..=max).text("worker threads"));
                if draft.workers == 0 {
                    ui.label(RichText::new(format!("auto ({})", crate::ocr::default_workers())).color(DIM));
                }
            });

            ui.add_space(12.0);
            ui.horizontal(|ui| {
                save = ui.button("Save").clicked();
                cancel = ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape));
            });
        });
        if save {
            let mut new = draft.clone();
            let lib = expand_home(lib_text.trim());
            if !lib.as_os_str().is_empty() {
                new.library = lib;
            }
            let lib_changed = new.library != self.settings.library;
            self.settings = new;
            if let Err(e) = self.settings.save() {
                self.error = Some((format!("Couldn't save settings: {e}"), false));
            }
            if lib_changed {
                self.selected = None;
                self.reload();
            }
            self.settings_draft = None;
        } else if cancel {
            self.settings_draft = None;
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll();

        // Reading screen.
        if let Some(r) = &mut self.reader {
            let title = format!("manga-type — {}", phone::display_name(&r.chapter_path().file_name().unwrap_or_default().to_string_lossy()));
            set_title(&ctx, &mut self.title, title);
            r.paused = self.settings_draft.is_some();
            let action = r.show(ui);
            if r.settings != self.settings && self.settings_draft.is_none() {
                self.settings = r.settings.clone();
            }
            self.settings_dialog(&ctx);
            if let Some(r) = &mut self.reader
                && r.settings != self.settings
                && self.settings_draft.is_none()
            {
                r.apply_settings(self.settings.clone());
            }
            match action {
                Some(reader::Action::Settings) => {
                    self.settings_draft = Some((self.settings.clone(), self.settings.library.display().to_string()));
                }
                Some(reader::Action::Back) => {
                    self.reader = None; // saves on drop
                    self.reload();
                }
                Some(reader::Action::Open(p)) => {
                    self.reader = None;
                    self.reload();
                    self.open(&ctx, &p);
                }
                None => {}
            }
            return;
        }

        if let Some(page) = &self.stats {
            set_title(&ctx, &mut self.title, "manga-type — stats".into());
            let mut back = false;
            egui::CentralPanel::default().show(ui, |ui| back = page.show(ui));
            if back {
                self.stats = None;
            }
            return;
        }

        set_title(&ctx, &mut self.title, "manga-type".into());
        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui));
        if self.copy.is_some() {
            egui::Panel::bottom("copy").show(ui, |ui| self.copy_bar(ui));
        }
        egui::Panel::left("series").resizable(true).default_size(320.0).min_size(220.0).show(ui, |ui| self.series_list(ui));
        egui::CentralPanel::default().show(ui, |ui| self.series_view(ui, &ctx));
        self.add_dialog(&ctx);
        self.settings_dialog(&ctx);
    }
}

fn set_title(ctx: &egui::Context, current: &mut String, title: String) {
    if *current != title {
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
        *current = title;
    }
}

fn expand_home(s: &str) -> PathBuf {
    match (s.strip_prefix("~/"), std::env::home_dir()) {
        (Some(rest), Some(h)) => h.join(rest),
        _ if s == "~" => std::env::home_dir().unwrap_or_default(),
        _ => PathBuf::from(s),
    }
}

/// `app.mihon_2026-11-23_12-00.tachibk` -> `2026-11-23 12:00`.
fn backup_label(name: &str) -> String {
    let stem = name.strip_suffix(".tachibk").unwrap_or(name);
    let parts: Vec<&str> = stem.split('_').collect();
    match parts.as_slice() {
        [.., date, time] if date.len() == 10 && time.len() == 5 => format!("{date} {}", time.replace('-', ":")),
        _ => name.to_string(),
    }
}

fn sync_mode_short(m: SyncMode) -> &'static str {
    match m {
        SyncMode::MihonUnread => "chapters Mihon hasn't marked read",
        SyncMode::AllMissing => "every chapter missing on the PC",
        SyncMode::AfterLastFinished => "chapters after the last one finished here",
    }
}

fn mark_style(m: Mark) -> (&'static str, Color32, &'static str) {
    match m {
        Mark::Read => ("✔", Color32::from_gray(120), "Read in Mihon"),
        Mark::Partly => ("◐", ACCENT, "Started in Mihon"),
        Mark::Unread => ("●", Color32::from_rgb(110, 170, 255), "Unread in Mihon"),
        Mark::Unknown => ("?", Color32::from_gray(90), "Not in Mihon's backup (newer than it)"),
    }
}

fn progress_text(c: &LocalChapter) -> String {
    let time = |p: &crate::library::ChapterProgress| {
        let mut s = if p.reading_seconds >= 1.0 { format!(" · {}", format_duration(p.reading_seconds)) } else { String::new() };
        let (w, approx) = p.stats.word_count();
        if w > 0 {
            s.push_str(&format!(" · {}", words_label(w, approx)));
        }
        s
    };
    match &c.progress {
        Some(p) if p.finished => format!("finished · {:.0} WPM · {:.0}%{}", p.stats.wpm(), p.stats.accuracy(), time(p)),
        Some(p) if p.current > 0 || p.stats.correct > 0 => {
            if p.bubbles > 0 {
                format!("bubble {} / {}{}", p.current + 1, p.bubbles, time(p))
            } else {
                format!("bubble {}{}", p.current + 1, time(p))
            }
        }
        Some(p) if p.reading_seconds >= 1.0 => format_duration(p.reading_seconds),
        _ => String::new(),
    }
}

/// Open the app window; with `open`, go straight to that chapter; with `stats`, to the
/// stats page.
pub fn run(open: Option<PathBuf>, stats: bool) -> Result<(), String> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("manga-type")
            .with_app_id("manga-type")
            .with_inner_size([1100.0, 1100.0]),
        ..Default::default()
    };
    eframe::run_native(
        "manga-type",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            // Ctrl +/- zooms the manga page in the reader, not the whole interface.
            cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
            Ok(Box::new(App::new(&cc.egui_ctx, open, stats)))
        }),
    )
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(backup_label("app.mihon_2026-11-23_12-00.tachibk"), "2026-11-23 12:00");
        assert_eq!(backup_label("weird.tachibk"), "weird.tachibk");
        assert_eq!(expand_home("/x/y"), PathBuf::from("/x/y"));
    }
}
