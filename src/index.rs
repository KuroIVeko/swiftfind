//! In-memory file index shared by all NTFS volumes.
//!
//! Layout is kept compact: every entry is a fixed-size struct that points into
//! two string arenas (original-case and lower-case names). `order` holds the
//! indices of all live entries sorted by lower-case name, so search results come
//! out already sorted without a per-query sort.

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
    lower_off: u32,
    parent: u32,
    name_len: u16,
    lower_len: u16,
    vol: u8,
    flags: u8,
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

    pub fn name(&self, r: &ScanRec) -> &str {
        &self.names[r.name_off as usize..r.name_off as usize + r.name_len as usize]
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
    lower: String,
    order: Vec<u32>,
    volumes: Vec<Volume>,
    /// Bumped on every mutation so the UI knows when to refresh.
    pub generation: AtomicU64,
}

#[inline]
fn record_no(frn: u64) -> usize {
    (frn & 0x0000_FFFF_FFFF_FFFF) as usize
}

impl Index {
    fn push_entry(&mut self, name: &str, parent: u32, vol: u8, flags: u8) -> u32 {
        let (name_off, name_len, lower_off, lower_len) = self.push_names(name);
        let e = Entry { name_off, lower_off, parent, name_len, lower_len, vol, flags };
        let idx = self.entries.len() as u32;
        self.entries.push(e);
        idx
    }

    /// Appends a name to both arenas; returns (name_off, name_len, lower_off, lower_len).
    fn push_names(&mut self, name: &str) -> (u32, u16, u32, u16) {
        let name_off = self.names.len() as u32;
        let lower_off = self.lower.len() as u32;
        self.names.push_str(name);
        if name.is_ascii() {
            self.lower.extend(name.bytes().map(|b| b.to_ascii_lowercase() as char));
        } else {
            self.lower.extend(name.chars().flat_map(char::to_lowercase));
        }
        let lower_len = (self.lower.len() - lower_off as usize) as u16;
        (name_off, name.len() as u16, lower_off, lower_len)
    }

    #[inline]
    fn lower_of(&self, idx: u32) -> &str {
        let e = &self.entries[idx as usize];
        &self.lower[e.lower_off as usize..e.lower_off as usize + e.lower_len as usize]
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
        self.build_path(idx, false)
    }

    /// Path of the directory containing the entry, e.g. `C:\Windows`.
    pub fn parent_path(&self, idx: u32) -> String {
        let p = self.entries[idx as usize].parent;
        if p == NONE {
            return String::new();
        }
        self.build_path(p, false)
    }

    fn build_path(&self, idx: u32, lower: bool) -> String {
        let mut parts: Vec<&str> = Vec::with_capacity(16);
        let mut cur = idx;
        // Guard against cycles caused by inconsistent journal data.
        for _ in 0..512 {
            if cur == NONE {
                break;
            }
            parts.push(if lower { self.lower_of(cur) } else { self.name(cur) });
            let e = &self.entries[cur as usize];
            if e.flags & FLAG_ROOT != 0 {
                break;
            }
            cur = e.parent;
        }
        let mut s = String::with_capacity(parts.iter().map(|p| p.len() + 1).sum());
        for (i, p) in parts.iter().rev().enumerate() {
            if i > 0 {
                s.push('\\');
            }
            s.push_str(p);
        }
        if parts.len() == 1 {
            s.push('\\'); // bare root: "C:\"
        }
        s
    }

    fn cmp_entries(&self, a: u32, b: u32) -> Ordering {
        self.lower_of(a).cmp(self.lower_of(b)).then(a.cmp(&b))
    }

    /// Adds a whole volume. If the volume already exists (re-index after a
    /// journal reset), it is replaced.
    pub fn add_volume(&mut self, scan: VolumeScan) {
        let letter = scan.letter;
        let existing = self.volume_index(letter);
        if let Some(v) = existing {
            for e in self.entries.iter_mut().filter(|e| e.vol == v as u8) {
                e.flags &= !FLAG_ALIVE;
            }
        }
        let vol = existing.unwrap_or(self.volumes.len()) as u8;
        let root_rec = record_no(scan.meta.root_frn);
        let root = self.push_entry(&format!("{letter}:"), NONE, vol, FLAG_DIR | FLAG_ALIVE | FLAG_ROOT);

        let max_rec = scan.recs.iter().map(|r| r.rec as usize).chain([root_rec]).max().unwrap_or(0);
        let mut by_record = vec![NONE; max_rec + 1];
        by_record[root_rec] = root;

        let first = self.entries.len();
        self.entries.reserve(scan.recs.len());
        self.names.reserve(scan.names.len());
        self.lower.reserve(scan.names.len());
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

    /// Extracts the live contents of every volume, for writing the cache.
    pub fn snapshot(&self) -> Vec<VolumeScan> {
        let mut rec_of = vec![u64::MAX; self.entries.len()];
        for v in &self.volumes {
            for (rec, &idx) in v.by_record.iter().enumerate() {
                if idx != NONE {
                    rec_of[idx as usize] = rec as u64;
                }
            }
        }
        self.volumes
            .iter()
            .map(|v| {
                let live = v.by_record.iter().filter(|&&i| i != NONE).count();
                let mut scan = VolumeScan::with_capacity(v.letter, v.meta, live, live * 16);
                for (rec, &idx) in v.by_record.iter().enumerate() {
                    if idx == NONE || idx == v.root || !self.is_alive(idx) {
                        continue;
                    }
                    let e = &self.entries[idx as usize];
                    let parent = match rec_of.get(e.parent as usize) {
                        Some(&p) if p != u64::MAX => p,
                        _ => record_no(v.meta.root_frn) as u64,
                    };
                    scan.push_str(rec as u64, parent, self.name(idx), e.flags & FLAG_DIR != 0);
                }
                scan
            })
            .collect()
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
                self.order_remove(existing);
                let (name_off, name_len, lower_off, lower_len) = self.push_names(&r.name);
                let e = &mut self.entries[existing as usize];
                (e.name_off, e.name_len, e.lower_off, e.lower_len) = (name_off, name_len, lower_off, lower_len);
                self.order_insert(existing);
            }
            self.entries[existing as usize].parent = parent;
        } else {
            let flags = FLAG_ALIVE | if r.is_dir { FLAG_DIR } else { 0 };
            let idx = self.push_entry(&r.name, parent, v as u8, flags);
            let map = &mut self.volumes[v].by_record;
            if rec >= map.len() {
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
        let name = self.lower_of(idx);
        let mut path: Option<String> = None;
        for t in &q.terms {
            let hay: &str = if t.on_path {
                path.get_or_insert_with(|| self.build_path(idx, true))
            } else {
                name
            };
            if !t.matcher.is_match(hay) {
                return false;
            }
        }
        true
    }
}

enum Matcher {
    Substr(memmem::Finder<'static>),
    Glob(Vec<char>),
}

impl Matcher {
    fn is_match(&self, hay: &str) -> bool {
        match self {
            Matcher::Substr(f) => f.find(hay.as_bytes()).is_some(),
            Matcher::Glob(p) => glob_match(p, hay),
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
                    Matcher::Glob(t.chars().collect())
                } else {
                    Matcher::Substr(memmem::Finder::new(t.as_bytes()).into_owned())
                };
                Term { matcher, on_path }
            })
            .collect();
        Query { terms }
    }
}

/// Iterative wildcard match supporting `*` and `?` over chars.
fn glob_match(pattern: &[char], text: &str) -> bool {
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut mark = 0usize;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            mark = t;
            p += 1;
        } else if let Some(s) = star {
            p = s + 1;
            mark += 1;
            t = mark;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
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
