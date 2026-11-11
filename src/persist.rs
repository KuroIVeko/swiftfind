//! On-disk index cache, so startup doesn't need a full MFT scan.
//!
//! Layout (little endian):
//! ```text
//! magic "SWFTIDX\0", version u32, volume count u32
//! per volume:
//!   letter u8, serial u32, root_frn u64, journal_id u64, next_usn i64,
//!   record count u64, names byte length u64, names (UTF-8, concatenated),
//!   records: { rec u32, parent u32, name_len u16, is_dir u8, size u64, mtime u32 } * count
//! ```
//! Names are stored back to back in record order, so offsets are implicit.

use crate::index::{LiveView, Stat, VolumeMeta, VolumeScan};
use anyhow::{Context, Result, bail, ensure};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"SWFTIDX\0";
const VERSION: u32 = 2;
const RECORD_BYTES: usize = 23;

pub fn cache_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("swiftfind").join("index.bin")
}

/// Anything whose per-volume records can be written to the cache.
pub trait CacheSource {
    fn volume_count(&self) -> usize;
    fn volume(&self, v: usize) -> (char, VolumeMeta);
    /// Calls `f(record, parent_record, name, is_dir, stat)` for every record.
    fn for_each(&self, v: usize, f: &mut dyn FnMut(u64, u64, &str, bool, Stat));
}

impl CacheSource for LiveView<'_> {
    fn volume_count(&self) -> usize {
        LiveView::volume_count(self)
    }
    fn volume(&self, v: usize) -> (char, VolumeMeta) {
        LiveView::volume(self, v)
    }
    fn for_each(&self, v: usize, f: &mut dyn FnMut(u64, u64, &str, bool, Stat)) {
        LiveView::for_each(self, v, f)
    }
}

impl CacheSource for [VolumeScan] {
    fn volume_count(&self) -> usize {
        self.len()
    }
    fn volume(&self, v: usize) -> (char, VolumeMeta) {
        (self[v].letter, self[v].meta)
    }
    fn for_each(&self, v: usize, f: &mut dyn FnMut(u64, u64, &str, bool, Stat)) {
        let s = &self[v];
        for r in &s.recs {
            f(r.rec, r.parent, s.name(r), r.is_dir, r.stat);
        }
    }
}

/// Writes the cache by streaming records from `src` (three passes per volume:
/// sizes, names, records), so no copy of the index is needed.
pub fn save(path: &Path, src: &(impl CacheSource + ?Sized)) -> Result<()> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_extension("tmp");
    {
        let mut w = BufWriter::with_capacity(1 << 20, std::fs::File::create(&tmp).context("create cache")?);
        w.write_all(MAGIC)?;
        w.write_all(&VERSION.to_le_bytes())?;
        w.write_all(&(src.volume_count() as u32).to_le_bytes())?;
        for v in 0..src.volume_count() {
            let (letter, m) = src.volume(v);
            w.write_all(&[letter as u8])?;
            w.write_all(&m.serial.to_le_bytes())?;
            w.write_all(&m.root_frn.to_le_bytes())?;
            w.write_all(&m.journal_id.to_le_bytes())?;
            w.write_all(&m.next_usn.to_le_bytes())?;

            let (mut count, mut names_len, mut too_large) = (0u64, 0u64, false);
            src.for_each(v, &mut |rec, parent, name, _, _| {
                count += 1;
                names_len += name.len() as u64;
                too_large |= rec > u32::MAX as u64 || parent > u32::MAX as u64;
            });
            if too_large {
                bail!("MFT record number too large for cache format");
            }
            w.write_all(&count.to_le_bytes())?;
            w.write_all(&names_len.to_le_bytes())?;

            let mut err = Ok(());
            src.for_each(v, &mut |_, _, name, _, _| {
                if err.is_ok() {
                    err = w.write_all(name.as_bytes());
                }
            });
            src.for_each(v, &mut |rec, parent, name, is_dir, stat| {
                if err.is_ok() {
                    let mut buf = [0u8; RECORD_BYTES];
                    buf[0..4].copy_from_slice(&(rec as u32).to_le_bytes());
                    buf[4..8].copy_from_slice(&(parent as u32).to_le_bytes());
                    buf[8..10].copy_from_slice(&(name.len() as u16).to_le_bytes());
                    buf[10] = is_dir as u8;
                    buf[11..19].copy_from_slice(&stat.size.to_le_bytes());
                    buf[19..23].copy_from_slice(&stat.mtime.to_le_bytes());
                    err = w.write_all(&buf);
                }
            });
            err?;
        }
        w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    }
    std::fs::rename(&tmp, path).context("replace cache")?;
    Ok(())
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        ensure!(self.buf.len() - self.pos >= n, "cache truncated");
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into()?))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into()?))
    }
}

pub fn load(path: &Path) -> Result<Vec<VolumeScan>> {
    let data = std::fs::read(path).context("read cache")?;
    parse(&data)
}

fn parse(data: &[u8]) -> Result<Vec<VolumeScan>> {
    let mut r = Reader { buf: data, pos: 0 };
    ensure!(r.take(8)? == MAGIC, "bad cache magic");
    ensure!(r.u32()? == VERSION, "cache version mismatch");
    let count = r.u32()?;
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let letter = r.u8()? as char;
        let meta = VolumeMeta {
            serial: r.u32()?,
            root_frn: r.u64()?,
            journal_id: r.u64()?,
            next_usn: r.u64()? as i64,
        };
        let n = r.u64()? as usize;
        let names_len = r.u64()? as usize;
        let names = std::str::from_utf8(r.take(names_len)?).context("cache names not UTF-8")?;
        ensure!(n.checked_mul(RECORD_BYTES).is_some_and(|b| b <= data.len()), "cache record count");
        let mut scan = VolumeScan::with_capacity(letter, meta, n, names_len);
        let mut off = 0usize;
        for _ in 0..n {
            let rec = r.u32()? as u64;
            let parent = r.u32()? as u64;
            let len = r.u16()? as usize;
            let is_dir = r.u8()? != 0;
            let stat = Stat { size: r.u64()?, mtime: r.u32()? };
            let name = names.get(off..off + len).context("cache name out of range")?;
            off += len;
            scan.push_str(rec, parent, name, is_dir, stat);
        }
        ensure!(off == names_len, "cache names length mismatch");
        out.push(scan);
    }
    ensure!(r.pos == data.len(), "trailing bytes in cache");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let meta = VolumeMeta { serial: 7, root_frn: 5, journal_id: 9, next_usn: 1234 };
        let mut s = VolumeScan::new('D', meta);
        s.push_str(10, 5, "文件夹", true, Stat::UNKNOWN);
        s.push_str(11, 10, "a.txt", false, Stat { size: 42, mtime: 1_700_000_000 });
        let path = std::env::temp_dir().join("swiftfind-test").join("idx.bin");
        save(&path, &[s][..]).unwrap();
        let back = load(&path).unwrap();
        assert_eq!(back.len(), 1);
        let b = &back[0];
        assert_eq!((b.letter, b.meta), ('D', meta));
        let names: Vec<_> = b.recs.iter().map(|r| (r.rec, r.parent, b.name(r), r.is_dir, r.stat.size)).collect();
        assert_eq!(names, [(10, 5, "文件夹", true, u64::MAX), (11, 10, "a.txt", false, 42)]);
        assert_eq!(b.recs[1].stat.mtime, 1_700_000_000);
        assert!(parse(&std::fs::read(&path).unwrap()[..20]).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn streaming_save_matches_snapshot() {
        use crate::index::{Index, RawRecord};
        let meta = VolumeMeta { serial: 1, root_frn: 5, journal_id: 2, next_usn: 3 };
        let mut s = VolumeScan::new('C', meta);
        s.push_str(10, 5, "Windows", true, Stat::UNKNOWN);
        s.push_str(11, 10, "notepad.exe", false, Stat { size: 1, mtime: 2 });
        s.push_str(12, 5, "用户", true, Stat::UNKNOWN);
        let mut idx = Index::default();
        idx.add_volume(s);
        idx.upsert('C', &RawRecord { frn: 13, parent_frn: 12, name: "新建.txt".into(), is_dir: false });
        idx.remove('C', 11);

        let path = std::env::temp_dir().join("swiftfind-test").join("stream.bin");
        save(&path, &idx.live_view()).unwrap();
        let loaded = load(&path).unwrap();
        let expected = idx.snapshot();
        let flat = |v: &[VolumeScan]| -> Vec<_> {
            v.iter()
                .flat_map(|s| s.recs.iter().map(move |r| (s.letter, s.meta, r.rec, r.parent, s.name(r).to_string(), r.is_dir, r.stat)))
                .collect()
        };
        assert_eq!(flat(&loaded), flat(&expected));
        assert_eq!(flat(&loaded).len(), 3);
        let _ = std::fs::remove_file(&path);
    }
}
