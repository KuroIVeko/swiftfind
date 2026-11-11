//! In-memory file index shared by all NTFS volumes.
//!
//! Layout is kept compact: every entry is a 12-byte struct that points into a
//! single string arena of names. Matching and sorting are case-insensitive on
//! the fly (no lower-cased copy, which would cost ~70 MB for 3M files).
//! `order` holds the indices of all live entries sorted case-insensitively by
//! name, so search results come out already sorted without a per-query sort.

use memchr::memmem;
use rayon::prelude::*;
use std::cmp::Ordering;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

pub const NONE: u32 = u32::MAX;

const FLAG_DIR: u8 = 1;
const FLAG_ALIVE: u8 = 2;
const FLAG_ROOT: u8 = 4;

#[derive(Clone, Copy)]
struct Entry {
    name_off: u32,
    parent: u32,
    name_len: u16,
    vol: u8,
    flags: u8,
}

// Entry count is in the millions; keep it small.
const _: () = assert!(size_of::<Entry>() == 12);

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
    pub fn push_utf16(&mut self, frn: u64, parent_frn: u64, name: impl Iterator<Item = u16>, is_dir: bool) {
        let off = self.names.len();
        self.names.extend(char::decode_utf16(name).map(|c| c.unwrap_or('\u{FFFD}')));
        self.push_raw(frn, parent_frn, off, is_dir);
    }

    pub fn push_str(&mut self, frn: u64, parent_frn: u64, name: &str, is_dir: bool) {
        let off = self.names.len();
        self.names.push_str(name);
        self.push_raw(frn, parent_frn, off, is_dir);
    }

    fn push_raw(&mut self, frn: u64, parent_frn: u64, off: usize, is_dir: bool) {
        self.recs.push(ScanRec {
            rec: record_no(frn) as u64,
            parent: record_no(parent_frn) as u64,
            name_off: off as u32,
            name_len: (self.names.len() - off) as u16,
            is_dir,
        });
    }

    /// Moves all records of `other` to the end of this scan.
    pub fn append(&mut self, other: &VolumeScan) {
        let base = self.names.len() as u32;
        self.names.push_str(&other.names);
        self.recs.extend(other.recs.iter().map(|r| ScanRec { name_off: r.name_off + base, ..*r }));
    }

    pub fn name(&self, r: &ScanRec) -> &str {
        &self.names[r.name_off as usize..r.name_off as usize + r.name_len as usize]
    }
}

/// See [`Index::live_view`].
pub struct LiveView<'a> {
    index: &'a Index,
    /// Entry index -> MFT record number (u64::MAX if unmapped).
    rec_of: Vec<u64>,
}

impl LiveView<'_> {
    pub fn volume_count(&self) -> usize {
        self.index.volumes.len()
    }

    pub fn volume(&self, v: usize) -> (char, VolumeMeta) {
        let vol = &self.index.volumes[v];
        (vol.letter, vol.meta)
    }

    /// (live record count, total name bytes) of volume `v`.
    pub fn stats(&self, v: usize) -> (usize, usize) {
        let (mut n, mut bytes) = (0, 0);
        self.for_each(v, |_, _, name, _| {
            n += 1;
            bytes += name.len();
        });
        (n, bytes)
    }

    /// Calls `f(record, parent_record, name, is_dir)` for every live, non-root
    /// entry of volume `v`, in record-number order.
    pub fn for_each(&self, v: usize, mut f: impl FnMut(u64, u64, &str, bool)) {
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
            f(rec as u64, parent, index.name(idx), e.flags & FLAG_DIR != 0);
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
    fn push_entry(&mut self, name: &str, parent: u32, vol: u8, flags: u8) -> u32 {
        let (name_off, name_len) = self.push_name(name);
        let e = Entry { name_off, parent, name_len, vol, flags };
        let idx = self.entries.len() as u32;
        reserve_vec(&mut self.entries, 1);
        self.entries.push(e);
        idx
    }

    /// Appends a name to the arena; returns (offset, length).
    fn push_name(&mut self, name: &str) -> (u32, u16) {
        let off = self.names.len() as u32;
        reserve_str(&mut self.names, name.len());
        self.names.push_str(name);
        (off, name.len() as u16)
    }

    #[inline]
    pub fn name(&self, idx: u32) -> &str {
        let e = &self.entries[idx as usize];
        &self.names[e.name_off as usize..e.name_off as usize + e.name_len as usize]
    }

    pub fn is_dir(&self, idx: u32) -> bool {
        self.entries[idx as usize].flags & FLAG_DIR != 0
    }

    pub fn is_alive(&self, idx: u32) -> bool {
        self.entries
            .get(idx as usize)
            .is_some_and(|e| e.flags & FLAG_ALIVE != 0)
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
            if e.flags & FLAG_ROOT != 0 {
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
            for e in self.entries.iter_mut().filter(|e| e.vol == v as u8 && e.flags & FLAG_ALIVE != 0) {
                e.flags &= !FLAG_ALIVE;
                self.garbage_bytes += e.name_len as usize;
            }
        }
        let vol = existing.unwrap_or(self.volumes.len()) as u8;
        let root_rec = record_no(scan.meta.root_frn);
        let root = self.push_entry(&format!("{letter}:"), NONE, vol, FLAG_DIR | FLAG_ALIVE | FLAG_ROOT);

        let max_rec = scan.recs.iter().map(|r| r.rec as usize).chain([root_rec]).max().unwrap_or(0);
        let mut by_record = vec![NONE; max_rec + 1];
        by_record[root_rec] = root;

        let first = self.entries.len();
        reserve_vec(&mut self.entries, scan.recs.len());
        reserve_str(&mut self.names, scan.names.len());
        let mut parents = Vec::with_capacity(scan.recs.len());
        for r in &scan.recs {
            if r.rec as usize == root_rec {
                continue;
            }
            let flags = FLAG_ALIVE | if r.is_dir { FLAG_DIR } else { 0 };
            let idx = self.push_entry(scan.name(r), NONE, vol, flags);
            by_record[r.rec as usize] = idx;
            parents.push(r.parent);
        }
        let meta = scan.meta;
        drop(scan);

        for (i, prec) in parents.into_iter().enumerate() {
            let p = by_record.get(prec as usize).copied().unwrap_or(NONE);
            self.entries[first + i].parent = if p == NONE { root } else { p };
        }

        let volume = Volume { letter, meta, root, by_record };
        match existing {
            Some(v) => self.volumes[v] = volume,
            None => self.volumes.push(volume),
        }
        self.rebuild_order();
    }

    /// Read-only view of the live records, for writing the cache without
    /// copying the index.
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

    /// Copies the live contents of every volume (used to build a compacted index).
    pub fn snapshot(&self) -> Vec<VolumeScan> {
        let view = self.live_view();
        (0..self.volumes.len())
            .map(|v| {
                let (letter, meta) = view.volume(v);
                let (count, name_bytes) = view.stats(v);
                let mut scan = VolumeScan::with_capacity(letter, meta, count, name_bytes);
                view.for_each(v, |rec, parent, name, is_dir| scan.push_str(rec, parent, name, is_dir));
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

    /// Builds a fresh copy containing only live data. Entry indices change, so
    /// the copy gets a new `epoch`. Safe to call under a read lock; the caller
    /// swaps the result in.
    pub fn compacted(&self) -> Index {
        let mut fresh = Index {
            epoch: self.epoch + 1,
            generation: AtomicU64::new(self.generation.load(AtomicOrdering::Relaxed)),
            ..Default::default()
        };
        for scan in self.snapshot() {
            fresh.add_volume(scan);
        }
        fresh
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
                let f = self.entries[i as usize].flags;
                f & FLAG_ALIVE != 0 && f & FLAG_ROOT == 0
            })
            .collect();
        order.par_sort_unstable_by(|&a, &b| self.cmp_entries(a, b));
        order.shrink_to_fit();
        order.reserve_exact(headroom(order.len()));
        self.order = order;
        self.bump();
    }

    fn bump(&self) {
        self.generation.fetch_add(1, AtomicOrdering::Relaxed);
    }

    fn order_remove(&mut self, idx: u32) {
        if let Ok(pos) = self.order.binary_search_by(|&o| self.cmp_entries(o, idx)) {
            self.order.remove(pos);
        }
    }

    fn order_insert(&mut self, idx: u32) {
        let pos = self
            .order
            .binary_search_by(|&o| self.cmp_entries(o, idx))
            .unwrap_or_else(|p| p);
        reserve_vec(&mut self.order, 1);
        self.order.insert(pos, idx);
    }

    fn volume_index(&self, letter: char) -> Option<usize> {
        self.volumes.iter().position(|v| v.letter == letter)
    }

    /// Create or rename/move an entry, keyed by FRN.
    pub fn upsert(&mut self, letter: char, r: &RawRecord) {
        let Some(v) = self.volume_index(letter) else { return };
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
            if self.entries[existing as usize].flags & FLAG_ROOT != 0 {
                return;
            }
            let same_name = self.name(existing) == r.name;
            let e = self.entries[existing as usize];
            if same_name && e.parent == parent {
                return;
            }
            if !same_name {
                self.garbage_bytes += e.name_len as usize;
                self.order_remove(existing);
                let (name_off, name_len) = self.push_name(&r.name);
                let e = &mut self.entries[existing as usize];
                (e.name_off, e.name_len) = (name_off, name_len);
                self.order_insert(existing);
            }
            self.entries[existing as usize].parent = parent;
        } else {
            let flags = FLAG_ALIVE | if r.is_dir { FLAG_DIR } else { 0 };
            let idx = self.push_entry(&r.name, parent, v as u8, flags);
            let map = &mut self.volumes[v].by_record;
            if rec >= map.len() {
                reserve_vec(map, rec + 1 - map.len());
                map.resize(rec + 1, NONE);
            }
            map[rec] = idx;
            self.order_insert(idx);
        }
        self.bump();
    }

    pub fn remove(&mut self, letter: char, frn: u64) {
        let Some(v) = self.volume_index(letter) else { return };
        let rec = record_no(frn);
        let idx = self.volumes[v].by_record.get(rec).copied().unwrap_or(NONE);
        if idx == NONE || !self.is_alive(idx) || self.entries[idx as usize].flags & FLAG_ROOT != 0 {
            return;
        }
        self.order_remove(idx);
        self.garbage_bytes += self.entries[idx as usize].name_len as usize;
        self.entries[idx as usize].flags &= !FLAG_ALIVE;
        self.volumes[v].by_record[rec] = NONE;
        self.bump();
    }

    /// Runs a query. Returns `None` if cancelled via `is_cancelled`.
    pub fn search(&self, query: &Query, is_cancelled: &(dyn Fn() -> bool + Sync)) -> Option<Vec<u32>> {
        if query.terms.is_empty() {
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

/// Diagnostic: times real searches (median of `runs`).
impl Index {
    pub fn bench_search(&self, queries: &[&str], runs: usize) -> Vec<String> {
        queries
            .iter()
            .map(|q| {
                let query = Query::parse(q);
                let mut times: Vec<f64> = Vec::new();
                let mut hits = 0;
                for _ in 0..runs {
                    let t = std::time::Instant::now();
                    hits = self.search(&query, &|| false).map_or(0, |r| r.len());
                    times.push(t.elapsed().as_secs_f64() * 1000.0);
                }
                times.sort_by(|a, b| a.partial_cmp(b).unwrap());
                format!("bench {q:>12}: {:7.2} ms, {hits} hits", times[times.len() / 2])
            })
            .collect()
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
pub struct Query {
    terms: Vec<Term>,
}

impl Query {
    pub fn parse(s: &str) -> Self {
        let lower = s.to_lowercase();
        let terms = lower
            .split_whitespace()
            .map(|t| {
                let t = t.replace('/', "\\");
                let on_path = t.contains('\\');
                let matcher = if t.contains('*') || t.contains('?') {
                    Matcher::Glob(t.chars().collect(), t.is_ascii().then(|| t.as_bytes().to_vec()))
                } else {
                    Matcher::Substr(t.as_bytes().to_vec(), memmem::Finder::new(t.as_bytes()).into_owned())
                };
                Term { matcher, on_path }
            })
            .collect::<Vec<Term>>();
        let mut terms = terms;
        // Cheap name terms first; path terms need a path to be built.
        terms.sort_by_key(|t| t.on_path);
        Query { terms }
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
            s.push_utf16(frn, parent, name.encode_utf16(), dir);
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
        let c = idx.compacted();
        assert_eq!(c.epoch, idx.epoch + 1);
        assert!(c.generation.load(AtomicOrdering::Relaxed) > gen_before);
        assert_eq!(c.dead_entries(), 0);
        assert_eq!(c.garbage_bytes, 0);
        assert_eq!(c.volumes[0].meta.next_usn, 42);
        assert_eq!(names(&c, ""), names(&idx, ""));
        let i = c.search(&Query::parse("renamed"), &|| false).unwrap()[0];
        assert_eq!(c.full_path(i), "C:\\Windows\\renamed.exe");
        // Live updates keep working on the compacted index.
        let mut c = c;
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
