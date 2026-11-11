//! On-disk cache in the index's own in-memory layout (format v3).
//!
//! The arrays are written as raw little-endian bytes and read straight back
//! into their final `Vec`s: no parsing, no intermediate copy, no re-sort.
//! Everything is validated after reading, so a damaged file is rejected
//! (and a full scan done) rather than trusted.
//!
//! ```text
//! magic "SWFTIDX\0", version u32 (3), endian marker u32,
//! entry count u64, names length u64, order length u64, garbage bytes u64,
//! volume count u32,
//! per volume: letter u32, serial u32, root_frn u64, journal_id u64,
//!             next_usn i64, root u32, by_record length u64
//! entries (20 bytes each), names (UTF-8), order (u32 each),
//! per volume: by_record (u32 each)
//! ```

use super::{Entry, FLAG_ALIVE, FLAG_DIR, FLAG_PENDING, FLAG_ROOT, Index, NONE, Volume, VolumeMeta, headroom};
use anyhow::{Context, Result, bail, ensure};
use rayon::prelude::*;
use std::io::{BufWriter, Read, Write};
use std::path::Path;

const MAGIC: &[u8; 8] = b"SWFTIDX\0";
const VERSION: u32 = 3;
const ENDIAN: u32 = 0x0102_0304;

#[cfg(not(target_endian = "little"))]
compile_error!("the cache format stores little-endian arrays");

/// Views a slice of plain-old-data values as bytes.
fn bytes_of<T: Copy>(v: &[T]) -> &[u8] {
    // SAFETY: only used with u32 and `Entry` (repr(C), no padding).
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
}

/// Reads `n` values into a new Vec (with growth headroom) straight from `r`.
fn read_vec<T: Copy + Default>(r: &mut impl Read, n: usize) -> Result<Vec<T>> {
    let mut v: Vec<T> = Vec::new();
    v.try_reserve_exact(n + headroom(n)).context("cache too large")?;
    v.resize(n, T::default());
    // SAFETY: as in `bytes_of`; every bit pattern is a valid u32 / `Entry`,
    // and validation happens before any value is used.
    let bytes = unsafe { std::slice::from_raw_parts_mut(v.as_mut_ptr() as *mut u8, n * size_of::<T>()) };
    r.read_exact(bytes).context("cache truncated")?;
    Ok(v)
}

pub fn save(path: &Path, index: &Index) -> Result<()> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_extension("tmp");
    {
        let mut w = BufWriter::with_capacity(1 << 20, std::fs::File::create(&tmp).context("create cache")?);
        w.write_all(MAGIC)?;
        for x in [VERSION, ENDIAN] {
            w.write_all(&x.to_le_bytes())?;
        }
        for x in [index.entries.len(), index.names.len(), index.order.len(), index.garbage_bytes] {
            w.write_all(&(x as u64).to_le_bytes())?;
        }
        w.write_all(&(index.volumes.len() as u32).to_le_bytes())?;
        for v in &index.volumes {
            w.write_all(&(v.letter as u32).to_le_bytes())?;
            w.write_all(&v.meta.serial.to_le_bytes())?;
            w.write_all(&v.meta.root_frn.to_le_bytes())?;
            w.write_all(&v.meta.journal_id.to_le_bytes())?;
            w.write_all(&v.meta.next_usn.to_le_bytes())?;
            w.write_all(&v.root.to_le_bytes())?;
            w.write_all(&(v.by_record.len() as u64).to_le_bytes())?;
        }
        w.write_all(bytes_of(&index.entries))?;
        w.write_all(index.names.as_bytes())?;
        w.write_all(bytes_of(&index.order))?;
        for v in &index.volumes {
            w.write_all(bytes_of(&v.by_record))?;
        }
        w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    }
    std::fs::rename(&tmp, path).context("replace cache")?;
    Ok(())
}

pub fn load(path: &Path) -> Result<Index> {
    let mut f = std::io::BufReader::with_capacity(1 << 16, std::fs::File::open(path).context("read cache")?);
    let file_len = f.get_ref().metadata()?.len();
    read(&mut f, file_len)
}

fn read(r: &mut impl Read, file_len: u64) -> Result<Index> {
    let mut u32_ = || -> Result<u32> {
        let mut b = [0u8; 4];
        r.read_exact(&mut b).context("cache truncated")?;
        Ok(u32::from_le_bytes(b))
    };
    let mut magic = [0u8; 8];
    magic[..4].copy_from_slice(&u32_()?.to_le_bytes());
    magic[4..].copy_from_slice(&u32_()?.to_le_bytes());
    ensure!(&magic == MAGIC, "bad cache magic");
    ensure!(u32_()? == VERSION, "cache version mismatch");
    ensure!(u32_()? == ENDIAN, "cache endianness mismatch");
    let mut u64_ = || -> Result<u64> {
        let mut b = [0u8; 8];
        r.read_exact(&mut b).context("cache truncated")?;
        Ok(u64::from_le_bytes(b))
    };
    let (n, names_len, order_len, garbage) = (u64_()?, u64_()?, u64_()?, u64_()?);
    // Sizes must be consistent with the file before allocating anything.
    let fixed = n.checked_mul(size_of::<Entry>() as u64).and_then(|e| e.checked_add(names_len)).and_then(|x| x.checked_add(order_len.checked_mul(4)?));
    ensure!(fixed.is_some_and(|x| x <= file_len) && n < NONE as u64, "cache sizes inconsistent");
    let mut b4 = [0u8; 4];
    r.read_exact(&mut b4)?;
    let volume_count = u32::from_le_bytes(b4);
    ensure!(volume_count <= 26, "cache volume count");
    let mut volumes = Vec::new();
    let mut by_record_lens = Vec::new();
    for _ in 0..volume_count {
        let mut rd = |len: usize| -> Result<Vec<u8>> {
            let mut b = vec![0u8; len];
            r.read_exact(&mut b).context("cache truncated")?;
            Ok(b)
        };
        let h = rd(4 + 4 + 8 + 8 + 8 + 4 + 8)?;
        let u32_at = |o: usize| u32::from_le_bytes(h[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(h[o..o + 8].try_into().unwrap());
        let letter = char::from_u32(u32_at(0)).filter(char::is_ascii_uppercase).context("cache volume letter")?;
        let meta = VolumeMeta { serial: u32_at(4), root_frn: u64_at(8), journal_id: u64_at(16), next_usn: u64_at(24) as i64 };
        let root = u32_at(32);
        let len = u64_at(36);
        ensure!(len.checked_mul(4).is_some_and(|b| b <= file_len), "cache record table size");
        by_record_lens.push(len as usize);
        volumes.push(Volume { letter, meta, root, by_record: Vec::new() });
    }
    let (n, names_len, order_len) = (n as usize, names_len as usize, order_len as usize);
    let entries: Vec<Entry> = read_vec(r, n)?;
    let names_bytes: Vec<u8> = read_vec(r, names_len)?;
    let names = String::from_utf8(names_bytes).context("cache names not UTF-8")?;
    let order: Vec<u32> = read_vec(r, order_len)?;
    for (v, len) in volumes.iter_mut().zip(by_record_lens) {
        v.by_record = read_vec(r, len)?;
    }
    let mut rest = [0u8; 1];
    ensure!(r.read(&mut rest)? == 0, "trailing bytes in cache");

    // Validate everything that is later used as an index or slice bound.
    let known = FLAG_ALIVE | FLAG_DIR | FLAG_ROOT;
    let entries_ok = entries.par_iter().all(|e| {
        let (start, end) = (e.name_off as usize, e.name_off as usize + e.name_len());
        end <= names.len()
            && names.is_char_boundary(start)
            && names.is_char_boundary(end)
            && (e.parent == NONE || (e.parent as usize) < n)
            && e.flags() & !known == 0
            && e.flags() & FLAG_PENDING == 0
    });
    ensure!(entries_ok, "cache entries invalid");
    let live = |i: u32| (i as usize) < n && entries[i as usize].flags() & FLAG_ALIVE != 0;
    ensure!(order.par_iter().all(|&i| live(i) && entries[i as usize].flags() & FLAG_ROOT == 0), "cache order invalid");
    for v in &volumes {
        ensure!(live(v.root) && entries[v.root as usize].flags() & FLAG_ROOT != 0, "cache volume root invalid");
        ensure!(v.by_record.par_iter().all(|&i| i == NONE || (i as usize) < n), "cache record table invalid");
    }
    if garbage as usize > names.len() {
        bail!("cache garbage count invalid");
    }

    let mut index = Index { entries, names, order, volumes, garbage_bytes: garbage as usize, ..Default::default() };
    index.tree_generation = 1;
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{Query, RawRecord, Stat, VolumeScan};

    fn sample() -> Index {
        let meta = VolumeMeta { serial: 7, root_frn: 5, journal_id: 9, next_usn: 1234 };
        let mut s = VolumeScan::new('D', meta);
        s.push_str(10, 5, "文件夹", true, Stat::UNKNOWN);
        s.push_str(11, 10, "a.txt", false, Stat { size: 42, mtime: 1_700_000_000 });
        s.push_str(12, 5, "Windows", true, Stat::UNKNOWN);
        let mut idx = Index::default();
        idx.add_volume(s);
        idx.upsert('D', &RawRecord { frn: 13, parent_frn: 12, name: "新建.txt".into(), is_dir: false });
        idx.remove('D', 11);
        idx
    }

    fn to_bytes(idx: &Index, test: &str) -> Vec<u8> {
        let path = std::env::temp_dir().join("swiftfind-test").join(format!("v3-{test}-{}.bin", std::process::id()));
        save(&path, idx).unwrap();
        let data = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        data
    }

    fn from_bytes(data: &[u8]) -> Result<Index> {
        read(&mut &data[..], data.len() as u64)
    }

    #[test]
    fn roundtrip_is_exact() {
        let idx = sample();
        let back = from_bytes(&to_bytes(&idx, "roundtrip")).unwrap();
        assert_eq!(back.entries, idx.entries);
        assert_eq!(back.names, idx.names);
        assert_eq!(back.order, idx.order);
        assert_eq!(back.garbage_bytes, idx.garbage_bytes);
        assert_eq!(back.volumes.len(), 1);
        assert_eq!(back.volumes[0].meta, idx.volumes[0].meta);
        assert_eq!(back.volumes[0].by_record, idx.volumes[0].by_record);
        let paths = |i: &Index| i.search(&Query::parse(""), &|| false).unwrap().iter().map(|&e| i.full_path(e)).collect::<Vec<_>>();
        assert_eq!(paths(&back), ["D:\\Windows", "D:\\文件夹", "D:\\Windows\\新建.txt"]);
        // Live updates keep working on a loaded index.
        let mut back = back;
        back.upsert('D', &RawRecord { frn: 14, parent_frn: 10, name: "b.txt".into(), is_dir: false });
        assert_eq!(paths(&back).len(), 4);
    }

    #[test]
    fn damaged_files_are_rejected() {
        let data = to_bytes(&sample(), "damaged");
        assert!(from_bytes(&data[..data.len() - 1]).is_err(), "truncated");
        let mut longer = data.clone();
        longer.push(0);
        assert!(from_bytes(&longer).is_err(), "trailing bytes");
        // Corrupt each byte after the header in turn: must never panic, and
        // anything accepted must still be safe to search.
        for i in 8..data.len() {
            let mut bad = data.clone();
            bad[i] ^= 0xFF;
            if let Ok(idx) = from_bytes(&bad) {
                let r = idx.search(&Query::parse("t"), &|| false).unwrap();
                for e in r {
                    let _ = idx.full_path(e);
                }
            }
        }
    }
}
