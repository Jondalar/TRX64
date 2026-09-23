//! folder_device.rs — Spec 873: a folder on the bus.
//!
//! An IEC device without a drive CPU, backed by a host directory. Two halves:
//!
//!   * the **line-level slave** — the serial protocol as a timed state machine that
//!     reads ATN/CLK/DATA and pulls CLK and DATA into its own `drv_bus` slot. Its
//!     states are VICE's (`serial/serial-iec-device.c:241-743`), its times come from a
//!     [`TimingProfile`] (default: the Ultimate's `software/io/iec/iec_code.iec`
//!     numbers, VICE's as the second profile);
//!   * the **DOS** — channels 0-15, the command channel, the host mapping of §8.
//!
//! The device reads and writes the host only through [`FolderSource`]. It is clocked at
//! every sync point the drives have (as an `IecDevice`, Spec 874), event-driven on the C64 clock:
//! between two sync points it runs every timed transition under the lines it saw
//! last, then takes the new lines at the sync point.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

// ── the host ────────────────────────────────────────────────────────────────────────

/// One directory entry as the host has it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostEntry {
    /// The host name (one path component).
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// The host lets us write it.
    pub writable: bool,
}

/// Spec 873 §10 — everything the device does to the host goes through this. Paths are
/// lists of host path components relative to the attached folder; the device never
/// builds a path that leaves it.
pub trait FolderSource: Send + Sync {
    /// The folder this source serves, for the checkpoint and the surfaces.
    fn root(&self) -> PathBuf;
    /// The entries of `dir`: files and directories only; dot-files, links leaving the
    /// root and anything else are left out.
    fn list(&self, dir: &[String]) -> io::Result<Vec<HostEntry>>;
    /// The entry at `path`, `None` when there is none.
    fn stat(&self, path: &[String]) -> io::Result<Option<HostEntry>>;
    /// Up to `len` bytes of the file at `path` from `offset`; fewer only at its end.
    fn read(&self, path: &[String], offset: u64, len: usize) -> io::Result<Vec<u8>>;
    /// Write `data` at `offset` of the file at `path`, creating it; `truncate` empties
    /// it first.
    fn write(&self, path: &[String], offset: u64, data: &[u8], truncate: bool) -> io::Result<()>;
    fn scratch(&self, path: &[String]) -> io::Result<()>;
    fn rename(&self, from: &[String], to: &[String]) -> io::Result<()>;
    fn mkdir(&self, path: &[String]) -> io::Result<()>;
    fn rmdir(&self, path: &[String]) -> io::Result<()>;
    /// Free space on the volume holding the folder, in bytes.
    fn free_bytes(&self) -> io::Result<u64>;
}

/// The plain host folder: `std::fs` under `root`.
#[derive(Debug)]
pub struct HostFolder {
    root: PathBuf,
    canon: PathBuf,
}

impl HostFolder {
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        let canon = std::fs::canonicalize(&root)?;
        if !canon.is_dir() {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("{} is not a folder", root.display())));
        }
        Ok(HostFolder { root, canon })
    }

    fn path(&self, p: &[String]) -> PathBuf {
        let mut out = self.canon.clone();
        for c in p {
            out.push(c);
        }
        out
    }

    fn entry(&self, dir: &Path, name: &str) -> Option<HostEntry> {
        if name.starts_with('.') {
            return None;
        }
        let full = dir.join(name);
        let lmeta = std::fs::symlink_metadata(&full).ok()?;
        if lmeta.file_type().is_symlink() {
            let target = std::fs::canonicalize(&full).ok()?;
            if !target.starts_with(&self.canon) {
                return None;
            }
        }
        let meta = std::fs::metadata(&full).ok()?;
        if !meta.is_file() && !meta.is_dir() {
            return None;
        }
        Some(HostEntry {
            name: name.to_string(),
            is_dir: meta.is_dir(),
            size: if meta.is_file() { meta.len() } else { 0 },
            writable: !meta.permissions().readonly(),
        })
    }
}

impl FolderSource for HostFolder {
    fn root(&self) -> PathBuf {
        self.root.clone()
    }

    fn list(&self, dir: &[String]) -> io::Result<Vec<HostEntry>> {
        let d = self.path(dir);
        let mut out = Vec::new();
        for e in std::fs::read_dir(&d)? {
            let e = e?;
            let Ok(name) = e.file_name().into_string() else { continue };
            if let Some(he) = self.entry(&d, &name) {
                out.push(he);
            }
        }
        Ok(out)
    }

    fn stat(&self, path: &[String]) -> io::Result<Option<HostEntry>> {
        let Some((last, dir)) = path.split_last() else { return Ok(None) };
        Ok(self.entry(&self.path(dir), last))
    }

    fn read(&self, path: &[String], offset: u64, len: usize) -> io::Result<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(self.path(path))?;
        f.seek(SeekFrom::Start(offset))?;
        let mut buf = Vec::with_capacity(len);
        f.take(len as u64).read_to_end(&mut buf)?;
        Ok(buf)
    }

    fn write(&self, path: &[String], offset: u64, data: &[u8], truncate: bool) -> io::Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new().create(true).write(true).truncate(truncate).open(self.path(path))?;
        f.seek(SeekFrom::Start(offset))?;
        f.write_all(data)
    }

    fn scratch(&self, path: &[String]) -> io::Result<()> {
        std::fs::remove_file(self.path(path))
    }

    fn rename(&self, from: &[String], to: &[String]) -> io::Result<()> {
        std::fs::rename(self.path(from), self.path(to))
    }

    fn mkdir(&self, path: &[String]) -> io::Result<()> {
        std::fs::create_dir(self.path(path))
    }

    fn rmdir(&self, path: &[String]) -> io::Result<()> {
        std::fs::remove_dir(self.path(path))
    }

    fn free_bytes(&self) -> io::Result<u64> {
        volume_free_bytes(&self.canon)
    }
}

#[cfg(unix)]
fn volume_free_bytes(p: &Path) -> io::Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path, `s` a properly sized out-struct.
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(s.f_bavail as u64 * s.f_frsize as u64)
}

#[cfg(windows)]
fn volume_free_bytes(p: &Path) -> io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetDiskFreeSpaceExW(dir: *const u16, avail: *mut u64, total: *mut u64, free: *mut u64) -> i32;
    }
    let w: Vec<u16> = p.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let (mut a, mut t, mut f) = (0u64, 0u64, 0u64);
    // SAFETY: `w` is NUL-terminated UTF-16, the out-pointers are valid u64s.
    if unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut a, &mut t, &mut f) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(a)
}

#[cfg(not(any(unix, windows)))]
fn volume_free_bytes(_p: &Path) -> io::Result<u64> {
    Err(io::Error::other("free space unknown on this host"))
}

// ── the timing profile (§6) ─────────────────────────────────────────────────────────

/// Spec 873 §6 — the device's times, in µs; turned into C64 cycles with the machine's
/// clock (VICE `US2CYCLES`). Data, carried in the checkpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimingProfile {
    /// Ignore the lines this long after ATN falls.
    pub t_atn_settle: u32,
    /// Listener: CLK high this long after ready-for-data = EOI.
    pub t_eoi_detect: u32,
    /// Listener: the DATA pulse acknowledging EOI.
    pub t_eoi_ack: u32,
    /// Listener: last bit's CLK fall → frame acknowledge (DATA low), data bytes.
    pub t_rx_ack: u32,
    /// Listener: the same under ATN.
    pub t_rx_ack_atn: u32,
    /// Talker: CLK low → first ready-to-send at the turnaround.
    pub t_turnaround: u32,
    /// Talker: ready-for-data seen → first bit (the Ultimate's `Tne`).
    pub t_bit_lead: u32,
    pub t_bit_low: u32,
    pub t_bit_high: u32,
    /// Talker: after an acknowledged byte, before the next ready-to-send.
    pub t_byte_gap: u32,
    /// Talker: wait this long for the listener's frame acknowledge.
    pub t_frame_ack: u32,
}

impl TimingProfile {
    /// The Ultimate's Software IEC (`iec_code.iec:3-11` and the waits in `TX_BYTE`,
    /// `RECEIVE_BYTE`, `atn_irq_vec`) — the default (owner, 2026-09-23).
    pub const ULTIMATE: TimingProfile = TimingProfile {
        t_atn_settle: 20,
        t_eoi_detect: 1475,
        t_eoi_ack: 70,
        t_rx_ack: 50,
        t_rx_ack_atn: 5,
        t_turnaround: 80,
        t_bit_lead: 40,
        t_bit_low: 80,
        t_bit_high: 80,
        t_byte_gap: 90,
        t_frame_ack: 1000,
    };
    /// VICE's IEC device (`serial-iec-device.c`).
    pub const VICE: TimingProfile = TimingProfile {
        t_atn_settle: 100,
        t_eoi_detect: 200,
        t_eoi_ack: 60,
        t_rx_ack: 0,
        t_rx_ack_atn: 0,
        t_turnaround: 80,
        t_bit_lead: 0,
        t_bit_low: 60,
        t_bit_high: 60,
        t_byte_gap: 0,
        t_frame_ack: 1000,
    };

    /// By name, for the wire: `ultimate` (default) or `vice`.
    pub fn by_name(name: &str) -> Option<TimingProfile> {
        match name.to_ascii_lowercase().as_str() {
            "ultimate" | "default" => Some(Self::ULTIMATE),
            "vice" => Some(Self::VICE),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        if *self == Self::ULTIMATE {
            "ultimate"
        } else if *self == Self::VICE {
            "vice"
        } else {
            "custom"
        }
    }
}

impl Default for TimingProfile {
    fn default() -> Self {
        Self::ULTIMATE
    }
}

// ── names (§8) ──────────────────────────────────────────────────────────────────────

/// PETSCII → host: `$41-$5A` ↔ `a-z`, `$C1-$DA` ↔ `A-Z`, `$20-$40` and `$5B-$5D` as
/// themselves except `/` and `%`; every other byte as `%XX`.
pub fn petscii_to_host(name: &[u8]) -> String {
    let mut s = String::new();
    for &b in name {
        match b {
            0x41..=0x5a => s.push((b + 0x20) as char),
            0xc1..=0xda => s.push((b - 0x80) as char),
            0x2f | 0x25 => s.push_str(&format!("%{b:02X}")),
            0x20..=0x40 | 0x5b..=0x5d => s.push(b as char),
            _ => s.push_str(&format!("%{b:02X}")),
        }
    }
    s
}

/// Host → PETSCII, the inverse of [`petscii_to_host`] on every name it produces. A host
/// byte outside the mapping (`_`, `~`, a UTF-8 byte) shows as the PETSCII byte of the
/// same value; the name still opens, because lookups compare in PETSCII.
pub fn host_to_petscii(name: &str) -> Vec<u8> {
    let b = name.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'%' && i + 2 < b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(v) = hex {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(match c {
            b'a'..=b'z' => c - 0x20,
            b'A'..=b'Z' => c + 0x80,
            _ => c,
        });
        i += 1;
    }
    out
}

/// Letters compare without case: shifted `$C1-$DA` as unshifted `$41-$5A`.
#[inline]
fn fold(b: u8) -> u8 {
    if (0xc1..=0xda).contains(&b) {
        b - 0x80
    } else {
        b
    }
}

/// 1541 wildcards: `?` one character, `*` matches the rest and ends the pattern.
pub fn matches(pattern: &[u8], name: &[u8]) -> bool {
    let mut i = 0;
    loop {
        match (pattern.get(i), name.get(i)) {
            (Some(b'*'), _) => return true,
            (None, None) => return true,
            (Some(b'?'), Some(_)) => {}
            (Some(&p), Some(&n)) if fold(p) == fold(n) => {}
            _ => return false,
        }
        i += 1;
    }
}

fn has_wildcard(p: &[u8]) -> bool {
    p.iter().any(|&b| b == b'*' || b == b'?')
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileType {
    Seq,
    Prg,
    Usr,
    Rel,
    Dir,
}

impl FileType {
    fn label(self) -> &'static [u8; 3] {
        match self {
            FileType::Seq => b"SEQ",
            FileType::Prg => b"PRG",
            FileType::Usr => b"USR",
            FileType::Rel => b"REL",
            FileType::Dir => b"DIR",
        }
    }
    fn ext(self) -> &'static str {
        match self {
            FileType::Seq => ".seq",
            FileType::Usr => ".usr",
            FileType::Rel => ".rel",
            _ => ".prg",
        }
    }
    /// A type parameter of OPEN (`,S` `,P` `,U` `,L`).
    fn from_open_letter(c: u8) -> Option<FileType> {
        match fold(c) {
            b'S' => Some(FileType::Seq),
            b'P' => Some(FileType::Prg),
            b'U' => Some(FileType::Usr),
            b'L' => Some(FileType::Rel),
            _ => None,
        }
    }
    /// A directory filter (`$:pattern=P|S|U|R|D`).
    fn from_filter_letter(c: u8) -> Option<FileType> {
        match fold(c) {
            b'S' => Some(FileType::Seq),
            b'P' => Some(FileType::Prg),
            b'U' => Some(FileType::Usr),
            b'R' => Some(FileType::Rel),
            b'D' => Some(FileType::Dir),
            _ => None,
        }
    }
}

/// One entry as the C64 sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub host: String,
    /// The full PETSCII name (the typed extension cut off).
    pub name: Vec<u8>,
    pub ftype: FileType,
    pub size: u64,
    pub writable: bool,
    /// The host name carried a type extension (`.prg .seq .usr .rel`).
    pub typed: bool,
}

impl Entry {
    fn from_host(h: &HostEntry) -> Entry {
        if h.is_dir {
            return Entry {
                host: h.name.clone(),
                name: host_to_petscii(&h.name),
                ftype: FileType::Dir,
                size: 0,
                writable: h.writable,
                typed: false,
            };
        }
        let lower = h.name.to_ascii_lowercase();
        let typed = [(".prg", FileType::Prg), (".seq", FileType::Seq), (".usr", FileType::Usr), (".rel", FileType::Rel)]
            .into_iter()
            .find(|(e, _)| lower.len() > e.len() && lower.ends_with(e));
        let (stem, ftype, typed) = match typed {
            Some((e, t)) => (&h.name[..h.name.len() - e.len()], t, true),
            None => (&h.name[..], FileType::Prg, false),
        };
        Entry { host: h.name.clone(), name: host_to_petscii(stem), ftype, size: h.size, writable: h.writable, typed }
    }

    /// Shown and matched on its first 16 characters.
    pub fn shown(&self) -> &[u8] {
        &self.name[..self.name.len().min(16)]
    }

    pub fn blocks(&self) -> u16 {
        self.size.div_ceil(254).min(65535) as u16
    }
}

// ── the DOS (§7) ────────────────────────────────────────────────────────────────────

/// The 1541 command buffer.
const CMD_MAX: usize = 58;
const READ_CHUNK: usize = 256;
const WRITE_CHUNK: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Channel {
    Closed,
    Read { path: Vec<String>, pos: u64, ftype: FileType },
    Write { path: Vec<String>, pos: u64, ftype: FileType, pending: Vec<u8> },
    /// A directory listing, built at open and kept.
    Dir { bytes: Vec<u8>, pos: usize },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub code: u8,
    pub text: String,
    pub track: u8,
    pub sector: u8,
}

impl Status {
    fn bytes(&self) -> Vec<u8> {
        format!("{:02},{},{:02},{:02}\r", self.code, self.text, self.track, self.sector).into_bytes()
    }
}

const POWER_ON: &str = "TRX64 FOLDER DOS V1.0";

/// What the device refused, for the host to show (`unit 9 refused M-E $0500`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderEvent {
    pub unit: u8,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Dos {
    pub channels: Vec<Channel>,
    pub status: Status,
    /// Read position in the status message (channel 15 as talker).
    pub status_pos: usize,
    /// Current directory, host components below the root.
    pub cwd: Vec<String>,
    /// The channel whose name is being received (between `$Fn` and UNLISTEN).
    pub naming: Option<u8>,
    pub name_buf: Vec<u8>,
    pub name_overflow: bool,
    /// Bytes written to channel 15 since its last UNLISTEN.
    pub cmd_buf: Vec<u8>,
    pub cmd_overflow: bool,
    /// The boot file for `LOAD"*"`, host path below the root, set at attach.
    pub boot: Option<Vec<String>>,
    pub read_only: bool,
    #[serde(skip)]
    read_cache: Option<(u8, u64, Vec<u8>)>,
}

fn st(code: u8, text: &str) -> Status {
    Status { code, text: text.to_string(), track: 0, sector: 0 }
}

impl Dos {
    fn new(read_only: bool, boot: Option<Vec<String>>) -> Dos {
        Dos {
            channels: vec![Channel::Closed; 16],
            status: st(73, POWER_ON),
            status_pos: 0,
            cwd: Vec::new(),
            naming: None,
            name_buf: Vec::new(),
            name_overflow: false,
            cmd_buf: Vec::new(),
            cmd_overflow: false,
            boot,
            read_only,
            read_cache: None,
        }
    }

    fn set_status(&mut self, s: Status) {
        self.status = s;
        self.status_pos = 0;
    }

    fn ok(&mut self) {
        self.set_status(st(0, " OK"));
    }

    fn path_in_cwd(&self, host: &str) -> Vec<String> {
        let mut p = self.cwd.clone();
        p.push(host.to_string());
        p
    }

    /// The current directory, sorted by host name bytes (§8).
    fn listing(&self, src: &dyn FolderSource) -> io::Result<Vec<Entry>> {
        let mut hs = src.list(&self.cwd)?;
        hs.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
        Ok(hs.iter().map(Entry::from_host).collect())
    }

    /// The first entry (listing order) whose shown name matches `pattern`.
    fn find(&self, src: &dyn FolderSource, pattern: &[u8], want: impl Fn(&Entry) -> bool) -> Option<Entry> {
        self.listing(src).ok()?.into_iter().find(|e| want(e) && matches(pattern, e.shown()))
    }

    /// Reset (the IEC RESET line, `UI`/`UJ`): channels closed, uncommitted writes
    /// dropped, back to the root, status 73.
    fn reset(&mut self) {
        for c in self.channels.iter_mut() {
            *c = Channel::Closed;
        }
        self.cwd.clear();
        self.naming = None;
        self.name_buf.clear();
        self.name_overflow = false;
        self.cmd_buf.clear();
        self.cmd_overflow = false;
        self.read_cache = None;
        self.set_status(st(73, POWER_ON));
    }

    // ── bus-side calls ──────────────────────────────────────────────────────────

    /// `$Fn` under ATN: the bytes up to the next UNLISTEN are the name.
    fn open_begin(&mut self, ch: u8) {
        self.naming = Some(ch);
        self.name_buf.clear();
        self.name_overflow = false;
    }

    /// A byte from the C64 on channel `ch`. `Err` = refuse it (stop listening).
    fn write(&mut self, src: &dyn FolderSource, ch: u8, b: u8) -> Result<(), ()> {
        if self.naming == Some(ch) {
            if self.name_buf.len() < CMD_MAX {
                self.name_buf.push(b);
            } else {
                self.name_overflow = true;
            }
            return Ok(());
        }
        if ch == 15 {
            if self.cmd_buf.len() < CMD_MAX {
                self.cmd_buf.push(b);
            } else {
                self.cmd_overflow = true;
            }
            return Ok(());
        }
        match &mut self.channels[ch as usize] {
            Channel::Write { path, pos, pending, .. } => {
                pending.push(b);
                if pending.len() >= WRITE_CHUNK {
                    let r = src.write(path, *pos, pending, false);
                    match r {
                        Ok(()) => {
                            *pos += pending.len() as u64;
                            pending.clear();
                        }
                        Err(_) => {
                            self.set_status(st(25, "WRITE ERROR"));
                            return Err(());
                        }
                    }
                }
                Ok(())
            }
            _ => {
                self.set_status(st(61, "FILE NOT OPEN"));
                Err(())
            }
        }
    }

    /// UNLISTEN after a LISTEN on channel `ch`: a pending OPEN executes; a command on
    /// channel 15 executes. Returns the channel's new `st` (0 = open).
    fn unlisten(&mut self, src: &dyn FolderSource, ch: u8, events: &mut Vec<String>) -> u8 {
        if self.naming == Some(ch) {
            self.naming = None;
            let name = std::mem::take(&mut self.name_buf);
            let over = std::mem::take(&mut self.name_overflow);
            if over {
                self.set_status(st(32, "SYNTAX ERROR"));
                return if ch == 15 { 0 } else { 2 };
            }
            return if self.open(src, ch, &name, events) { 0 } else { 2 };
        }
        if ch == 15 {
            let cmd = std::mem::take(&mut self.cmd_buf);
            let over = std::mem::take(&mut self.cmd_overflow);
            if over {
                self.set_status(st(32, "SYNTAX ERROR"));
            } else {
                self.command(src, &cmd, events);
            }
        }
        0
    }

    /// The byte the talker would send next on `ch`, and whether it is the last.
    /// Nothing is consumed until [`Self::commit`].
    fn peek(&mut self, src: &dyn FolderSource, ch: u8) -> Option<(u8, bool)> {
        if ch == 15 {
            let b = self.status.bytes();
            let i = self.status_pos.min(b.len() - 1);
            return Some((b[i], i + 1 >= b.len()));
        }
        match &self.channels[ch as usize] {
            Channel::Dir { bytes, pos } => bytes.get(*pos).map(|&b| (b, pos + 1 >= bytes.len())),
            Channel::Read { path, pos, .. } => {
                let (path, pos) = (path.clone(), *pos);
                // The cache answers when it holds `pos` and either the byte after it
                // or the knowledge that there is none (a short read = end of file).
                let fresh = match &self.read_cache {
                    Some((c, start, data)) => {
                        let end = start + data.len() as u64;
                        let covered = *c == ch && pos >= *start && pos < end;
                        !(covered && (pos + 1 < end || data.len() < READ_CHUNK))
                    }
                    None => true,
                };
                if fresh {
                    match src.read(&path, pos, READ_CHUNK) {
                        Ok(d) => self.read_cache = Some((ch, pos, d)),
                        Err(_) => {
                            self.read_cache = None;
                            self.set_status(st(62, "FILE NOT FOUND"));
                            return None;
                        }
                    }
                }
                let (_, start, data) = self.read_cache.as_ref()?;
                let i = (pos - start) as usize;
                let b = *data.get(i)?;
                let last = i + 1 >= data.len() && data.len() < READ_CHUNK;
                Some((b, last))
            }
            _ => None,
        }
    }

    /// The listener acknowledged the byte [`Self::peek`] gave.
    fn commit(&mut self, ch: u8) {
        if ch == 15 {
            self.status_pos += 1;
            if self.status_pos >= self.status.bytes().len() {
                self.ok();
            }
            return;
        }
        match &mut self.channels[ch as usize] {
            Channel::Dir { pos, .. } => *pos += 1,
            Channel::Read { pos, .. } => *pos += 1,
            _ => {}
        }
    }

    /// `$En`: close the channel; closing 15 closes them all, as a 1541 does.
    fn close(&mut self, src: &dyn FolderSource, ch: u8) -> u8 {
        if ch == 15 {
            let mut worst = 0;
            for c in 0..15 {
                worst = worst.max(self.close_one(src, c));
            }
            return worst;
        }
        self.close_one(src, ch)
    }

    fn close_one(&mut self, src: &dyn FolderSource, ch: u8) -> u8 {
        let c = std::mem::replace(&mut self.channels[ch as usize], Channel::Closed);
        if matches!(&self.read_cache, Some((cc, _, _)) if *cc == ch) {
            self.read_cache = None;
        }
        if let Channel::Write { path, pos, pending, .. } = c {
            // Always touch the file, so a SAVE of nothing still leaves it.
            if src.write(&path, pos, &pending, false).is_err() {
                self.set_status(st(25, "WRITE ERROR"));
                return 2;
            }
        }
        0
    }

    fn free_blocks(src: &dyn FolderSource) -> u16 {
        src.free_bytes().map(|b| (b / 254).min(65535) as u16).unwrap_or(0)
    }

    /// The directory as a 1541's BASIC listing, load address `$0401`.
    fn directory(&self, src: &dyn FolderSource, pattern: &[u8], filter: Option<FileType>) -> io::Result<Vec<u8>> {
        let entries = self.listing(src)?;
        let mut out = vec![0x01, 0x04];
        // Header: `0 ␒"<folder>" 00 2A`.
        let folder = match self.cwd.last() {
            Some(d) => host_to_petscii(d),
            None => {
                let r = src.root();
                host_to_petscii(r.file_name().and_then(|n| n.to_str()).unwrap_or(""))
            }
        };
        let tail = &folder[folder.len().saturating_sub(16)..];
        out.extend_from_slice(&[1, 1, 0, 0, 0x12, b'"']);
        for i in 0..16 {
            out.push(*tail.get(i).unwrap_or(&b' '));
        }
        out.extend_from_slice(b"\" 00 2A");
        out.push(0);
        for e in entries.iter().filter(|e| matches(pattern, e.shown()) && filter.is_none_or(|t| t == e.ftype)) {
            let blocks = e.blocks();
            let mut line = vec![1, 1, blocks as u8, (blocks >> 8) as u8];
            let mut text = [b' '; 27];
            let mut q = 0;
            if blocks < 10 {
                q += 1;
            }
            if blocks < 100 {
                q += 1;
            }
            q += 1;
            // vdrive-dir.c layout: quote, 16 name bytes (closing quote after the
            // name), splat, type, lock — a 32-byte entry with its line link.
            let t = &mut text[q..];
            t[0] = b'"';
            let shown = e.shown();
            t[1..1 + shown.len()].copy_from_slice(shown);
            t[1 + shown.len()] = b'"';
            t[18] = b' ';
            t[19..22].copy_from_slice(e.ftype.label());
            t[22] = if e.writable { b' ' } else { b'<' };
            line.extend_from_slice(&text[..27]);
            line.push(0);
            out.extend_from_slice(&line);
        }
        let free = Self::free_blocks(src);
        out.extend_from_slice(&[1, 1, free as u8, (free >> 8) as u8]);
        out.extend_from_slice(b"BLOCKS FREE.");
        out.extend_from_slice(&[b' '; 13]);
        out.extend_from_slice(&[0, 0, 0]);
        Ok(out)
    }

    /// Parse `[@][d]:name[,t[,m]]`: (replace, name, type, mode).
    fn parse_name(raw: &[u8]) -> (bool, Vec<u8>, Option<FileType>, Option<u8>) {
        let mut s = raw;
        let replace = s.first() == Some(&b'@');
        if replace {
            s = &s[1..];
        }
        if let Some(colon) = s.iter().position(|&b| b == b':') {
            if s[..colon].iter().all(|b| b.is_ascii_digit()) {
                s = &s[colon + 1..];
            }
        }
        let mut parts = s.split(|&b| b == b',');
        let name = parts.next().unwrap_or(&[]).to_vec();
        let mut ftype = None;
        let mut mode = None;
        for p in parts {
            let Some(&c) = p.first() else { continue };
            match fold(c) {
                b'R' | b'W' | b'A' | b'M' => mode = Some(fold(c)),
                _ => {
                    if ftype.is_none() {
                        ftype = FileType::from_open_letter(c);
                    }
                }
            }
        }
        (replace, name, ftype, mode)
    }

    /// OPEN with a name on channel `ch`. `true` = open.
    fn open(&mut self, src: &dyn FolderSource, ch: u8, raw: &[u8], events: &mut Vec<String>) -> bool {
        let mut name = raw.to_vec();
        if name.last() == Some(&0x0d) {
            name.pop();
        }
        if ch == 15 {
            self.command(src, &name, events);
            return true;
        }
        // An OPEN on a channel in use closes the old one first.
        self.close_one(src, ch);
        if name.first() == Some(&b'#') {
            events.push(format!("refused {}", String::from_utf8_lossy(&name)));
            self.set_status(st(78, "BLOCK ACCESS DENIED"));
            return false;
        }
        if name.first() == Some(&b'$') {
            return self.open_dir(src, ch, &name[1..]);
        }
        let (replace, pat, ftype, mode) = Self::parse_name(&name);
        if pat.is_empty() {
            self.set_status(st(34, "SYNTAX ERROR"));
            return false;
        }
        let write = match mode {
            Some(b'W') | Some(b'A') => true,
            Some(_) => false,
            None => ch == 1,
        };
        if ftype == Some(FileType::Rel) {
            self.set_status(st(64, "FILE TYPE MISMATCH"));
            return false;
        }
        if !write {
            // `*` alone on the LOAD channel: the boot file, else the first PRG.
            let found = if ch == 0 && pat == b"*" {
                match &self.boot {
                    Some(b) => src
                        .stat(b)
                        .ok()
                        .flatten()
                        .filter(|h| !h.is_dir)
                        .map(|h| (b.clone(), Entry::from_host(&h))),
                    None => self
                        .find(src, b"*", |e| e.ftype == FileType::Prg)
                        .map(|e| (self.path_in_cwd(&e.host), e)),
                }
            } else {
                self.find(src, &pat, |e| e.ftype != FileType::Dir && ftype.is_none_or(|t| t == e.ftype))
                    .map(|e| (self.path_in_cwd(&e.host), e))
            };
            let Some((path, e)) = found else {
                // A file of another type by that name is a mismatch, not a miss.
                if ftype.is_some() && self.find(src, &pat, |e| e.ftype != FileType::Dir).is_some() {
                    self.set_status(st(64, "FILE TYPE MISMATCH"));
                } else {
                    self.set_status(st(62, "FILE NOT FOUND"));
                }
                return false;
            };
            if e.ftype == FileType::Rel {
                self.set_status(st(64, "FILE TYPE MISMATCH"));
                return false;
            }
            self.channels[ch as usize] = Channel::Read { path, pos: 0, ftype: e.ftype };
            self.ok();
            return true;
        }
        if self.read_only {
            self.set_status(st(26, "WRITE PROTECT ON"));
            return false;
        }
        if has_wildcard(&pat) {
            self.set_status(st(33, "SYNTAX ERROR"));
            return false;
        }
        let ftype = ftype.unwrap_or(if ch == 1 { FileType::Prg } else { FileType::Seq });
        if ftype == FileType::Dir {
            self.set_status(st(64, "FILE TYPE MISMATCH"));
            return false;
        }
        let existing = self.find(src, &pat, |_| true);
        if mode == Some(b'A') {
            let Some(e) = existing.filter(|e| e.ftype != FileType::Dir) else {
                self.set_status(st(62, "FILE NOT FOUND"));
                return false;
            };
            let path = self.path_in_cwd(&e.host);
            self.channels[ch as usize] = Channel::Write { path, pos: e.size, ftype: e.ftype, pending: Vec::new() };
            self.ok();
            return true;
        }
        let path = match existing {
            Some(e) if !replace || e.ftype == FileType::Dir => {
                self.set_status(st(63, "FILE EXISTS"));
                return false;
            }
            Some(e) => self.path_in_cwd(&e.host),
            None => self.path_in_cwd(&(petscii_to_host(&pat) + ftype.ext())),
        };
        if src.write(&path, 0, &[], true).is_err() {
            self.set_status(st(25, "WRITE ERROR"));
            return false;
        }
        self.channels[ch as usize] = Channel::Write { path, pos: 0, ftype, pending: Vec::new() };
        self.ok();
        true
    }

    /// `"$"`, `"$0"`, `"$:pattern"`, `"$:pattern=P"`.
    fn open_dir(&mut self, src: &dyn FolderSource, ch: u8, rest: &[u8]) -> bool {
        let mut r = rest;
        while r.first().is_some_and(|b| b.is_ascii_digit()) {
            r = &r[1..];
        }
        let (pattern, filter): (Vec<u8>, Option<FileType>) = if r.first() == Some(&b':') {
            let p = &r[1..];
            match p.iter().position(|&b| b == b'=') {
                Some(eq) => (p[..eq].to_vec(), p.get(eq + 1).and_then(|&c| FileType::from_filter_letter(c))),
                None => (p.to_vec(), None),
            }
        } else {
            (b"*".to_vec(), None)
        };
        let pattern = if pattern.is_empty() { b"*".to_vec() } else { pattern };
        match self.directory(src, &pattern, filter) {
            Ok(bytes) => {
                self.channels[ch as usize] = Channel::Dir { bytes, pos: 0 };
                self.ok();
                true
            }
            Err(_) => {
                self.set_status(st(74, "DRIVE NOT READY"));
                false
            }
        }
    }

    // ── the command channel ─────────────────────────────────────────────────────

    fn command(&mut self, src: &dyn FolderSource, raw: &[u8], events: &mut Vec<String>) {
        let mut cmd = raw.to_vec();
        if cmd.last() == Some(&0x0d) {
            cmd.pop();
        }
        if cmd.is_empty() {
            return;
        }
        let c0 = fold(cmd[0]);
        let c1 = cmd.get(1).map(|&c| fold(c));
        let text = String::from_utf8_lossy(&cmd).to_string();
        match (c0, c1) {
            (b'C', Some(b'D')) => self.cd(src, &cmd[2..]),
            (b'M', Some(b'D')) => self.md(src, after_colon(&cmd[2..])),
            (b'R', Some(b'D')) => self.rd(src, after_colon(&cmd[2..])),
            (b'M', Some(b'-')) => {
                let what = match cmd.get(2).map(|&c| fold(c)) {
                    Some(b'W') => "M-W",
                    Some(b'E') => "M-E",
                    Some(b'R') => "M-R",
                    _ => "M-",
                };
                let addr = match (cmd.get(3), cmd.get(4)) {
                    (Some(&lo), Some(&hi)) => format!(" ${:04X}", lo as u16 | (hi as u16) << 8),
                    _ => String::new(),
                };
                events.push(format!("refused {what}{addr}"));
                self.set_status(st(33, "SYNTAX ERROR"));
            }
            (b'B', Some(b'-')) => {
                events.push(format!("refused {}", text.chars().take(3).collect::<String>()));
                self.set_status(st(78, "BLOCK ACCESS DENIED"));
            }
            (b'U', Some(u)) => match u & 0x0f {
                1 | 2 => {
                    events.push(format!("refused U{}", u & 0x0f));
                    self.set_status(st(78, "BLOCK ACCESS DENIED"));
                }
                9 | 10 => {
                    self.close_all_dropping();
                    self.cwd.clear();
                    self.set_status(st(73, POWER_ON));
                }
                n => {
                    events.push(format!("refused U{n}"));
                    self.set_status(st(33, "SYNTAX ERROR"));
                }
            },
            (b'S', _) => self.scratch(src, after_colon(&cmd[1..])),
            (b'R', _) => self.rename(src, after_colon(&cmd[1..])),
            (b'I', _) | (b'V', _) => self.ok(),
            (b'N', _) | (b'C', _) => self.set_status(st(31, "SYNTAX ERROR")),
            _ => {
                if c0 == b'M' {
                    events.push(format!("refused {}", text.chars().take(3).collect::<String>()));
                }
                self.set_status(st(33, "SYNTAX ERROR"));
            }
        }
    }

    fn close_all_dropping(&mut self) {
        for c in self.channels.iter_mut() {
            *c = Channel::Closed;
        }
        self.read_cache = None;
    }

    fn scratch(&mut self, src: &dyn FolderSource, arg: &[u8]) {
        if self.read_only {
            return self.set_status(st(26, "WRITE PROTECT ON"));
        }
        let Ok(entries) = self.listing(src) else {
            return self.set_status(st(74, "DRIVE NOT READY"));
        };
        let mut n = 0u8;
        for pat in arg.split(|&b| b == b',') {
            let pat = after_colon(pat);
            for e in entries.iter().filter(|e| e.ftype != FileType::Dir && matches(pat, e.shown())) {
                if src.scratch(&self.path_in_cwd(&e.host)).is_ok() {
                    n = n.saturating_add(1);
                }
            }
        }
        self.set_status(Status { code: 1, text: " FILES SCRATCHED".into(), track: n, sector: 0 });
    }

    fn rename(&mut self, src: &dyn FolderSource, arg: &[u8]) {
        if self.read_only {
            return self.set_status(st(26, "WRITE PROTECT ON"));
        }
        let Some(eq) = arg.iter().position(|&b| b == b'=') else {
            return self.set_status(st(34, "SYNTAX ERROR"));
        };
        let new = after_colon(&arg[..eq]);
        let old = after_colon(&arg[eq + 1..]);
        if new.is_empty() || has_wildcard(new) {
            return self.set_status(st(33, "SYNTAX ERROR"));
        }
        let Some(o) = self.find(src, old, |_| true) else {
            return self.set_status(st(62, "FILE NOT FOUND"));
        };
        if self.find(src, new, |_| true).is_some() {
            return self.set_status(st(63, "FILE EXISTS"));
        }
        let ext = if o.typed { o.ftype.ext() } else { "" };
        let to = self.path_in_cwd(&(petscii_to_host(new) + ext));
        match src.rename(&self.path_in_cwd(&o.host), &to) {
            Ok(()) => self.ok(),
            Err(_) => self.set_status(st(25, "WRITE ERROR")),
        }
    }

    fn md(&mut self, src: &dyn FolderSource, name: &[u8]) {
        if self.read_only {
            return self.set_status(st(26, "WRITE PROTECT ON"));
        }
        if name.is_empty() || has_wildcard(name) {
            return self.set_status(st(33, "SYNTAX ERROR"));
        }
        if self.find(src, name, |_| true).is_some() {
            return self.set_status(st(63, "FILE EXISTS"));
        }
        match src.mkdir(&self.path_in_cwd(&petscii_to_host(name))) {
            Ok(()) => self.ok(),
            Err(_) => self.set_status(st(25, "WRITE ERROR")),
        }
    }

    fn rd(&mut self, src: &dyn FolderSource, name: &[u8]) {
        if self.read_only {
            return self.set_status(st(26, "WRITE PROTECT ON"));
        }
        let Some(d) = self.find(src, name, |e| e.ftype == FileType::Dir) else {
            return self.set_status(st(62, "FILE NOT FOUND"));
        };
        let path = self.path_in_cwd(&d.host);
        if src.list(&path).map(|l| !l.is_empty()).unwrap_or(true) {
            return self.set_status(st(63, "FILE EXISTS"));
        }
        match src.rmdir(&path) {
            Ok(()) => self.ok(),
            Err(_) => self.set_status(st(63, "FILE EXISTS")),
        }
    }

    /// `CD:dir`, `CD/dir/`, `CD←` / `CD:←` (parent), `CD//` (root). Never leaves the root.
    fn cd(&mut self, src: &dyn FolderSource, rest: &[u8]) {
        let mut rest = rest;
        while rest.first().is_some_and(|b| b.is_ascii_digit()) {
            rest = &rest[1..];
        }
        let not_found = |d: &mut Dos| d.set_status(st(39, "SYNTAX ERROR"));
        if rest == [0x5f] || rest == [b':', 0x5f] {
            if self.cwd.pop().is_none() {
                return not_found(self);
            }
            return self.ok();
        }
        let mut target = self.cwd.clone();
        let comps: Vec<&[u8]> = if let Some(r) = rest.strip_prefix(b"//") {
            target.clear();
            r.split(|&b| b == b'/').filter(|c| !c.is_empty()).collect()
        } else if let Some(r) = rest.strip_prefix(b"/") {
            r.split(|&b| b == b'/').filter(|c| !c.is_empty()).collect()
        } else if let Some(r) = rest.strip_prefix(b":") {
            vec![r]
        } else {
            return self.set_status(st(30, "SYNTAX ERROR"));
        };
        for c in comps {
            if c == [0x5f] {
                if target.pop().is_none() {
                    return not_found(self);
                }
                continue;
            }
            let mut hs = match src.list(&target) {
                Ok(h) => h,
                Err(_) => return not_found(self),
            };
            hs.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
            let Some(d) = hs.iter().map(Entry::from_host).find(|e| e.ftype == FileType::Dir && matches(c, e.shown())) else {
                return not_found(self);
            };
            target.push(d.host);
        }
        self.cwd = target;
        self.ok();
    }

    /// After a restore: re-open each channel's host file at its position; a file that
    /// is gone closes its channel with 62.
    fn reopen(&mut self, src: &dyn FolderSource) {
        self.read_cache = None;
        for c in self.channels.iter_mut() {
            if let Channel::Read { path, .. } = c {
                if !matches!(src.stat(path), Ok(Some(h)) if !h.is_dir) {
                    *c = Channel::Closed;
                    self.status = st(62, "FILE NOT FOUND");
                    self.status_pos = 0;
                }
            }
        }
    }
}

fn after_colon(s: &[u8]) -> &[u8] {
    match s.iter().position(|&b| b == b':') {
        Some(i) if s[..i].iter().all(|b| b.is_ascii_digit()) => &s[i + 1..],
        _ => s,
    }
}

// ── the line-level slave (§5) ───────────────────────────────────────────────────────

// States as VICE names them (serial-iec-device.c:224-239).
pub const P_PRE0: u8 = 0;
pub const P_PRE1: u8 = 1;
pub const P_PRE2: u8 = 2;
pub const P_READY: u8 = 3;
pub const P_EOI: u8 = 4;
pub const P_EOIW: u8 = 5;
pub const P_BIT0: u8 = 6;
pub const P_BIT7W: u8 = 21;
pub const P_DONE0: u8 = 22;
pub const P_DONE1: u8 = 23;
pub const P_FRAMEERR0: u8 = 24;
pub const P_FRAMEERR1: u8 = 25;
/// TRX64: the listener's frame acknowledge, delayed by `t_rx_ack` (VICE acknowledges at
/// once; the Ultimate 50 µs after the last CLK fall, 5 µs under ATN).
pub const P_ACK: u8 = 26;

pub const P_TALKING: u8 = 0x20;
pub const P_LISTENING: u8 = 0x40;
pub const P_ATN: u8 = 0x80;

/// `drv_bus` bits (`IECBUS_DEVICE_WRITE_*`): set = released.
const W_CLK: u8 = 0x40;
const W_DATA: u8 = 0x80;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Line {
    pub state: u8,
    pub flags: u8,
    pub byte: u8,
    pub primary: u8,
    pub secondary: u8,
    pub secondary_prev: u8,
    /// Per channel: 0 = fine; else the last OPEN/write failed (VICE `st[16]`).
    pub st: [u8; 16],
    /// The pending timeout, an absolute C64 cycle.
    pub timeout: u64,
    /// What it drives into its slot (`WRITE_CLK | WRITE_DATA` = released).
    pub pull: u8,
    /// Talker: the byte being sent is the last.
    pub last: bool,
    /// The lines as it saw them at the last sync: everyone else's CLK/DATA
    /// (`cpu_bus` bit positions) and ATN.
    pub others: u8,
    pub atn_low: bool,
    /// The clock it was last run to.
    pub now: u64,
}

impl Line {
    fn released() -> Line {
        Line {
            state: P_PRE0,
            flags: 0,
            byte: 0,
            primary: 0,
            secondary: 0,
            secondary_prev: 0,
            st: [0; 16],
            timeout: 0,
            pull: W_CLK | W_DATA,
            last: false,
            others: 0xff,
            atn_low: false,
            now: 0,
        }
    }
}

/// Options for [`crate::Machine::attach_folder`].
#[derive(Clone, Debug, Default)]
pub struct FolderOpts {
    pub read_only: bool,
    /// Host path below the root of the file `LOAD"*"` loads.
    pub boot: Option<String>,
    pub profile: TimingProfile,
}

/// Spec 873 — the device: its unit, its host, its line state and its DOS.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderDevice {
    pub unit: u8,
    pub root: PathBuf,
    pub profile: TimingProfile,
    /// Whether the C64's RESET reaches it (the 870 rule, default connected).
    pub reset_line_connected: bool,
    pub line: Line,
    pub dos: Dos,
    /// The machine's clock rate, for µs → cycles.
    pub cpu_hz: u32,
    #[serde(skip)]
    source: Option<Arc<dyn FolderSource>>,
    #[serde(skip)]
    events: Vec<FolderEvent>,
}

impl std::fmt::Debug for FolderDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FolderDevice").field("unit", &self.unit).field("root", &self.root).finish()
    }
}

impl FolderDevice {
    pub fn new(unit: u8, source: Arc<dyn FolderSource>, opts: &FolderOpts, cpu_hz: u32) -> FolderDevice {
        let boot = opts.boot.as_ref().map(|b| b.split('/').filter(|c| !c.is_empty() && *c != "..").map(String::from).collect());
        FolderDevice {
            unit,
            root: source.root(),
            profile: opts.profile,
            reset_line_connected: true,
            line: Line::released(),
            dos: Dos::new(opts.read_only, boot),
            cpu_hz,
            source: Some(source),
            events: Vec::new(),
        }
    }

    pub fn source(&self) -> Option<&Arc<dyn FolderSource>> {
        self.source.as_ref()
    }

    pub fn read_only(&self) -> bool {
        self.dos.read_only
    }

    /// Give a restored device its host again and re-open its channels (§9).
    pub fn reattach(&mut self, source: Arc<dyn FolderSource>) {
        self.dos.reopen(source.as_ref());
        self.source = Some(source);
    }

    /// The refusals since the last call, for the host to show.
    pub fn take_events(&mut self) -> Vec<FolderEvent> {
        std::mem::take(&mut self.events)
    }

    /// The status message as channel 15 would send it now, without reading it.
    pub fn status_text(&self) -> String {
        let s = &self.dos.status;
        format!("{:02},{},{:02},{:02}", s.code, s.text, s.track, s.sector)
    }

    /// The pull it drives into its slot.
    pub fn pull(&self) -> u8 {
        self.line.pull
    }

    /// The C64's RESET over the IEC RESET line, when connected.
    pub fn reset_from_c64(&mut self) {
        if self.reset_line_connected {
            self.reset();
        }
    }

    /// VICE `serial_iec_device_reset` + `fsdrive_reset`: lines released, flags and
    /// channel states cleared, channels closed, uncommitted writes dropped, back to
    /// the root, status 73.
    pub fn reset(&mut self) {
        let now = self.line.now;
        let (others, atn) = (self.line.others, self.line.atn_low);
        self.line = Line::released();
        self.line.now = now;
        self.line.others = others;
        self.line.atn_low = atn;
        self.dos.reset();
    }

    /// Detach: close every channel, writing what is pending.
    pub fn close_all(&mut self) {
        if let Some(src) = self.source.clone() {
            for c in 0..16u8 {
                self.dos.close_one(src.as_ref(), c);
            }
        }
    }

    #[inline]
    fn cyc(&self, us: u32) -> u64 {
        ((us as f64) * (self.cpu_hz as f64) / 1_000_000.0 + 0.5) as u64
    }

    /// The time the current state waits for, if it waits for one.
    fn due(&self) -> Option<u64> {
        let l = &self.line;
        if l.flags & (P_ATN | P_LISTENING) != 0 {
            match l.state {
                P_PRE0 | P_EOI | P_ACK => Some(l.timeout),
                P_READY if l.flags & P_ATN == 0 => Some(l.timeout),
                _ => None,
            }
        } else if l.flags & P_TALKING != 0 {
            match l.state {
                P_PRE1 | P_DONE0 | P_DONE1 | P_FRAMEERR0 => Some(l.timeout),
                s if (P_BIT0..=P_BIT7W).contains(&s) => Some(l.timeout),
                _ => None,
            }
        } else {
            None
        }
    }

    /// Run the device to C64 cycle `t`: every timed transition due before `t` under the
    /// lines it saw last, then the new lines at `t`, each settled until nothing moves.
    pub fn advance(&mut self, t: u64, others: u8, atn_low: bool) {
        let t = t.max(self.line.now);
        let mut guard = 0;
        while let Some(due) = self.due() {
            if due >= t || guard > 4096 {
                break;
            }
            let at = due.max(self.line.now);
            self.settle(at);
            guard += 1;
        }
        self.line.others = others;
        self.line.atn_low = atn_low;
        self.settle(t);
    }

    fn settle(&mut self, now: u64) {
        self.line.now = now;
        for _ in 0..64 {
            let before = (self.line.state, self.line.flags, self.line.pull, self.line.timeout, self.line.primary, self.line.secondary);
            self.step(now);
            let after = (self.line.state, self.line.flags, self.line.pull, self.line.timeout, self.line.primary, self.line.secondary);
            if before == after {
                break;
            }
        }
    }

    #[inline]
    fn write_bus(&mut self, v: u8) {
        self.line.pull = v;
    }

    fn event(&mut self, text: String) {
        self.events.push(FolderEvent { unit: self.unit, text });
    }

    /// One evaluation of VICE `serial_iec_device_exec_main` (serial-iec-device.c:241-743),
    /// with the profile's times and the DOS's peek/commit in place of `fsdrive_read`.
    fn step(&mut self, now: u64) {
        let Some(src) = self.source.clone() else { return };
        let src = src.as_ref();
        let unit = self.unit;
        let lines = self.line.others & self.line.pull;
        let data_high = lines & 0x80 != 0;
        let clk_high = lines & 0x40 != 0;
        let atn_high = !self.line.atn_low;

        if self.line.flags & P_ATN == 0 && !atn_high {
            // Falling ATN: "I am here", whatever it was doing.
            let settle = self.cyc(self.profile.t_atn_settle);
            let l = &mut self.line;
            l.state = P_PRE0;
            l.flags |= P_ATN;
            l.primary = 0;
            l.secondary_prev = l.secondary;
            l.secondary = 0;
            l.timeout = now + settle;
            self.write_bus(W_CLK);
        } else if self.line.flags & P_ATN != 0 && atn_high {
            // Rising ATN.
            self.line.flags &= !P_ATN;
            let (primary, secondary) = (self.line.primary, self.line.secondary);
            if primary == 0x20 + unit || primary == 0x40 + unit {
                let ch = secondary & 0x0f;
                match secondary & 0xf0 {
                    0xe0 => {
                        let r = self.dos.close(src, ch);
                        self.line.st[ch as usize] = r;
                    }
                    0xf0 => {
                        self.dos.open_begin(ch);
                        self.line.st[ch as usize] = 0;
                    }
                    _ => {}
                }
                if primary == 0x20 + unit {
                    self.line.flags &= !P_TALKING;
                    if self.line.st[ch as usize] == 0 {
                        self.line.flags |= P_LISTENING;
                        self.line.state = P_PRE1;
                    }
                    self.write_bus(W_CLK);
                } else {
                    self.line.flags &= !P_LISTENING;
                    self.line.flags |= P_TALKING;
                    self.line.state = P_PRE0;
                }
            } else if primary == 0x3f && self.line.flags & P_LISTENING != 0 {
                self.line.flags &= !P_LISTENING;
                let ch = self.line.secondary_prev & 0x0f;
                let mut ev = Vec::new();
                let r = self.dos.unlisten(src, ch, &mut ev);
                self.line.st[ch as usize] = r;
                for e in ev {
                    self.event(e);
                }
            } else if primary == 0x5f && self.line.flags & P_TALKING != 0 {
                self.line.flags &= !P_TALKING;
            }
            if self.line.flags & (P_LISTENING | P_TALKING) == 0 {
                self.write_bus(W_CLK | W_DATA);
            }
        }

        let p = self.profile;
        if self.line.flags & (P_ATN | P_LISTENING) != 0 {
            let under_atn = self.line.flags & P_ATN != 0;
            match self.line.state {
                P_PRE0 => {
                    if now >= self.line.timeout {
                        self.line.state = P_PRE1;
                    }
                }
                P_PRE1 => {
                    if !clk_high {
                        self.line.state = P_PRE2;
                    }
                }
                P_PRE2 => {
                    if clk_high {
                        // Ready-for-data. (A DOS that is not ready would hold DATA
                        // here; this one always is.)
                        self.write_bus(W_CLK | W_DATA);
                        self.line.timeout = now + self.cyc(p.t_eoi_detect);
                        self.line.state = P_READY;
                    }
                }
                P_READY => {
                    if !clk_high {
                        self.line.state = P_BIT0;
                    } else if !under_atn && now >= self.line.timeout {
                        self.write_bus(W_CLK);
                        self.line.state = P_EOI;
                        self.line.timeout = now + self.cyc(p.t_eoi_ack);
                    }
                }
                P_EOI => {
                    if now >= self.line.timeout {
                        self.write_bus(W_CLK | W_DATA);
                        self.line.state = P_EOIW;
                    }
                }
                P_EOIW => {
                    if !clk_high {
                        self.line.state = P_BIT0;
                    }
                }
                s if (P_BIT0..=P_BIT7W).contains(&s) && (s - P_BIT0).is_multiple_of(2) => {
                    if clk_high {
                        let bit = 1u8 << ((s - P_BIT0) / 2);
                        self.line.byte = (self.line.byte & !bit) | if data_high { bit } else { 0 };
                        self.line.state += 1;
                    }
                }
                s if (P_BIT0..P_BIT7W).contains(&s) => {
                    if !clk_high {
                        self.line.state += 1;
                    }
                }
                P_BIT7W => {
                    if !clk_high {
                        let b = self.line.byte;
                        let take = if under_atn {
                            if self.line.primary == 0 {
                                self.line.primary = b;
                            } else if self.line.secondary == 0 {
                                self.line.secondary = b;
                            }
                            let pr = self.line.primary;
                            pr == 0x3f || pr == 0x5f || (pr & 0x1f) == unit
                        } else {
                            let ch = self.line.secondary & 0x0f;
                            let ok = self.dos.write(src, ch, b).is_ok();
                            if !ok {
                                self.line.st[ch as usize] = 2;
                            }
                            ok
                        };
                        if !take {
                            self.line.state = P_DONE0;
                        } else {
                            let d = if under_atn { p.t_rx_ack_atn } else { p.t_rx_ack };
                            if d == 0 {
                                self.write_bus(W_CLK);
                                self.line.state = P_PRE2;
                            } else {
                                self.line.timeout = now + self.cyc(d);
                                self.line.state = P_ACK;
                            }
                        }
                    }
                }
                P_ACK if now >= self.line.timeout => {
                    self.write_bus(W_CLK);
                    self.line.state = P_PRE2;
                }
                _ => {} // P_DONE0: wait for ATN to rise.
            }
        } else if self.line.flags & P_TALKING != 0 {
            let ch = self.line.secondary & 0x0f;
            match self.line.state {
                P_PRE0 => {
                    if clk_high {
                        // Role reversal: CLK low, DATA released.
                        self.write_bus(W_DATA);
                        self.line.state = P_PRE1;
                        self.line.timeout = now + self.cyc(p.t_turnaround);
                    }
                }
                P_PRE1 => {
                    if now >= self.line.timeout {
                        self.write_bus(W_CLK | W_DATA);
                        self.line.state = P_READY;
                    }
                }
                P_READY => {
                    if data_high {
                        match self.dos.peek(src, ch) {
                            Some((b, last)) => {
                                self.line.byte = b;
                                self.line.last = last;
                                if last {
                                    // EOI: keep CLK released until the listener
                                    // acknowledges it.
                                    self.line.state = P_EOI;
                                } else {
                                    self.line.state = P_BIT0;
                                    self.line.timeout = now + self.cyc(p.t_bit_lead);
                                }
                            }
                            None => {
                                // Nothing to send: stop talking (FILE NOT FOUND).
                                self.line.flags &= !P_TALKING;
                            }
                        }
                    }
                }
                P_EOI => {
                    if !data_high {
                        self.line.state = P_EOIW;
                    }
                }
                P_EOIW => {
                    if data_high {
                        self.line.state = P_BIT0;
                        self.line.timeout = now + self.cyc(p.t_bit_lead);
                    }
                }
                s if (P_BIT0..=P_BIT7W).contains(&s) && (s - P_BIT0).is_multiple_of(2) => {
                    if now >= self.line.timeout {
                        let bit = 1u8 << ((s - P_BIT0) / 2);
                        self.write_bus(if self.line.byte & bit != 0 { W_DATA } else { 0 });
                        self.line.timeout = now + self.cyc(p.t_bit_low);
                        self.line.state += 1;
                    }
                }
                s if (P_BIT0..=P_BIT7W).contains(&s) => {
                    if now >= self.line.timeout {
                        let pull = self.line.pull | W_CLK;
                        self.write_bus(pull);
                        self.line.timeout = now + self.cyc(p.t_bit_high);
                        self.line.state += 1;
                    }
                }
                P_DONE0 => {
                    if now >= self.line.timeout {
                        self.write_bus(W_DATA);
                        self.line.timeout = now + self.cyc(p.t_frame_ack);
                        self.line.state = P_DONE1;
                    }
                }
                P_DONE1 => {
                    if !data_high {
                        self.dos.commit(ch);
                        if self.line.last {
                            self.line.flags &= !P_TALKING;
                            self.line.last = false;
                            self.write_bus(W_CLK | W_DATA);
                        } else {
                            self.line.timeout = now + self.cyc(p.t_byte_gap);
                            self.line.state = P_PRE1;
                        }
                    } else if now >= self.line.timeout {
                        self.write_bus(W_CLK | W_DATA);
                        self.line.timeout = now + self.cyc(100);
                        self.line.state = P_FRAMEERR0;
                    }
                }
                P_FRAMEERR0 => {
                    if now >= self.line.timeout {
                        self.write_bus(W_DATA);
                        self.line.state = P_FRAMEERR1;
                    }
                }
                P_FRAMEERR1 if !data_high => {
                    self.line.timeout = now;
                    self.line.state = P_PRE1;
                }
                _ => {}
            }
        }
    }

    /// For the monitor: protocol state, open channels, last status.
    pub fn describe(&self) -> String {
        let l = &self.line;
        let role = if l.flags & P_ATN != 0 {
            "under ATN"
        } else if l.flags & P_TALKING != 0 {
            "talker"
        } else if l.flags & P_LISTENING != 0 {
            "listener"
        } else {
            "idle"
        };
        let pull = |bit: u8, name: &str| if l.pull & bit == 0 { format!("{name} low") } else { format!("{name} rel") };
        let mut s = format!(
            "folder unit {}: {} ({}{}), profile {}\n  line: {role}, state {}, flags ${:02X}, primary ${:02X}, secondary ${:02X}, {} {}\n  cwd: /{}\n  status: {}\n",
            self.unit,
            self.root.display(),
            if self.dos.read_only { "read-only" } else { "writable" },
            if self.reset_line_connected { "" } else { ", reset line cut" },
            self.profile.name(),
            l.state,
            l.flags,
            l.primary,
            l.secondary,
            pull(W_CLK, "CLK"),
            pull(W_DATA, "DATA"),
            self.dos.cwd.join("/"),
            self.status_text()
        );
        for (i, c) in self.dos.channels.iter().enumerate() {
            let d = match c {
                Channel::Closed => continue,
                Channel::Read { path, pos, ftype } => format!("read {:?} {} @{pos}", ftype, path.join("/")),
                Channel::Write { path, pos, ftype, pending } => {
                    format!("write {:?} {} @{} (+{} pending)", ftype, path.join("/"), pos, pending.len())
                }
                Channel::Dir { bytes, pos } => format!("directory {pos}/{}", bytes.len()),
            };
            s.push_str(&format!("  channel {i}: {d}\n"));
        }
        s
    }
}

// ── on the bus (§4) — Spec 874: the first `IecDevice` ─────────────────────────────

/// Spec 874 §9 — the folder as an [`IecDevice`], byte-identical with Spec 873's direct
/// calls: `clock_to` is `advance` with the byte Spec 873's sync built (`0x3f` | CLK | DATA), `outputs` is its
/// pull, `rebase` is what attach and reattach set by hand.
impl crate::iec_device::IecDevice for FolderDevice {
    fn name(&self) -> String {
        format!("folder {}", self.unit)
    }

    fn clock_to(&mut self, clk: u64, bus: crate::iec_device::IecLines) {
        let others = 0x3f | ((bus.clk as u8) << 6) | ((bus.data as u8) << 7);
        self.advance(clk, others, !bus.atn);
    }

    fn outputs(&self) -> crate::iec_device::IecOut {
        crate::iec_device::IecOut { clk: self.line.pull & W_CLK == 0, data: self.line.pull & W_DATA == 0 }
    }

    fn rebase(&mut self, clk: u64, bus: crate::iec_device::IecLines) {
        self.line.now = clk;
        self.line.timeout = self.line.timeout.min(clk);
        self.line.atn_low = !bus.atn;
    }

    fn units(&self) -> u16 {
        1 << self.unit
    }

    fn set_cpu_hz(&mut self, hz: u32) {
        self.cpu_hz = hz;
    }

    fn c64_reset(&mut self) {
        self.reset_from_c64();
    }

    fn clone_device(&self) -> Option<Box<dyn crate::iec_device::IecDevice>> {
        Some(Box::new(self.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_every_byte() {
        for b in 0u8..=255 {
            let host = petscii_to_host(&[b]);
            assert_eq!(host_to_petscii(&host), vec![b], "byte ${b:02X} via {host:?}");
        }
        assert_eq!(petscii_to_host(b"HELLO"), "hello");
        assert_eq!(petscii_to_host(&[0xc8, 0x49]), "Hi");
        assert_eq!(petscii_to_host(b"A/B%"), "a%2Fb%25");
        assert_eq!(host_to_petscii("title.bin"), b"TITLE.BIN".to_vec());
    }

    #[test]
    fn wildcards_as_a_1541() {
        assert!(matches(b"FOO*", b"FOO"));
        assert!(matches(b"FOO*", b"FOOBAR"));
        assert!(matches(b"F?O", b"FOO"));
        assert!(!matches(b"F?O", b"FO"));
        assert!(matches(b"*", b"ANY"));
        assert!(!matches(b"FOO", b"FOOBAR"));
        assert!(matches(b"foo", &[0xc6, 0xcf, 0xcf]) || matches(&[0x46, 0x4f, 0x4f], &[0xc6, 0xcf, 0xcf]));
    }

    #[test]
    fn name_parsing() {
        assert_eq!(Dos::parse_name(b"@0:NEW,P,W"), (true, b"NEW".to_vec(), Some(FileType::Prg), Some(b'W')));
        assert_eq!(Dos::parse_name(b"F,S,W"), (false, b"F".to_vec(), Some(FileType::Seq), Some(b'W')));
        assert_eq!(Dos::parse_name(b"F,S,R"), (false, b"F".to_vec(), Some(FileType::Seq), Some(b'R')));
        assert_eq!(Dos::parse_name(b"F,R"), (false, b"F".to_vec(), None, Some(b'R')));
        assert_eq!(Dos::parse_name(b"F,W"), (false, b"F".to_vec(), None, Some(b'W')));
        assert_eq!(Dos::parse_name(b"0:NAME"), (false, b"NAME".to_vec(), None, None));
    }
}
