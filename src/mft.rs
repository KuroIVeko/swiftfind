//! Full volume scan by reading the raw `$MFT` straight off the disk.
//!
//! Much faster than `FSCTL_ENUM_USN_DATA`: the MFT is read sequentially in
//! large unbuffered chunks while worker threads parse the chunks already read.
//! The caller falls back to the ioctl if anything here looks unexpected.

use crate::index::{VolumeMeta, VolumeScan};
use anyhow::{Context, Result, bail, ensure};
use parking_lot::Mutex;
use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::collections::{BTreeMap, HashMap};
use std::ptr::NonNull;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::ReadFile;
use windows::Win32::System::IO::OVERLAPPED;
use windows::Win32::System::Ioctl::NTFS_VOLUME_DATA_BUFFER;

/// Bytes read per request; a multiple of every possible cluster/record size.
const CHUNK: usize = 4 << 20;
/// Chunk buffers in flight (being read or parsed): bounds memory to 32 MB.
const BUFFERS: usize = 8;

const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
const ATTR_END: u32 = 0xFFFF_FFFF;

const RECORD_IN_USE: u16 = 1;
const RECORD_IS_DIR: u16 = 2;

/// Sector-aligned heap buffer, as required for unbuffered reads.
struct AlignedBuf {
    ptr: NonNull<u8>,
    len: usize,
}

unsafe impl Send for AlignedBuf {}

impl AlignedBuf {
    const ALIGN: usize = 4096;

    fn new(len: usize) -> Self {
        let layout = Layout::from_size_align(len, Self::ALIGN).unwrap();
        let ptr = NonNull::new(unsafe { alloc_zeroed(layout) }).expect("out of memory");
        AlignedBuf { ptr, len }
    }
}

impl std::ops::Deref for AlignedBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl std::ops::DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr.as_ptr(), Layout::from_size_align(self.len, Self::ALIGN).unwrap()) }
    }
}

fn read_at(h: HANDLE, offset: u64, buf: &mut [u8]) -> Result<()> {
    let mut ov = OVERLAPPED::default();
    ov.Anonymous.Anonymous.Offset = offset as u32;
    ov.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
    let mut n = 0u32;
    unsafe { ReadFile(h, Some(buf), Some(&mut n), Some(&mut ov)) }
        .with_context(|| format!("read volume at {offset:#x}"))?;
    ensure!(n as usize == buf.len(), "short read at {offset:#x}: {n} of {}", buf.len());
    Ok(())
}

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().unwrap()))
}
fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().unwrap()))
}
fn u64_at(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().unwrap()))
}

/// Verifies the record signature and undoes the update sequence ("fixup")
/// protection on the last two bytes of every 512-byte stride. Returns false
/// for unused or torn records.
fn apply_fixup(rec: &mut [u8]) -> bool {
    if rec.get(0..4) != Some(b"FILE") {
        return false;
    }
    let (Some(uso), Some(uss)) = (u16_at(rec, 4), u16_at(rec, 6)) else { return false };
    let (uso, uss) = (uso as usize, uss as usize);
    if uss < 2 || uso + uss * 2 > rec.len() || (uss - 1) * 512 > rec.len() {
        return false;
    }
    let usn = [rec[uso], rec[uso + 1]];
    for i in 1..uss {
        let end = i * 512 - 2;
        if rec[end..end + 2] != usn {
            return false;
        }
        rec[end] = rec[uso + 2 * i];
        rec[end + 1] = rec[uso + 2 * i + 1];
    }
    true
}

/// Attributes of a (fixed-up) record: `(type, non_resident, name_len, attr bytes)`.
fn attributes(rec: &[u8]) -> impl Iterator<Item = (u32, bool, u8, &[u8])> {
    let end = u32_at(rec, 0x18).map_or(0, |n| (n as usize).min(rec.len()));
    let mut off = u16_at(rec, 0x14).map_or(usize::MAX, |o| o as usize);
    std::iter::from_fn(move || {
        let ty = u32_at(rec, off).filter(|_| off + 16 <= end)?;
        let len = u32_at(rec, off + 4)? as usize;
        if ty == ATTR_END || len < 16 || off + len > end {
            return None;
        }
        let a = &rec[off..off + len];
        off += len;
        Some((ty, a[8] != 0, a[9], a))
    })
}

/// Value of a resident attribute.
fn resident_value(a: &[u8]) -> Option<&[u8]> {
    let len = u32_at(a, 0x10)? as usize;
    let off = u16_at(a, 0x14)? as usize;
    a.get(off..off + len)
}

/// A `$FILE_NAME` candidate. Higher `rank` wins, then the first one; this
/// picks the same hard link as `FSCTL_ENUM_USN_DATA` in most cases.
#[derive(Clone)]
struct Name {
    rank: u8,
    parent: u64,
    utf16: Vec<u16>,
}

fn file_name(a: &[u8]) -> Option<(u8, u64, &[u8])> {
    let v = resident_value(a)?;
    let parent = u64_at(v, 0)?;
    let len = *v.get(0x40)? as usize;
    let rank = match *v.get(0x41)? {
        3 => 4, // Win32 & DOS
        1 => 3, // Win32
        0 => 2, // POSIX
        _ => 1, // DOS 8.3 alias only
    };
    Some((rank, parent, v.get(0x42..0x42 + len * 2)?))
}

fn to_utf16(b: &[u8]) -> impl Iterator<Item = u16> + '_ {
    b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]]))
}

/// Output of parsing one chunk.
struct Chunk {
    scan: VolumeScan,
    /// Base records whose best name may live in an extension record.
    pending: Vec<(u64, bool, Option<Name>)>,
    /// Names found in extension records, keyed by base record number.
    ext: Vec<(u64, Name)>,
}

fn parse_chunk(buf: &mut [u8], record_size: usize, first_rec: u64, count: usize) -> Chunk {
    let mut out = Chunk {
        scan: VolumeScan::with_capacity('?', VolumeMeta::default(), count, count * 24),
        pending: Vec::new(),
        ext: Vec::new(),
    };
    for (i, rec) in buf.chunks_exact_mut(record_size).take(count).enumerate() {
        if !apply_fixup(rec) {
            continue;
        }
        let flags = u16_at(rec, 0x16).unwrap_or(0);
        if flags & RECORD_IN_USE == 0 {
            continue;
        }
        let rec_no = first_rec + i as u64;
        let base = u64_at(rec, 0x20).unwrap_or(0) & 0x0000_FFFF_FFFF_FFFF;
        let mut best: Option<(u8, u64, &[u8])> = None;
        let mut has_list = false;
        for (ty, non_resident, _, a) in attributes(rec) {
            match ty {
                ATTR_ATTRIBUTE_LIST => has_list = true,
                ATTR_FILE_NAME if !non_resident => {
                    if let Some(n) = file_name(a)
                        && best.is_none_or(|b| n.0 > b.0)
                    {
                        best = Some(n);
                    }
                }
                _ => {}
            }
        }
        let is_dir = flags & RECORD_IS_DIR != 0;
        let owned = |(rank, parent, name): (u8, u64, &[u8])| Name { rank, parent, utf16: to_utf16(name).collect() };
        if base != 0 {
            // Extension record: its names belong to the base record.
            if let Some(n) = best {
                out.ext.push((base, owned(n)));
            }
        } else if has_list && best.is_none_or(|b| b.0 < 3) {
            out.pending.push((rec_no, is_dir, best.map(owned)));
        } else if let Some((_, parent, name)) = best {
            out.scan.push_utf16(rec_no, parent, to_utf16(name), is_dir);
        }
    }
    out
}

/// Joins chunk results in on-disk order as they complete, so at most a few
/// out-of-order chunks are buffered.
struct Merger {
    next: usize,
    waiting: BTreeMap<usize, Chunk>,
    scan: VolumeScan,
    pending: Vec<(u64, bool, Option<Name>)>,
    ext: Vec<(u64, Name)>,
}

impl Merger {
    fn push(&mut self, idx: usize, chunk: Chunk) {
        self.waiting.insert(idx, chunk);
        while let Some(c) = self.waiting.remove(&self.next) {
            self.scan.append(&c.scan);
            self.pending.extend(c.pending);
            self.ext.extend(c.ext);
            self.next += 1;
        }
    }
}

/// Decodes a non-resident attribute's run list into
/// `(file offset, disk offset, length)` byte extents.
fn data_runs(a: &[u8], cluster: u64) -> Option<Vec<(u64, u64, u64)>> {
    let mut vcn = u64_at(a, 0x10)?;
    let mut p = u16_at(a, 0x20)? as usize;
    let mut lcn: i64 = 0;
    let mut runs = Vec::new();
    loop {
        let header = *a.get(p)?;
        if header == 0 {
            return Some(runs);
        }
        let (len_size, off_size) = ((header & 15) as usize, (header >> 4) as usize);
        if len_size == 0 || len_size > 8 || off_size == 0 || off_size > 8 {
            return None; // sparse runs never occur in $MFT
        }
        let len_bytes = a.get(p + 1..p + 1 + len_size)?;
        let off_bytes = a.get(p + 1 + len_size..p + 1 + len_size + off_size)?;
        let len = len_bytes.iter().rev().fold(0u64, |acc, &b| acc << 8 | b as u64);
        let mut delta = off_bytes.iter().rev().fold(0i64, |acc, &b| acc << 8 | b as i64);
        let shift = 64 - 8 * off_size as u32;
        delta = (delta << shift) >> shift; // sign-extend
        lcn += delta;
        if lcn < 0 {
            return None;
        }
        runs.push((vcn * cluster, lcn as u64 * cluster, len * cluster));
        vcn += len;
        p += 1 + len_size + off_size;
    }
}

/// Reads and parses the whole MFT. `h` must be a volume handle opened with
/// `FILE_FLAG_NO_BUFFERING`.
pub fn scan(h: HANDLE, geo: &NTFS_VOLUME_DATA_BUFFER, letter: char, meta: VolumeMeta) -> Result<VolumeScan> {
    let cluster = geo.BytesPerCluster as u64;
    let record_size = geo.BytesPerFileRecordSegment as usize;
    let sector = geo.BytesPerSector as usize;
    let valid_len = geo.MftValidDataLength as u64;
    ensure!(
        cluster.is_power_of_two()
            && record_size.is_power_of_two()
            && sector.is_power_of_two()
            && sector <= AlignedBuf::ALIGN
            && CHUNK as u64 % cluster == 0
            && CHUNK % record_size == 0
            && record_size >= 512,
        "unsupported geometry: cluster {cluster}, record {record_size}, sector {sector}"
    );

    // Record 0 is $MFT itself; its unnamed $DATA attribute maps the MFT on disk.
    let mut first = AlignedBuf::new((cluster as usize).max(record_size));
    read_at(h, geo.MftStartLcn as u64 * cluster, &mut first)?;
    let rec0 = &mut first[..record_size];
    ensure!(apply_fixup(rec0), "bad $MFT record 0");
    let runs = attributes(rec0)
        .find(|&(ty, non_resident, name_len, _)| ty == ATTR_DATA && non_resident && name_len == 0)
        .and_then(|(_, _, _, a)| data_runs(a, cluster))
        .context("decode $MFT data runs")?;
    let mapped: u64 = runs.iter().map(|r| r.2).sum();
    let read_len = valid_len.div_ceil(cluster) * cluster;
    if mapped < read_len {
        // The rest of the run list lives in extension records; not handled.
        bail!("$MFT run list covers {mapped} of {read_len} bytes");
    }
    drop(first);

    let total_records = (valid_len / record_size as u64) as usize;
    let chunks = read_len.div_ceil(CHUNK as u64) as usize;
    let merger = Mutex::new(Merger {
        next: 0,
        waiting: BTreeMap::new(),
        scan: VolumeScan::with_capacity(letter, meta, total_records, total_records * 24),
        pending: Vec::new(),
        ext: Vec::new(),
    });
    let (free_tx, free_rx) = std::sync::mpsc::channel();
    for _ in 0..BUFFERS {
        free_tx.send(AlignedBuf::new(CHUNK)).unwrap();
    }

    // This thread reads sequentially; the rayon pool parses behind it.
    let read_result = rayon::in_place_scope(|s| -> Result<()> {
        for idx in 0..chunks {
            let pos = idx as u64 * CHUNK as u64;
            let len = (read_len - pos).min(CHUNK as u64);
            let mut buf = free_rx.recv().unwrap();
            for &(file_off, disk_off, run_len) in &runs {
                let start = pos.max(file_off);
                let end = (pos + len).min(file_off + run_len);
                if start < end {
                    let dst = &mut buf[(start - pos) as usize..(end - pos) as usize];
                    read_at(h, disk_off + (start - file_off), dst)?;
                }
            }
            let first_rec = pos / record_size as u64;
            let count = ((len / record_size as u64) as usize).min(total_records.saturating_sub(first_rec as usize));
            let (free_tx, merger) = (free_tx.clone(), &merger);
            s.spawn(move |_| {
                let chunk = parse_chunk(&mut buf, record_size, first_rec, count);
                free_tx.send(buf).ok();
                merger.lock().push(idx, chunk);
            });
        }
        Ok(())
    });
    read_result?;

    let Merger { mut scan, pending, ext, .. } = merger.into_inner();
    // Rare: files with so many attributes that their names spilled into
    // extension records.
    let mut ext_best: HashMap<u64, Name> = HashMap::new();
    for (base, n) in ext {
        let slot = ext_best.entry(base).or_insert_with(|| n.clone());
        if n.rank > slot.rank {
            *slot = n;
        }
    }
    for (rec, is_dir, own) in pending {
        let best = match (own, ext_best.remove(&rec)) {
            (Some(a), Some(b)) => Some(if b.rank > a.rank { b } else { a }),
            (a, b) => a.or(b),
        };
        if let Some(n) = best {
            scan.push_utf16(rec, n.parent, n.utf16.into_iter(), is_dir);
        }
    }
    drop_metafiles(&mut scan);
    Ok(scan)
}

/// Removes NTFS metadata files (`$MFT`, `$Bitmap`, ... and everything under
/// `$Extend`), which `FSCTL_ENUM_USN_DATA` doesn't report either. Their
/// names are not reused, so the index won't need them for parent lookups.
fn drop_metafiles(scan: &mut VolumeScan) {
    const ROOT: u64 = 5;
    const FIRST_USER_RECORD: u64 = 16;
    let mut hidden: std::collections::HashSet<u64> = (0..FIRST_USER_RECORD).filter(|&r| r != ROOT).collect();
    // $Extend's subtree is shallow ($Extend\\$RmMetadata\\$Txf\\...).
    loop {
        let before = hidden.len();
        for r in &scan.recs {
            if hidden.contains(&r.parent) {
                hidden.insert(r.rec);
            }
        }
        if hidden.len() == before {
            break;
        }
    }
    scan.recs.retain(|r| !hidden.contains(&r.rec));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_list_decoding() {
        // Run list at offset 0x40: 0x21 → 1-byte length, 2-byte offset.
        let mut a = vec![0u8; 0x50];
        a[0x20] = 0x40;
        a[0x40..0x48].copy_from_slice(&[0x21, 0x10, 0x00, 0x01, 0x21, 0x08, 0x00, 0xFF]);
        // second run: delta 0xFF00 sign-extended = -256 clusters
        let runs = data_runs(&a, 4096).unwrap();
        assert_eq!(runs, vec![(0, 256 * 4096, 16 * 4096), (16 * 4096, 0, 8 * 4096)]);
    }

    #[test]
    fn fixup_roundtrip() {
        let mut rec = vec![0u8; 1024];
        rec[0..4].copy_from_slice(b"FILE");
        rec[4] = 0x30; // update sequence offset
        rec[6] = 3; // usn + 2 strides
        rec[0x30..0x36].copy_from_slice(&[0xAB, 0xCD, 1, 2, 3, 4]);
        rec[510..512].copy_from_slice(&[0xAB, 0xCD]);
        rec[1022..1024].copy_from_slice(&[0xAB, 0xCD]);
        assert!(apply_fixup(&mut rec));
        assert_eq!(&rec[510..512], &[1, 2]);
        assert_eq!(&rec[1022..1024], &[3, 4]);
        // Torn record: second stride doesn't carry the sequence number.
        let mut torn = rec.clone();
        torn[0x30..0x32].copy_from_slice(&[0xAB, 0xCD]);
        torn[510..512].copy_from_slice(&[0xAB, 0xCD]);
        torn[1022..1024].copy_from_slice(&[0, 0]);
        assert!(!apply_fixup(&mut torn));
    }
}
