#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod hotkey;
mod index;
mod indexer;
mod launch;
mod mft;
mod ntfs;
mod os;
mod persist;
mod text_input;
mod tray;

use futures::StreamExt;
use gpui::{App, Bounds, Window, WindowBounds, WindowOptions, prelude::*, px, size};
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

fn main() {
    let diagnostic = std::env::args().any(|a| a == "--no-ui" || a == "--compare-scan" || a == "--test-launch");
    // Already running (possibly hidden in the tray): bring it up instead.
    // Done before elevating, so this needs no UAC prompt.
    if !diagnostic && tray::activate_existing() {
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
        for i in 1..=6 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            indexer::log(format!("no-ui: idle {}ms", i * 500));
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
        let bounds = Bounds::centered(None, size(px(1100.), px(700.)), cx);
        let mut hwnd = 0;
        let main = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some("SwiftFind".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |window, cx| {
                    hwnd = hwnd_of(window);
                    os::window::set_icon(hwnd, tray::icon());
                    // Closing only hides to the tray; quit from the tray menu.
                    window.on_window_should_close(cx, move |_, _| {
                        os::window::hide(hwnd);
                        false
                    });
                    cx.new(|cx| app::SwiftFindApp::new(shared.clone(), tray, hotkey, notice, hwnd, window, cx))
                },
            )
            .expect("open window");

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
                        os::window::hide(hwnd);
                        false
                    }
                    TrayEvent::Toggle | TrayEvent::Show => true,
                    TrayEvent::Settings => {
                        os::window::show(hwnd);
                        main.update(cx, |app, window, cx| app.open_settings(window, cx)).ok();
                        continue;
                    }
                    TrayEvent::Quit => {
                        shared.save_if_dirty();
                        tray.remove();
                        cx.update(|cx| cx.quit());
                        break;
                    }
                };
                if show {
                    os::window::show(hwnd);
                    main.update(cx, |app, window, cx| app.focus_search(window, cx)).ok();
                }
            }
        })
        .detach();
        cx.activate(true);
    });
}
