#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod index;
mod indexer;
mod ntfs;
mod os;
mod persist;
mod text_input;

use gpui::{App, Bounds, WindowBounds, WindowOptions, prelude::*, px, size};
use std::sync::Arc;

fn main() {
    // Reading the MFT / USN journal requires administrator rights.
    if !os::is_elevated() {
        if !os::relaunch_elevated() {
            os::error_box("需要管理员权限才能读取 NTFS 主文件表 (MFT)。");
        }
        return;
    }

    let shared = Arc::new(indexer::Shared::default());

    // Diagnostic mode: build the index without any UI, log memory, and exit.
    if std::env::args().any(|a| a == "--no-ui") {
        let (tx, rx) = std::sync::mpsc::channel();
        indexer::start(shared.clone(), Some(Box::new(move || tx.send(()).unwrap())));
        rx.recv().ok();
        if std::env::args().any(|a| a == "--compact") {
            indexer::compact(&shared);
        }
        for i in 1..=6 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            indexer::log(format!("no-ui: idle {}ms", i * 500));
        }
        indexer::log("no-ui: done".into());
        return;
    }

    indexer::start(shared.clone(), None);

    gpui_platform::application().run(move |cx: &mut App| {
        app::bind_keys(cx);
        let bounds = Bounds::centered(None, size(px(1100.), px(700.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("SwiftFind".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| app::SwiftFindApp::new(shared.clone(), window, cx)),
        )
        .expect("open window");
        let on_close = shared.clone();
        cx.on_window_closed(move |cx, _| {
            if cx.windows().is_empty() {
                on_close.save_if_dirty();
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}
