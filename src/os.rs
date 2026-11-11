//! Small Windows helpers: elevation, opening files, revealing in Explorer.

use std::os::windows::process::CommandExt;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW, SW_SHOWNORMAL};
use windows::core::{HSTRING, PCWSTR, w};

pub fn is_elevated() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut _),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// Re-launches the current executable with a UAC prompt. Returns false if the
/// user declined or launching failed.
pub fn relaunch_elevated() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let args: Vec<String> = std::env::args().skip(1).map(|a| quote(&a)).collect();
    let r = unsafe {
        ShellExecuteW(
            None,
            w!("runas"),
            &HSTRING::from(exe.as_os_str()),
            &HSTRING::from(args.join(" ")),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    r.0 as isize > 32
}

fn quote(s: &str) -> String {
    if s.is_empty() || s.contains([' ', '\t', '"']) {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

pub fn error_box(msg: &str) {
    unsafe {
        MessageBoxW(None, &HSTRING::from(msg), w!("SwiftFind"), MB_OK | MB_ICONERROR);
    }
}

/// Opens a file or folder with its default handler.
pub fn open(path: &str) {
    unsafe {
        ShellExecuteW(None, w!("open"), &HSTRING::from(path), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

/// Opens Explorer with the item selected.
pub fn reveal(path: &str) {
    let _ = std::process::Command::new("explorer.exe")
        .raw_arg(format!("/select,\"{path}\""))
        .spawn();
}

/// (private bytes, working set, peak working set) of this process, in MB.
pub fn memory_mb() -> (u64, u64, u64) {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
    let mut c = PROCESS_MEMORY_COUNTERS_EX::default();
    unsafe {
        let _ = GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut c as *mut _ as *mut PROCESS_MEMORY_COUNTERS,
            size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        );
    }
    let mb = |b: usize| (b / (1024 * 1024)) as u64;
    (mb(c.PrivateUsage), mb(c.WorkingSetSize), mb(c.PeakWorkingSetSize))
}
