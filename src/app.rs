//! Main window: search box, sortable virtualized result list, context menu
//! and status bar.

use crate::index::{Query, SortKey, Stat};
use crate::indexer::Shared;
use crate::os;
use crate::text_input::{self, Changed, TextInput};
use gpui::{
    App, ClickEvent, ClipboardItem, Context, Entity, FocusHandle, Focusable, KeyBinding,
    MouseButton, MouseDownEvent, Pixels, Point, ScrollStrategy, SharedString,
    UniformListScrollHandle, Window, actions, anchored, deferred, div, prelude::*, px, rgb,
    uniform_list,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

actions!(
    swiftfind,
    [SelectNext, SelectPrev, SelectFirst, SelectLast, OpenSelected, RevealSelected, CopyPath, Dismiss]
);

pub fn bind_keys(cx: &mut App) {
    text_input::bind_keys(cx);
    cx.bind_keys([
        KeyBinding::new("down", SelectNext, Some("SwiftFind")),
        KeyBinding::new("up", SelectPrev, Some("SwiftFind")),
        KeyBinding::new("ctrl-home", SelectFirst, Some("SwiftFind")),
        KeyBinding::new("ctrl-end", SelectLast, Some("SwiftFind")),
        KeyBinding::new("enter", OpenSelected, Some("SwiftFind")),
        KeyBinding::new("ctrl-enter", RevealSelected, Some("SwiftFind")),
        KeyBinding::new("ctrl-shift-c", CopyPath, Some("SwiftFind")),
        KeyBinding::new("escape", Dismiss, Some("SwiftFind")),
    ]);
}

const ROW_H: f32 = 24.;
const COL_SIZE: f32 = 100.;
const COL_DATE: f32 = 150.;

#[derive(Clone, Default)]
struct Row {
    name: SharedString,
    path: SharedString,
    full: Arc<str>,
    size: SharedString,
    modified: SharedString,
}

/// Open context menu for one result.
#[derive(Clone)]
struct Menu {
    position: Point<Pixels>,
    row: Row,
}

pub struct SwiftFindApp {
    input: Entity<TextInput>,
    shared: Arc<Shared>,
    results: Arc<Vec<u32>>,
    selected: Option<usize>,
    scroll: UniformListScrollHandle,
    /// Latest issued search; older in-flight searches cancel themselves.
    search_seq: Arc<AtomicU64>,
    searching: bool,
    search_time: Duration,
    /// Index generation the current results were computed against.
    result_gen: u64,
    /// Index epoch of `results`; entry indices from another epoch are invalid.
    result_epoch: u64,
    rows: HashMap<u32, Row>,
    rows_gen: u64,
    sort: SortKey,
    sort_desc: bool,
    menu: Option<Menu>,
    /// Shown in the status bar (e.g. hotkey registration failure).
    notice: Option<String>,
    focus_handle: FocusHandle,
}

impl SwiftFindApp {
    pub fn new(shared: Arc<Shared>, notice: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| TextInput::new("搜索文件名…（空格分隔多个关键词，支持 * ? 通配符）", cx));
        cx.subscribe(&input, |this, _, _: &Changed, cx| {
            this.selected = None;
            this.search(cx);
        })
        .detach();
        window.focus(&input.focus_handle(cx), cx);

        // Poll for index changes (initial indexing / live USN updates).
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(300)).await;
                let alive = this
                    .update(cx, |this, cx| {
                        let current = this.shared.index.read().generation.load(Ordering::Relaxed);
                        if current != this.result_gen && !this.searching {
                            this.search(cx);
                        }
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();

        let mut this = Self {
            input,
            shared,
            results: Arc::new(Vec::new()),
            selected: None,
            scroll: UniformListScrollHandle::new(),
            search_seq: Arc::new(AtomicU64::new(0)),
            searching: false,
            search_time: Duration::ZERO,
            result_gen: u64::MAX,
            result_epoch: u64::MAX,
            rows: HashMap::new(),
            rows_gen: u64::MAX,
            sort: SortKey::Name,
            sort_desc: false,
            menu: None,
            notice,
            focus_handle: cx.focus_handle(),
        };
        this.search(cx);
        this
    }

    fn search(&mut self, cx: &mut Context<Self>) {
        let seq = self.search_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let latest = self.search_seq.clone();
        let shared = self.shared.clone();
        let query = self.input.read(cx).text().to_string();
        let (sort, desc) = (self.sort, self.sort_desc);
        self.searching = true;
        let task = cx.background_executor().spawn(async move {
            let t = Instant::now();
            let index = shared.index.read();
            let generation = index.generation.load(Ordering::Relaxed);
            let epoch = index.epoch;
            let mut res = index.search(&Query::parse(&query), &|| latest.load(Ordering::Relaxed) != seq)?;
            index.sort_results(&mut res, sort, desc);
            Some((res, generation, epoch, t.elapsed()))
        });
        cx.spawn(async move |this, cx| {
            let res = task.await;
            this.update(cx, |this, cx| {
                if this.search_seq.load(Ordering::SeqCst) != seq {
                    return;
                }
                this.searching = false;
                if let Some((results, generation, epoch, elapsed)) = res {
                    this.results = Arc::new(results);
                    this.result_gen = generation;
                    this.result_epoch = epoch;
                    this.search_time = elapsed;
                    if let Some(s) = this.selected {
                        if s >= this.results.len() {
                            this.selected = None;
                        }
                    }
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Focuses the search box with its text selected (after the window is shown).
    pub fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        window.focus(&self.input.focus_handle(cx), cx);
        self.input.update(cx, |input, cx| input.select_all_text(window, cx));
    }

    /// Header click: same column flips direction; a new column starts
    /// ascending (name/path) or descending (size/date, biggest/newest first).
    fn set_sort(&mut self, key: SortKey, cx: &mut Context<Self>) {
        if self.sort == key {
            self.sort_desc = !self.sort_desc;
        } else {
            self.sort = key;
            self.sort_desc = matches!(key, SortKey::Size | SortKey::Modified);
        }
        self.selected = None;
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        self.search(cx);
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.results.is_empty() {
            return;
        }
        let ix = ix.min(self.results.len() - 1);
        self.selected = Some(ix);
        self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        let ix = self.selected.map_or(0, |s| s + 1);
        self.select(ix, cx);
    }

    fn select_prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        let ix = self.selected.map_or(0, |s| s.saturating_sub(1));
        self.select(ix, cx);
    }

    fn select_first(&mut self, _: &SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        self.select(0, cx);
    }

    fn select_last(&mut self, _: &SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        self.select(usize::MAX, cx);
    }

    fn selected_path(&self) -> Option<String> {
        let entry = *self.results.get(self.selected?)?;
        let index = self.shared.index.read();
        (index.epoch == self.result_epoch && index.is_alive(entry)).then(|| index.full_path(entry))
    }

    fn open_selected(&mut self, _: &OpenSelected, _: &mut Window, _: &mut Context<Self>) {
        if let Some(p) = self.selected_path() {
            os::open(&p);
        }
    }

    fn reveal_selected(&mut self, _: &RevealSelected, _: &mut Window, _: &mut Context<Self>) {
        if let Some(p) = self.selected_path() {
            os::reveal(&p);
        }
    }

    fn copy_path(&mut self, _: &CopyPath, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(p) = self.selected_path() {
            cx.write_to_clipboard(ClipboardItem::new_string(p));
        }
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            cx.notify();
        }
    }

    /// Builds (and caches) display data for the given result rows.
    fn rows_for(&mut self, range: std::ops::Range<usize>) -> Vec<Option<Row>> {
        let wanted: Vec<u32> = self.results[range.clone()].to_vec();
        if let Some(index) = self.shared.index.try_read() {
            if index.epoch != self.result_epoch {
                // Results refer to a previous index layout; a fresh search is
                // already on its way (generation was bumped). Show nothing.
                self.rows.clear();
                return vec![None; wanted.len()];
            }
            let generation = index.generation.load(Ordering::Relaxed);
            if generation != self.rows_gen {
                self.rows.clear();
                self.rows_gen = generation;
            }
            for &e in &wanted {
                if self.rows.contains_key(&e) || !index.is_alive(e) {
                    continue;
                }
                let full = index.full_path(e);
                let is_dir = index.is_dir(e);
                let mut stat = index.stat(e);
                // Not known when the volume was scanned via the slow fallback.
                if stat.mtime == 0 || (!is_dir && stat.size == u64::MAX) {
                    stat = Stat::from_fs(&full).unwrap_or(stat);
                }
                let size = if is_dir || stat.size == u64::MAX { String::new() } else { format_size(stat.size) };
                let modified = format_time(stat.mtime);
                self.rows.insert(
                    e,
                    Row {
                        name: index.name(e).to_string().into(),
                        path: index.parent_path(e).into(),
                        full: full.into(),
                        size: size.into(),
                        modified: modified.into(),
                    },
                );
            }
            if self.rows.len() > 4096 {
                self.rows.clear();
            }
        }
        wanted.iter().map(|e| self.rows.get(e).cloned()).collect()
    }
}

fn format_size(bytes: u64) -> String {
    let kb = bytes.div_ceil(1024);
    let s = kb.to_string();
    let mut out = String::with_capacity(s.len() + 4);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out + " KB"
}

fn format_time(unix: u32) -> String {
    use chrono::TimeZone;
    if unix == 0 {
        return String::new();
    }
    match chrono::Local.timestamp_opt(unix as i64, 0) {
        chrono::LocalResult::Single(dt) | chrono::LocalResult::Ambiguous(dt, _) => dt.format("%Y-%m-%d %H:%M").to_string(),
        chrono::LocalResult::None => String::new(),
    }
}

fn menu_item(
    id: &'static str,
    label: &'static str,
    shortcut: &'static str,
    cx: &mut Context<SwiftFindApp>,
    action: impl Fn(&Row, &mut Context<SwiftFindApp>) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .flex_row()
        .justify_between()
        .gap_6()
        .px_3()
        .py(px(5.))
        .rounded_sm()
        .hover(|s| s.bg(rgb(0xe5f3ff)))
        .child(label)
        .child(div().text_color(rgb(0x888888)).child(shortcut))
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            if let Some(menu) = this.menu.take() {
                action(&menu.row, cx);
            }
            cx.notify();
        }))
}

fn menu_separator() -> impl IntoElement {
    div().my_1().h(px(1.)).bg(rgb(0xe5e5e5))
}

fn cell(text: SharedString) -> gpui::Div {
    div().px_2().overflow_hidden().whitespace_nowrap().text_ellipsis().child(text)
}

impl SwiftFindApp {
    fn header_cell(&self, text: &'static str, key: SortKey, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let arrow = match (self.sort == key, self.sort_desc) {
            (false, _) => "",
            (true, false) => " ▲",
            (true, true) => " ▼",
        };
        div()
            .id(text)
            .flex()
            .flex_row()
            .items_center()
            .h_full()
            .px_2()
            .overflow_hidden()
            .whitespace_nowrap()
            .border_r_1()
            .border_color(rgb(0xe2e2e2))
            .hover(|s| s.bg(rgb(0xe5f3ff)))
            .child(format!("{text}{arrow}"))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.set_sort(key, cx)))
    }

    fn render_menu(&self, menu: Menu, cx: &mut Context<Self>) -> impl IntoElement {
        deferred(
            anchored().position(menu.position).snap_to_window().child(
                div()
                    .id("context-menu")
                    .occlude()
                    .min_w(px(220.))
                    .p_1()
                    .bg(rgb(0xffffff))
                    .border_1()
                    .border_color(rgb(0xcccccc))
                    .rounded_md()
                    .shadow_md()
                    .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                        this.menu = None;
                        cx.notify();
                    }))
                    .child(menu_item("m-open", "打开", "Enter", cx, |r, _| os::open(&r.full)))
                    .child(menu_item("m-reveal", "打开所在文件夹", "Ctrl+Enter", cx, |r, _| os::reveal(&r.full)))
                    .child(menu_separator())
                    .child(menu_item("m-copy-full", "复制完整路径", "Ctrl+Shift+C", cx, |r, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(r.full.to_string()))
                    }))
                    .child(menu_item("m-copy-name", "复制名称", "", cx, |r, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(r.name.to_string()))
                    }))
                    .child(menu_item("m-copy-dir", "复制所在文件夹路径", "", cx, |r, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(r.path.to_string()))
                    }))
                    .child(menu_separator())
                    .child(menu_item("m-props", "属性", "", cx, |r, _| os::properties(&r.full))),
            ),
        )
        .with_priority(1)
    }
}

impl Focusable for SwiftFindApp {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SwiftFindApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = self.shared.status.lock().clone();
        let count = self.results.len();
        let selected = self.selected;

        let list = uniform_list(
            "results",
            count,
            cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                let start = range.start;
                this.rows_for(range)
                    .into_iter()
                    .enumerate()
                    .map(|(i, row)| {
                        let ix = start + i;
                        let row = row.unwrap_or_default();
                        let full_open = row.full.clone();
                        let menu_row = row.clone();
                        let is_selected = selected == Some(ix);
                        div()
                            .id(ix)
                            .flex()
                            .flex_row()
                            .items_center()
                            .h(px(ROW_H))
                            .w_full()
                            .when(is_selected, |d| d.bg(rgb(0xcce8ff)))
                            .when(!is_selected, |d| d.hover(|s| s.bg(rgb(0xe5f3ff))))
                            .on_click(cx.listener(move |this, ev: &ClickEvent, _, cx| {
                                this.selected = Some(ix);
                                cx.notify();
                                if ev.click_count() >= 2 {
                                    os::open(&full_open);
                                }
                            }))
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                                    this.selected = Some(ix);
                                    if !menu_row.full.is_empty() {
                                        this.menu = Some(Menu { position: ev.position, row: menu_row.clone() });
                                    }
                                    cx.notify();
                                }),
                            )
                            .child(cell(row.name).flex_basis(px(0.)).flex_grow(3.).min_w_0())
                            .child(cell(row.path).flex_basis(px(0.)).flex_grow(5.).min_w_0())
                            .child(cell(row.size).w(px(COL_SIZE)).flex_none().text_right())
                            .child(cell(row.modified).w(px(COL_DATE)).flex_none())
                    })
                    .collect()
            }),
        )
        .track_scroll(&self.scroll)
        .flex_grow(1.)
        .w_full();

        div()
            .key_context("SwiftFind")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::open_selected))
            .on_action(cx.listener(Self::reveal_selected))
            .on_action(cx.listener(Self::copy_path))
            .on_action(cx.listener(Self::dismiss))
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0xffffff))
            .text_color(rgb(0x1e1e1e))
            .text_size(px(13.))
            .child(div().p_2().bg(rgb(0xf3f3f3)).child(self.input.clone()))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .h(px(ROW_H))
                    .items_center()
                    .bg(rgb(0xf3f3f3))
                    .border_b_1()
                    .border_color(rgb(0xdddddd))
                    .text_color(rgb(0x555555))
                    .child(self.header_cell("名称", SortKey::Name, cx).flex_basis(px(0.)).flex_grow(3.).min_w_0())
                    .child(self.header_cell("路径", SortKey::Path, cx).flex_basis(px(0.)).flex_grow(5.).min_w_0())
                    .child(self.header_cell("大小", SortKey::Size, cx).w(px(COL_SIZE)).flex_none().justify_end())
                    .child(self.header_cell("修改时间", SortKey::Modified, cx).w(px(COL_DATE)).flex_none()),
            )
            .child(list)
            .when_some(self.menu.clone(), |d, menu| d.child(self.render_menu(menu, cx)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .px_2()
                    .h(px(ROW_H))
                    .items_center()
                    .bg(rgb(0xf3f3f3))
                    .border_t_1()
                    .border_color(rgb(0xdddddd))
                    .text_color(rgb(0x555555))
                    .child(format!("{count} 个对象"))
                    .child(format!(
                        "{}{status}  ·  搜索 {:.1} ms",
                        self.notice.as_ref().map(|n| format!("{n}  ·  ")).unwrap_or_default(),
                        self.search_time.as_secs_f64() * 1000.
                    )),
            )
    }
}
