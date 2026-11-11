//! Background thread that builds the index and keeps it in sync with the
//! USN journal of every NTFS volume.
//!
//! Startup: load the on-disk cache; for each volume whose journal still covers
//! the cached position, replay the journal from there. Otherwise do a full MFT
//! scan. The cache is rewritten after full scans, periodically while changes
//! arrive, and on exit.

use crate::index::{Index, Stat, VolumeScan};
use crate::ntfs::{Change, JournalError, NtfsVolume, ntfs_drives};
use crate::persist;
use parking_lot::{Mutex, RwLock};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const SAVE_INTERVAL: Duration = Duration::from_secs(5 * 60);

#[derive(Default)]
pub struct Shared {
    pub index: RwLock<Index>,
    pub status: Mutex<String>,
    /// Index changed since the cache was last written.
    dirty: AtomicBool,
    /// Serializes cache writes (indexer thread vs. exit).
    save_lock: Mutex<()>,
}

impl Shared {
    fn set_status(&self, s: impl Into<String>) {
        *self.status.lock() = s.into();
    }

    /// Writes the index to the on-disk cache if it changed.
    pub fn save_if_dirty(&self) {
        let _guard = self.save_lock.lock();
        if !self.dirty.swap(false, Ordering::SeqCst) {
            return;
        }
        let t = Instant::now();
        // Streams straight from the index under a read lock (~0.2 s); only
        // the indexer thread writes, and it is either us or briefly blocked.
        let result = {
            let index = self.index.read();
            persist::save(&persist::cache_path(), &index.live_view())
        };
        match result {
            Ok(()) => log(format!("saved cache in {:?}", t.elapsed())),
            Err(e) => {
                self.dirty.store(true, Ordering::SeqCst);
                log(format!("save cache: {e:#}"));
            }
        }
    }
}

/// Logs to stderr and to `%TEMP%\swiftfind.log` (release builds have no console).
pub fn log(msg: String) {
    use std::io::Write;
    let (private, ws, peak) = crate::os::memory_mb();
    let msg = format!("{msg}  [private {private} MB, ws {ws} MB, peak ws {peak} MB]");
    eprintln!("{msg}");
    let path = std::env::temp_dir().join("swiftfind.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{} {msg}", chrono::Local::now().format("%H:%M:%S%.3f"));
    }
}

/// Starts the indexer thread. `on_ready` runs once the initial index is built.
pub fn start(shared: Arc<Shared>, on_ready: Option<Box<dyn FnOnce() + Send>>) {
    std::thread::Builder::new()
        .name("indexer".into())
        .spawn(move || run(shared, on_ready))
        .expect("spawn indexer thread");
}

fn full_scan(shared: &Shared, vol: &mut NtfsVolume) -> anyhow::Result<()> {
    shared.set_status(format!("正在索引 {}: ...", vol.letter));
    let t = Instant::now();
    let scan = match vol.scan_mft() {
        Ok(scan) => scan,
        Err(e) => {
            log(format!("raw MFT scan of {}: failed ({e:#}), using FSCTL_ENUM_USN_DATA", vol.letter));
            vol.enumerate()?
        }
    };
    let n = scan.len();
    let t_enum = t.elapsed();
    shared.index.write().add_volume(scan);
    shared.dirty.store(true, Ordering::SeqCst);
    log(format!(
        "indexed {}: {} records in {:?} (read MFT {:?}, build index {:?})",
        vol.letter,
        n,
        t.elapsed(),
        t_enum,
        t.elapsed() - t_enum
    ));
    Ok(())
}

fn load_cache() -> Vec<VolumeScan> {
    let t = Instant::now();
    match persist::load(&persist::cache_path()) {
        Ok(scans) => {
            let n: usize = scans.iter().map(|s| s.len()).sum();
            log(format!("loaded cache: {n} records in {:?}", t.elapsed()));
            scans
        }
        Err(e) => {
            log(format!("no usable cache: {e:#}"));
            Vec::new()
        }
    }
}

fn run(shared: Arc<Shared>, on_ready: Option<Box<dyn FnOnce() + Send>>) {
    let t_start = Instant::now();
    log("start".into());
    let mut cached = load_cache();
    let mut volumes = Vec::new();
    let mut errors = Vec::new();
    for letter in ntfs_drives() {
        let mut vol = match NtfsVolume::open(letter) {
            Ok(v) => v,
            Err(e) => {
                log(format!("volume {letter}: {e:#}"));
                errors.push(format!("{letter}: {e:#}"));
                continue;
            }
        };
        let from_cache = cached
            .iter()
            .position(|s| s.letter == letter)
            .map(|i| cached.swap_remove(i))
            .filter(|s| {
                let ok = vol.resume_from(&s.meta);
                if !ok {
                    log(format!("cache for {letter}: is stale, rescanning"));
                }
                ok
            });
        if let Some(scan) = from_cache {
            let t = Instant::now();
            shared.index.write().add_volume(scan);
            log(format!("{letter}: restored from cache in {:?}", t.elapsed()));
            // Replay what happened while we weren't running.
            sync_volume(&shared, &mut vol);
            volumes.push(vol);
            continue;
        }
        match full_scan(&shared, &mut vol) {
            Ok(()) => volumes.push(vol),
            Err(e) => {
                log(format!("volume {letter}: {e:#}"));
                errors.push(format!("{letter}: {e:#}"));
            }
        }
    }
    drop(cached);
    log(format!("ready in {:?}", t_start.elapsed()));
    log(shared.index.read().memory_report());

    let letters: String = volumes.iter().map(|v| format!("{}: ", v.letter)).collect();
    if errors.is_empty() {
        shared.set_status(format!("已索引 {letters}"));
    } else {
        shared.set_status(format!("已索引 {letters}| 失败: {}", errors.join("; ")));
    }
    shared.save_if_dirty();
    if let Some(done) = on_ready {
        done();
    }

    let mut last_save = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(500));
        for vol in &mut volumes {
            sync_volume(&shared, vol);
        }
        // Only this thread mutates the index, so nothing can change between
        // building the compacted copy and swapping it in.
        if shared.index.read().needs_compaction() {
            compact(&shared);
        }
        if last_save.elapsed() >= SAVE_INTERVAL {
            shared.save_if_dirty();
            last_save = Instant::now();
        }
    }
}

pub fn compact(shared: &Shared) {
    log(format!("compacting: {}", shared.index.read().memory_report()));
    let t = Instant::now();
    let fresh = shared.index.read().compacted();
    let t_build = t.elapsed();
    let old = std::mem::replace(&mut *shared.index.write(), fresh);
    // Bump after the swap so the UI re-runs its query against the new epoch.
    shared.index.read().generation.fetch_add(1, Ordering::Relaxed);
    drop(old);
    shared.dirty.store(true, Ordering::SeqCst);
    log(format!(
        "compacted in {:?} (build {t_build:?}): {}",
        t.elapsed(),
        shared.index.read().memory_report()
    ));
}

/// Applies all pending journal entries of one volume to the index.
fn sync_volume(shared: &Shared, vol: &mut NtfsVolume) {
    let mut changes = Vec::new();
    match vol.read_changes(|c| changes.push(c)) {
        Ok(()) => {}
        Err(JournalError::Reset) => {
            log(format!("USN journal reset on {}:, re-indexing", vol.letter));
            if let Err(e) = full_scan(shared, vol) {
                log(format!("re-index {}: {e:#}", vol.letter));
            }
            return;
        }
        Err(JournalError::Other(e)) => {
            log(format!("read journal {}: {e:#}", vol.letter));
            return;
        }
    }
    // Entries whose size/mtime must be re-read: changed files, plus the
    // directories they were created in / removed from.
    let mut touched = Vec::new();
    {
        let mut index = shared.index.write();
        for c in &changes {
            match c {
                Change::Upsert(r) => {
                    // Could be a create or a move: refresh the parent too.
                    if let Some(idx) = index.upsert(vol.letter, r) {
                        touched.extend([idx, index.parent(idx)]);
                    }
                }
                Change::Removed(frn) => touched.extend(index.remove(vol.letter, *frn)),
                Change::Modified(frn) => touched.extend(index.entry_of(vol.letter, *frn)),
            }
        }
        // Keep the recorded journal position in step with the applied changes, so
        // a snapshot is always consistent with the USN it claims.
        index.set_next_usn(vol.letter, vol.next_usn);
    }
    if changes.is_empty() {
        return;
    }
    shared.dirty.store(true, Ordering::SeqCst);

    // Stat outside the lock. Only this thread mutates the index, so the
    // entry indices stay valid in between.
    touched.sort_unstable();
    touched.dedup();
    let paths: Vec<(u32, String)> = {
        let index = shared.index.read();
        touched.iter().filter(|&&i| index.is_alive(i)).map(|&i| (i, index.full_path(i))).collect()
    };
    let stats: Vec<(u32, Stat)> = paths.iter().filter_map(|(i, p)| Some((*i, Stat::from_fs(p)?))).collect();
    let mut index = shared.index.write();
    for (i, st) in stats {
        index.set_stat(i, st);
    }
}


/// Diagnostic: scans every volume both ways and reports differences.
pub fn compare_scans() {
    use std::collections::HashMap;
    for letter in ntfs_drives() {
        let mut vol = match NtfsVolume::open(letter) {
            Ok(v) => v,
            Err(e) => {
                log(format!("compare {letter}: {e:#}"));
                continue;
            }
        };
        let t = Instant::now();
        let mft = vol.scan_mft();
        let t_mft = t.elapsed();
        let t = Instant::now();
        let usn = vol.enumerate();
        let t_usn = t.elapsed();
        let (mft, usn) = match (mft, usn) {
            (Ok(a), Ok(b)) => (a, b),
            (a, b) => {
                log(format!("compare {letter}: mft {:?} / usn {:?}", a.err(), b.err()));
                continue;
            }
        };
        log(format!("compare {letter}: raw MFT {} records in {t_mft:?}, USN enum {} records in {t_usn:?}", mft.len(), usn.len()));
        let map = |s: &VolumeScan| -> HashMap<u64, (u64, String, bool)> {
            s.recs.iter().map(|r| (r.rec, (r.parent, s.name(r).to_owned(), r.is_dir))).collect()
        };
        let (a, b) = (map(&mft), map(&usn));
        let only_mft: Vec<_> = a.iter().filter(|(k, _)| !b.contains_key(k)).collect();
        let only_usn: Vec<_> = b.iter().filter(|(k, _)| !a.contains_key(k)).collect();
        let differ: Vec<_> = a.iter().filter_map(|(k, v)| b.get(k).filter(|w| *w != v).map(|w| (k, v, w))).collect();
        log(format!("  only in MFT: {}, only in USN: {}, different: {}", only_mft.len(), only_usn.len(), differ.len()));
        for (k, v) in only_mft.iter().take(15) {
            log(format!("  mft-only #{k}: {v:?}"));
        }
        for (k, v) in only_usn.iter().take(15) {
            log(format!("  usn-only #{k}: {v:?}"));
        }
        for (k, v, w) in differ.iter().take(15) {
            log(format!("  diff #{k}: mft {v:?} usn {w:?}"));
        }
    }
    log("compare: done".into());
}
