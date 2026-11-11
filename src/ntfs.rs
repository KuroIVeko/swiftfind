//! NTFS volume access: full enumeration via `FSCTL_ENUM_USN_DATA` and live
//! change tracking via `FSCTL_READ_USN_JOURNAL`. Requires administrator rights.

use crate::index::{RawRecord, Stat, VolumeMeta, VolumeScan};
use anyhow::{Context, Result};
use std::ffi::c_void;
use std::mem::size_of;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_HANDLE_EOF, ERROR_JOURNAL_DELETE_IN_PROGRESS, ERROR_JOURNAL_ENTRY_DELETED,
    ERROR_JOURNAL_NOT_ACTIVE, GENERIC_READ, HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_NO_BUFFERING,
    FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GetFileInformationByHandle, GetLogicalDrives, GetVolumeInformationW, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    CREATE_USN_JOURNAL_DATA, FSCTL_CREATE_USN_JOURNAL, FSCTL_ENUM_USN_DATA, FSCTL_GET_NTFS_VOLUME_DATA, FSCTL_QUERY_USN_JOURNAL,
    FSCTL_READ_USN_JOURNAL, MFT_ENUM_DATA_V0, NTFS_VOLUME_DATA_BUFFER, READ_USN_JOURNAL_DATA_V0, USN_JOURNAL_DATA_V0,
    USN_REASON_BASIC_INFO_CHANGE, USN_REASON_DATA_EXTEND, USN_REASON_DATA_OVERWRITE,
    USN_REASON_DATA_TRUNCATION, USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE,
    USN_REASON_RENAME_NEW_NAME,
};
use windows::core::HSTRING;

/// Lists drive letters whose file system is NTFS.
pub fn ntfs_drives() -> Vec<char> {
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| (b'A' + i) as char)
        .filter(|&l| {
            let root = HSTRING::from(format!("{l}:\\"));
            let mut fs = [0u16; 32];
            let ok = unsafe {
                GetVolumeInformationW(&root, None, None, None, None, Some(&mut fs))
            };
            ok.is_ok() && String::from_utf16_lossy(&fs).trim_end_matches('\0') == "NTFS"
        })
        .collect()
}

struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn open(path: &str, flags: FILE_FLAGS_AND_ATTRIBUTES) -> Result<OwnedHandle> {
    let h = unsafe {
        CreateFileW(
            &HSTRING::from(path),
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            flags,
            None,
        )
    }
    .with_context(|| format!("open {path}"))?;
    Ok(OwnedHandle(h))
}

unsafe fn ioctl<I>(h: HANDLE, code: u32, input: Option<&I>, out: &mut [u8]) -> windows::core::Result<u32> {
    let mut returned = 0u32;
    unsafe {
        DeviceIoControl(
            h,
            code,
            input.map(|i| i as *const I as *const c_void),
            input.map_or(0, |_| size_of::<I>() as u32),
            Some(out.as_mut_ptr() as *mut c_void),
            out.len() as u32,
            Some(&mut returned),
            None,
        )?;
    }
    Ok(returned)
}

pub struct NtfsVolume {
    pub letter: char,
    handle: OwnedHandle,
    serial: u32,
    root_frn: u64,
    journal_id: u64,
    /// Oldest USN still present in the journal.
    lowest_valid_usn: i64,
    pub next_usn: i64,
}

pub enum JournalError {
    /// Journal was reset/wrapped; a full re-index is required.
    Reset,
    Other(anyhow::Error),
}

impl NtfsVolume {
    pub fn open(letter: char) -> Result<Self> {
        let handle = open(&format!("\\\\.\\{letter}:"), FILE_FLAGS_AND_ATTRIBUTES(0))?;

        // FRN of the root directory.
        let root = open(&format!("{letter}:\\"), FILE_FLAG_BACKUP_SEMANTICS)?;
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        unsafe { GetFileInformationByHandle(root.0, &mut info) }.context("root file info")?;
        let root_frn = ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64;

        let mut serial = 0u32;
        unsafe {
            GetVolumeInformationW(&HSTRING::from(format!("{letter}:\\")), None, Some(&mut serial), None, None, None)
        }
        .context("volume serial")?;

        let mut v = NtfsVolume {
            letter,
            handle,
            serial,
            root_frn,
            journal_id: 0,
            lowest_valid_usn: 0,
            next_usn: 0,
        };
        v.query_journal()?;
        Ok(v)
    }

    fn query_journal(&mut self) -> Result<()> {
        let mut buf = [0u8; size_of::<USN_JOURNAL_DATA_V0>()];
        let r = unsafe { ioctl::<()>(self.handle.0, FSCTL_QUERY_USN_JOURNAL, None, &mut buf) };
        if let Err(e) = &r {
            if e.code() == ERROR_JOURNAL_NOT_ACTIVE.to_hresult()
                || e.code() == ERROR_JOURNAL_DELETE_IN_PROGRESS.to_hresult()
            {
                let create = CREATE_USN_JOURNAL_DATA { MaximumSize: 32 << 20, AllocationDelta: 8 << 20 };
                unsafe { ioctl(self.handle.0, FSCTL_CREATE_USN_JOURNAL, Some(&create), &mut []) }
                    .context("create USN journal")?;
                unsafe { ioctl::<()>(self.handle.0, FSCTL_QUERY_USN_JOURNAL, None, &mut buf) }
                    .context("query USN journal")?;
            } else {
                r.context("query USN journal")?;
            }
        }
        let data: USN_JOURNAL_DATA_V0 = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const _) };
        self.journal_id = data.UsnJournalID;
        self.lowest_valid_usn = data.LowestValidUsn;
        self.next_usn = data.NextUsn;
        Ok(())
    }

    pub fn meta(&self) -> VolumeMeta {
        VolumeMeta {
            serial: self.serial,
            root_frn: self.root_frn,
            journal_id: self.journal_id,
            next_usn: self.next_usn,
        }
    }

    /// If a cached index taken at `cached` can be brought up to date by
    /// replaying the journal, rewinds the read position to it and returns true.
    pub fn resume_from(&mut self, cached: &VolumeMeta) -> bool {
        let ok = cached.serial == self.serial
            && cached.root_frn == self.root_frn
            && cached.journal_id == self.journal_id
            && cached.next_usn >= self.lowest_valid_usn
            && cached.next_usn <= self.next_usn;
        if ok {
            self.next_usn = cached.next_usn;
        }
        ok
    }

    /// Enumerates every file record by reading the raw MFT (fast path).
    pub fn scan_mft(&mut self) -> Result<VolumeScan> {
        self.query_journal()?;
        let mut buf = [0u8; size_of::<NTFS_VOLUME_DATA_BUFFER>()];
        unsafe { ioctl::<()>(self.handle.0, FSCTL_GET_NTFS_VOLUME_DATA, None, &mut buf) }
            .context("FSCTL_GET_NTFS_VOLUME_DATA")?;
        let geo: NTFS_VOLUME_DATA_BUFFER = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const _) };
        let raw = open(&format!("\\\\.\\{}:", self.letter), FILE_FLAG_NO_BUFFERING)?;
        crate::mft::scan(raw.0, &geo, self.letter, self.meta())
    }

    /// Enumerates every file record via `FSCTL_ENUM_USN_DATA` (slow fallback).
    pub fn enumerate(&mut self) -> Result<VolumeScan> {
        self.query_journal()?;
        let mut scan = VolumeScan::with_capacity(self.letter, self.meta(), 1 << 20, 16 << 20);
        let mut buf = vec![0u8; 1 << 20];
        let mut med = MFT_ENUM_DATA_V0 { StartFileReferenceNumber: 0, LowUsn: 0, HighUsn: self.next_usn };
        loop {
            let n = match unsafe { ioctl(self.handle.0, FSCTL_ENUM_USN_DATA, Some(&med), &mut buf) } {
                Ok(n) => n as usize,
                Err(e) if e.code() == ERROR_HANDLE_EOF.to_hresult() => break,
                Err(e) => return Err(e).context("FSCTL_ENUM_USN_DATA"),
            };
            if n <= 8 {
                break;
            }
            med.StartFileReferenceNumber = u64::from_le_bytes(buf[0..8].try_into().unwrap());
            parse_records(&buf[8..n], |r| {
                scan.push_utf16(r.frn, r.parent_frn, r.name_utf16(), r.is_dir, Stat::UNKNOWN);
            });
        }
        Ok(scan)
    }

    /// Reads journal entries since the last call. Non-blocking.
    pub fn read_changes(&mut self, mut f: impl FnMut(Change)) -> Result<(), JournalError> {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let rd = READ_USN_JOURNAL_DATA_V0 {
                StartUsn: self.next_usn,
                ReasonMask: USN_REASON_FILE_CREATE
                    | USN_REASON_FILE_DELETE
                    | USN_REASON_RENAME_NEW_NAME
                    | USN_REASON_DATA_OVERWRITE
                    | USN_REASON_DATA_EXTEND
                    | USN_REASON_DATA_TRUNCATION
                    | USN_REASON_BASIC_INFO_CHANGE,
                ReturnOnlyOnClose: 0,
                Timeout: 0,
                BytesToWaitFor: 0,
                UsnJournalID: self.journal_id,
            };
            let n = match unsafe { ioctl(self.handle.0, FSCTL_READ_USN_JOURNAL, Some(&rd), &mut buf) } {
                Ok(n) => n as usize,
                Err(e)
                    if e.code() == ERROR_JOURNAL_ENTRY_DELETED.to_hresult()
                        || e.code() == ERROR_JOURNAL_NOT_ACTIVE.to_hresult()
                        || e.code() == ERROR_JOURNAL_DELETE_IN_PROGRESS.to_hresult() =>
                {
                    return Err(JournalError::Reset);
                }
                Err(e) => return Err(JournalError::Other(e.into())),
            };
            if n < 8 {
                return Ok(());
            }
            let next = i64::from_le_bytes(buf[0..8].try_into().unwrap());
            let had_records = n > 8;
            parse_records(&buf[8..n], |r| {
                // Reasons accumulate until the file is closed, so a record
                // for a renamed file also carries its later data changes.
                f(if r.reason & USN_REASON_FILE_DELETE != 0 {
                    Change::Removed(r.frn)
                } else if r.reason & (USN_REASON_FILE_CREATE | USN_REASON_RENAME_NEW_NAME) == 0 {
                    Change::Modified(r.frn)
                } else {
                    Change::Upsert(RawRecord {
                        frn: r.frn,
                        parent_frn: r.parent_frn,
                        name: String::from_utf16_lossy(&r.name_utf16().collect::<Vec<_>>()),
                        is_dir: r.is_dir,
                    })
                })
            });
            self.next_usn = next;
            if !had_records {
                return Ok(());
            }
        }
    }
}

pub enum Change {
    /// Created, renamed or moved.
    Upsert(RawRecord),
    Removed(u64),
    /// Contents or timestamps changed; only size/mtime need refreshing.
    Modified(u64),
}

/// A USN record borrowed from the ioctl output buffer.
struct RecordRef<'a> {
    frn: u64,
    parent_frn: u64,
    reason: u32,
    is_dir: bool,
    /// UTF-16LE bytes.
    name: &'a [u8],
}

impl RecordRef<'_> {
    fn name_utf16(&self) -> impl Iterator<Item = u16> + '_ {
        self.name.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]]))
    }
}

/// Parses a buffer of USN_RECORD_V2 / V3 records.
fn parse_records<'a>(mut buf: &'a [u8], mut f: impl FnMut(RecordRef<'a>)) {
    let u16_at = |b: &[u8], o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let u32_at = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let u64_at = |b: &[u8], o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    while buf.len() >= 8 {
        let len = u32_at(buf, 0) as usize;
        if len == 0 || len > buf.len() {
            break;
        }
        let rec = &buf[..len];
        // (frn, parent, reason, attrs, name_len, name_off)
        let fields = match u16_at(rec, 4) {
            2 if len >= 60 => Some((
                u64_at(rec, 8),
                u64_at(rec, 16),
                u32_at(rec, 40),
                u32_at(rec, 52),
                u16_at(rec, 56) as usize,
                u16_at(rec, 58) as usize,
            )),
            3 if len >= 76 => Some((
                u64_at(rec, 8),
                u64_at(rec, 24),
                u32_at(rec, 56),
                u32_at(rec, 68),
                u16_at(rec, 72) as usize,
                u16_at(rec, 74) as usize,
            )),
            _ => None,
        };
        if let Some((frn, parent_frn, reason, attrs, nlen, noff)) = fields
            && noff + nlen <= len
        {
            f(RecordRef {
                frn,
                parent_frn,
                reason,
                is_dir: attrs & FILE_ATTRIBUTE_DIRECTORY.0 != 0,
                name: &rec[noff..noff + nlen],
            });
        }
        buf = &buf[len..];
    }
}
