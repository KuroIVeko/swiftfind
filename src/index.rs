//! In-memory file index shared by all NTFS volumes.
//!
//! Layout is kept compact: every entry is a 24-byte struct (name, parent,
//! size, modified time) that points into a single string arena of names. Matching and sorting are case-insensitive on
//! the fly (no lower-cased copy, which would cost ~70 MB for 3M files).
//! `order` holds the indices of all live entries sorted case-insensitively by
//! name, so search results come out already sorted without a per-query sort.

pub mod cache;
mod filter;

use filter::Filter;

use memchr::memmem;
use rayon::prelude::*;
use std::cmp::Ordering;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

pub const NONE: u32 = u32::MAX;

const FLAG_DIR: u8 = 1;
const FLAG_ALIVE: u8 = 2;
const FLAG_ROOT: u8 = 4;
/// Inserted or renamed during an open batch; its place in `order` is fixed
/// up by [`Index::end_batch`].
const FLAG_PENDING: u8 = 8;

/// One file or directory. Plain old data with no padding, so the on-disk
/// cache can store `entries` as raw bytes.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
#[repr(C)]
struct Entry {
    name_off: u32,
    parent: u32,
    /// Unix seconds; 0 if unknown.
    mtime: u32,
    /// Size in bytes is `size_hi << 32 | size_lo` (48 bits; all ones = unknown).
    size_lo: u32,
    size_hi: u16,
    /// Name length in bytes (low 10 bits) and `FLAG_*` bits (high 6 bits).
    meta: u16,
}

// Entry count is in the millions; keep it small.
const _: () = assert!(size_of::<Entry>() == 20);

/// NTFS names are at most 255 UTF-16 units, i.e. at most 765 UTF-8 bytes.
const MAX_NAME_LEN: usize = (1 << 10) - 1;
const SIZE_UNKNOWN: u64 = (1 << 48) - 1;

impl Entry {
    fn new(name_off: u32, name_len: u16, parent: u32, flags: u8, stat: Stat) -> Entry {
        let mut e = Entry { name_off, parent, mtime: stat.mtime, ..Default::default() };
        e.meta = name_len | (flags as u16) << 10;
        e.set_size(stat.size);
        e
    }

    #[inline]
    fn name_len(&self) -> usize {
        (self.meta & MAX_NAME_LEN as u16) as usize
    }

    #[inline]
    fn flags(&self) -> u8 {
        (self.meta >> 10) as u8
    }

    fn set_flag(&mut self, f: u8) {
        self.meta |= (f as u16) << 10;
    }

    fn clear_flag(&mut self, f: u8) {
        self.meta &= !((f as u16) << 10);
    }

    fn set_name(&mut self, off: u32, len: u16) {
        self.name_off = off;
        self.meta = (self.meta & !(MAX_NAME_LEN as u16)) | len;
    }

    /// Bytes, or `u64::MAX` if unknown.
    #[inline]
    fn size(&self) -> u64 {
        match (self.size_hi as u64) << 32 | self.size_lo as u64 {
            SIZE_UNKNOWN => u64::MAX,
            s => s,
        }
    }

    fn set_size(&mut self, size: u64) {
        let s = if size == u64::MAX { SIZE_UNKNOWN } else { size.min(SIZE_UNKNOWN - 1) };
        (self.size_hi, self.size_lo) = ((s >> 32) as u16, s as u32);
    }
}

/// File size and last-write time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    /// Bytes; `u64::MAX` if unknown (directories, or not read yet).
    pub size: u64,
    /// Unix seconds; 0 if unknown.
    pub mtime: u32,
}

impl Stat {
    pub const UNKNOWN: Stat = Stat { size: u64::MAX, mtime: 0 };

    /// Converts a Windows FILETIME (100 ns ticks since 1601).
    pub fn mtime_from_filetime(ft: u64) -> u32 {
        const EPOCH_DIFF: u64 = 11_644_473_600;
        (ft / 10_000_000).saturating_sub(EPOCH_DIFF).clamp(1, u32::MAX as u64) as u32
    }

    /// Reads size and mtime from the file system (used for journal changes).
    pub fn from_fs(path: &str) -> Option<Stat> {
        let m = std::fs::symlink_metadata(path).ok()?;
        let mtime = m
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs().clamp(1, u32::MAX as u64) as u32);
        let size = if m.is_dir() { u64::MAX } else { m.len() };
        Some(Stat { size, mtime })
    }
}

/// Column the results are ordered by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Path,
    Size,
    Modified,
}

/// A raw record coming from the MFT / USN journal.
pub struct RawRecord {
    pub frn: u64,
    pub parent_frn: u64,
    pub name: String,
    pub is_dir: bool,
}

/// Identity and journal position of a volume, used to validate the on-disk cache.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct VolumeMeta {
    pub serial: u32,
    pub root_frn: u64,
    pub journal_id: u64,
    /// The index reflects all journal entries before this USN.
    pub next_usn: i64,
}

#[derive(Clone, Copy)]
pub struct ScanRec {
    /// MFT record number (lower 48 bits of the FRN).
    pub rec: u64,
    pub parent: u64,
    pub name_off: u32,
    pub name_len: u16,
    pub is_dir: bool,
    pub stat: Stat,
}

/// Compact list of all records of one volume. Produced by a full MFT scan or
/// loaded from the on-disk cache, then consumed by [`Index::add_volume`].
/// All names live in one string arena, so there is no per-record allocation.
pub struct VolumeScan {
    pub letter: char,
    pub meta: VolumeMeta,
    pub names: String,
    pub recs: Vec<ScanRec>,
}

impl VolumeScan {
    #[cfg(test)]
    pub fn new(letter: char, meta: VolumeMeta) -> Self {
        VolumeScan { letter, meta, names: String::new(), recs: Vec::new() }
    }

    pub fn with_capacity(letter: char, meta: VolumeMeta, recs: usize, name_bytes: usize) -> Self {
        VolumeScan {
            letter,
            meta,
            names: String::with_capacity(name_bytes),
            recs: Vec::with_capacity(recs),
        }
    }

    pub fn len(&self) -> usize {
        self.recs.len()
    }

    /// Appends a record whose name is given as UTF-16 code units.
    pub fn push_utf16(&mut self, frn: u64, parent_frn: u64, name: impl Iterator<Item = u16>, is_dir: bool, stat: Stat) {
        let off = self.names.len();
        self.names.extend(char::decode_utf16(name).map(|c| c.unwrap_or('\u{FFFD}')));
        self.push_raw(frn, parent_frn, off, is_dir, stat);
    }

    #[cfg(test)]
    pub fn push_str(&mut self, frn: u64, parent_frn: u64, name: &str, is_dir: bool, stat: Stat) {
        let off = self.names.len();
        self.names.push_str(name);
        self.push_raw(frn, parent_frn, off, is_dir, stat);
    }

    fn push_raw(&mut self, frn: u64, parent_frn: u64, off: usize, is_dir: bool, stat: Stat) {
        self.recs.push(ScanRec {
            rec: record_no(frn) as u64,
            parent: record_no(parent_frn) as u64,
            name_off: off as u32,
            name_len: (self.names.len() - off) as u16,
            is_dir,
            stat,
        });
    }

    /// Moves all records of `other` to the end of this scan.
    pub fn append(&mut self, other: &VolumeScan) {
        // Grow by a little instead of doubling: these get to ~70 MB / ~110 MB.
        reserve_str(&mut self.names, other.names.len());
        reserve_vec(&mut self.recs, other.recs.len());
        let base = self.names.len() as u32;
        self.names.push_str(&other.names);
        self.recs.extend(other.recs.iter().map(|r| ScanRec { name_off: r.name_off + base, ..*r }));
    }

    pub fn name(&self, r: &ScanRec) -> &str {
        &self.names[r.name_off as usize..r.name_off as usize + r.name_len as usize]
    }
}

/// See [`Index::live_view`].
#[cfg(test)]
pub struct LiveView<'a> {
    index: &'a Index,
    /// Entry index -> MFT record number (u64::MAX if unmapped).
    rec_of: Vec<u64>,
}

#[cfg(test)]
impl LiveView<'_> {
    pub fn volume(&self, v: usize) -> (char, VolumeMeta) {
        let vol = &self.index.volumes[v];
        (vol.letter, vol.meta)
    }

    /// (live record count, total name bytes) of volume `v`.
    #[cfg(test)]
    pub fn stats(&self, v: usize) -> (usize, usize) {
        let (mut n, mut bytes) = (0, 0);
        self.for_each(v, |_, _, name, _, _| {
            n += 1;
            bytes += name.len();
        });
        (n, bytes)
    }

    /// Calls `f(record, parent_record, name, is_dir, stat)` for every live,
    /// non-root entry of volume `v`, in record-number order.
    pub fn for_each(&self, v: usize, mut f: impl FnMut(u64, u64, &str, bool, Stat)) {
        let index = self.index;
        let vol = &index.volumes[v];
        let root_rec = record_no(vol.meta.root_frn) as u64;
        for (rec, &idx) in vol.by_record.iter().enumerate() {
            if idx == NONE || idx == vol.root || !index.is_alive(idx) {
                continue;
            }
            let e = &index.entries[idx as usize];
            let parent = match self.rec_of.get(e.parent as usize) {
                Some(&p) if p != u64::MAX => p,
                _ => root_rec,
            };
            f(rec as u64, parent, index.name(idx), e.flags() & FLAG_DIR != 0, index.stat(idx));
        }
    }
}

pub struct Volume {
    /// e.g. 'C'
    pub letter: char,
    pub meta: VolumeMeta,
    root: u32,
    /// MFT record number (lower 48 bits of FRN) -> entry index.
    by_record: Vec<u32>,
}

#[derive(Default)]
pub struct Index {
    entries: Vec<Entry>,
    names: String,
    order: Vec<u32>,
    volumes: Vec<Volume>,
    /// Bumped on every mutation so the UI knows when to refresh.
    pub generation: AtomicU64,
    /// Changes only when entry indices are reassigned (compaction). Result
    /// lists holding entry indices from another epoch must not be used.
    pub epoch: u64,
    /// Bytes of `names` no longer referenced by a live entry.
    garbage_bytes: usize,
    /// Bumped when names or parents change (not on size/time updates).
    pub tree_generation: u64,
    /// Entries to (re)insert into `order` when the open batch ends.
    batch: Option<Vec<u32>>,
}

/// Compact once at least this many entries are dead...
const COMPACT_MIN_DEAD: usize = 64 * 1024;
/// ...or this many name bytes are garbage, and either exceeds 10%.
const COMPACT_MIN_GARBAGE: usize = 4 << 20;

/// Spare capacity kept after growing: ~3% instead of the default doubling,
/// which would commit hundreds of MB for a multi-million entry index.
fn headroom(len: usize) -> usize {
    len / 32 + 4096
}

fn reserve_vec<T>(v: &mut Vec<T>, extra: usize) {
    if v.capacity() - v.len() < extra {
        v.reserve_exact(extra + headroom(v.len()));
    }
}

fn reserve_str(s: &mut String, extra: usize) {
    if s.capacity() - s.len() < extra {
        s.reserve_exact(extra + headroom(s.len()));
    }
}

#[inline]
fn record_no(frn: u64) -> usize {
    (frn & 0x0000_FFFF_FFFF_FFFF) as usize
}

impl Index {
    fn push_entry(&mut self, name: &str, parent: u32, flags: u8, stat: Stat) -> u32 {
        let (name_off, name_len) = self.push_name(name);
        let e = Entry::new(name_off, name_len, parent, flags, stat);
        let idx = self.entries.len() as u32;
        reserve_vec(&mut self.entries, 1);
        self.entries.push(e);
        idx
    }

    /// Appends a name to the arena; returns (offset, length).
    fn push_name(&mut self, mut name: &str) -> (u32, u16) {
        if name.len() > MAX_NAME_LEN {
            // Can't happen on NTFS; truncate at a char boundary to stay valid.
            let mut end = MAX_NAME_LEN;
            while !name.is_char_boundary(end) {
                end -= 1;
            }
            name = &name[..end];
        }
        let off = self.names.len() as u32;
        reserve_str(&mut self.names, name.len());
        self.names.push_str(name);
        (off, name.len() as u16)
    }

    #[inline]
    pub fn name(&self, idx: u32) -> &str {
        let e = &self.entries[idx as usize];
        &self.names[e.name_off as usize..e.name_off as usize + e.name_len()]
    }

    pub fn is_dir(&self, idx: u32) -> bool {
        self.entries[idx as usize].flags() & FLAG_DIR != 0
    }

    pub fn stat(&self, idx: u32) -> Stat {
        let e = &self.entries[idx as usize];
        Stat { size: e.size(), mtime: e.mtime }
    }

    /// Updates size/mtime after a journal change (`idx` from [`Index::upsert`]
    /// or [`Index::entry_of`]).
    pub fn set_stat(&mut self, idx: u32, stat: Stat) {
        if let Some(e) = self.entries.get_mut(idx as usize)
            && e.flags() & FLAG_ALIVE != 0
            && (e.size(), e.mtime) != (stat.size, stat.mtime)
        {
            e.set_size(stat.size);
            e.mtime = stat.mtime;
            self.bump();
        }
    }

    /// Entry of a live record, if indexed.
    pub fn entry_of(&self, letter: char, frn: u64) -> Option<u32> {
        let v = self.volume_index(letter)?;
        let idx = *self.volumes[v].by_record.get(record_no(frn))?;
        (idx != NONE && self.is_alive(idx)).then_some(idx)
    }

    pub fn parent(&self, idx: u32) -> u32 {
        self.entries[idx as usize].parent
    }

    pub fn is_alive(&self, idx: u32) -> bool {
        self.entries
            .get(idx as usize)
            .is_some_and(|e| e.flags() & FLAG_ALIVE != 0)
    }

    /// Full path of the entry, e.g. `C:\Windows\notepad.exe`.
    pub fn full_path(&self, idx: u32) -> String {
        self.build_path(idx)
    }

    /// Path of the directory containing the entry, e.g. `C:\Windows`.
    pub fn parent_path(&self, idx: u32) -> String {
        let p = self.entries[idx as usize].parent;
        if p == NONE {
            return String::new();
        }
        self.build_path(p)
    }

    fn build_path(&self, idx: u32) -> String {
        let mut s = String::new();
        self.build_path_into(idx, &mut s);
        s
    }

    /// Writes the full path of `idx` into `s` (cleared first), without allocating
    /// once `s` has grown to path size.
    fn build_path_into(&self, idx: u32, s: &mut String) {
        s.clear();
        let mut parts: [u32; 128] = [NONE; 128];
        let mut n = 0;
        let mut cur = idx;
        // Bounded walk also guards against cycles from inconsistent journal data.
        while cur != NONE && n < parts.len() {
            parts[n] = cur;
            n += 1;
            let e = &self.entries[cur as usize];
            if e.flags() & FLAG_ROOT != 0 {
                break;
            }
            cur = e.parent;
        }
        for (i, &p) in parts[..n].iter().rev().enumerate() {
            if i > 0 {
                s.push('\\');
            }
            s.push_str(self.name(p));
        }
        if n == 1 {
            s.push('\\'); // bare root: "C:\"
        }
    }

    fn cmp_entries(&self, a: u32, b: u32) -> Ordering {
        cmp_ci(self.name(a), self.name(b)).then(a.cmp(&b))
    }

    /// Adds a whole volume. If the volume already exists (re-index after a
    /// journal reset), it is replaced.
    pub fn add_volume(&mut self, scan: VolumeScan) {
        let letter = scan.letter;
        let existing = self.volume_index(letter);
        if let Some(v) = existing {
            self.kill_volume_entries(v);
        }
        let root_rec = record_no(scan.meta.root_frn);
        let max_rec = scan.recs.iter().map(|r| r.rec as usize).chain([root_rec]).max().unwrap_or(0);
        let mut by_record = vec![NONE; max_rec + 1];

        let first = self.entries.len();
        reserve_vec(&mut self.entries, scan.recs.len() + 1);
        let meta = scan.meta;
        let VolumeScan { names, recs, .. } = scan;
        // First volume: adopt the scan's name arena instead of copying it.
        let name_base = if self.names.is_empty() {
            self.names = names;
            0
        } else {
            reserve_str(&mut self.names, names.len());
            let base = self.names.len() as u32;
            self.names.push_str(&names);
            drop(names);
            base
        };
        for r in &recs {
            if r.rec as usize == root_rec || r.name_len as usize > MAX_NAME_LEN {
                continue;
            }
            let flags = FLAG_ALIVE | if r.is_dir { FLAG_DIR } else { 0 };
            // `parent` holds the parent's record number until all entries exist.
            let parent = u32::try_from(r.parent).unwrap_or(NONE);
            let idx = self.entries.len() as u32;
            self.entries.push(Entry::new(name_base + r.name_off, r.name_len, parent, flags, r.stat));
            by_record[r.rec as usize] = idx;
        }
        drop(recs);
        let root = self.push_entry(&format!("{letter}:"), NONE, FLAG_DIR | FLAG_ALIVE | FLAG_ROOT, Stat::UNKNOWN);
        by_record[root_rec] = root;
        for e in &mut self.entries[first..root as usize] {
            let p = by_record.get(e.parent as usize).copied().unwrap_or(NONE);
            e.parent = if p == NONE { root } else { p };
        }

        let volume = Volume { letter, meta, root, by_record };
        match existing {
            Some(v) => self.volumes[v] = volume,
            None => self.volumes.push(volume),
        }
        self.rebuild_order();
    }

    /// Marks every entry of volume `v` (and its root) dead.
    fn kill_volume_entries(&mut self, v: usize) {
        let vol = &self.volumes[v];
        for &idx in vol.by_record.iter().chain([&vol.root]) {
            if idx != NONE {
                let e = &mut self.entries[idx as usize];
                if e.flags() & FLAG_ALIVE != 0 {
                    e.clear_flag(FLAG_ALIVE);
                    self.garbage_bytes += e.name_len();
                }
            }
        }
    }

    /// Drops a volume that is no longer present (e.g. a removed drive).
    pub fn remove_volume(&mut self, letter: char) {
        if let Some(v) = self.volume_index(letter) {
            self.kill_volume_entries(v);
            self.volumes.remove(v);
            self.rebuild_order();
        }
    }

    /// Letters of the indexed volumes.
    pub fn volume_letters(&self) -> Vec<char> {
        self.volumes.iter().map(|v| v.letter).collect()
    }

    pub fn volume_meta(&self, letter: char) -> Option<VolumeMeta> {
        Some(self.volumes[self.volume_index(letter)?].meta)
    }

    /// Replaces this index with `fresh` (loaded from the cache), making sure
    /// every observer sees a change.
    pub fn install(&mut self, mut fresh: Index) {
        fresh.epoch = self.epoch + 1;
        fresh.tree_generation = self.tree_generation + 1;
        fresh.generation = AtomicU64::new(self.generation.load(AtomicOrdering::Relaxed) + 1);
        *self = fresh;
    }

    /// Read-only view of the live records (tests compare against it).
    #[cfg(test)]
    pub fn live_view(&self) -> LiveView<'_> {
        let mut rec_of = vec![u64::MAX; self.entries.len()];
        for v in &self.volumes {
            for (rec, &idx) in v.by_record.iter().enumerate() {
                if idx != NONE {
                    rec_of[idx as usize] = rec as u64;
                }
            }
        }
        LiveView { index: self, rec_of }
    }

    /// Copies the live contents of every volume (tests compare against it).
    #[cfg(test)]
    pub fn snapshot(&self) -> Vec<VolumeScan> {
        let view = self.live_view();
        (0..self.volumes.len())
            .map(|v| {
                let (letter, meta) = view.volume(v);
                let (count, name_bytes) = view.stats(v);
                let mut scan = VolumeScan::with_capacity(letter, meta, count, name_bytes);
                view.for_each(v, |rec, parent, name, is_dir, stat| scan.push_str(rec, parent, name, is_dir, stat));
                scan
            })
            .collect()
    }

    /// Approximate heap usage of the index data structures, by component (MB).
    pub fn memory_report(&self) -> String {
        let mb = |b: usize| b as f64 / (1024.0 * 1024.0);
        let by_record: usize = self.volumes.iter().map(|v| v.by_record.capacity() * 4).sum();
        let parts = [
            ("entries", self.entries.capacity() * size_of::<Entry>()),
            ("names", self.names.capacity()),
            ("order", self.order.capacity() * 4),
            ("by_record", by_record),
        ];
        let total: usize = parts.iter().map(|p| p.1).sum();
        let detail: Vec<String> = parts.iter().map(|(n, b)| format!("{n} {:.1}", mb(*b))).collect();
        format!(
            "index heap {:.1} MB ({}), {} entries ({} live, {} dead), {:.1} MB stale names, epoch {}",
            mb(total),
            detail.join(", "),
            self.entries.len(),
            self.order.len(),
            self.dead_entries(),
            mb(self.garbage_bytes),
            self.epoch
        )
    }

    /// Number of entry slots that no longer hold a live file.
    fn dead_entries(&self) -> usize {
        self.entries.len() - self.order.len() - self.volumes.len()
    }

    /// Whether enough dead entries / stale names piled up to be worth a rebuild.
    pub fn needs_compaction(&self) -> bool {
        let dead = self.dead_entries();
        (dead >= COMPACT_MIN_DEAD && dead * 10 >= self.entries.len())
            || (self.garbage_bytes >= COMPACT_MIN_GARBAGE && self.garbage_bytes * 10 >= self.names.len())
    }

    /// Drops dead entries and stale names in place. Live entries keep their
    /// relative order, so `order` stays sorted and is only renumbered (no
    /// re-sort). Entry indices change, hence a new `epoch`. Extra memory is
    /// one new names buffer plus a remap table.
    pub fn compact(&mut self) {
        let n = self.entries.len();
        // Volume of each live entry, to re-hang orphans under the right root.
        let mut owner = vec![0u8; n];
        for (v, vol) in self.volumes.iter().enumerate() {
            for &idx in vol.by_record.iter().filter(|&&i| i != NONE) {
                owner[idx as usize] = v as u8;
            }
        }
        let mut remap = vec![NONE; n];
        let live_bytes = self.names.len() - self.garbage_bytes;
        let mut names = String::with_capacity(live_bytes + headroom(live_bytes));
        let mut next = 0usize;
        for i in 0..n {
            let mut e = self.entries[i];
            if e.flags() & FLAG_ALIVE == 0 {
                continue;
            }
            let name = &self.names[e.name_off as usize..e.name_off as usize + e.name_len()];
            e.name_off = names.len() as u32;
            names.push_str(name);
            self.entries[next] = e;
            remap[i] = next as u32;
            next += 1;
        }
        self.entries.truncate(next);
        self.entries.shrink_to(next + headroom(next));
        self.names = names;
        self.garbage_bytes = 0;

        for v in &mut self.volumes {
            v.root = remap[v.root as usize];
            for slot in &mut v.by_record {
                if *slot != NONE {
                    *slot = remap[*slot as usize];
                }
            }
        }
        let roots: Vec<u32> = self.volumes.iter().map(|v| v.root).collect();
        let mut owner_new = vec![0u8; next];
        for (i, &j) in remap.iter().enumerate() {
            if j != NONE {
                owner_new[j as usize] = owner[i];
            }
        }
        drop(owner);
        for (j, e) in self.entries.iter_mut().enumerate() {
            if e.parent != NONE {
                // A parent that died without its children: hang under the root.
                e.parent = match remap[e.parent as usize] {
                    NONE => roots[owner_new[j] as usize],
                    p => p,
                };
            }
        }
        for o in &mut self.order {
            *o = remap[*o as usize];
        }
        self.epoch += 1;
        self.tree_generation += 1;
        self.bump();
    }

    /// Records how far into the journal the index is up to date.
    pub fn set_next_usn(&mut self, letter: char, usn: i64) {
        if let Some(v) = self.volume_index(letter) {
            self.volumes[v].meta.next_usn = usn;
        }
    }

    fn rebuild_order(&mut self) {
        let mut order: Vec<u32> = (0..self.entries.len() as u32)
            .filter(|&i| {
                let f = self.entries[i as usize].flags();
                f & FLAG_ALIVE != 0 && f & FLAG_ROOT == 0
            })
            .collect();
        order.par_sort_unstable_by(|&a, &b| self.cmp_entries(a, b));
        order.shrink_to_fit();
        order.reserve_exact(headroom(order.len()));
        self.order = order;
        self.tree_generation += 1;
        self.bump();
    }

    fn bump(&self) {
        self.generation.fetch_add(1, AtomicOrdering::Relaxed);
    }

    fn order_remove(&mut self, idx: u32) {
        if self.batch.is_some() {
            return; // end_batch drops dead and pending entries in one pass
        }
        match self.order.binary_search_by(|&o| self.cmp_entries(o, idx)) {
            Ok(pos) => {
                self.order.remove(pos);
            }
            // Shouldn't happen; never leave a stale entry behind.
            Err(_) => self.order.retain(|&o| o != idx),
        }
    }

    fn order_insert(&mut self, idx: u32) {
        if let Some(pending) = &mut self.batch {
            pending.push(idx);
            self.entries[idx as usize].set_flag(FLAG_PENDING);
            return;
        }
        let pos = self
            .order
            .binary_search_by(|&o| self.cmp_entries(o, idx))
            .unwrap_or_else(|p| p);
        reserve_vec(&mut self.order, 1);
        self.order.insert(pos, idx);
    }

    /// Starts a batch of journal changes: `order` is no longer kept sorted per
    /// change (each insert/remove there is an O(n) memmove), but fixed up once
    /// in [`Index::end_batch`]. `order` must not be read in between, so the
    /// caller holds the write lock for the whole batch.
    pub fn begin_batch(&mut self) {
        self.batch.get_or_insert_with(Vec::new);
    }

    /// Ends a batch: drops dead and moved entries from `order` in one pass,
    /// then merges the sorted pending entries in from the back.
    pub fn end_batch(&mut self) {
        let Some(mut pending) = self.batch.take() else { return };
        let mut order = std::mem::take(&mut self.order);
        order.retain(|&i| {
            let f = self.entries[i as usize].flags();
            f & FLAG_ALIVE != 0 && f & FLAG_PENDING == 0
        });
        pending.sort_unstable();
        pending.dedup();
        pending.retain(|&i| {
            let e = &mut self.entries[i as usize];
            let keep = e.flags() & FLAG_ALIVE != 0;
            e.clear_flag(FLAG_PENDING);
            keep
        });
        pending.par_sort_unstable_by(|&a, &b| self.cmp_entries(a, b));

        // Insertion points by binary search (few string comparisons), then
        // shift the runs in between with plain memory moves, back to front.
        let at: Vec<usize> = pending
            .par_iter()
            .map(|&p| order.partition_point(|&o| self.cmp_entries(o, p) == Ordering::Less))
            .collect();
        let (old_len, add) = (order.len(), pending.len());
        reserve_vec(&mut order, add);
        order.resize(old_len + add, 0);
        let mut end = old_len; // runs of `order` above `end` are already moved
        for j in (0..add).rev() {
            let pos = at[j];
            order.copy_within(pos..end, pos + j + 1);
            order[pos + j] = pending[j];
            end = pos;
        }
        self.order = order;
    }

    fn volume_index(&self, letter: char) -> Option<usize> {
        self.volumes.iter().position(|v| v.letter == letter)
    }

    /// Create or rename/move an entry, keyed by FRN. Returns its entry index.
    pub fn upsert(&mut self, letter: char, r: &RawRecord) -> Option<u32> {
        let v = self.volume_index(letter)?;
        let root = self.volumes[v].root;
        let parent = self.volumes[v]
            .by_record
            .get(record_no(r.parent_frn))
            .copied()
            .filter(|&p| p != NONE)
            .unwrap_or(root);
        let rec = record_no(r.frn);
        let existing = self.volumes[v].by_record.get(rec).copied().unwrap_or(NONE);

        if existing != NONE && self.is_alive(existing) {
            if self.entries[existing as usize].flags() & FLAG_ROOT != 0 {
                return None;
            }
            let same_name = self.name(existing) == r.name;
            let e = self.entries[existing as usize];
            if same_name && e.parent == parent {
                return Some(existing);
            }
            if !same_name {
                self.garbage_bytes += e.name_len();
                self.order_remove(existing);
                let (name_off, name_len) = self.push_name(&r.name);
                self.entries[existing as usize].set_name(name_off, name_len);
                self.order_insert(existing);
            }
            self.entries[existing as usize].parent = parent;
            self.tree_generation += 1;
            self.bump();
            Some(existing)
        } else {
            let flags = FLAG_ALIVE | if r.is_dir { FLAG_DIR } else { 0 };
            let idx = self.push_entry(&r.name, parent, flags, Stat::UNKNOWN);
            let map = &mut self.volumes[v].by_record;
            if rec >= map.len() {
                reserve_vec(map, rec + 1 - map.len());
                map.resize(rec + 1, NONE);
            }
            map[rec] = idx;
            self.order_insert(idx);
            self.tree_generation += 1;
            self.bump();
            Some(idx)
        }
    }

    /// Removes an entry. Returns its former parent (whose mtime changed).
    pub fn remove(&mut self, letter: char, frn: u64) -> Option<u32> {
        let v = self.volume_index(letter)?;
        let rec = record_no(frn);
        let idx = self.volumes[v].by_record.get(rec).copied().unwrap_or(NONE);
        if idx == NONE || !self.is_alive(idx) || self.entries[idx as usize].flags() & FLAG_ROOT != 0 {
            return None;
        }
        self.order_remove(idx);
        self.garbage_bytes += self.entries[idx as usize].name_len();
        self.entries[idx as usize].clear_flag(FLAG_ALIVE);
        self.volumes[v].by_record[rec] = NONE;
        self.tree_generation += 1;
        self.bump();
        Some(self.entries[idx as usize].parent)
    }

    /// Reorders name-sorted search results by `key`. Ties keep name order;
    /// `desc` reverses the primary key only.
    ///
    /// Keys are extracted once (in parallel) and sorted together with the
    /// original position, so the comparator never touches `entries`.
    pub fn sort_results(&self, results: &mut [u32], key: SortKey, desc: bool) {
        // Each key gets packed so that ascending order of the packed value is
        // the wanted order; `pos` breaks ties by name.
        fn by<K: Ord + Copy + Send + Sync>(results: &mut [u32], key: impl Fn(u32) -> K + Sync) {
            let mut keyed: Vec<(K, u32)> =
                results.par_iter().enumerate().map(|(pos, &i)| (key(i), pos as u32)).collect();
            keyed.par_sort_unstable();
            let sorted: Vec<u32> = keyed.par_iter().map(|&(_, pos)| results[pos as usize]).collect();
            results.copy_from_slice(&sorted);
        }
        let flip64 = |k: u64| if desc { !k } else { k };
        let flip32 = |k: u32| if desc { !k } else { k };
        match key {
            SortKey::Name => {
                if desc {
                    results.reverse();
                }
            }
            SortKey::Size => by(results, |i| {
                let e = &self.entries[i as usize];
                // Directories (no size) sort before the smallest file.
                flip64(if e.flags() & FLAG_DIR != 0 || e.size() == u64::MAX { 0 } else { e.size() + 1 })
            }),
            SortKey::Modified => by(results, |i| flip32(self.entries[i as usize].mtime)),
            SortKey::Path => {
                let ranks = self.dir_tree_ranks();
                by(results, |i| {
                    let p = self.entries[i as usize].parent;
                    flip32(if p == NONE { 0 } else { ranks[p as usize] })
                })
            }
        }
    }

    /// Entry index -> pre-order number of that directory in the directory
    /// tree, children visited by name (0 for files and unreachable entries).
    /// Sorting by the parent's number gives Explorer-like tree order:
    /// everything in `foo` comes right after `foo`, before `foo.bar`.
    fn dir_tree_ranks(&self) -> Vec<u32> {
        let n = self.entries.len();
        let is_dir = |i: u32| self.entries[i as usize].flags() & FLAG_DIR != 0;
        // Directories in name order, grouped by parent (CSR layout).
        let dirs: Vec<u32> = self.order.par_iter().copied().filter(|&i| is_dir(i)).collect();
        let mut start = vec![0u32; n + 1];
        for &d in &dirs {
            start[self.entries[d as usize].parent as usize + 1] += 1;
        }
        for i in 0..n {
            start[i + 1] += start[i];
        }
        let mut fill = start.clone();
        let mut children = vec![0u32; dirs.len()];
        for &d in &dirs {
            let p = self.entries[d as usize].parent as usize;
            children[fill[p] as usize] = d;
            fill[p] += 1;
        }
        drop(fill);

        let mut ranks = vec![0u32; n];
        let mut next = 1u32;
        let mut roots: Vec<(char, u32)> = self.volumes.iter().map(|v| (v.letter, v.root)).collect();
        roots.sort();
        let mut stack: Vec<u32> = roots.iter().rev().map(|r| r.1).collect();
        while let Some(d) = stack.pop() {
            ranks[d as usize] = next;
            next += 1;
            let (a, b) = (start[d as usize] as usize, start[d as usize + 1] as usize);
            stack.extend(children[a..b].iter().rev());
        }
        ranks
    }

    /// Number of live entries (the result count of an empty query).
    pub fn live_count(&self) -> usize {
        self.order.len()
    }

    /// Entry at `pos` in name order, for showing all entries without copying `order`.
    pub fn order_at(&self, pos: usize) -> Option<u32> {
        self.order.get(pos).copied()
    }

    /// Runs a query. Returns `None` if cancelled via `is_cancelled`.
    ///
    /// Two phases: names are matched in physical (entry) order, which walks
    /// `entries` and the names arena sequentially, setting bits; then `order`
    /// is filtered by those bits to get name-sorted results. Walking `order`
    /// directly would cost two cache misses per entry.
    pub fn search(&self, query: &Query, is_cancelled: &(dyn Fn() -> bool + Sync)) -> Option<Vec<u32>> {
        if query.is_empty() {
            return Some(self.order.clone());
        }
        const WORDS_PER_TASK: usize = 512; // 32K entries
        let n = self.entries.len();
        let mut bits = vec![0u64; n.div_ceil(64)];
        let completed = bits
            .par_chunks_mut(WORDS_PER_TASK)
            .enumerate()
            .map(|(task, words)| {
                if is_cancelled() {
                    return false;
                }
                let first = task * WORDS_PER_TASK * 64;
                for (w, word) in words.iter_mut().enumerate() {
                    let start = first + w * 64;
                    let mut acc = 0u64;
                    for b in 0..(n - start).min(64) {
                        let idx = (start + b) as u32;
                        let f = self.entries[idx as usize].flags();
                        if f & FLAG_ALIVE != 0 && f & FLAG_ROOT == 0 && self.matches(idx, query) {
                            acc |= 1 << b;
                        }
                    }
                    *word = acc;
                }
                true
            })
            .reduce(|| true, |a, b| a && b);
        if !completed {
            return None;
        }
        let hit = |i: u32| bits[i as usize / 64] >> (i % 64) & 1 != 0;
        let chunks: Vec<Vec<u32>> =
            self.order.par_chunks(64 * 1024).map(|c| c.iter().copied().filter(|&i| hit(i)).collect()).collect();
        let mut out = Vec::with_capacity(chunks.iter().map(|c| c.len()).sum());
        for c in chunks {
            out.extend(c);
        }
        Some(out)
    }

    /// Previous single-pass implementation over `order`; kept as a reference
    /// for tests and the benchmark.
    pub fn search_by_order(&self, query: &Query, is_cancelled: &(dyn Fn() -> bool + Sync)) -> Option<Vec<u32>> {
        if query.is_empty() {
            return Some(self.order.clone());
        }
        let chunks: Vec<Option<Vec<u32>>> = self
            .order
            .par_chunks(32 * 1024)
            .map(|chunk| {
                if is_cancelled() {
                    return None;
                }
                Some(chunk.iter().copied().filter(|&i| self.matches(i, query)).collect())
            })
            .collect();
        let mut out = Vec::new();
        for c in chunks {
            out.extend(c?);
        }
        Some(out)
    }

    fn matches(&self, idx: u32, q: &Query) -> bool {
        thread_local! {
            static PATH: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
        }
        if !q.filters.is_empty() && !self.passes_filters(idx, &q.filters) {
            return false;
        }
        let name = self.name(idx);
        // Terms are sorted name-first, so the path is only built for entries
        // whose name already matched.
        let mut path_built = false;
        for t in &q.terms {
            let ok = if t.on_path {
                PATH.with_borrow_mut(|path| {
                    if !path_built {
                        self.build_path_into(idx, path);
                        path_built = true;
                    }
                    t.matcher.is_match(path)
                })
            } else {
                t.matcher.is_match(name)
            };
            if !ok {
                return false;
            }
        }
        true
    }
}

impl Index {
    /// Filters only read the entry itself, so they run before any name test.
    fn passes_filters(&self, idx: u32, filters: &[Filter]) -> bool {
        let e = &self.entries[idx as usize];
        let is_dir = e.flags() & FLAG_DIR != 0;
        filters.iter().all(|f| match f {
            Filter::Dir(want_dir) => is_dir == *want_dir,
            Filter::Size(r) => !is_dir && e.size() != u64::MAX && r.contains(&e.size()),
            Filter::Modified(r) => e.mtime != 0 && r.contains(&(e.mtime as u64)),
            Filter::Ext(exts) => {
                let name = self.name(idx);
                match name.rsplit_once('.') {
                    Some((stem, ext)) if !stem.is_empty() => {
                        exts.iter().any(|x| x.eq_ignore_ascii_case(ext) || (!ext.is_ascii() && ext.to_lowercase() == *x))
                    }
                    _ => false,
                }
            }
        })
    }
}

/// Diagnostic: times real searches (median of `runs`).
impl Index {
    pub fn bench_search(&self, queries: &[&str], runs: usize) -> Vec<String> {
        queries
            .iter()
            .map(|q| {
                let query = Query::parse(q);
                let median = |f: &dyn Fn() -> Vec<u32>| {
                    let mut times: Vec<f64> = Vec::new();
                    let mut out = Vec::new();
                    for _ in 0..runs {
                        let t = std::time::Instant::now();
                        out = f();
                        times.push(t.elapsed().as_secs_f64() * 1000.0);
                    }
                    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
                    (times[times.len() / 2], out)
                };
                let (new_ms, new) = median(&|| self.search(&query, &|| false).unwrap());
                let (old_ms, old) = median(&|| self.search_by_order(&query, &|| false).unwrap());
                let same = if new == old { "same" } else { "DIFFERENT" };
                format!("bench {q:>12}: {new_ms:7.2} ms (old {old_ms:7.2} ms), {} hits, {same}", new.len())
            })
            .collect()
    }
}

/// Diagnostic: sort timings and a spot check of size/mtime against the file system.
impl Index {
    pub fn bench_sort(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (key, desc) in [
            (SortKey::Name, true),
            (SortKey::Size, true),
            (SortKey::Modified, true),
            (SortKey::Path, false),
            (SortKey::Path, false),
        ] {
            let mut r = self.order.clone();
            let t = std::time::Instant::now();
            self.sort_results(&mut r, key, desc);
            out.push(format!("sort {key:?} desc={desc}: {:?} for {} entries", t.elapsed(), r.len()));
        }
        out
    }

    pub fn verify_stats(&self, samples: usize) -> Vec<String> {
        let mut out = Vec::new();
        let (mut checked, mut size_bad, mut time_bad, mut gone) = (0, 0, 0, 0);
        let step = (self.order.len() / samples).max(1);
        for &i in self.order.iter().step_by(step) {
            let path = self.full_path(i);
            let Some(fs) = Stat::from_fs(&path) else {
                gone += 1;
                continue;
            };
            checked += 1;
            let ix = self.stat(i);
            let size_ok = self.is_dir(i) || ix.size == fs.size;
            // Allow for rounding of sub-second timestamps.
            let time_ok = ix.mtime.abs_diff(fs.mtime) <= 1;
            size_bad += !size_ok as usize;
            time_bad += !time_ok as usize;
            if (!size_ok || !time_ok) && out.len() < 10 {
                out.push(format!("  mismatch {path}: index {ix:?} fs {fs:?}"));
            }
        }
        out.insert(0, format!("verify: {checked} checked, {size_bad} size mismatches, {time_bad} mtime mismatches, {gone} unreadable"));
        out
    }
}

/// Case-insensitive total order on names, consistent with byte order of the
/// lower-cased strings (so ASCII and non-ASCII names interleave correctly).
fn cmp_ci(a: &str, b: &str) -> Ordering {
    if a.is_ascii() && b.is_ascii() {
        let (a, b) = (a.as_bytes(), b.as_bytes());
        for (x, y) in a.iter().zip(b) {
            match x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase()) {
                Ordering::Equal => {}
                o => return o,
            }
        }
        a.len().cmp(&b.len())
    } else {
        a.chars().flat_map(char::to_lowercase).cmp(b.chars().flat_map(char::to_lowercase))
    }
}

/// Case-insensitive substring test without a pre-lowered copy of `hay`.
/// `needle` must already be lower-case.
fn ci_contains(hay: &str, needle: &[u8], finder: &memmem::Finder) -> bool {
    if hay.is_ascii() {
        if !needle.is_ascii() {
            return false;
        }
        ascii_ci_find(hay.as_bytes(), needle)
    } else {
        finder.find(hay.to_lowercase().as_bytes()).is_some()
    }
}

fn ascii_ci_find(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > hay.len() {
        return false;
    }
    let (lo, up) = (needle[0], needle[0].to_ascii_uppercase());
    let last_start = hay.len() - needle.len();
    let mut pos = 0;
    while let Some(off) = memchr::memchr2(lo, up, &hay[pos..=last_start]) {
        let start = pos + off;
        if hay[start + 1..start + needle.len()].eq_ignore_ascii_case(&needle[1..]) {
            return true;
        }
        pos = start + 1;
        if pos > last_start {
            break;
        }
    }
    false
}

enum Matcher {
    /// Lower-case needle and a finder for it.
    Substr(Vec<u8>, memmem::Finder<'static>),
    /// Lower-case pattern, as chars and (if ASCII) as bytes for a fast path.
    Glob(Vec<char>, Option<Vec<u8>>),
}

impl Matcher {
    /// Case-insensitive match against an original-case `hay`.
    fn is_match(&self, hay: &str) -> bool {
        match self {
            Matcher::Substr(needle, f) => ci_contains(hay, needle, f),
            Matcher::Glob(chars, ascii) => match ascii {
                Some(p) if hay.is_ascii() => glob_match_by(p, hay.as_bytes(), |a, b| a == b.to_ascii_lowercase()),
                _ => glob_match(chars, hay),
            },
        }
    }
}

struct Term {
    matcher: Matcher,
    /// Terms containing a path separator are matched against the full path.
    on_path: bool,
}

/// A parsed search query: whitespace separated terms, all of which must match.
/// Terms with `*` / `?` are wildcards matched against the whole name (or path).
/// See [`filter`] for `ext:`, `size:`, `dm:`, `file:` and `folder:`.
pub struct Query {
    terms: Vec<Term>,
    filters: Vec<Filter>,
    /// Filters that couldn't be parsed (and are ignored), for the status bar.
    pub errors: Vec<String>,
}

impl Query {
    /// True for an empty query (matches every entry).
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty() && self.filters.is_empty()
    }

    /// Whether results depend on sizes / modification times, which change
    /// without the tree changing.
    pub fn uses_stats(&self) -> bool {
        self.filters.iter().any(|f| matches!(f, Filter::Size(_) | Filter::Modified(_)))
    }

    pub fn parse(s: &str) -> Self {
        Self::parse_at(s, chrono::Local::now().date_naive())
    }

    fn parse_at(s: &str, today: chrono::NaiveDate) -> Self {
        let lower = s.to_lowercase();
        let mut q = Query { terms: Vec::new(), filters: Vec::new(), errors: Vec::new() };
        for token in lower.split_whitespace() {
            let text = match filter::parse(token, today) {
                filter::Token::Text => Some(token.to_string()),
                filter::Token::Filter(f, text) => {
                    q.filters.push(f);
                    text
                }
                filter::Token::Incomplete => None,
                filter::Token::Error(e) => {
                    q.errors.push(e);
                    None
                }
            };
            if let Some(t) = text {
                q.terms.push(Term::new(&t));
            }
        }
        // Cheap name terms first; path terms need a path to be built.
        q.terms.sort_by_key(|t| t.on_path);
        q
    }
}

impl Term {
    fn new(t: &str) -> Term {
        let t = t.replace('/', "\\");
        let on_path = t.contains('\\');
        let matcher = if t.contains('*') || t.contains('?') {
            Matcher::Glob(t.chars().collect(), t.is_ascii().then(|| t.as_bytes().to_vec()))
        } else {
            Matcher::Substr(t.as_bytes().to_vec(), memmem::Finder::new(t.as_bytes()).into_owned())
        };
        Term { matcher, on_path }
    }
}

/// Wildcard match over chars. `pattern` must be lower-case; `text` is
/// lower-cased here (allocates; the ASCII fast path in `Matcher` avoids this).
fn glob_match(pattern: &[char], text: &str) -> bool {
    let text: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    glob_match_by(pattern, &text, |a, b| a == b)
}

/// Iterative wildcard match supporting `*` and `?`. `eq(pattern_elem, text_elem)`.
fn glob_match_by<T: Copy + PartialEq + From<u8>>(pattern: &[T], text: &[T], eq: impl Fn(T, T) -> bool) -> bool {
    let (star, any) = (T::from(b'*'), T::from(b'?'));
    let (mut p, mut t) = (0usize, 0usize);
    let mut star_at: Option<usize> = None;
    let mut mark = 0usize;
    while t < text.len() {
        if p < pattern.len() && pattern[p] != star && (pattern[p] == any || eq(pattern[p], text[t])) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == star {
            star_at = Some(p);
            mark = t;
            p += 1;
        } else if let Some(s) = star_at {
            p = s + 1;
            mark += 1;
            t = mark;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == star {
        p += 1;
    }
    p == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(frn: u64, parent: u64, name: &str, dir: bool) -> RawRecord {
        RawRecord { frn, parent_frn: parent, name: name.into(), is_dir: dir }
    }

    fn sample() -> Index {
        let mut idx = Index::default();
        idx.add_volume(scan(&[
            (10, 5, "Windows", true),
            (11, 10, "notepad.exe", false),
            (12, 5, "Users", true),
            (13, 12, "Readme.TXT", false),
        ]));
        idx
    }

    fn scan(recs: &[(u64, u64, &str, bool)]) -> VolumeScan {
        let meta = VolumeMeta { root_frn: 5, ..Default::default() };
        let mut s = VolumeScan::new('C', meta);
        for &(frn, parent, name, dir) in recs {
            s.push_utf16(frn, parent, name.encode_utf16(), dir, Stat::UNKNOWN);
        }
        s
    }

    fn names(idx: &Index, q: &str) -> Vec<String> {
        idx.search(&Query::parse(q), &|| false)
            .unwrap()
            .into_iter()
            .map(|i| idx.name(i).to_string())
            .collect()
    }

    #[test]
    fn sorting() {
        let meta = VolumeMeta { root_frn: 5, ..Default::default() };
        let mut s = VolumeScan::new('C', meta);
        let st = |size, mtime| Stat { size, mtime };
        s.push_str(10, 5, "b", true, Stat::UNKNOWN);
        s.push_str(11, 5, "a", true, Stat::UNKNOWN);
        s.push_str(20, 10, "x.txt", false, st(300, 3));
        s.push_str(21, 11, "y.txt", false, st(100, 9));
        s.push_str(22, 11, "z.txt", false, st(200, 1));
        let mut idx = Index::default();
        idx.add_volume(s);
        let sorted = |idx: &Index, key, desc| {
            let mut r = idx.search(&Query::parse(".txt"), &|| false).unwrap();
            idx.sort_results(&mut r, key, desc);
            r.iter().map(|&i| idx.name(i).to_string()).collect::<Vec<_>>()
        };
        assert_eq!(sorted(&idx, SortKey::Name, true), ["z.txt", "y.txt", "x.txt"]);
        assert_eq!(sorted(&idx, SortKey::Size, false), ["y.txt", "z.txt", "x.txt"]);
        assert_eq!(sorted(&idx, SortKey::Modified, true), ["y.txt", "x.txt", "z.txt"]);
        // C:\a before C:\b; within a folder, by name.
        assert_eq!(sorted(&idx, SortKey::Path, false), ["y.txt", "z.txt", "x.txt"]);
        assert_eq!(sorted(&idx, SortKey::Path, true), ["x.txt", "y.txt", "z.txt"]);
        // Moving a file invalidates the cached directory ranks.
        let x = idx.upsert('C', &rec(20, 11, "x.txt", false)).unwrap();
        assert_eq!(sorted(&idx, SortKey::Path, false), ["x.txt", "y.txt", "z.txt"]);
        idx.set_stat(x, st(1, 99));
        assert_eq!(sorted(&idx, SortKey::Size, false), ["x.txt", "y.txt", "z.txt"]);
        assert_eq!(idx.stat(x), st(1, 99));
    }

    #[test]
    fn filters() {
        let meta = VolumeMeta { root_frn: 5, ..Default::default() };
        let mut s = VolumeScan::new('C', meta);
        let st = |size, mtime| Stat { size, mtime };
        let day = |y, m, d| filter_secs(y, m, d);
        s.push_str(10, 5, "docs.old", true, st(u64::MAX, day(2024, 1, 10)));
        s.push_str(20, 10, "a.TXT", false, st(500, day(2024, 1, 15)));
        s.push_str(21, 10, "b.md", false, st(5 << 20, day(2024, 3, 1)));
        s.push_str(22, 10, ".gitignore", false, st(10, day(2025, 6, 1)));
        s.push_str(23, 10, "c.txt", false, Stat::UNKNOWN);
        let mut idx = Index::default();
        idx.add_volume(s);
        let today = chrono::NaiveDate::from_ymd_opt(2025, 6, 1).unwrap();
        let names = |q: &str| -> Vec<String> {
            let q = Query::parse_at(q, today);
            idx.search(&q, &|| false).unwrap().into_iter().map(|i| idx.name(i).to_string()).collect()
        };
        assert_eq!(names("ext:txt"), ["a.TXT", "c.txt"]);
        assert_eq!(names("ext:md;txt size:>1k"), ["b.md"]);
        assert_eq!(names("ext:gitignore"), Vec::<String>::new());
        assert_eq!(names("size:<=500"), [".gitignore", "a.TXT"]);
        assert_eq!(names("dm:2024-01"), ["a.TXT", "docs.old"]);
        assert_eq!(names("dm:today"), [".gitignore"]);
        assert_eq!(names("dm:<2024-02 file:"), ["a.TXT"]);
        assert_eq!(names("folder:"), ["docs.old"]);
        assert_eq!(names("file:txt"), ["a.TXT", "c.txt"]);
        assert_eq!(names("size: d"), ["b.md", "docs.old"]);
        let q = Query::parse_at("size:huge dm:soon", today);
        assert_eq!(q.errors.len(), 1);
        assert!(q.uses_stats());
        assert!(!Query::parse_at("ext:txt", today).uses_stats());
    }

    fn filter_secs(y: i32, m: u32, d: u32) -> u32 {
        use chrono::TimeZone;
        let t = chrono::NaiveDate::from_ymd_opt(y, m, d).unwrap().and_hms_opt(12, 0, 0).unwrap();
        chrono::Local.from_local_datetime(&t).unwrap().timestamp() as u32
    }

    #[test]
    fn path_sort_is_tree_order() {
        let meta = VolumeMeta { root_frn: 5, ..Default::default() };
        let mut s = VolumeScan::new('C', meta);
        s.push_str(10, 5, "foo", true, Stat::UNKNOWN);
        s.push_str(11, 5, "foo.bar", true, Stat::UNKNOWN);
        s.push_str(12, 10, "sub", true, Stat::UNKNOWN);
        s.push_str(20, 11, "in_foobar.txt", false, Stat::UNKNOWN);
        s.push_str(21, 12, "in_sub.txt", false, Stat::UNKNOWN);
        s.push_str(22, 10, "in_foo.txt", false, Stat::UNKNOWN);
        let mut idx = Index::default();
        idx.add_volume(s);
        let mut r = idx.search(&Query::parse(".txt"), &|| false).unwrap();
        idx.sort_results(&mut r, SortKey::Path, false);
        let names: Vec<_> = r.iter().map(|&i| idx.name(i)).collect();
        // C:\foo, C:\foo\sub, then C:\foo.bar (a plain string sort puts foo.bar first).
        assert_eq!(names, ["in_foo.txt", "in_sub.txt", "in_foobar.txt"]);
    }

    #[test]
    fn two_phase_search_matches_reference() {
        let mut idx = sample();
        idx.upsert('C', &rec(20, 12, "zeta notes.txt", false));
        idx.upsert('C', &rec(21, 10, "Alpha.TXT", false));
        idx.remove('C', 11);
        for q in ["", "t", "txt", "*.txt", "users\\ read", "windows\\", "zzz", "ä"] {
            let query = Query::parse(q);
            assert_eq!(idx.search(&query, &|| false), idx.search_by_order(&query, &|| false), "{q}");
        }
    }

    /// Same random journal applied per change and as batches must give the
    /// same index.
    #[test]
    fn batched_updates_match_immediate() {
        let mut seed = 12345u64;
        let mut rand = move |n: u64| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) % n
        };
        let words = ["a", "B", "c.txt", "Ä", "文档", "d", "z", "a.txt", "readme"];
        let mut ops = Vec::new();
        for _ in 0..3000 {
            let frn = 10 + rand(200);
            if rand(4) == 0 {
                ops.push(Err(frn));
            } else {
                let parent = if rand(3) == 0 { 5 } else { 10 + rand(200) };
                let name = format!("{}{}", words[rand(words.len() as u64) as usize], rand(5));
                ops.push(Ok(rec(frn, parent, &name, rand(5) == 0)));
            }
        }
        let apply = |idx: &mut Index, op: &Result<RawRecord, u64>| match op {
            Ok(r) => {
                idx.upsert('C', r);
            }
            Err(frn) => {
                idx.remove('C', *frn);
            }
        };
        let mut a = sample();
        let mut b = sample();
        for op in &ops {
            apply(&mut a, op);
        }
        for chunk in ops.chunks(97) {
            b.begin_batch();
            for op in chunk {
                apply(&mut b, op);
            }
            b.end_batch();
        }
        assert_eq!(a.order, b.order);
        assert!(b.order.windows(2).all(|w| b.cmp_entries(w[0], w[1]) == Ordering::Less));
        assert!(b.entries.iter().all(|e| e.flags() & FLAG_PENDING == 0));
        // Compaction keeps names and order; paths are unchanged except that
        // children of deleted directories move under the root (as before).
        let chain_alive = |idx: &Index, mut i: u32| {
            for _ in 0..128 {
                let e = &idx.entries[i as usize];
                if e.flags() & FLAG_ALIVE == 0 {
                    return false;
                }
                if e.flags() & FLAG_ROOT != 0 {
                    return true;
                }
                i = e.parent;
            }
            false
        };
        let names_before: Vec<String> = b.order.iter().map(|&i| b.name(i).to_string()).collect();
        let paths_before: Vec<Option<String>> =
            b.order.iter().map(|&i| chain_alive(&b, i).then(|| b.full_path(i))).collect();
        b.compact();
        let names_after: Vec<String> = b.order.iter().map(|&i| b.name(i).to_string()).collect();
        assert_eq!(names_after, names_before);
        for (k, &i) in b.order.iter().enumerate() {
            if let Some(p) = &paths_before[k] {
                assert_eq!(&b.full_path(i), p);
            }
            let parent = b.entries[i as usize].parent;
            assert!(b.entries[parent as usize].flags() & FLAG_ALIVE != 0);
        }
        assert!(b.order.windows(2).all(|w| b.cmp_entries(w[0], w[1]) == Ordering::Less));
    }

    #[test]
    fn filetime_conversion() {
        // 2024-01-01T00:00:00Z
        assert_eq!(Stat::mtime_from_filetime(133_485_408_000_000_000), 1_704_067_200);
        assert_eq!(Stat::mtime_from_filetime(0), 1);
    }

    #[test]
    fn glob() {
        let p: Vec<char> = "*.txt".chars().collect();
        assert!(glob_match(&p, "readme.txt"));
        assert!(!glob_match(&p, "readme.txt.bak"));
        let p: Vec<char> = "n?tepad*".chars().collect();
        assert!(glob_match(&p, "notepad.exe"));
        assert!(glob_match(&p, "NOTEPAD.EXE"));
        let eq = |a: u8, b: u8| a == b.to_ascii_lowercase();
        assert!(glob_match_by(b"*.txt", b"README.TXT", eq));
        assert!(glob_match_by(b"a*b*c", b"AxxBxxC", eq));
        assert!(!glob_match_by(b"a*b*c", b"AxxBxx", eq));
        assert!(glob_match_by(b"*", b"", eq));
    }

    #[test]
    fn search_and_paths() {
        let idx = sample();
        // Sorted case-insensitively; display keeps original case.
        assert_eq!(names(&idx, ""), ["notepad.exe", "Readme.TXT", "Users", "Windows"]);
        assert_eq!(names(&idx, "README"), ["Readme.TXT"]);
        assert_eq!(names(&idx, "*.exe"), ["notepad.exe"]);
        assert_eq!(names(&idx, "windows\\ note"), ["notepad.exe"]);
        let i = idx.search(&Query::parse("notepad"), &|| false).unwrap()[0];
        assert_eq!(idx.full_path(i), "C:\\Windows\\notepad.exe");
        assert_eq!(idx.parent_path(i), "C:\\Windows");
    }

    #[test]
    fn live_updates() {
        let mut idx = sample();
        idx.upsert('C', &rec(14, 12, "a.txt", false));
        assert_eq!(names(&idx, ".txt"), ["a.txt", "Readme.TXT"]);
        idx.upsert('C', &rec(14, 10, "z.txt", false)); // rename + move
        let i = idx.search(&Query::parse("z.txt"), &|| false).unwrap()[0];
        assert_eq!(idx.full_path(i), "C:\\Windows\\z.txt");
        idx.remove('C', 14);
        assert_eq!(names(&idx, ".txt"), ["Readme.TXT"]);
    }

    #[test]
    fn compaction() {
        let mut idx = sample();
        for i in 0..1000u64 {
            idx.upsert('C', &rec(100 + i, 12, &format!("tmp{i}.dat"), false));
        }
        idx.upsert('C', &rec(11, 10, "renamed.exe", false));
        for i in 0..1000u64 {
            idx.remove('C', 100 + i);
        }
        idx.set_next_usn('C', 42);
        assert_eq!(idx.dead_entries(), 1000);
        assert!(idx.garbage_bytes > 0);

        let gen_before = idx.generation.load(AtomicOrdering::Relaxed);
        let before = names(&idx, "");
        let mut c = idx;
        c.compact();
        assert_eq!(c.epoch, 1);
        assert!(c.generation.load(AtomicOrdering::Relaxed) > gen_before);
        assert_eq!(c.dead_entries(), 0);
        assert_eq!(c.garbage_bytes, 0);
        assert_eq!(c.volumes[0].meta.next_usn, 42);
        assert_eq!(names(&c, ""), before);
        assert_eq!(c.entries.len(), 5); // root + 4 files
        let i = c.search(&Query::parse("renamed"), &|| false).unwrap()[0];
        assert_eq!(c.full_path(i), "C:\\Windows\\renamed.exe");
        // Live updates keep working on the compacted index.
        c.upsert('C', &rec(11, 12, "renamed.exe", false));
        let i = c.search(&Query::parse("renamed"), &|| false).unwrap()[0];
        assert_eq!(c.full_path(i), "C:\\Users\\renamed.exe");
    }

    #[test]
    fn case_insensitive_everywhere() {
        let idx = sample();
        assert_eq!(names(&idx, "NOTEPAD"), ["notepad.exe"]);
        assert_eq!(names(&idx, "*.TXT"), ["Readme.TXT"]);
        assert_eq!(names(&idx, "WINDOWS\\ NOTE"), ["notepad.exe"]);
        assert_eq!(names(&idx, "users\\*.txt"), Vec::<String>::new()); // glob matches whole path
        assert_eq!(names(&idx, "c:\\users\\*.txt"), ["Readme.TXT"]);
        assert!(ascii_ci_find(b"ABCabc", b"cab"));
        assert!(!ascii_ci_find(b"ab", b"abc"));
        assert!(ascii_ci_find(b"xAAB", b"aab"));
    }

    #[test]
    fn ci_order_is_consistent() {
        let mut v = vec!["b", "A", "a", "Ä", "z", "文", "B", "ab"];
        v.sort_by(|a, b| cmp_ci(a, b));
        let lowered: Vec<String> = v.iter().map(|s| s.to_lowercase()).collect();
        let mut sorted = lowered.clone();
        sorted.sort();
        assert_eq!(lowered, sorted);
    }

    #[test]
    fn non_ascii_lowercase() {
        let mut idx = Index::default();
        idx.add_volume(scan(&[(10, 5, "Ärger 文档.TXT", false)]));
        assert_eq!(names(&idx, "ärger 文档"), ["Ärger 文档.TXT"]);
    }

    #[test]
    fn snapshot_roundtrip() {
        let mut idx = sample();
        idx.upsert('C', &rec(14, 10, "new.txt", false));
        idx.remove('C', 13);
        let snap = idx.snapshot();
        let mut restored = Index::default();
        for s in snap {
            restored.add_volume(s);
        }
        assert_eq!(names(&restored, ""), ["new.txt", "notepad.exe", "Users", "Windows"]);
        let i = restored.search(&Query::parse("new.txt"), &|| false).unwrap()[0];
        assert_eq!(restored.full_path(i), "C:\\Windows\\new.txt");
    }
}
