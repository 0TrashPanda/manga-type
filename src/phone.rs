//! Phone layer: talk to the Android phone over USB (pure Rust, no `adb` binary),
//! find Mihon's download folder, list series/chapters and copy chapters to the PC.
//!
//! Nothing here ever writes to or deletes anything on the phone.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use adb_client::server::ADBServer;
use adb_client::usb::ADBUSBDevice;
use adb_client::{ADBDeviceExt, ADBListItemType, RustADBError};

/// Android's shared storage, as seen over adb.
const STORAGE: &str = "/storage/emulated/0";

/// Where Mihon/Tachiyomi usually keep their folder, relative to [`STORAGE`].
/// Checked in order before falling back to a scan.
const ROOT_CANDIDATES: &[&str] = &[
    "Books/Tachiyomi",
    "Books/Mihon",
    "Mihon",
    "Tachiyomi",
    "Documents/Mihon",
    "Documents/Tachiyomi",
];

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Everything that can go wrong talking to the phone, in terms the UI can show.
#[derive(Debug)]
pub enum PhoneError {
    /// No phone with USB debugging found on USB.
    NoDevice,
    /// More than one phone plugged in.
    MultipleDevices(String),
    /// The adb server (from the Android SDK, Android Studio, other tools) holds the phone.
    AdbServerBusy,
    /// The phone didn't accept this computer's key ("Allow USB debugging?" not accepted).
    Unauthorized,
    /// Linux won't let us open the USB device (udev rules).
    NoUsbPermission,
    /// No adb key at the expected path; without it the phone would ask to authorize a new key.
    NoAdbKey(PathBuf),
    /// Phone was unplugged or the connection dropped.
    Disconnected,
    /// No Mihon/Tachiyomi folder with `downloads/` found on the phone.
    LibraryNotFound,
    /// A path on the phone doesn't exist.
    NotFound(String),
    /// Copy was cancelled by the user.
    Cancelled,
    /// Error writing on the PC side.
    LocalIo(io::Error),
    /// Anything else, with the underlying message.
    Other(String),
}

impl fmt::Display for PhoneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDevice => write!(
                f,
                "No phone found. Plug it in over USB and check that USB debugging is on."
            ),
            Self::MultipleDevices(d) => write!(f, "More than one phone is plugged in ({d}). Unplug the others."),
            Self::AdbServerBusy => write!(
                f,
                "The adb server is using the phone, so manga-type can't open it. Stop the adb server and try again."
            ),
            Self::Unauthorized => write!(
                f,
                "The phone hasn't allowed this computer. Unlock it, accept \"Allow USB debugging?\" (tick \"Always allow\"), then retry."
            ),
            Self::NoUsbPermission => write!(
                f,
                "No permission to open the phone over USB. Install android-udev rules (pacman -S android-udev) and replug."
            ),
            Self::NoAdbKey(p) => write!(f, "No adb key found at {}.", p.display()),
            Self::Disconnected => write!(f, "The phone was disconnected. Plug it back in and retry."),
            Self::LibraryNotFound => write!(
                f,
                "Couldn't find Mihon's folder on the phone (looked for a folder with downloads/ inside)."
            ),
            Self::NotFound(p) => write!(f, "Not found on the phone: {p}"),
            Self::Cancelled => write!(f, "Copy cancelled."),
            Self::LocalIo(e) => write!(f, "Couldn't write on this computer: {e}"),
            Self::Other(m) => write!(f, "Phone error: {m}"),
        }
    }
}

impl std::error::Error for PhoneError {}

/// Map a crate error to ours. `connecting` tells whether we were in the handshake,
/// where a timeout means the phone is waiting for the user to accept our key.
fn map_err(e: RustADBError, connecting: bool) -> PhoneError {
    use rusb::Error as U;
    match e {
        RustADBError::DeviceBusy => PhoneError::AdbServerBusy,
        RustADBError::DeviceNotFound(msg) => {
            if msg.contains("two Android devices") {
                PhoneError::MultipleDevices(msg)
            } else {
                PhoneError::NoDevice
            }
        }
        RustADBError::USBDeviceNotFound(..) => PhoneError::NoDevice,
        RustADBError::UsbError(u) => match u {
            U::Access => PhoneError::NoUsbPermission,
            U::Busy => PhoneError::AdbServerBusy,
            U::NoDevice | U::NotFound | U::Pipe | U::Io => PhoneError::Disconnected,
            U::Timeout if connecting => PhoneError::Unauthorized,
            U::Timeout => PhoneError::Disconnected,
            other => PhoneError::Other(other.to_string()),
        },
        RustADBError::IOError(io) if io.kind() == io::ErrorKind::TimedOut && connecting => {
            PhoneError::Unauthorized
        }
        RustADBError::IOError(io) if io.kind() == io::ErrorKind::Interrupted => PhoneError::Cancelled,
        // The crate's reader thread reports a dropped USB link (e.g. unplugged) this way.
        RustADBError::IOError(io)
            if matches!(
                io.kind(),
                io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
            ) =>
        {
            PhoneError::Disconnected
        }
        other => {
            let msg = other.to_string();
            if connecting && msg.to_lowercase().contains("timed out") {
                PhoneError::Unauthorized
            } else {
                PhoneError::Other(msg)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// A series folder on the phone: `downloads/<source>/<title>/`.
#[derive(Debug, Clone)]
pub struct Series {
    /// Source folder name, e.g. `Asura Scans (EN)`.
    pub source: String,
    /// Series folder name, e.g. `Nano Machine`.
    pub title: String,
}

/// How a chapter is stored on the phone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChapterKind {
    /// One `.cbz` (zip) file.
    Cbz,
    /// A folder of loose images.
    Folder,
}

/// A downloaded chapter on the phone.
#[derive(Debug, Clone)]
pub struct Chapter {
    /// File or folder name on the phone, e.g. `Chapter 337 - Finale (2)_bcc36f.cbz`.
    pub file_name: String,
    /// Name for display, without Mihon's `_hash` suffix and extension.
    pub display_name: String,
    pub kind: ChapterKind,
    /// Size in bytes for a cbz; 0 for folders (known once copied).
    pub size: u64,
}

/// Copy progress, reported while pulling a chapter.
#[derive(Debug, Clone, Copy)]
pub struct Progress {
    pub bytes_done: u64,
    /// Total bytes, 0 if unknown.
    pub bytes_total: u64,
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

/// A connected phone plus the location of its Mihon folder.
pub struct Phone {
    device: ADBUSBDevice,
    /// Absolute path of the Mihon folder (contains `downloads/`).
    root: String,
}

impl fmt::Debug for Phone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Phone").field("root", &self.root).finish()
    }
}

/// Default adb key location (`$ANDROID_USER_HOME/android/adbkey` or `~/.android/adbkey`),
/// the same key the SDK's adb uses, so an already-authorized PC stays authorized.
pub fn default_key_path() -> Option<PathBuf> {
    if let Ok(h) = std::env::var("ANDROID_USER_HOME") {
        return Some(PathBuf::from(h).join("android").join("adbkey"));
    }
    std::env::home_dir().map(|h| h.join(".android").join("adbkey"))
}

/// Is an adb server listening on the default port?
pub fn adb_server_running() -> bool {
    std::net::TcpStream::connect_timeout(
        &SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5037).into(),
        Duration::from_millis(300),
    )
    .is_ok()
}

/// Ask a running adb server to quit, so we can open the phone ourselves.
/// Does nothing if no server is running (and never starts one).
pub fn stop_adb_server() -> Result<(), PhoneError> {
    if !adb_server_running() {
        return Ok(());
    }
    let mut server = ADBServer::new(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5037));
    server.kill().map_err(|e| PhoneError::Other(e.to_string()))?;
    // Give the server a moment to release the USB interface.
    for _ in 0..20 {
        if !adb_server_running() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(300));
    Ok(())
}

impl Phone {
    /// Connect to the single phone on USB and locate its Mihon folder.
    pub fn connect(key_path: &Path) -> Result<Self, PhoneError> {
        // Without the existing key the crate would silently make a new random one,
        // which the phone has never seen. Fail clearly instead.
        if !key_path.is_file() {
            return Err(PhoneError::NoAdbKey(key_path.to_path_buf()));
        }
        let mut attempt = 0;
        loop {
            attempt += 1;
            match Self::connect_once(key_path) {
                // Right after the user accepts the "Allow USB debugging?" prompt, the phone can
                // send a leftover AUTH message on the new connection. A fresh connection works.
                Err(PhoneError::Other(msg)) if msg.contains("unexpected AUTH") && attempt < 2 => {}
                // Right after plugging in, the USB device exists before udev has granted
                // access to it. Give udev a moment.
                Err(PhoneError::NoUsbPermission) if attempt < 4 => {}
                other => return other,
            }
            std::thread::sleep(Duration::from_millis(400));
        }
    }

    fn connect_once(key_path: &Path) -> Result<Self, PhoneError> {
        let device = ADBUSBDevice::autodetect_with_custom_private_key(key_path.to_path_buf())
            .map_err(|e| map_err(e, true))?;
        let mut phone = Self { device, root: String::new() };
        phone.root = phone.find_root()?;
        Ok(phone)
    }

    /// Absolute path of the Mihon folder on the phone.
    pub fn root(&self) -> &str {
        &self.root
    }

    /// Absolute path of the downloads folder on the phone.
    pub fn downloads(&self) -> String {
        format!("{}/downloads", self.root)
    }

    /// List a directory; returns (directories, files with size).
    fn list(&self, path: &str) -> Result<(Vec<String>, Vec<(String, u64)>), PhoneError> {
        let items = self.device.list(path).map_err(|e| map_err(e, false))?;
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        for item in items {
            match item {
                ADBListItemType::Directory(i) if i.name != "." && i.name != ".." => dirs.push(i.name),
                ADBListItemType::File(i) => files.push((i.name, u64::from(i.size))),
                _ => {}
            }
        }
        dirs.sort_by(|a, b| natord::compare_ignore_case(a, b));
        files.sort_by(|a, b| natord::compare_ignore_case(&a.0, &b.0));
        Ok((dirs, files))
    }

    fn subdirs(&self, path: &str) -> Vec<String> {
        self.list(path).map(|(d, _)| d).unwrap_or_default()
    }

    /// A Mihon folder has `downloads/` plus `autobackup/` or `local/` next to it.
    fn looks_like_root(&self, path: &str) -> bool {
        let d = self.subdirs(path);
        d.iter().any(|n| n == "downloads") && d.iter().any(|n| n == "autobackup" || n == "local")
    }

    fn find_root(&self) -> Result<String, PhoneError> {
        // Make sure storage is reachable at all (also surfaces auth/disconnect errors).
        let (top, _) = self.list(STORAGE)?;
        for c in ROOT_CANDIDATES {
            let p = format!("{STORAGE}/{c}");
            if self.looks_like_root(&p) {
                return Ok(p);
            }
        }
        // Fallback: scan two levels deep (custom storage location picked in Mihon).
        for d in top.iter().filter(|d| d.as_str() != "Android") {
            let p = format!("{STORAGE}/{d}");
            if self.looks_like_root(&p) {
                return Ok(p);
            }
            for sub in self.subdirs(&p) {
                let p2 = format!("{p}/{sub}");
                if self.looks_like_root(&p2) {
                    return Ok(p2);
                }
            }
        }
        Err(PhoneError::LibraryNotFound)
    }

    /// Every series in Mihon's downloads, sorted by source then title.
    pub fn list_series(&self) -> Result<Vec<Series>, PhoneError> {
        let downloads = self.downloads();
        let mut out = Vec::new();
        for source in self.list(&downloads)?.0 {
            for title in self.list(&format!("{downloads}/{source}"))?.0 {
                out.push(Series { source: source.clone(), title });
            }
        }
        Ok(out)
    }

    /// Download the newest Mihon auto-backup (`autobackup/*.tachibk`) into memory.
    /// Returns its file name, modification time (unix seconds) and bytes; `None` if there is none.
    pub fn read_newest_backup(&self) -> Result<Option<(String, u32, Vec<u8>)>, PhoneError> {
        let dir = format!("{}/autobackup", self.root);
        let items = match self.device.list(&dir) {
            Ok(items) => items,
            Err(e) => match map_err(e, false) {
                PhoneError::Other(_) | PhoneError::NotFound(_) => return Ok(None),
                other => return Err(other),
            },
        };
        let newest = items
            .into_iter()
            .filter_map(|i| match i {
                ADBListItemType::File(f) if f.name.ends_with(".tachibk") => Some(f),
                _ => None,
            })
            .max_by_key(|f| (f.time, f.name.clone()));
        let Some(f) = newest else { return Ok(None) };
        let mut bytes = Vec::with_capacity(f.size as usize);
        self.device
            .pull(&format!("{dir}/{}", f.name), &mut bytes)
            .map_err(|e| map_err(e, false))?;
        Ok(Some((f.name, f.time, bytes)))
    }

    fn series_path(&self, s: &Series) -> String {
        format!("{}/{}/{}", self.downloads(), s.source, s.title)
    }

    /// Finished chapters of a series. Unfinished downloads (`*_tmp`) are skipped.
    pub fn list_chapters(&self, series: &Series) -> Result<Vec<Chapter>, PhoneError> {
        let path = self.series_path(series);
        let (dirs, files) = self.list(&path)?;
        let mut out: Vec<Chapter> = files
            .into_iter()
            .filter(|(n, _)| n.to_lowercase().ends_with(".cbz"))
            .map(|(n, size)| Chapter {
                display_name: display_name(&n),
                file_name: n,
                kind: ChapterKind::Cbz,
                size,
            })
            .chain(dirs.into_iter().filter(|n| !is_unfinished(n)).map(|n| Chapter {
                display_name: display_name(&n),
                file_name: n,
                kind: ChapterKind::Folder,
                size: 0,
            }))
            .collect();
        out.sort_by(|a, b| natord::compare_ignore_case(&a.display_name, &b.display_name));
        Ok(out)
    }

    /// Copy one chapter into `dest_dir` (created if needed), keeping its file/folder name.
    /// Writes to a `.part` name first and renames when complete, so a failed or cancelled
    /// copy never leaves something that looks finished. Returns the final local path.
    pub fn pull_chapter(
        &self,
        series: &Series,
        chapter: &Chapter,
        dest_dir: &Path,
        cancel: &AtomicBool,
        on_progress: &mut dyn FnMut(Progress),
    ) -> Result<PathBuf, PhoneError> {
        fs::create_dir_all(dest_dir).map_err(PhoneError::LocalIo)?;
        let remote = format!("{}/{}", self.series_path(series), chapter.file_name);
        let final_path = dest_dir.join(&chapter.file_name);
        let part_path = dest_dir.join(format!("{}.part", chapter.file_name));

        let result = match chapter.kind {
            ChapterKind::Cbz => {
                let total = chapter.size;
                let mut done = 0;
                self.pull_file(&remote, &part_path, total, &mut done, cancel, on_progress)
            }
            ChapterKind::Folder => self.pull_folder(&remote, &part_path, cancel, on_progress),
        };

        match result {
            Ok(()) => {
                if final_path.exists() {
                    remove_any(&final_path).map_err(PhoneError::LocalIo)?;
                }
                fs::rename(&part_path, &final_path).map_err(PhoneError::LocalIo)?;
                Ok(final_path)
            }
            Err(e) => {
                let _ = remove_any(&part_path);
                Err(e)
            }
        }
    }

    fn pull_folder(
        &self,
        remote: &str,
        local: &Path,
        cancel: &AtomicBool,
        on_progress: &mut dyn FnMut(Progress),
    ) -> Result<(), PhoneError> {
        let (_, files) = self.list(remote)?;
        if files.is_empty() {
            return Err(PhoneError::NotFound(remote.to_string()));
        }
        fs::create_dir_all(local).map_err(PhoneError::LocalIo)?;
        let total: u64 = files.iter().map(|(_, s)| s).sum();
        let mut done = 0;
        for (name, _) in files {
            self.pull_file(
                &format!("{remote}/{name}"),
                &local.join(&name),
                total,
                &mut done,
                cancel,
                on_progress,
            )?;
        }
        Ok(())
    }

    fn pull_file(
        &self,
        remote: &str,
        local: &Path,
        total: u64,
        done: &mut u64,
        cancel: &AtomicBool,
        on_progress: &mut dyn FnMut(Progress),
    ) -> Result<(), PhoneError> {
        let file = File::create(local).map_err(PhoneError::LocalIo)?;
        let mut writer = ProgressWriter {
            inner: BufWriter::with_capacity(1 << 20, file),
            done,
            total,
            cancel,
            on_progress,
            local_err: None,
        };
        let res = self.device.pull(remote, &mut writer);
        if let Some(e) = writer.local_err.take() {
            return Err(e);
        }
        res.map_err(|e| map_err(e, false))?;
        writer.inner.flush().map_err(PhoneError::LocalIo)?;
        Ok(())
    }
}

/// Counts bytes for progress and stops the transfer when cancelled.
struct ProgressWriter<'a> {
    inner: BufWriter<File>,
    done: &'a mut u64,
    total: u64,
    cancel: &'a AtomicBool,
    on_progress: &'a mut dyn FnMut(Progress),
    /// Our own error (cancel / disk), so it isn't mistaken for a phone error.
    local_err: Option<PhoneError>,
}

impl Write for ProgressWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            self.local_err = Some(PhoneError::Cancelled);
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let n = match self.inner.write(buf) {
            Ok(n) => n,
            Err(e) => {
                let kind = e.kind();
                self.local_err = Some(PhoneError::LocalIo(e));
                return Err(io::Error::new(kind, "local write failed"));
            }
        };
        *self.done += n as u64;
        (self.on_progress)(Progress { bytes_done: *self.done, bytes_total: self.total });
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Mihon writes `<name>_tmp` folders while a download is in progress.
pub fn is_unfinished(name: &str) -> bool {
    name.ends_with("_tmp") || name.ends_with(".tmp")
}

/// `Chapter 337 - Finale (2)_bcc36f.cbz` -> `Chapter 337 - Finale (2)`.
pub fn display_name(file_name: &str) -> String {
    let stem = file_name
        .strip_suffix(".cbz")
        .or_else(|| file_name.strip_suffix(".CBZ"))
        .unwrap_or(file_name);
    match stem.rsplit_once('_') {
        Some((head, hash))
            if !head.is_empty() && hash.len() == 6 && hash.chars().all(|c| c.is_ascii_hexdigit()) =>
        {
            head.to_string()
        }
        _ => stem.to_string(),
    }
}

fn remove_any(p: &Path) -> io::Result<()> {
    if p.is_dir() { fs::remove_dir_all(p) } else { fs::remove_file(p) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_mihon_hash() {
        assert_eq!(display_name("Chapter 337 - Finale (2)_bcc36f.cbz"), "Chapter 337 - Finale (2)");
        assert_eq!(display_name("www.manganato.gg_Chapter 1.2_a4eba9.cbz"), "www.manganato.gg_Chapter 1.2");
        assert_eq!(display_name("Chapter 5"), "Chapter 5");
        assert_eq!(display_name("Chapter_12"), "Chapter_12");
    }

    #[test]
    fn skips_tmp() {
        assert!(is_unfinished("Chapter 120_4dbce9_tmp"));
        assert!(!is_unfinished("Chapter 120_4dbce9"));
    }
}
