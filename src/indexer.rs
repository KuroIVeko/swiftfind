//! Background thread that builds the index and keeps it in sync with the
//! USN journal of every NTFS volume.

use crate::index::Index;
use crate::ntfs::{Change, JournalError, NtfsVolume, ntfs_drives};
use parking_lot::{Mutex, RwLock};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Shared {
    pub index: RwLock<Index>,
    pub status: Mutex<String>,
}

impl Shared {
    fn set_status(&self, s: impl Into<String>) {
        *self.status.lock() = s.into();
    }
}

pub fn start(shared: Arc<Shared>) {
    std::thread::Builder::new()
        .name("indexer".into())
        .spawn(move || run(shared))
        .expect("spawn indexer thread");
}

fn full_scan(shared: &Shared, vol: &mut NtfsVolume) -> anyhow::Result<()> {
    shared.set_status(format!("正在索引 {}: ...", vol.letter));
    let t = Instant::now();
    let records = vol.enumerate()?;
    let n = records.len();
    shared.index.write().add_volume(vol.letter, vol.root_frn, records);
    eprintln!("indexed {}: {} records in {:?}", vol.letter, n, t.elapsed());
    Ok(())
}

fn run(shared: Arc<Shared>) {
    let mut volumes = Vec::new();
    let mut errors = Vec::new();
    for letter in ntfs_drives() {
        match NtfsVolume::open(letter).and_then(|mut v| full_scan(&shared, &mut v).map(|_| v)) {
            Ok(v) => volumes.push(v),
            Err(e) => {
                eprintln!("volume {letter}: {e:#}");
                errors.push(format!("{letter}: {e:#}"));
            }
        }
    }
    let letters: String = volumes.iter().map(|v| format!("{}: ", v.letter)).collect();
    if errors.is_empty() {
        shared.set_status(format!("已索引 {letters}"));
    } else {
        shared.set_status(format!("已索引 {letters}| 失败: {}", errors.join("; ")));
    }

    loop {
        std::thread::sleep(Duration::from_millis(500));
        for vol in &mut volumes {
            let mut changes = Vec::new();
            match vol.read_changes(|c| changes.push(c)) {
                Ok(()) => {}
                Err(JournalError::Reset) => {
                    eprintln!("USN journal reset on {}:, re-indexing", vol.letter);
                    if let Err(e) = vol.reset_journal().and_then(|_| full_scan(&shared, vol)) {
                        eprintln!("re-index {}: {e:#}", vol.letter);
                    }
                    continue;
                }
                Err(JournalError::Other(e)) => {
                    eprintln!("read journal {}: {e:#}", vol.letter);
                    continue;
                }
            }
            if changes.is_empty() {
                continue;
            }
            let mut index = shared.index.write();
            for c in &changes {
                match c {
                    Change::Upsert(r) => index.upsert(vol.letter, r),
                    Change::Removed(frn) => index.remove(vol.letter, *frn),
                }
            }
        }
    }
}
