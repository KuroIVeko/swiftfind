#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod autostart;
mod hotkey;
mod index;
mod indexer;
mod launch;
mod mft;
mod ntfs;
mod os;
mod persist;
mod settings;
mod text_input;
mod theme;
mod tray;

use futures::StreamExt;
use gpui::{App, Bounds, DisplayId, Window, WindowBounds, WindowOptions, point, prelude::*, px, size};
use std::sync::Arc;
use tray::TrayEvent;

/// Win32 handle of a gpui window (0 if unavailable).
fn hwnd_of(window: &Window) -> isize {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match HasWindowHandle::window_handle(window).map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
        _ => 0,
    }
}

/// Hides the window to the tray, and takes the chance to write the cache in
/// the background (rate-limited).
fn hide(hwnd: isize, shared: &Arc<indexer::Shared>) {
    os::window::hide(hwnd);
    let shared = shared.clone();
    std::thread::spawn(move || shared.save_if_older_than(indexer::HIDE_SAVE_INTERVAL));
}

/// Saves the window's placement (`window=` in settings.ini): state, restore
/// bounds in gpui's logical pixels, and the display they belong to.
fn remember_window(window: &Window, cx: &App) {
    let (state, b) = match window.window_bounds() {
        WindowBounds::Maximized(b) => ("max", b),
        WindowBounds::Windowed(b) | WindowBounds::Fullscreen(b) => ("normal", b),
    };
    let Some(uuid) = window.display(cx).and_then(|d| d.uuid().ok()) else { return };
    let v = format!(
        "{state},{:.0},{:.0},{:.0},{:.0},{uuid}",
        b.origin.x.as_f32(),
        b.origin.y.as_f32(),
        b.size.width.as_f32(),
        b.size.height.as_f32()
    );
    if let Err(e) = settings::set("window", &v) {
        indexer::log(format!("save window placement: {e}"));
    }
}

/// The saved placement, if its display is still connected.
fn saved_window(cx: &App) -> Option<(WindowBounds, DisplayId)> {
    let v = settings::get("window")?;
    let parts: Vec<&str> = v.split(',').collect();
    let [state, x, y, w, h, uuid] = parts[..] else { return None };
    let n = |s: &str| s.trim().parse::<f32>().ok().filter(|v| v.is_finite());
    let (w, h) = (n(w)?, n(h)?);
    if w < 400. || h < 300. {
        return None;
    }
    let bounds = Bounds { origin: point(px(n(x)?), px(n(y)?)), size: size(px(w), px(h)) };
    let display = cx.displays().into_iter().find(|d| d.uuid().is_ok_and(|u| u.to_string() == uuid))?;
    let bounds = if state == "max" { WindowBounds::Maximized(bounds) } else { WindowBounds::Windowed(bounds) };
    Some((bounds, display.id()))
}

fn main() {
    let diagnostic = std::env::args().any(|a| a == "--no-ui" || a == "--compare-scan" || a == "--test-launch");
    // Already running (possibly hidden in the tray): bring it up instead.
    // Done before elevating, so this needs no UAC prompt.
    let background = std::env::args().any(|a| a == autostart::BACKGROUND_FLAG);
    if !diagnostic && (if background { tray::is_running() } else { tray::activate_existing() }) {
        return;
    }

    // Reading the MFT / USN journal requires administrator rights.
    if !os::is_elevated() {
        if !os::relaunch_elevated() {
            os::error_box("需要管理员权限才能读取 NTFS 主文件表 (MFT)。");
        }
        return;
    }

    let shared = Arc::new(indexer::Shared::default());

    if std::env::args().any(|a| a == "--test-launch") {
        launch::self_test();
        return;
    }

    if std::env::args().any(|a| a == "--compare-scan") {
        indexer::compare_scans();
        return;
    }

    // Diagnostic mode: build the index without any UI, log memory, and exit.
    if std::env::args().any(|a| a == "--no-ui") {
        let (tx, rx) = std::sync::mpsc::channel();
        indexer::start(shared.clone(), Some(Box::new(move || tx.send(()).unwrap())));
        rx.recv().ok();
        if std::env::args().any(|a| a == "--compact") {
            indexer::compact(&shared);
        }
        if std::env::args().any(|a| a == "--bench-search") {
            let queries = [
                "", "a", "e", "win", "notepad", ".dll", "readme", "config.json", "zzqxj", "文档", "测试",
                "*.txt", "windows\\ note", "windows\\",
            ];
            for line in shared.index.read().bench_search(&queries, 7) {
                indexer::log(line);
            }
            for line in shared.index.read().bench_sort() {
                indexer::log(line);
            }
            for line in shared.index.read().verify_stats(3000) {
                indexer::log(line);
            }
        }
        let args: Vec<String> = std::env::args().collect();
        let idle_secs: u64 = args
            .iter()
            .position(|a| a == "--idle-secs")
            .and_then(|i| args.get(i + 1)?.parse().ok())
            .unwrap_or(3);
        for i in 1..=idle_secs {
            std::thread::sleep(std::time::Duration::from_secs(1));
            if i % 10 == 0 || i == idle_secs {
                indexer::log(format!("no-ui: idle {i}s"));
            }
        }
        indexer::log("no-ui: done".into());
        return;
    }

    indexer::start(shared.clone(), None);

    let (events_tx, mut events) = futures::channel::mpsc::unbounded();
    let hotkey = hotkey::load();
    let tray = tray::start(events_tx, hotkey);
    let notice = (!tray.hotkey_ok).then(|| format!("热键 {} 已被其他程序占用，可在设置中更换", hotkey.label()));
    if let Some(n) = &notice {
        indexer::log(n.clone());
    }

    gpui_platform::application().run(move |cx: &mut App| {
        app::bind_keys(cx);
        let (bounds, display_id) = match saved_window(cx) {
            Some((b, d)) => (b, Some(d)),
            None => (WindowBounds::Windowed(Bounds::centered(None, size(px(1100.), px(700.)), cx)), None),
        };
        let mut hwnd = 0;
        let main = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(bounds),
                    display_id,
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some("SwiftFind".into()),
                        ..Default::default()
                    }),
                    // Started at logon: stay in the tray until asked for.
                    show: !background,
                    focus: !background,
                    ..Default::default()
                },
                |window, cx| {
                    theme::follow_system(window, cx);
                    hwnd = hwnd_of(window);
                    os::window::set_icon(hwnd, tray::icon_large(), tray::icon());
                    // Closing only hides to the tray; quit from the tray menu.
                    let on_close = shared.clone();
                    window.on_window_should_close(cx, move |window, cx| {
                        remember_window(window, cx);
                        hide(hwnd, &on_close);
                        false
                    });
                    cx.new(|cx| app::SwiftFindApp::new(shared.clone(), tray, hotkey, notice, hwnd, window, cx))
                },
            )
            .expect("open window");

        // A window opened hidden gets its size and position applied by gpui
        // on its first activation, so that one goes through gpui.
        let mut never_shown = background;
        cx.spawn(async move |cx| {
            while let Some(event) = events.next().await {
                let show = match event {
                    TrayEvent::Toggle
                        if main
                            .update(cx, |app, _, cx| app.hotkey_pressed_while_recording(cx))
                            .unwrap_or(false) =>
                    {
                        continue;
                    }
                    TrayEvent::Toggle if os::window::is_front(hwnd) => {
                        main.update(cx, |_, window, cx| remember_window(window, cx)).ok();
                        hide(hwnd, &shared);
                        false
                    }
                    TrayEvent::Toggle | TrayEvent::Show => true,
                    TrayEvent::Settings => {
                        show_window(hwnd, &mut never_shown, &main, cx);
                        main.update(cx, |app, window, cx| app.open_settings(window, cx)).ok();
                        continue;
                    }
                    TrayEvent::Quit => {
                        // Not while hidden: the placement was saved on hiding.
                        if os::window::is_visible(hwnd) {
                            main.update(cx, |_, window, cx| remember_window(window, cx)).ok();
                        }
                        shared.save_if_dirty();
                        tray.remove();
                        cx.update(|cx| cx.quit());
                        break;
                    }
                };
                if show {
                    show_window(hwnd, &mut never_shown, &main, cx);
                    main.update(cx, |app, window, cx| app.focus_search(window, cx)).ok();
                }
            }
        })
        .detach();
        if !background {
            cx.activate(true);
        }
    });
}

fn show_window(
    hwnd: isize,
    never_shown: &mut bool,
    main: &gpui::WindowHandle<app::SwiftFindApp>,
    cx: &mut gpui::AsyncApp,
) {
    if std::mem::take(never_shown) {
        main.update(cx, |_, window, _| window.activate_window()).ok();
    } else {
        os::window::show(hwnd);
    }
}
