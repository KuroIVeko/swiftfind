//! Small Windows helpers: elevation, opening files, revealing in Explorer.

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

/// Opens a file or folder with its default handler, as the normal user.
pub fn open(path: &str) {
    let dir = std::path::Path::new(path).parent().map(|p| p.display().to_string()).unwrap_or_default();
    crate::launch::spawn(path.to_string(), String::new(), dir);
}

/// Opens Explorer with the item selected, as the normal user.
pub fn reveal(path: &str) {
    crate::launch::spawn("explorer.exe".into(), format!("/select,\"{path}\""), String::new());
}

/// Direct `ShellExecute` from this (elevated) process; fallback only.
pub fn shell_execute_elevated(file: &str, args: &str, dir: &str) {
    let opt = |s: &str| (!s.is_empty()).then(|| HSTRING::from(s));
    let (args, dir) = (opt(args), opt(dir));
    let ptr = |h: &Option<HSTRING>| h.as_ref().map_or(PCWSTR::null(), |h| PCWSTR(h.as_ptr()));
    unsafe {
        ShellExecuteW(None, PCWSTR::null(), &HSTRING::from(file), ptr(&args), ptr(&dir), SW_SHOWNORMAL);
    }
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

/// Shows the shell "Properties" dialog for a file or folder.
pub fn properties(path: &str) {
    use windows::Win32::UI::Shell::{SEE_MASK_INVOKEIDLIST, SHELLEXECUTEINFOW, ShellExecuteExW};
    let file = HSTRING::from(path);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_INVOKEIDLIST,
        lpVerb: w!("properties"),
        lpFile: PCWSTR(file.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe {
        let _ = ShellExecuteExW(&mut info);
    }
}

pub mod window {
    //! Direct Win32 control of the main window (gpui has no hide/show on Windows).
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::WindowsAndMessaging::*;

    fn h(hwnd: isize) -> HWND {
        HWND(hwnd as _)
    }

    pub fn hide(hwnd: isize) {
        unsafe {
            let _ = ShowWindow(h(hwnd), SW_HIDE);
        }
    }

    pub fn is_visible(hwnd: isize) -> bool {
        unsafe { IsWindowVisible(h(hwnd)).as_bool() && !IsIconic(h(hwnd)).as_bool() }
    }

    /// Whether the window is visible and the active foreground window.
    pub fn is_front(hwnd: isize) -> bool {
        unsafe { IsWindowVisible(h(hwnd)).as_bool() && !IsIconic(h(hwnd)).as_bool() && GetForegroundWindow() == h(hwnd) }
    }

    /// Shows, restores and activates the window.
    pub fn show(hwnd: isize) {
        let hwnd = h(hwnd);
        unsafe {
            let _ = ShowWindow(hwnd, if IsIconic(hwnd).as_bool() { SW_RESTORE } else { SW_SHOW });
            if SetForegroundWindow(hwnd).as_bool() && GetForegroundWindow() == hwnd {
                return;
            }
            // Foreground lock: borrow the input state of the current
            // foreground thread so the activation is allowed.
            let fg = GetForegroundWindow();
            let fg_thread = GetWindowThreadProcessId(fg, None);
            let me = GetCurrentThreadId();
            if fg_thread != 0 && fg_thread != me {
                let _ = AttachThreadInput(me, fg_thread, true);
                let _ = BringWindowToTop(hwnd);
                let _ = SetForegroundWindow(hwnd);
                let _ = AttachThreadInput(me, fg_thread, false);
            }
        }
    }

    pub fn set_icon(hwnd: isize, big: HICON, small: HICON) {
        unsafe {
            for (kind, icon) in [(ICON_BIG, big), (ICON_SMALL, small)] {
                SendMessageW(h(hwnd), WM_SETICON, Some(WPARAM(kind as usize)), Some(LPARAM(icon.0 as isize)));
            }
        }
    }
}
