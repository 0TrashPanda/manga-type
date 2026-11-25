//! Phone work on a background thread: connect, list, copy. The UI sends [`Cmd`]s and
//! polls [`Ev`]s. The thread keeps the USB connection open between commands and
//! reconnects once if it dropped. Nothing is ever written to the phone.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::library::{self, Library, Mark, SeriesMeta};
use crate::mihon::{self, Backup};
use crate::phone::{self, Phone, PhoneError, Series};
use crate::settings::SyncMode;

pub enum Cmd {
    /// Connect (if needed), list the phone's series, save Mihon's newest backup.
    Connect { library: PathBuf },
    /// List one series' chapters with Mihon markers (for the add-series popup).
    Chapters(Series),
    /// Add a series: remember its start chapter and copy everything from there.
    Add { library: PathBuf, series: Series, start_from: Option<String> },
    /// Copy what the sync mode says for each series.
    Sync { library: PathBuf, series: Vec<Series>, mode: SyncMode },
    /// Ask a running adb server to quit, then reconnect.
    StopAdb { library: PathBuf },
}

pub enum Ev {
    Connected { series: Vec<Series>, backup: Option<String> },
    Chapters { series: Series, chapters: Vec<(phone::Chapter, Mark)>, first_unread: Option<usize> },
    /// Copy progress for the current chapter.
    Copy { label: String, index: usize, count: usize, done: u64, total: u64 },
    /// A chapter landed in the library (reload it).
    Copied,
    /// A sync/add finished; summary for the status line.
    Finished(String),
    Error { message: String, adb_busy: bool },
}

pub struct Worker {
    tx: mpsc::Sender<Cmd>,
    rx: mpsc::Receiver<Option<Ev>>,
    cancel: Arc<AtomicBool>,
    /// Commands sent but not finished yet.
    pending: usize,
}

impl Worker {
    pub fn start(ctx: egui::Context) -> Self {
        let (tx, cmd_rx) = mpsc::channel::<Cmd>();
        let (ev_tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let c = cancel.clone();
        thread::spawn(move || {
            let mut w = Inner { phone: None, backup: None, ev: ev_tx, ctx, cancel: c };
            for cmd in cmd_rx {
                w.cancel.store(false, Ordering::Relaxed);
                w.run(cmd);
                // A `None` marks "command done" for the pending counter.
                w.send(None);
            }
        });
        Self { tx, rx, cancel, pending: 0 }
    }

    pub fn send(&mut self, cmd: Cmd) {
        if self.tx.send(cmd).is_ok() {
            self.pending += 1;
        }
    }

    pub fn busy(&self) -> bool {
        self.pending > 0
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Events since the last poll.
    pub fn poll(&mut self) -> Vec<Ev> {
        let mut out = Vec::new();
        // The channel carries `Option<Ev>`; see `Inner::send`.
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Some(ev) => out.push(ev),
                None => self.pending = self.pending.saturating_sub(1),
            }
        }
        out
    }
}

// The event channel carries Option<Ev>: None = a command finished.
type EvTx = mpsc::Sender<Option<Ev>>;

struct Inner {
    phone: Option<Phone>,
    backup: Option<Backup>,
    ev: EvTx,
    ctx: egui::Context,
    cancel: Arc<AtomicBool>,
}

impl Inner {
    fn send(&self, ev: Option<Ev>) {
        let _ = self.ev.send(ev);
        self.ctx.request_repaint();
    }

    fn error(&self, e: &PhoneError) {
        self.send(Some(Ev::Error { message: e.to_string(), adb_busy: matches!(e, PhoneError::AdbServerBusy) }));
    }

    fn run(&mut self, cmd: Cmd) {
        let result = match cmd {
            Cmd::Connect { library } => self.with_retry(|w| w.connect(&library)),
            Cmd::Chapters(series) => self.with_retry(|w| w.chapters(&series)),
            Cmd::Add { library, series, start_from } => {
                // Adding again (also a removed series) starts fresh: shown, nothing removed.
                let meta = SeriesMeta { start_from: start_from.clone(), ..Default::default() };
                let dir = library.join(&series.source).join(&series.title);
                if let Err(e) = meta.save(&dir) {
                    self.error(&PhoneError::Other(format!("couldn't create {}: {e}", dir.display())));
                    return;
                }
                self.send(Some(Ev::Copied));
                self.with_retry(|w| w.sync(&library, std::slice::from_ref(&series), None))
            }
            Cmd::Sync { library, series, mode } => self.with_retry(|w| w.sync(&library, &series, Some(mode))),
            Cmd::StopAdb { library } => match phone::stop_adb_server() {
                Ok(()) => self.with_retry(|w| w.connect(&library)),
                Err(e) => Err(e),
            },
        };
        if let Err(e) = result {
            self.error(&e);
        }
    }

    /// Run `f` with a connection; if the connection had gone stale, reconnect and retry once.
    fn with_retry(&mut self, f: impl Fn(&mut Self) -> Result<(), PhoneError>) -> Result<(), PhoneError> {
        let had_phone = self.phone.is_some();
        match f(self) {
            Err(PhoneError::Disconnected | PhoneError::NoDevice) if had_phone => {
                self.phone = None;
                f(self)
            }
            Err(e @ (PhoneError::Disconnected | PhoneError::NoDevice)) => {
                self.phone = None;
                Err(e)
            }
            other => other,
        }
    }

    fn phone(&mut self) -> Result<&Phone, PhoneError> {
        if self.phone.is_none() {
            let key = phone::default_key_path().ok_or(PhoneError::Other("no home directory".into()))?;
            self.phone = Some(Phone::connect(&key)?);
        }
        Ok(self.phone.as_ref().expect("just connected"))
    }

    fn connect(&mut self, library: &std::path::Path) -> Result<(), PhoneError> {
        let p = self.phone()?;
        let series = p.list_series()?;
        let backup = p.read_newest_backup()?;
        let mut backup_name = None;
        if let Some((name, _, bytes)) = backup {
            match mihon::parse_backup(&bytes) {
                Ok(b) => {
                    self.backup = Some(b);
                    backup_name = Some(name);
                    // Keep a copy so markers work without the phone.
                    let _ = std::fs::create_dir_all(library);
                    let tmp = library.join(".mihon-backup.tmp");
                    if std::fs::write(&tmp, &bytes).is_ok() {
                        let _ = std::fs::rename(&tmp, library.join(library::BACKUP_FILE));
                    }
                }
                Err(e) => eprintln!("couldn't read Mihon backup {name}: {e}"),
            }
        }
        self.send(Some(Ev::Connected { series, backup: backup_name }));
        Ok(())
    }

    fn backup_series(&self, s: &Series) -> Option<&mihon::BackupSeries> {
        self.backup.as_ref().and_then(|b| b.find_series(&s.source, &s.title))
    }

    /// Phone chapters of a series in reading order.
    fn ordered_chapters(&mut self, s: &Series) -> Result<Vec<phone::Chapter>, PhoneError> {
        let mut chapters = self.phone()?.list_chapters(s)?;
        let bs = self.backup_series(s);
        library::reading_order(&mut chapters, |c| &c.file_name, |c| &c.display_name, bs);
        Ok(chapters)
    }

    fn chapters(&mut self, s: &Series) -> Result<(), PhoneError> {
        let chapters = self.ordered_chapters(s)?;
        let bs = self.backup_series(s);
        let marked: Vec<(phone::Chapter, Mark)> =
            chapters.into_iter().map(|c| { let m = library::mark(bs, &c.file_name); (c, m) }).collect();
        // First unread after the last read one (Mihon's "continue" point).
        let after_read = marked.iter().rposition(|(_, m)| *m == Mark::Read).map_or(0, |i| i + 1);
        let first_unread = (after_read < marked.len()).then_some(after_read);
        self.send(Some(Ev::Chapters { series: s.clone(), chapters: marked, first_unread }));
        Ok(())
    }

    fn sync(&mut self, library: &std::path::Path, list: &[Series], mode: Option<SyncMode>) -> Result<(), PhoneError> {
        // Make sure we have the backup for the Mihon-unread mode.
        if self.phone.is_none() || self.backup.is_none() {
            self.connect(library)?;
        }
        let lib = Library::load(library);
        let on_phone: HashSet<(String, String)> =
            self.phone()?.list_series()?.into_iter().map(|s| (s.source, s.title)).collect();

        // Plan everything first so the progress shows a total.
        let mut jobs: Vec<(Series, phone::Chapter)> = Vec::new();
        let mut missing_on_phone = 0;
        for s in list {
            if !on_phone.contains(&(s.source.clone(), s.title.clone())) {
                missing_on_phone += 1;
                continue;
            }
            let chapters = self.ordered_chapters(s)?;
            let local_series = lib.series.iter().find(|l| l.source == s.source && l.title == s.title);
            // On the PC, or removed from it (never copied again unless restored).
            let local: HashSet<String> = local_series
                .map(|l| l.chapters.iter().map(|c| c.file_name.clone()).chain(l.meta.removed.iter().cloned()).collect())
                .unwrap_or_default();
            let finished: HashSet<String> = local_series
                .map(|l| l.chapters.iter().filter(|c| c.finished()).map(|c| c.file_name.clone()).collect())
                .unwrap_or_default();
            let start = local_series.and_then(|l| l.meta.start_from.clone());
            let bs = self.backup_series(s);
            for i in library::plan_sync(&chapters, &local, &finished, start.as_deref(), bs, mode) {
                jobs.push((s.clone(), chapters[i].clone()));
            }
        }

        let count = jobs.len();
        let mut copied = 0;
        let mut bytes = 0u64;
        let t0 = Instant::now();
        for (index, (s, c)) in jobs.iter().enumerate() {
            if self.cancel.load(Ordering::Relaxed) {
                break;
            }
            let label = format!("{} — {}", s.title, c.display_name);
            let dest = library.join(&s.source).join(&s.title);
            let mut last = Instant::now() - Duration::from_secs(1);
            let ev = self.ev.clone();
            let ctx = self.ctx.clone();
            let cancel = self.cancel.clone();
            let result = self.phone()?.pull_chapter(s, c, &dest, &cancel, &mut |p| {
                if last.elapsed() >= Duration::from_millis(100) || p.bytes_done == p.bytes_total {
                    last = Instant::now();
                    let _ = ev.send(Some(Ev::Copy {
                        label: label.clone(),
                        index,
                        count,
                        done: p.bytes_done,
                        total: p.bytes_total,
                    }));
                    ctx.request_repaint();
                }
            });
            match result {
                Ok(path) => {
                    copied += 1;
                    bytes += std::fs::metadata(&path).map_or(c.size, |m| if m.is_file() { m.len() } else { c.size });
                    self.send(Some(Ev::Copied));
                }
                Err(PhoneError::Cancelled) => break,
                Err(e) => return Err(e),
            }
        }

        let mut msg = if count == 0 {
            "Nothing new to copy.".to_string()
        } else if copied < count {
            format!("Stopped after {copied} of {count} chapters.")
        } else {
            format!(
                "Copied {copied} chapter{} ({:.0} MB in {:.0} s).",
                if copied == 1 { "" } else { "s" },
                bytes as f64 / 1e6,
                t0.elapsed().as_secs_f64()
            )
        };
        if missing_on_phone > 0 {
            msg.push_str(&format!(" {missing_on_phone} series not in the phone's downloads."));
        }
        self.send(Some(Ev::Finished(msg)));
        Ok(())
    }
}
