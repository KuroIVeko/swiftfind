//! Main window: search box, virtualized result list and status bar.

use crate::index::Query;
use crate::indexer::Shared;
use crate::os;
use crate::text_input::{self, Changed, TextInput};
use gpui::{
    App, ClickEvent, Context, Entity, FocusHandle, Focusable, KeyBinding, MouseButton,
    MouseDownEvent, ScrollStrategy, SharedString, UniformListScrollHandle, Window, actions, div,
    prelude::*, px, rgb, uniform_list,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

actions!(everything, [SelectNext, SelectPrev, SelectFirst, SelectLast, OpenSelected, RevealSelected]);

pub fn bind_keys(cx: &mut App) {
    text_input::bind_keys(cx);
    cx.bind_keys([
        KeyBinding::new("down", SelectNext, Some("Everything")),
        KeyBinding::new("up", SelectPrev, Some("Everything")),
        KeyBinding::new("ctrl-home", SelectFirst, Some("Everything")),
        KeyBinding::new("ctrl-end", SelectLast, Some("Everything")),
        KeyBinding::new("enter", OpenSelected, Some("Everything")),
        KeyBinding::new("ctrl-enter", RevealSelected, Some("Everything")),
    ]);
}

const ROW_H: f32 = 24.;
const COL_SIZE: f32 = 100.;
const COL_DATE: f32 = 150.;

#[derive(Clone)]
struct Row {
    name: SharedString,
    path: SharedString,
    full: Arc<str>,
    size: SharedString,
    modified: SharedString,
}

pub struct EverythingApp {
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
    rows: HashMap<u32, Row>,
    rows_gen: u64,
    focus_handle: FocusHandle,
}

impl EverythingApp {
    pub fn new(shared: Arc<Shared>, window: &mut Window, cx: &mut Context<Self>) -> Self {
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
            rows: HashMap::new(),
            rows_gen: u64::MAX,
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
        self.searching = true;
        let task = cx.background_executor().spawn(async move {
            let t = Instant::now();
            let index = shared.index.read();
            let generation = index.generation.load(Ordering::Relaxed);
            let res = index.search(&Query::parse(&query), &|| latest.load(Ordering::Relaxed) != seq);
            res.map(|r| (r, generation, t.elapsed()))
        });
        cx.spawn(async move |this, cx| {
            let res = task.await;
            this.update(cx, |this, cx| {
                if this.search_seq.load(Ordering::SeqCst) != seq {
                    return;
                }
                this.searching = false;
                if let Some((results, generation, elapsed)) = res {
                    this.results = Arc::new(results);
                    this.result_gen = generation;
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
        index.is_alive(entry).then(|| index.full_path(entry))
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

    /// Builds (and caches) display data for the given result rows.
    fn rows_for(&mut self, range: std::ops::Range<usize>) -> Vec<Option<Row>> {
        let wanted: Vec<u32> = self.results[range.clone()].to_vec();
        if let Some(index) = self.shared.index.try_read() {
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
                let meta = std::fs::symlink_metadata(&full).ok();
                let size = match (&meta, is_dir) {
                    (Some(m), false) => format_size(m.len()),
                    _ => String::new(),
                };
                let modified = meta
                    .and_then(|m| m.modified().ok())
                    .map(format_time)
                    .unwrap_or_default();
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

fn format_time(t: SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Local> = t.into();
    dt.format("%Y-%m-%d %H:%M").to_string()
}

fn header_cell(text: &'static str) -> gpui::Div {
    div().px_2().overflow_hidden().whitespace_nowrap().child(text)
}

fn cell(text: SharedString) -> gpui::Div {
    div().px_2().overflow_hidden().whitespace_nowrap().text_ellipsis().child(text)
}

impl Focusable for EverythingApp {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for EverythingApp {
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
                        let row = row.unwrap_or_else(|| Row {
                            name: "".into(),
                            path: "".into(),
                            full: "".into(),
                            size: "".into(),
                            modified: "".into(),
                        });
                        let full_open = row.full.clone();
                        let full_reveal = row.full.clone();
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
                                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                    this.selected = Some(ix);
                                    cx.notify();
                                    os::reveal(&full_reveal);
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
            .key_context("Everything")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::open_selected))
            .on_action(cx.listener(Self::reveal_selected))
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
                    .child(header_cell("名称").flex_basis(px(0.)).flex_grow(3.).min_w_0())
                    .child(header_cell("路径").flex_basis(px(0.)).flex_grow(5.).min_w_0())
                    .child(header_cell("大小").w(px(COL_SIZE)).flex_none().text_right())
                    .child(header_cell("修改时间").w(px(COL_DATE)).flex_none()),
            )
            .child(list)
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
                        "{status}  ·  搜索 {:.1} ms",
                        self.search_time.as_secs_f64() * 1000.
                    )),
            )
    }
}
