//! The stats page: totals, activity per day, WPM over finished chapters, per series, and
//! per-key miss rates and typos. Built from the chapters' progress files (totals, keys)
//! and the session log (`history.rs`, per day).

use eframe::egui::{self, Color32, RichText, Stroke};

use crate::history::{self, Session};
use crate::library::{Library, format_duration};
use crate::typing::{KeyCount, Stats};

const DIM: Color32 = Color32::from_gray(140);
const BAR: Color32 = Color32::from_rgb(110, 170, 255);
const GOOD: Color32 = Color32::from_rgb(120, 215, 140);
const BAD: Color32 = Color32::from_rgb(235, 90, 90);
const ACCENT: Color32 = Color32::from_rgb(255, 170, 40);

struct SeriesRow {
    title: String,
    chapters: usize,
    finished: usize,
    reading: f64,
    stats: Stats,
}

pub struct StatsPage {
    total: Stats,
    reading: f64,
    finished: usize,
    started: usize,
    typed_bubbles: usize,
    skipped_bubbles: usize,
    series: Vec<SeriesRow>,
    /// (label, minutes, wpm) for the last 14 days, oldest first.
    days: Vec<(String, f32, f32)>,
    /// (chapter label, wpm, accuracy) of finished chapters, in the order they were finished.
    trend: Vec<(String, f32, f32)>,
    /// (key, hits, misses), worst miss rate first (keys with >= 10 presses).
    worst_keys: Vec<(char, u32, u32)>,
    /// (group, hits, misses).
    groups: Vec<(&'static str, u32, u32)>,
    typos: Vec<(String, u32)>,
    has_key_data: bool,
}

fn rate(hit: u32, miss: u32) -> f32 {
    if hit + miss == 0 { 0.0 } else { miss as f32 / (hit + miss) as f32 }
}

impl StatsPage {
    pub fn new(lib: &Library) -> Self {
        let mut total = Stats::default();
        let (mut reading, mut finished, mut started, mut typed_bubbles, mut skipped_bubbles) = (0.0, 0, 0, 0, 0);
        let mut series = Vec::new();
        let mut done_chapters: Vec<(u64, String, f32, f32)> = Vec::new();
        for s in &lib.series {
            let mut row = SeriesRow { title: s.title.clone(), chapters: s.chapters.len(), finished: 0, reading: 0.0, stats: Stats::default() };
            for c in &s.chapters {
                let Some(p) = &c.progress else { continue };
                started += 1;
                row.reading += p.reading_seconds;
                row.stats.merge(&p.stats);
                typed_bubbles += p.states.chars().filter(|&c| c == 'd').count();
                skipped_bubbles += p.states.chars().filter(|&c| c == 's').count();
                if p.finished {
                    row.finished += 1;
                    done_chapters.push((p.updated, format!("{} {}", short_title(&s.title), chapter_no(&c.display_name)), p.stats.wpm() as f32, p.stats.accuracy() as f32));
                }
            }
            reading += row.reading;
            finished += row.finished;
            total.merge(&row.stats);
            series.push(row);
        }
        series.sort_by(|a, b| b.reading.total_cmp(&a.reading));
        done_chapters.sort_by_key(|c| c.0);
        let trend = done_chapters.into_iter().map(|(_, l, w, a)| (l, w, a)).collect();

        // Activity per day from the session log.
        let sessions: Vec<Session> = history::load();
        let offset = history::utc_offset();
        let today = history::local_day(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()), offset);
        let days = (today - 13..=today)
            .map(|d| {
                let of_day: Vec<&Session> = sessions.iter().filter(|s| history::local_day(s.ended, offset) == d).collect();
                let minutes = of_day.iter().map(|s| s.seconds).sum::<f64>() as f32 / 60.0;
                let (correct, unc, secs) = of_day.iter().fold((0u64, 0u64, 0.0f64), |a, s| (a.0 + s.correct, a.1 + s.uncorrected, a.2 + s.typing_seconds));
                let wpm = if secs < 1.0 { 0.0 } else { ((correct as f64 / 5.0 - unc as f64) / (secs / 60.0)).max(0.0) as f32 };
                (history::day_label(d), minutes, wpm)
            })
            .collect();

        // Keys.
        let mut worst_keys: Vec<(char, u32, u32)> =
            total.keys.iter().filter(|(_, k)| k.hit + k.miss >= 10).map(|(c, k)| (*c, k.hit, k.miss)).collect();
        worst_keys.sort_by(|a, b| rate(b.1, b.2).total_cmp(&rate(a.1, a.2)));
        worst_keys.truncate(15);
        let group = |f: &dyn Fn(char) -> bool| {
            total.keys.iter().filter(|(c, _)| f(**c)).fold(KeyCount::default(), |a, (_, k)| KeyCount { hit: a.hit + k.hit, miss: a.miss + k.miss })
        };
        let groups: Vec<(&'static str, u32, u32)> = [
            ("Capital letters", group(&|c: char| c.is_uppercase())),
            ("Small letters", group(&|c: char| c.is_lowercase())),
            ("Digits", group(&|c: char| c.is_ascii_digit())),
            ("Space", group(&|c: char| c == ' ')),
            ("Punctuation", group(&|c: char| c.is_ascii_punctuation())),
        ]
        .into_iter()
        .filter(|(_, k)| k.hit + k.miss > 0)
        .map(|(n, k)| (n, k.hit, k.miss))
        .collect();
        let mut typos: Vec<(String, u32)> = total.typos.iter().map(|(t, n)| (t.clone(), *n)).collect();
        typos.sort_by_key(|t| std::cmp::Reverse(t.1));
        typos.truncate(15);
        let has_key_data = !total.keys.is_empty();

        Self { total, reading, finished, started, typed_bubbles, skipped_bubbles, series, days, trend, worst_keys, groups, typos, has_key_data }
    }

    /// Draw the page. Returns true when the user wants to go back.
    pub fn show(&self, ui: &mut egui::Ui) -> bool {
        let mut back = false;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                back = ui.button("← Library").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape));
                ui.heading(RichText::new("Stats").strong());
            });
            ui.add_space(10.0);

            // Tiles.
            let t = &self.total;
            let tiles = [
                ("Time spent", format_duration(self.reading)),
                ("Typing time", format_duration(t.seconds)),
                ("Words typed", format!("{:.0}", t.correct as f64 / 5.0)),
                ("Average WPM", format!("{:.0}", t.wpm())),
                ("Accuracy", format!("{:.1}%", t.accuracy())),
                ("Chapters finished", format!("{} / {}", self.finished, self.started)),
                ("Bubbles typed", self.typed_bubbles.to_string()),
                ("Bubbles skipped", self.skipped_bubbles.to_string()),
            ];
            // As many tiles per row as fit.
            let per_row = ((ui.available_width() / 165.0) as usize).clamp(1, tiles.len());
            for row in tiles.chunks(per_row) {
                ui.horizontal(|ui| {
                    for (label, value) in row {
                        egui::Frame::group(ui.style()).inner_margin(10.0).show(ui, |ui| {
                            ui.set_width(135.0);
                            ui.vertical(|ui| {
                                ui.label(RichText::new(value).size(22.0).strong());
                                ui.label(RichText::new(*label).color(DIM));
                            });
                        });
                    }
                });
            }

            section(ui, "Time spent per day (last 14 days)");
            if self.days.iter().all(|d| d.1 < 0.05) {
                ui.label(RichText::new("No sessions logged yet: they're recorded from now on, when you leave a chapter.").color(DIM));
            } else {
                let items: Vec<(String, f32, String)> = self
                    .days
                    .iter()
                    .map(|(l, m, w)| (l.clone(), *m, if *m < 0.05 { String::new() } else if *w > 0.0 { format!("{:.0}m\n{:.0} wpm", m, w) } else { format!("{:.0}m", m) }))
                    .collect();
                columns(ui, &items, 120.0, BAR);
            }

            section(ui, "WPM per finished chapter");
            if self.trend.is_empty() {
                ui.label(RichText::new("Finish a chapter to see this.").color(DIM));
            } else {
                let items: Vec<(String, f32, String)> =
                    self.trend.iter().map(|(l, w, a)| (l.clone(), *w, format!("{w:.0}\n{a:.0}%"))).collect();
                columns(ui, &items, 120.0, GOOD);
            }

            section(ui, "Per series");
            egui::Grid::new("per-series").striped(true).spacing([18.0, 6.0]).show(ui, |ui| {
                for h in ["Series", "Finished", "Time spent", "WPM", "Accuracy", "Words"] {
                    ui.label(RichText::new(h).color(DIM));
                }
                ui.end_row();
                for s in &self.series {
                    ui.label(&s.title);
                    ui.label(format!("{} / {}", s.finished, s.chapters));
                    ui.label(format_duration(s.reading));
                    ui.label(format!("{:.0}", s.stats.wpm()));
                    ui.label(format!("{:.1}%", s.stats.accuracy()));
                    ui.label(format!("{:.0}", s.stats.correct as f64 / 5.0));
                    ui.end_row();
                }
            });

            section(ui, "Misses by kind of key");
            if !self.has_key_data {
                ui.label(RichText::new("Per-key data is recorded from now on; earlier chapters only kept totals.").color(DIM));
            } else {
                for (name, hit, miss) in &self.groups {
                    hbar(ui, name, rate(*hit, *miss), &format!("{:.1}% of {}", 100.0 * rate(*hit, *miss), hit + miss), 0.25);
                }

                section(ui, "Keys you miss most (at least 10 presses)");
                for (c, hit, miss) in &self.worst_keys {
                    hbar(ui, &key_label(*c), rate(*hit, *miss), &format!("{:.1}% of {}", 100.0 * rate(*hit, *miss), hit + miss), 0.25);
                }

                section(ui, "Most common typos (wanted → pressed)");
                ui.horizontal_wrapped(|ui| {
                    for (t, n) in &self.typos {
                        let shown: String = t.chars().map(|c| if c == ' ' { '␣' } else { c }).collect();
                        egui::Frame::group(ui.style()).inner_margin(6.0).show(ui, |ui| {
                            ui.label(RichText::new(shown).monospace().size(16.0));
                            ui.label(RichText::new(format!("×{n}")).color(DIM));
                        });
                    }
                });
            }
            ui.add_space(20.0);
        });
        back
    }
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(16.0);
    ui.label(RichText::new(title).size(17.0).strong());
    ui.add_space(4.0);
}

fn key_label(c: char) -> String {
    match c {
        ' ' => "space".into(),
        c => format!("{c}"),
    }
}

/// "Star-Embracing Swordmaster" -> "SES".
fn short_title(t: &str) -> String {
    t.split(|c: char| c.is_whitespace() || c == '-').filter_map(|w| w.chars().next()).filter(|c| c.is_alphanumeric()).collect::<String>().to_uppercase()
}

/// "Chapter 12" -> "12".
fn chapter_no(name: &str) -> String {
    name.rsplit(' ').next().unwrap_or(name).to_string()
}

/// Vertical columns with a label under each and a value text above.
fn columns(ui: &mut egui::Ui, items: &[(String, f32, String)], height: f32, color: Color32) {
    let max = items.iter().map(|i| i.1).fold(0.0f32, f32::max).max(1e-3);
    let col_w = 54.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(col_w * items.len() as f32, height + 56.0), egui::Sense::hover());
    let p = ui.painter_at(rect);
    let base = rect.top() + height + 26.0;
    p.line_segment([egui::pos2(rect.left(), base), egui::pos2(rect.right(), base)], Stroke::new(1.0, Color32::from_gray(70)));
    for (i, (label, v, text)) in items.iter().enumerate() {
        let x = rect.left() + col_w * i as f32;
        let h = height * v / max;
        let bar = egui::Rect::from_min_max(egui::pos2(x + 10.0, base - h), egui::pos2(x + col_w - 10.0, base));
        p.rect_filled(bar, 3.0, color);
        p.text(egui::pos2(x + col_w / 2.0, base - h - 2.0), egui::Align2::CENTER_BOTTOM, text, egui::FontId::proportional(11.0), Color32::from_gray(200));
        p.text(egui::pos2(x + col_w / 2.0, base + 4.0), egui::Align2::CENTER_TOP, label, egui::FontId::proportional(11.0), DIM);
    }
}

/// One horizontal bar: label, bar (scaled so `full` fills it), value text. Red if high.
fn hbar(ui: &mut egui::Ui, label: &str, value: f32, text: &str, full: f32) {
    ui.horizontal(|ui| {
        ui.add_sized([120.0, 18.0], egui::Label::new(RichText::new(label).monospace()));
        let (rect, _) = ui.allocate_exact_size(egui::vec2(260.0, 14.0), egui::Sense::hover());
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 3.0, Color32::from_gray(45));
        let frac = (value / full).clamp(0.0, 1.0);
        let color = if value >= 0.15 { BAD } else if value >= 0.07 { ACCENT } else { GOOD };
        p.rect_filled(egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * frac, rect.height())), 3.0, color);
        ui.label(RichText::new(text).color(DIM));
    });
}
