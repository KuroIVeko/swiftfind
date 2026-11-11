//! Main window: search box, sortable virtualized result list, context menu
//! and status bar.

use crate::hotkey::{self, Hotkey};
use crate::index::{Index, Query, SortKey, Stat};
use crate::tray::Tray;
use crate::indexer::Shared;
use crate::os;
use crate::text_input::{self, Changed, TextInput};
use crate::theme::{self, Theme};
use gpui::{
    App, ClickEvent, ClipboardItem, Context, Entity, FocusHandle, Focusable, KeyBinding,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollStrategy, SharedString,
    UniformListScrollHandle, Window, actions, anchored, deferred, div, prelude::*, px,
    uniform_list,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

actions!(
    swiftfind,
    [
        SelectNext,
        SelectPrev,
        SelectFirst,
        SelectLast,
        OpenSelected,
        RevealSelected,
        CopyPath,
        Dismiss,
        OpenSettings
    ]
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
        KeyBinding::new("ctrl-,", OpenSettings, Some("SwiftFind")),
    ]);
}

const ROW_H: f32 = 24.;

/// Resizable columns; the path column takes the remaining width.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Col {
    Name,
    Size,
    Date,
}

#[derive(Clone, Copy)]
struct Columns {
    name: f32,
    size: f32,
    date: f32,
}

impl Columns {
    const DEFAULT: Columns = Columns { name: 320., size: 100., date: 150. };
    const MIN: f32 = 50.;
    /// Width always left for the path column while dragging.
    const MIN_PATH: f32 = 80.;

    /// From `columns=name,size,date` in settings.ini.
    fn load() -> Columns {
        let parsed = crate::settings::get("columns").and_then(|v| {
            let w: Vec<f32> = v.split(',').filter_map(|p| p.trim().parse().ok()).collect();
            let ok = w.len() == 3 && w.iter().all(|x| x.is_finite() && (Self::MIN..=4000.).contains(x));
            ok.then(|| Columns { name: w[0], size: w[1], date: w[2] })
        });
        parsed.unwrap_or(Self::DEFAULT)
    }

    fn save(&self) {
        let v = format!("{:.0},{:.0},{:.0}", self.name, self.size, self.date);
        if let Err(e) = crate::settings::set("columns", &v) {
            crate::indexer::log(format!("save column widths: {e}"));
        }
    }

    fn get(&self, c: Col) -> f32 {
        match c {
            Col::Name => self.name,
            Col::Size => self.size,
            Col::Date => self.date,
        }
    }

    fn set(&mut self, c: Col, w: f32) {
        match c {
            Col::Name => self.name = w,
            Col::Size => self.size = w,
            Col::Date => self.date = w,
        }
    }
}

/// A column border being dragged.
#[derive(Clone, Copy)]
struct ColDrag {
    col: Col,
    start_x: Pixels,
    start_w: f32,
}

#[derive(Clone, Default)]
struct Row {
    name: SharedString,
    path: SharedString,
    full: Arc<str>,
    size: SharedString,
    modified: SharedString,
}

/// Current result list. An empty query sorted by name is every live entry
/// in `Index::order`, which is read in place instead of copying 11 MB.
enum Results {
    All { len: usize, rev: bool },
    List(Vec<u32>),
}

impl Results {
    fn len(&self) -> usize {
        match self {
            Results::All { len, .. } => *len,
            Results::List(v) => v.len(),
        }
    }

    fn entry(&self, pos: usize, index: &Index) -> Option<u32> {
        match self {
            Results::All { len, rev } if pos < *len => index.order_at(if *rev { len - 1 - pos } else { pos }),
            Results::All { .. } => None,
            Results::List(v) => v.get(pos).copied(),
        }
    }
}

/// State of the settings panel while it is open.
struct Settings {
    focus: FocusHandle,
    /// Waiting for the user to press a new hotkey.
    recording: bool,
    /// (text, is_error)
    message: Option<(String, bool)>,
    /// Start-at-logon state; None while it is being read or changed.
    autostart: Option<bool>,
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
    results: Arc<Results>,
    /// Position -> entry for `Results::All`, so rows can be drawn while the
    /// indexer holds the write lock.
    positions: HashMap<usize, u32>,
    selected: Option<usize>,
    scroll: UniformListScrollHandle,
    /// Latest issued search; older in-flight searches cancel themselves.
    search_seq: Arc<AtomicU64>,
    searching: bool,
    search_time: Duration,
    /// Index generation the current results were computed against.
    result_gen: u64,
    /// Index tree generation (names/parents) of the current results.
    result_tree_gen: u64,
    /// Status text last drawn, to repaint only when it changes.
    last_status: String,
    /// Main window, to skip work while it is hidden in the tray.
    hwnd: isize,
    /// Index epoch of `results`; entry indices from another epoch are invalid.
    result_epoch: u64,
    rows: HashMap<u32, Row>,
    rows_gen: u64,
    sort: SortKey,
    sort_desc: bool,
    /// The current query has size/date filters (results change with stats).
    query_uses_stats: bool,
    /// Unrecognized filters in the current query.
    query_error: Option<String>,
    cols: Columns,
    col_drag: Option<ColDrag>,
    menu: Option<Menu>,
    settings: Option<Settings>,
    tray: Tray,
    hotkey: Hotkey,
    /// Shown in the status bar (e.g. hotkey registration failure).
    notice: Option<String>,
    focus_handle: FocusHandle,
}

impl SwiftFindApp {
    pub fn new(
        shared: Arc<Shared>,
        tray: Tray,
        hotkey: Hotkey,
        notice: Option<String>,
        hwnd: isize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| TextInput::new("搜索文件名…（空格分隔多个关键词；支持 * ?、ext:、size:、dm:、file:、folder:）", cx));
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
                let alive = this.update(cx, |this, cx| this.poll(cx)).is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();

        let mut this = Self {
            input,
            shared,
            results: Arc::new(Results::List(Vec::new())),
            positions: HashMap::new(),
            selected: None,
            scroll: UniformListScrollHandle::new(),
            search_seq: Arc::new(AtomicU64::new(0)),
            searching: false,
            search_time: Duration::ZERO,
            result_gen: u64::MAX,
            result_tree_gen: u64::MAX,
            last_status: String::new(),
            hwnd,
            result_epoch: u64::MAX,
            rows: HashMap::new(),
            rows_gen: u64::MAX,
            sort: SortKey::Name,
            sort_desc: false,
            query_uses_stats: false,
            query_error: None,
            cols: Columns::load(),
            col_drag: None,
            menu: None,
            settings: None,
            tray,
            hotkey,
            notice,
            focus_handle: cx.focus_handle(),
        };
        this.search(cx);
        this
    }

    /// Periodic check for index changes. Re-searches only when the result set
    /// or its order can have changed; size/mtime-only changes just repaint.
    /// Does nothing while the window is hidden (catches up once shown).
    fn poll(&mut self, cx: &mut Context<Self>) {
        if !os::window::is_visible(self.hwnd) {
            return;
        }
        let Some((generation, tree_gen, epoch)) = self
            .shared
            .index
            .try_read()
            .map(|i| (i.generation.load(Ordering::Relaxed), i.tree_generation, i.epoch))
        else {
            return; // indexer is writing; try next tick
        };
        let stat_sorted = matches!(self.sort, SortKey::Size | SortKey::Modified) || self.query_uses_stats;
        let stale = epoch != self.result_epoch
            || tree_gen != self.result_tree_gen
            || (stat_sorted && generation != self.result_gen);
        if stale && !self.searching {
            self.search(cx);
        } else if generation != self.rows_gen {
            cx.notify(); // rows_for drops cached sizes/times
        }
        let status = self.shared.status.lock().clone();
        if status != self.last_status {
            self.last_status = status;
            cx.notify();
        }
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
            let stamp = (generation, index.tree_generation, index.epoch);
            let q = Query::parse(&query);
            let info = (q.uses_stats(), (!q.errors.is_empty()).then(|| q.errors.join("；")));
            let res = if q.is_empty() && sort == SortKey::Name {
                Results::All { len: index.live_count(), rev: desc }
            } else {
                let mut res = index.search(&q, &|| latest.load(Ordering::Relaxed) != seq)?;
                index.sort_results(&mut res, sort, desc);
                Results::List(res)
            };
            Some((res, stamp, t.elapsed(), info))
        });
        cx.spawn(async move |this, cx| {
            let res = task.await;
            this.update(cx, |this, cx| {
                if this.search_seq.load(Ordering::SeqCst) != seq {
                    return;
                }
                this.searching = false;
                if let Some((results, (generation, tree_gen, epoch), elapsed, (uses_stats, error))) = res {
                    this.query_uses_stats = uses_stats;
                    this.query_error = error;
                    this.results = Arc::new(results);
                    this.positions.clear();
                    this.result_gen = generation;
                    this.result_tree_gen = tree_gen;
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
        if self.settings.is_some() {
            return; // keep the open settings panel focused
        }
        self.menu = None;
        window.focus(&self.input.focus_handle(cx), cx);
        self.input.update(cx, |input, cx| input.select_all_text(window, cx));
    }

    pub fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        self.settings = Some(Settings { focus, recording: false, message: None, autostart: None });
        self.run_autostart(None, cx);
        cx.notify();
    }

    /// Reads (`None`) or changes the start-at-logon task off the UI thread;
    /// `schtasks` takes a few hundred milliseconds.
    fn run_autostart(&mut self, enable: Option<bool>, cx: &mut Context<Self>) {
        if let Some(st) = self.settings.as_mut() {
            st.autostart = None;
        }
        let task = cx.background_spawn(async move {
            let result = enable.map(crate::autostart::set);
            (result, crate::autostart::is_enabled())
        });
        cx.spawn(async move |this, cx| {
            let (result, enabled) = task.await;
            this.update(cx, |this, cx| {
                let Some(st) = this.settings.as_mut() else { return };
                st.autostart = Some(enabled);
                match result {
                    Some(Err(e)) => st.message = Some((format!("开机自启设置失败：{e}"), true)),
                    Some(Ok(())) => {
                        let text = if enabled { "已开启开机自动启动（启动后隐藏在托盘）" } else { "已关闭开机自动启动" };
                        st.message = Some((text.into(), false));
                    }
                    None => {}
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn open_settings_action(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
        self.open_settings(window, cx);
    }

    fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings = None;
        window.focus(&self.input.focus_handle(cx), cx);
        cx.notify();
    }

    /// The current global hotkey can't reach the window (the system takes
    /// it), so the tray reports it instead. Returns true if it was consumed.
    pub fn hotkey_pressed_while_recording(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(st) = self.settings.as_mut().filter(|s| s.recording) else { return false };
        st.recording = false;
        st.message = Some((format!("仍为 {}", self.hotkey.label()), false));
        cx.notify();
        true
    }

    fn settings_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(st) = self.settings.as_mut() else { return };
        let k = &ev.keystroke;
        cx.stop_propagation();
        if !st.recording {
            if k.key == "escape" {
                self.close_settings(window, cx);
            }
            return;
        }
        match k.key.as_str() {
            "shift" | "control" | "alt" | "platform" | "function" => return,
            "escape" => {
                st.recording = false;
                st.message = None;
                cx.notify();
                return;
            }
            _ => {}
        }
        let m = &k.modifiers;
        let candidate = Hotkey::from_keystroke(m.control, m.alt, m.shift, m.platform, &k.key);
        match candidate.map(|h| h.validate().map(|_| h)) {
            None => st.message = Some(("不支持这个按键，请换一个".into(), true)),
            Some(Err(e)) => st.message = Some((e.into(), true)),
            Some(Ok(h)) => self.apply_hotkey(h),
        }
        cx.notify();
    }

    fn apply_hotkey(&mut self, h: Hotkey) {
        let old = self.hotkey;
        let message = if self.tray.set_hotkey(h) {
            self.hotkey = h;
            self.notice = None;
            match hotkey::save(&h) {
                Ok(()) => (format!("已设置为 {}", h.label()), false),
                Err(e) => (format!("已设置为 {}，但保存失败：{e}", h.label()), true),
            }
        } else {
            // Put the previous one back (fails too if it was never available).
            if !self.tray.set_hotkey(old) {
                self.notice = Some(format!("热键 {} 不可用", old.label()));
            }
            (format!("{} 已被其他程序占用或被系统保留，请换一个", h.label()), true)
        };
        if let Some(st) = self.settings.as_mut() {
            st.recording = false;
            st.message = Some(message);
        }
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
        if self.results.len() == 0 {
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
        let index = self.shared.index.read();
        let entry = self.results.entry(self.selected?, &index)?;
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
        let range = range.start..range.end.min(self.results.len());
        let index = self.shared.index.try_read();
        let wanted: Vec<Option<u32>> = match (&*self.results, &index) {
            (Results::List(v), _) => v[range.clone()].iter().map(|&e| Some(e)).collect(),
            (all, Some(index)) => range
                .clone()
                .map(|pos| {
                    let e = all.entry(pos, index);
                    if let Some(e) = e {
                        self.positions.insert(pos, e);
                    }
                    e
                })
                .collect(),
            (_, None) => range.clone().map(|pos| self.positions.get(&pos).copied()).collect(),
        };
        if self.positions.len() > 4096 {
            self.positions.clear();
        }
        if let Some(index) = index {
            if index.epoch != self.result_epoch {
                // Results refer to a previous index layout; a fresh search is
                // already on its way (generation was bumped). Show nothing.
                self.rows.clear();
                self.positions.clear();
                return vec![None; wanted.len()];
            }
            let generation = index.generation.load(Ordering::Relaxed);
            if generation != self.rows_gen {
                self.rows.clear();
                self.rows_gen = generation;
            }
            for &e in wanted.iter().flatten() {
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
        wanted.iter().map(|e| e.and_then(|e| self.rows.get(&e).cloned())).collect()
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
    let t = theme::get(cx);
    div()
        .id(id)
        .flex()
        .flex_row()
        .justify_between()
        .gap_6()
        .px_3()
        .py(px(5.))
        .rounded_sm()
        .hover(move |s| s.bg(t.hover))
        .child(label)
        .child(div().text_color(t.faint).child(shortcut))
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            if let Some(menu) = this.menu.take() {
                action(&menu.row, cx);
            }
            cx.notify();
        }))
}

fn menu_separator(t: &Theme) -> impl IntoElement {
    div().my_1().h(px(1.)).bg(t.separator)
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
        let t = theme::get(cx);
        div()
            .id(text)
            .flex()
            .flex_row()
            .items_center()
            .h_full()
            .px_2()
            .overflow_hidden()
            .whitespace_nowrap()
            .relative()
            .border_r_1()
            .border_color(t.divider)
            .hover(move |s| s.bg(t.hover))
            .child(format!("{text}{arrow}"))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.set_sort(key, cx)))
    }

    /// Grip over a column border, `GRIP` wide and centered on it: the name
    /// column's right edge, and the left edge of the columns right of the
    /// (flexible) path column, so the border always follows the mouse.
    /// Highlighted while hovered or dragged.
    fn resize_handle(&self, col: Col, cx: &mut Context<Self>) -> impl IntoElement {
        const GRIP: f32 = 10.;
        let t = theme::get(cx);
        let c = self.cols;
        let active = self.col_drag.is_some_and(|d| d.col == col);
        div()
            .id(match col {
                Col::Name => "grip-name",
                Col::Size => "grip-size",
                Col::Date => "grip-date",
            })
            .absolute()
            .top_0()
            .h_full()
            .w(px(GRIP))
            .map(|d| match col {
                Col::Name => d.left(px(c.name - GRIP / 2.)),
                Col::Size => d.right(px(c.date + c.size - GRIP / 2.)),
                Col::Date => d.right(px(c.date - GRIP / 2.)),
            })
            .flex()
            .justify_center()
            .cursor_col_resize()
            .group("grip")
            .child(
                div()
                    .w(px(2.))
                    .h_full()
                    .when(active, |d| d.bg(t.accent))
                    .group_hover("grip", move |s| s.bg(t.accent)),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                    // Keep the header cell from treating this as a sort click.
                    cx.stop_propagation();
                    this.col_drag = Some(ColDrag { col, start_x: ev.position.x, start_w: this.cols.get(col) });
                    cx.notify();
                }),
            )
    }

    fn drag_column(&mut self, ev: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = self.col_drag else { return };
        if ev.pressed_button != Some(MouseButton::Left) {
            self.end_col_drag(cx); // released outside the window
            return;
        }
        let dx = (ev.position.x - d.start_x).as_f32();
        let w = if d.col == Col::Name { d.start_w + dx } else { d.start_w - dx };
        let others = self.cols.name + self.cols.size + self.cols.date - self.cols.get(d.col);
        let max = (window.viewport_size().width.as_f32() - others - Columns::MIN_PATH).max(Columns::MIN);
        self.cols.set(d.col, w.clamp(Columns::MIN, max));
        cx.notify();
    }

    fn end_col_drag(&mut self, cx: &mut Context<Self>) {
        if self.col_drag.take().is_some() {
            self.cols.save();
            cx.notify();
        }
    }

    fn render_menu(&self, menu: Menu, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme::get(cx);
        deferred(
            anchored().position(menu.position).snap_to_window().child(
                div()
                    .id("context-menu")
                    .occlude()
                    .min_w(px(220.))
                    .p_1()
                    .bg(t.popup)
                    .text_color(t.text)
                    .border_1()
                    .border_color(t.popup_border)
                    .rounded_md()
                    .shadow_md()
                    .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                        this.menu = None;
                        cx.notify();
                    }))
                    .child(menu_item("m-open", "打开", "Enter", cx, |r, _| os::open(&r.full)))
                    .child(menu_item("m-reveal", "打开所在文件夹", "Ctrl+Enter", cx, |r, _| os::reveal(&r.full)))
                    .child(menu_separator(&t))
                    .child(menu_item("m-copy-full", "复制完整路径", "Ctrl+Shift+C", cx, |r, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(r.full.to_string()))
                    }))
                    .child(menu_item("m-copy-name", "复制名称", "", cx, |r, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(r.name.to_string()))
                    }))
                    .child(menu_item("m-copy-dir", "复制所在文件夹路径", "", cx, |r, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(r.path.to_string()))
                    }))
                    .child(menu_separator(&t))
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
        let t = theme::get(cx);
        let cols = self.cols;

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
                            .when(is_selected, |d| d.bg(t.selected))
                            .when(!is_selected, |d| d.hover(|s| s.bg(t.hover)))
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
                            .child(cell(row.name).w(px(cols.name)).flex_none())
                            .child(cell(row.path).flex_grow(1.).flex_basis(px(0.)).min_w_0())
                            .child(cell(row.size).w(px(cols.size)).flex_none().text_right())
                            .child(cell(row.modified).w(px(cols.date)).flex_none())
                    })
                    .collect()
            }),
        )
        .track_scroll(&self.scroll)
        .flex_grow(1.)
        .w_full();

        let main = div()
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
            .on_action(cx.listener(Self::open_settings_action))
            .when(self.col_drag.is_some(), |d| d.cursor_col_resize())
            .on_mouse_move(cx.listener(Self::drag_column))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _: &MouseUpEvent, _, cx| this.end_col_drag(cx)))
            .on_mouse_up_out(MouseButton::Left, cx.listener(|this, _: &MouseUpEvent, _, cx| this.end_col_drag(cx)))
            .flex()
            .flex_col()
            .size_full()
            .bg(t.bg)
            .text_color(t.text)
            .text_size(px(13.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .p_2()
                    .bg(t.chrome)
                    .child(div().flex_grow(1.).min_w_0().child(self.input.clone()))
                    .child(
                        div()
                            .id("settings-button")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .text_color(t.muted)
                            .hover(|s| s.bg(t.button_hover))
                            .child("设置")
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.open_settings(window, cx))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .relative()
                    .flex_row()
                    .h(px(ROW_H))
                    .items_center()
                    .bg(t.chrome)
                    .border_b_1()
                    .border_color(t.chrome_border)
                    .text_color(t.muted)
                    .child(
                        self.header_cell("名称", SortKey::Name, cx)
                            .w(px(cols.name))
                            .flex_none(),
                    )
                    .child(self.header_cell("路径", SortKey::Path, cx).flex_grow(1.).flex_basis(px(0.)).min_w_0())
                    .child(
                        self.header_cell("大小", SortKey::Size, cx)
                            .w(px(cols.size))
                            .flex_none()
                            .justify_end(),
                    )
                    .child(
                        self.header_cell("修改时间", SortKey::Modified, cx)
                            .w(px(cols.date))
                            .flex_none(),
                    )
                    // Grips go last so they sit above the cells and can
                    // straddle the borders.
                    .child(self.resize_handle(Col::Name, cx))
                    .child(self.resize_handle(Col::Size, cx))
                    .child(self.resize_handle(Col::Date, cx)),
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
                    .bg(t.chrome)
                    .border_t_1()
                    .border_color(t.chrome_border)
                    .text_color(t.muted)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_3()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(format!("{count} 个对象"))
                            .when_some(self.query_error.clone(), |d, e| d.child(div().text_color(t.error).child(e))),
                    )
                    .child(format!(
                        "{}{status}  ·  搜索 {:.1} ms",
                        self.notice.as_ref().map(|n| format!("{n}  ·  ")).unwrap_or_default(),
                        self.search_time.as_secs_f64() * 1000.
                    )),
            );

        // The settings panel sits outside the "SwiftFind" key context, so the
        // list's bindings (Enter, Esc, ...) don't fire while recording a key.
        div()
            .relative()
            .size_full()
            .child(main)
            .when(self.settings.is_some(), |d| d.child(self.render_settings(cx)))
    }
}

fn button(t: &Theme, id: &'static str, label: impl Into<SharedString>, primary: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(if primary { t.accent } else { t.popup_border })
        .bg(if primary { t.accent } else { t.field })
        .text_color(if primary { t.on_accent } else { t.text })
        .hover(|s| s.opacity(0.85))
        .child(label.into())
}

impl SwiftFindApp {
    fn render_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let st = self.settings.as_ref().unwrap();
        let recording = st.recording;
        let t = theme::get(cx);
        let key_text = if recording { "请按下新的组合键…".to_string() } else { self.hotkey.label() };
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(t.overlay)
            .occlude()
            .child(
                div()
                    .key_context("Settings")
                    .track_focus(&st.focus)
                    .on_key_down(cx.listener(Self::settings_key))
                    .w(px(460.))
                    .p_5()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .bg(t.popup)
                    .text_color(t.text)
                    .text_size(px(13.))
                    .rounded_lg()
                    .shadow_lg()
                    .child(div().text_size(px(16.)).child("设置"))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .child("显示 / 隐藏窗口的全局热键")
                            .child(
                                div()
                                    .id("hotkey-field")
                                    .min_w(px(170.))
                                    .px_3()
                                    .py_1()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(if recording { t.accent } else { t.popup_border })
                                    .bg(if recording { t.field_active } else { t.field })
                                    .text_center()
                                    .child(key_text)
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        if let Some(st) = this.settings.as_mut() {
                                            st.recording = true;
                                            st.message = None;
                                            window.focus(&st.focus, cx);
                                        }
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(div().text_color(t.faint).child(if recording {
                        "按 Esc 取消"
                    } else {
                        "点击右侧按钮后按下新的组合键，需包含 Ctrl / Alt / Win，或使用 F1–F24"
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .child("开机自动启动")
                            .child({
                                let state = st.autostart;
                                let label = match state {
                                    None => "…",
                                    Some(true) => "已开启",
                                    Some(false) => "已关闭",
                                };
                                button(&t, "autostart-toggle", label, state == Some(true))
                                    .min_w(px(170.))
                                    .text_center()
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        if let Some(on) = state {
                                            this.run_autostart(Some(!on), cx);
                                            cx.notify();
                                        }
                                    }))
                            }),
                    )
                    .when_some(st.message.clone(), |d, (text, is_error)| {
                        d.child(div().text_color(if is_error { t.error } else { t.ok }).child(text))
                    })
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .mt_2()
                            .child(button(&t, "hotkey-default", format!("恢复默认（{}）", Hotkey::DEFAULT.label()), false).on_click(
                                cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.apply_hotkey(Hotkey::DEFAULT);
                                    cx.notify();
                                }),
                            ))
                            .child(button(&t, "settings-close", "关闭", true).on_click(cx.listener(
                                |this, _: &ClickEvent, window, cx| this.close_settings(window, cx),
                            ))),
                    ),
            )
    }
}
