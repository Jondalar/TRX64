//! Spec 910 D1 — a daemon that never writes a medium back into the file it was handed.
//!
//! `--scratch <DIR>` turns it on. Every medium the daemon is handed by a path (a disk, a
//! cartridge, a folder, whatever door and whenever) is opened from a COPY in `<DIR>`, and
//! every write a medium makes goes only into that copy. The machine inside sees its own
//! writes (a SAVE to a disk, a flash write to an EasyFlash) because the copy is the file it
//! reads back from; the file the caller named is never opened for writing.
//!
//! Two layers, so that a door added later cannot slip past:
//! - the READ side, [`open_medium`] / [`open_folder`]: the one function a path that names a
//!   medium goes through before the daemon reads it. A path already inside `<DIR>` passes
//!   through; the same source mounted twice is the same copy, so a remount sees the writes.
//! - the WRITE side, [`sink`]: the one function every write of a medium file goes through.
//!   Whatever path a medium carries as its backing file (also one that came out of a snapshot
//!   or a checkpoint, never through a door), the write lands in `<DIR>`.
//!
//! `tests/no_writeback.rs` reads `main.rs` and fails when a path-taking door or a file write
//! is not on one of the two.
//!
//! Without the option every function here returns its argument unchanged: the shared
//! machine keeps its write-back.

use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub struct Scratch {
    dir: PathBuf,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    next: u64,
    /// canonical source -> its copy in the scratch dir.
    copies: HashMap<PathBuf, PathBuf>,
    /// every path the daemon was handed (canonical): none of them is ever written.
    originals: HashSet<PathBuf>,
}

/// The identity of a path: the real file when it exists, else the absolute lexical path.
fn key(p: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(p) {
        return c;
    }
    if p.is_absolute() {
        return p.to_path_buf();
    }
    std::env::current_dir().map(|c| c.join(p)).unwrap_or_else(|_| p.to_path_buf())
}

fn base_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).filter(|n| !n.is_empty()).unwrap_or_else(|| "medium".to_string())
}

fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let (from, to) = (e.path(), dst.join(e.file_name()));
        if from.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            copy_writable(&from, &to)?;
        }
    }
    Ok(())
}

/// A copy the machine can write to, whatever the source's permissions were.
fn copy_writable(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::copy(from, to)?;
    let mut perm = std::fs::metadata(to)?.permissions();
    if perm.readonly() {
        #[allow(clippy::permissions_set_readonly_false)]
        perm.set_readonly(false);
        std::fs::set_permissions(to, perm)?;
    }
    Ok(())
}

impl Scratch {
    pub fn new(dir: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let dir = std::fs::canonicalize(dir).map_err(|e| format!("cannot resolve {}: {e}", dir.display()))?;
        Ok(Self { dir, inner: Mutex::new(Inner::default()) })
    }

    /// Every medium handed in, as {original, copy}, in the order the copies were made.
    pub fn copies_json(&self) -> Vec<Value> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut v: Vec<(&PathBuf, &PathBuf)> = inner.copies.iter().collect();
        v.sort_by(|a, b| a.1.cmp(b.1));
        v.into_iter()
            .map(|(o, c)| json!({ "original": o.to_string_lossy(), "copy": c.to_string_lossy() }))
            .collect()
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn inside(&self, p: &Path) -> bool {
        key(p).starts_with(&self.dir)
    }

    fn alloc(&self, inner: &mut Inner, src: &Path) -> PathBuf {
        inner.next += 1;
        let copy = self.dir.join(format!("{:03}-{}", inner.next, base_name(src)));
        inner.copies.insert(src.to_path_buf(), copy.clone());
        inner.originals.insert(src.to_path_buf());
        copy
    }

    /// The path to read a medium from. A file that does not exist passes through (the door
    /// reports it by the name the caller gave); the write side covers it if bytes arrive later.
    pub fn open_medium(&self, path: &str) -> Result<String, String> {
        let p = Path::new(path);
        if path.is_empty() || self.inside(p) || !p.is_file() {
            return Ok(path.to_string());
        }
        let k = key(p);
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = inner.copies.get(&k) {
            if c.is_file() {
                return Ok(c.to_string_lossy().to_string());
            }
        }
        let copy = match inner.copies.get(&k) {
            Some(c) => c.clone(),
            None => self.alloc(&mut inner, &k),
        };
        copy_writable(&k, &copy).map_err(|e| format!("scratch: cannot copy {} to {}: {e}", k.display(), copy.display()))?;
        Ok(copy.to_string_lossy().to_string())
    }

    /// The same for a host folder served as a device: the whole tree is copied.
    pub fn open_folder(&self, path: &str) -> Result<String, String> {
        let p = Path::new(path);
        if path.is_empty() || self.inside(p) || !p.is_dir() {
            return Ok(path.to_string());
        }
        let k = key(p);
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = inner.copies.get(&k) {
            if c.is_dir() {
                return Ok(c.to_string_lossy().to_string());
            }
        }
        let copy = match inner.copies.get(&k) {
            Some(c) => c.clone(),
            None => self.alloc(&mut inner, &k),
        };
        copy_tree(&k, &copy).map_err(|e| format!("scratch: cannot copy {} to {}: {e}", k.display(), copy.display()))?;
        Ok(copy.to_string_lossy().to_string())
    }

    /// The path a write of a medium file goes to. Always inside the scratch dir.
    pub fn sink(&self, path: &str) -> String {
        let p = Path::new(path);
        if self.inside(p) {
            return path.to_string();
        }
        let k = key(p);
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = inner.copies.get(&k) {
            return c.to_string_lossy().to_string();
        }
        let copy = self.alloc(&mut inner, &k);
        trx64_core::elog!("[trx64] --scratch: a write to {} was redirected to {}", k.display(), copy.display());
        copy.to_string_lossy().to_string()
    }

    /// `Err` for a path that is a file the daemon was handed.
    pub fn refuse_original(&self, path: &str) -> Result<(), String> {
        let k = key(Path::new(path));
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.originals.contains(&k) {
            return Err(format!(
                "{path} is a medium this daemon was handed; it runs with --scratch and never writes into it"
            ));
        }
        Ok(())
    }

    /// A fresh directory in the scratch dir for what a door writes out (an undump's media).
    pub fn fresh_dir(&self, name: &str) -> PathBuf {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.next += 1;
        self.dir.join(format!("{:03}-{name}", inner.next))
    }
}

static GLOBAL: OnceLock<Scratch> = OnceLock::new();

/// Turn the option on for this process. Called once, from `main`, before a request is served.
pub fn enable(dir: &str) -> Result<(), String> {
    let s = Scratch::new(Path::new(dir))?;
    GLOBAL.set(s).map_err(|_| "--scratch given twice".to_string())
}

pub fn get() -> Option<&'static Scratch> {
    GLOBAL.get()
}

pub fn active() -> bool {
    GLOBAL.get().is_some()
}

pub fn open_medium(path: &str) -> Result<String, String> {
    match get() {
        Some(s) => s.open_medium(path),
        None => Ok(path.to_string()),
    }
}

pub fn open_folder(path: &str) -> Result<String, String> {
    match get() {
        Some(s) => s.open_folder(path),
        None => Ok(path.to_string()),
    }
}

pub fn sink(path: &str) -> String {
    match get() {
        Some(s) => s.sink(path),
        None => path.to_string(),
    }
}

pub fn refuse_original(path: &str) -> Result<(), String> {
    match get() {
        Some(s) => s.refuse_original(path),
        None => Ok(()),
    }
}

/// Where a door that writes a directory of media puts it: `wanted` normally, a fresh
/// directory in the scratch dir under the option.
pub fn media_dir(wanted: PathBuf) -> PathBuf {
    match get() {
        Some(s) => s.fresh_dir(&base_name(&wanted)),
        None => wanted,
    }
}

/// What `session/state` reports.
pub fn status() -> Value {
    // `scratchCopies`: every medium the daemon was handed, as {original, copy}. The daemon reports a mounted medium by the
    // copy's name (`001-name`); this is how a client finds the identity of what it handed in.
    let media: Vec<Value> = get().map(|s| s.copies_json()).unwrap_or_default();
    json!({
        "noWriteback": active(),
        "scratchDir": get().map(|s| s.dir().to_string_lossy().to_string()),
        "scratchCopies": media,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("trx64_scratch_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_medium_is_opened_from_one_copy_and_never_from_the_original() {
        let d = tmp("open");
        let src = d.join("a.d64");
        std::fs::write(&src, b"original").unwrap();
        let s = Scratch::new(&d.join("scratch")).unwrap();
        let c1 = s.open_medium(src.to_str().unwrap()).unwrap();
        assert_ne!(Path::new(&c1), src);
        assert!(Path::new(&c1).starts_with(s.dir()));
        assert_eq!(std::fs::read(&c1).unwrap(), b"original");
        // The machine writes; a second mount of the same source sees it.
        std::fs::write(&c1, b"written").unwrap();
        let c2 = s.open_medium(src.to_str().unwrap()).unwrap();
        assert_eq!(c1, c2);
        assert_eq!(std::fs::read(&c2).unwrap(), b"written");
        // The copy opens as itself.
        assert_eq!(s.open_medium(&c1).unwrap(), c1);
        assert_eq!(std::fs::read(&src).unwrap(), b"original");
        // The identity of what was handed in survives the copy: the copy's name carries a prefix, the list names the original.
        let list = s.copies_json();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["copy"], c1.as_str());
        assert_eq!(Path::new(list[0]["original"].as_str().unwrap()).file_name().unwrap(), "a.d64");
        assert!(Path::new(&c1).file_name().unwrap().to_string_lossy().ends_with("-a.d64"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_write_to_any_path_lands_in_the_scratch_dir() {
        let d = tmp("sink");
        let s = Scratch::new(&d.join("scratch")).unwrap();
        let elsewhere = d.join("never.crt");
        let t = s.sink(elsewhere.to_str().unwrap());
        assert!(Path::new(&t).starts_with(s.dir()), "{t}");
        assert_eq!(s.sink(elsewhere.to_str().unwrap()), t, "same path, same target");
        assert!(s.refuse_original(elsewhere.to_str().unwrap()).is_err());
        assert!(!elsewhere.exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_folder_is_copied_whole() {
        let d = tmp("folder");
        let src = d.join("f");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("A"), b"a").unwrap();
        std::fs::write(src.join("sub").join("B"), b"b").unwrap();
        let s = Scratch::new(&d.join("scratch")).unwrap();
        let c = s.open_folder(src.to_str().unwrap()).unwrap();
        assert_eq!(std::fs::read(Path::new(&c).join("sub").join("B")).unwrap(), b"b");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_missing_file_passes_through_for_the_door_to_report() {
        let d = tmp("missing");
        let s = Scratch::new(&d.join("scratch")).unwrap();
        let m = d.join("nope.d64");
        assert_eq!(s.open_medium(m.to_str().unwrap()).unwrap(), m.to_str().unwrap());
        let _ = std::fs::remove_dir_all(&d);
    }
}
