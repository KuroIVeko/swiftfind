//! Background thread that builds the index and keeps it in sync with the
//! USN journal of every NTFS volume.
//!
//! Startup: load the on-disk cache; for each volume whose journal still covers
//! the cached position, replay the journal from there. Otherwise do a full MFT
//! scan. The cache is rewritten after full scans, periodically while changes
//! arrive, and on exit.

use crate::index::{Index, VolumeScan};
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
    let scan = vol.enumerate()?;
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
    let mut index = shared.index.write();
    for c in &changes {
        match c {
            Change::Upsert(r) => index.upsert(vol.letter, r),
            Change::Removed(frn) => index.remove(vol.letter, *frn),
        }
    }
    // Keep the recorded journal position in step with the applied changes, so
    // a snapshot is always consistent with the USN it claims.
    index.set_next_usn(vol.letter, vol.next_usn);
    if !changes.is_empty() {
        shared.dirty.store(true, Ordering::SeqCst);
    }
}
