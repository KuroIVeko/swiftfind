//! On-disk index cache, so startup doesn't need a full MFT scan.
//!
//! Layout (little endian):
//! ```text
//! magic "SWFTIDX\0", version u32, volume count u32
//! per volume:
//!   letter u8, serial u32, root_frn u64, journal_id u64, next_usn i64,
//!   record count u64, names byte length u64, names (UTF-8, concatenated),
//!   records: { rec u32, parent u32, name_len u16, is_dir u8 } * count
//! ```
//! Names are stored back to back in record order, so offsets are implicit.

use crate::index::{VolumeMeta, VolumeScan};
use anyhow::{Context, Result, bail, ensure};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"SWFTIDX\0";
const VERSION: u32 = 1;

pub fn cache_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("swiftfind").join("index.bin")
}

pub fn save(path: &Path, scans: &[VolumeScan]) -> Result<()> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_extension("tmp");
    {
        let mut w = BufWriter::with_capacity(1 << 20, std::fs::File::create(&tmp).context("create cache")?);
        w.write_all(MAGIC)?;
        w.write_all(&VERSION.to_le_bytes())?;
        w.write_all(&(scans.len() as u32).to_le_bytes())?;
        for s in scans {
            let m = &s.meta;
            w.write_all(&[s.letter as u8])?;
            w.write_all(&m.serial.to_le_bytes())?;
            w.write_all(&m.root_frn.to_le_bytes())?;
            w.write_all(&m.journal_id.to_le_bytes())?;
            w.write_all(&m.next_usn.to_le_bytes())?;
            w.write_all(&(s.recs.len() as u64).to_le_bytes())?;
            // Names in record order (they are already contiguous for scans
            // produced by this program, but don't rely on it).
            let names_len: u64 = s.recs.iter().map(|r| r.name_len as u64).sum();
            w.write_all(&names_len.to_le_bytes())?;
            for r in &s.recs {
                w.write_all(s.name(r).as_bytes())?;
            }
            for r in &s.recs {
                let (Ok(rec), Ok(parent)) = (u32::try_from(r.rec), u32::try_from(r.parent)) else {
                    bail!("MFT record number too large for cache format");
                };
                w.write_all(&rec.to_le_bytes())?;
                w.write_all(&parent.to_le_bytes())?;
                w.write_all(&r.name_len.to_le_bytes())?;
                w.write_all(&[r.is_dir as u8])?;
            }
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
        ensure!(n.checked_mul(11).is_some_and(|b| b <= data.len()), "cache record count");
        let mut scan = VolumeScan::with_capacity(letter, meta, n, names_len);
        let mut off = 0usize;
        for _ in 0..n {
            let rec = r.u32()? as u64;
            let parent = r.u32()? as u64;
            let len = r.u16()? as usize;
            let is_dir = r.u8()? != 0;
            let name = names.get(off..off + len).context("cache name out of range")?;
            off += len;
            scan.push_str(rec, parent, name, is_dir);
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
        s.push_str(10, 5, "文件夹", true);
        s.push_str(11, 10, "a.txt", false);
        let path = std::env::temp_dir().join("swiftfind-test").join("idx.bin");
        save(&path, &[s]).unwrap();
        let back = load(&path).unwrap();
        assert_eq!(back.len(), 1);
        let b = &back[0];
        assert_eq!((b.letter, b.meta), ('D', meta));
        let names: Vec<_> = b.recs.iter().map(|r| (r.rec, r.parent, b.name(r), r.is_dir)).collect();
        assert_eq!(names, [(10, 5, "文件夹", true), (11, 10, "a.txt", false)]);
        assert!(parse(&std::fs::read(&path).unwrap()[..20]).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
