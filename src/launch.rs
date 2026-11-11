//! Starting programs and documents as the normal (unelevated) user.
//!
//! SwiftFind runs elevated to read the MFT, and anything it starts directly
//! would inherit administrator rights. Instead we ask the desktop's Explorer
//! process to do the `ShellExecute`, via `IShellDispatch2` obtained from the
//! desktop folder view, so the new process is Explorer's child and runs at
//! medium integrity like anything launched from the Start menu.

use windows::Win32::System::Com::{
    CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize, IDispatch,
    IServiceProvider,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::{
    CSIDL_DESKTOP, IShellBrowser, IShellDispatch2, IShellFolderViewDual, IShellWindows, SID_STopLevelBrowser,
    SVGIO_BACKGROUND, SWC_DESKTOP, SWFO_NEEDDISPATCH, ShellWindows,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{BSTR, Interface, Result};

/// `ShellExecute(file, args, dir)` with the default verb, performed by Explorer.
fn via_explorer(file: &str, args: &str, dir: &str) -> Result<()> {
    unsafe {
        let init = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let result = (|| {
            let windows: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER)?;
            let mut hwnd = 0i32;
            let desktop = windows.FindWindowSW(
                &VARIANT::from(CSIDL_DESKTOP as i32),
                &VARIANT::default(),
                SWC_DESKTOP,
                &mut hwnd,
                SWFO_NEEDDISPATCH,
            )?;
            let browser: IShellBrowser = desktop.cast::<IServiceProvider>()?.QueryService(&SID_STopLevelBrowser)?;
            let view: IDispatch = browser.QueryActiveShellView()?.GetItemObject(SVGIO_BACKGROUND)?;
            let shell: IShellDispatch2 = view.cast::<IShellFolderViewDual>()?.Application()?.cast()?;
            shell.ShellExecute(
                &BSTR::from(file),
                &VARIANT::from(args),
                &VARIANT::from(dir),
                &VARIANT::from(""), // default verb
                &VARIANT::from(SW_SHOWNORMAL.0),
            )
        })();
        if init.is_ok() {
            CoUninitialize();
        }
        result
    }
}

/// Launches unelevated on a worker thread (the COM calls go to another
/// process and may take a moment). Falls back to a direct, elevated launch
/// if Explorer is unavailable (e.g. it was killed or replaced).
pub fn spawn(file: String, args: String, dir: String) {
    std::thread::spawn(move || {
        if let Err(e) = via_explorer(&file, &args, &dir) {
            crate::indexer::log(format!("launch via Explorer failed ({e}), launching elevated: {file} {args}"));
            crate::os::shell_execute_elevated(&file, &args, &dir);
        }
    });
}

/// Diagnostic: integrity level of a process launched via Explorer vs directly.
pub fn self_test() {
    let tmp = std::env::temp_dir();
    let cases = [("explorer", tmp.join("swiftfind-il-explorer.txt")), ("direct", tmp.join("swiftfind-il-direct.txt"))];
    for (how, out) in &cases {
        let _ = std::fs::remove_file(out);
        let args = format!("/c whoami /groups > \"{}\"", out.display());
        if *how == "explorer" {
            if let Err(e) = via_explorer("cmd.exe", &args, "") {
                crate::indexer::log(format!("test-launch {how}: {e}"));
            }
        } else {
            crate::os::shell_execute_elevated("cmd.exe", &args, "");
        }
    }
    std::thread::sleep(std::time::Duration::from_secs(4));
    for (how, out) in &cases {
        let text = std::fs::read(out).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
        let level = text
            .lines()
            .find(|l| l.contains("Mandatory Label\\"))
            .map(|l| l.split_whitespace().take(4).collect::<Vec<_>>().join(" "))
            .unwrap_or_else(|| "(no output)".into());
        crate::indexer::log(format!("test-launch {how}: {level}"));
    }
}
