//! Notification-area icon, global hotkey and single-instance activation.
//!
//! A dedicated thread owns a hidden window that receives the hotkey, tray
//! icon callbacks and "show yourself" requests from a second instance. Events
//! are forwarded to the UI thread through a channel.

use crate::hotkey::Hotkey;
use futures::channel::mpsc::UnboundedSender;
use parking_lot::Mutex;
use std::sync::OnceLock;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

/// What the user asked for, via hotkey / tray / second instance.
#[derive(Debug, Clone, Copy)]
pub enum TrayEvent {
    /// Hotkey: hide if in front, otherwise show.
    Toggle,
    Show,
    /// Show the window with the settings panel open.
    Settings,
    Quit,
}

const CLASS: PCWSTR = w!("SwiftFindTray");
/// Tray icon callback.
const WM_TRAY: u32 = WM_APP + 1;
/// Sent by a second instance (possibly unelevated).
const WM_SHOW_REQUEST: u32 = WM_APP + 2;
/// wparam: packed [`Hotkey`] to register in place of the current one.
/// Returns 1 on success.
const WM_SET_HOTKEY: u32 = WM_APP + 3;
const MENU_SHOW: usize = 1;
const MENU_QUIT: usize = 2;
const MENU_SETTINGS: usize = 3;
const HOTKEY_ID: i32 = 1;

static EVENTS: OnceLock<UnboundedSender<TrayEvent>> = OnceLock::new();
static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();
static ICON: OnceLock<isize> = OnceLock::new();
/// Tooltip of the tray icon (mentions the current hotkey).
static TIP: Mutex<String> = Mutex::new(String::new());

#[derive(Clone, Copy)]
pub struct Tray {
    hwnd: isize,
    pub hotkey_ok: bool,
}

impl Tray {
    /// Replaces the global hotkey. Returns false if the combination is taken
    /// or reserved; no hotkey is registered then.
    pub fn set_hotkey(&self, hotkey: Hotkey) -> bool {
        let w = hotkey.pack();
        let r = unsafe { SendMessageW(HWND(self.hwnd as _), WM_SET_HOTKEY, Some(WPARAM(w)), Some(LPARAM(0))) };
        r.0 == 1
    }

    /// Removes the icon (otherwise it lingers until hovered after exit).
    pub fn remove(&self) {
        let nid = notify_data(HWND(self.hwnd as _));
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        }
    }
}

/// Whether another instance is already running.
pub fn is_running() -> bool {
    unsafe { FindWindowW(CLASS, PCWSTR::null()).is_ok_and(|h| !h.is_invalid()) }
}

/// Asks an already running instance to show its window. Returns true if one exists.
pub fn activate_existing() -> bool {
    unsafe {
        match FindWindowW(CLASS, PCWSTR::null()) {
            Ok(hwnd) if !hwnd.is_invalid() => {
                let _ = PostMessageW(Some(hwnd), WM_SHOW_REQUEST, WPARAM(0), LPARAM(0));
                true
            }
            _ => false,
        }
    }
}

/// The app icon (a magnifying glass), drawn at runtime.
pub fn icon() -> HICON {
    HICON(*ICON.get_or_init(|| make_icon().0 as isize) as _)
}

pub fn start(events: UnboundedSender<TrayEvent>, hotkey: Hotkey) -> Tray {
    EVENTS.set(events).ok();
    *TIP.lock() = tip_for(&hotkey);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("tray".into())
        .spawn(move || unsafe {
            let instance = GetModuleHandleW(None).unwrap_or_default();
            let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: instance.into(), lpszClassName: CLASS, ..Default::default() };
            RegisterClassW(&wc);
            TASKBAR_CREATED.set(RegisterWindowMessageW(w!("TaskbarCreated"))).ok();
            // A real (never shown) top-level window, not HWND_MESSAGE: those
            // don't get the TaskbarCreated broadcast after Explorer restarts.
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                CLASS,
                w!("SwiftFind"),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .unwrap_or_default();
            // Let an unelevated second instance reach us through UIPI.
            let _ = ChangeWindowMessageFilterEx(hwnd, WM_SHOW_REQUEST, MSGFLT_ALLOW, None);
            add_icon(hwnd);
            let hotkey_ok = register(hwnd, &hotkey);
            tx.send(Tray { hwnd: hwnd.0 as isize, hotkey_ok }).ok();
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        })
        .expect("spawn tray thread");
    rx.recv().expect("tray thread init")
}

fn tip_for(h: &Hotkey) -> String {
    format!("SwiftFind ({})", h.label())
}

/// Must run on the tray thread (hotkeys belong to the thread of their window).
fn register(hwnd: HWND, h: &Hotkey) -> bool {
    unsafe { RegisterHotKey(Some(hwnd), HOTKEY_ID, h.modifiers() | MOD_NOREPEAT, h.vk).is_ok() }
}

fn notify_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: icon(),
        ..Default::default()
    };
    let tip: Vec<u16> = TIP.lock().encode_utf16().take(nid.szTip.len() - 1).collect();
    nid.szTip[..tip.len()].copy_from_slice(&tip);
    nid
}

fn add_icon(hwnd: HWND) {
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &notify_data(hwnd));
    }
}

fn send(e: TrayEvent) {
    if let Some(tx) = EVENTS.get() {
        tx.unbounded_send(e).ok();
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_HOTKEY if wparam.0 as i32 == HOTKEY_ID => send(TrayEvent::Toggle),
        WM_SHOW_REQUEST => send(TrayEvent::Show),
        WM_SET_HOTKEY => unsafe {
            let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID);
            let h = Hotkey::unpack(wparam.0);
            if !register(hwnd, &h) {
                return LRESULT(0);
            }
            *TIP.lock() = tip_for(&h);
            let _ = Shell_NotifyIconW(NIM_MODIFY, &notify_data(hwnd));
            return LRESULT(1);
        },
        WM_TRAY => match (lparam.0 & 0xFFFF) as u32 {
            WM_LBUTTONUP => send(TrayEvent::Show),
            WM_RBUTTONUP => show_menu(hwnd),
            _ => {}
        },
        _ if Some(&msg) == TASKBAR_CREATED.get() => add_icon(hwnd),
        _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
    LRESULT(0)
}

fn show_menu(hwnd: HWND) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let _ = AppendMenuW(menu, MF_STRING, MENU_SHOW, w!("显示 SwiftFind"));
        let _ = AppendMenuW(menu, MF_STRING, MENU_SETTINGS, w!("设置…"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, MENU_QUIT, w!("退出"));
        let _ = SetMenuDefaultItem(menu, MENU_SHOW as u32, 0);
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // Required so the menu closes when clicking elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, Some(0), hwnd, None);
        let _ = DestroyMenu(menu);
        match cmd.0 as usize {
            MENU_SHOW => send(TrayEvent::Show),
            MENU_SETTINGS => send(TrayEvent::Settings),
            MENU_QUIT => send(TrayEvent::Quit),
            _ => {}
        }
    }
}

/// Draws a 32×32 magnifying glass with anti-aliased edges.
fn make_icon() -> HICON {
    const N: usize = 32;
    let mut px = vec![0u32; N * N];
    let (cx, cy, r) = (13.0f32, 13.0f32, 8.5f32);
    for y in 0..N {
        for x in 0..N {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let d = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();
            // Ring of the lens.
            let ring = (2.0 - (d - r).abs()).clamp(0.0, 1.0);
            // Lens glass, lightly tinted.
            let glass = (r - 1.0 - d).clamp(0.0, 1.0);
            // Handle: distance to the segment from the ring to the corner.
            let (ax, ay, bx, by) = (19.5f32, 19.5f32, 28.0f32, 28.0f32);
            let t = (((fx - ax) * (bx - ax) + (fy - ay) * (by - ay)) / ((bx - ax).powi(2) + (by - ay).powi(2))).clamp(0.0, 1.0);
            let dh = ((fx - (ax + t * (bx - ax))).powi(2) + (fy - (ay + t * (by - ay))).powi(2)).sqrt();
            let handle = (3.0 - dh).clamp(0.0, 1.0);
            let solid = ring.max(handle);
            let (a, rgb) = if solid > 0.0 {
                (solid, 0x1f6fd6u32)
            } else {
                (glass * 0.35, 0x8cc4ff)
            };
            let a = (a * 255.0) as u32;
            // Premultiplied BGRA.
            let ch = |shift: u32| ((rgb >> shift) & 0xFF) * a / 255;
            px[y * N + x] = (a << 24) | (ch(16) << 16) | (ch(8) << 8) | ch(0);
        }
    }
    unsafe {
        let color = CreateBitmap(N as i32, N as i32, 1, 32, Some(px.as_ptr() as *const _));
        let mask_bits = vec![0u8; N * N / 8];
        let mask = CreateBitmap(N as i32, N as i32, 1, 1, Some(mask_bits.as_ptr() as *const _));
        let info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info).unwrap_or_default();
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}
