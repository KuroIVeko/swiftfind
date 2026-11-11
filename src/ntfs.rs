//! NTFS volume access: full enumeration via `FSCTL_ENUM_USN_DATA` and live
//! change tracking via `FSCTL_READ_USN_JOURNAL`. Requires administrator rights.

use crate::index::RawRecord;
use anyhow::{Context, Result};
use std::ffi::c_void;
use std::mem::size_of;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_HANDLE_EOF, ERROR_JOURNAL_DELETE_IN_PROGRESS, ERROR_JOURNAL_ENTRY_DELETED,
    ERROR_JOURNAL_NOT_ACTIVE, GENERIC_READ, HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GetFileInformationByHandle, GetLogicalDrives, GetVolumeInformationW, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    CREATE_USN_JOURNAL_DATA, FSCTL_CREATE_USN_JOURNAL, FSCTL_ENUM_USN_DATA, FSCTL_QUERY_USN_JOURNAL,
    FSCTL_READ_USN_JOURNAL, MFT_ENUM_DATA_V0, READ_USN_JOURNAL_DATA_V0, USN_JOURNAL_DATA_V0,
    USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE, USN_REASON_RENAME_NEW_NAME,
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
    pub root_frn: u64,
    journal_id: u64,
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

        let mut v = NtfsVolume { letter, handle, root_frn, journal_id: 0, next_usn: 0 };
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
        self.next_usn = data.NextUsn;
        Ok(())
    }

    /// Enumerates every file record on the volume.
    pub fn enumerate(&mut self) -> Result<Vec<RawRecord>> {
        self.query_journal()?;
        let mut out = Vec::with_capacity(1 << 20);
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
            parse_records(&buf[8..n], |r, _| out.push(r));
        }
        Ok(out)
    }

    /// Reads journal entries since the last call. Non-blocking.
    pub fn read_changes(&mut self, mut f: impl FnMut(Change)) -> Result<(), JournalError> {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let rd = READ_USN_JOURNAL_DATA_V0 {
                StartUsn: self.next_usn,
                ReasonMask: USN_REASON_FILE_CREATE | USN_REASON_FILE_DELETE | USN_REASON_RENAME_NEW_NAME,
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
            parse_records(&buf[8..n], |r, reason| {
                f(if reason & USN_REASON_FILE_DELETE != 0 {
                    Change::Removed(r.frn)
                } else {
                    Change::Upsert(r)
                })
            });
            self.next_usn = next;
            if !had_records {
                return Ok(());
            }
        }
    }

    /// Re-queries the journal after a reset.
    pub fn reset_journal(&mut self) -> Result<()> {
        self.query_journal()
    }
}

pub enum Change {
    Upsert(RawRecord),
    Removed(u64),
}

/// Parses a buffer of USN_RECORD_V2 / V3 records.
fn parse_records(mut buf: &[u8], mut f: impl FnMut(RawRecord, u32)) {
    let u16_at = |b: &[u8], o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let u32_at = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let u64_at = |b: &[u8], o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    while buf.len() >= 8 {
        let len = u32_at(buf, 0) as usize;
        if len == 0 || len > buf.len() {
            break;
        }
        let rec = &buf[..len];
        let major = u16_at(rec, 4);
        // (frn, parent, reason, attrs, name_len, name_off)
        let fields = match major {
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
        if let Some((frn, parent_frn, reason, attrs, nlen, noff)) = fields {
            if noff + nlen <= len {
                let units: Vec<u16> = rec[noff..noff + nlen]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                let name = String::from_utf16_lossy(&units);
                let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
                f(RawRecord { frn, parent_frn, name, is_dir }, reason);
            }
        }
        buf = &buf[len..];
    }
}
