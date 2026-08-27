//! Library browser: the UI of a library window.
//!
//! Shape follows the Quest app's BrowserScreen — a source list down the
//! left, that source's items on the right with a live filter, a sort
//! order, and the two distinct ways to queue things: bulk "add N to queue"
//! (which folds into the queue's order) and a per-row "+" (which appends
//! without rewriting the order). See `PlaybackQueue::add_items` vs
//! `append_items`.
//!
//! v1 lists each source flat rather than walking a folder tree: `Source`
//! has no directory model yet, and search over a flat list covers most of
//! "find things and queue them". Folder navigation is the obvious next
//! step and nothing here assumes its absence.

use tessera_core::sources::Source;
use tessera_core::{MediaItem, QueueOrder};

use crate::catalog::{Listing, SourceCatalog};
use crate::overlay::OverlayAction;
use crate::thumbs::ThumbCache;

/// How long "clear queue" stays armed before it disarms itself.
const CLEAR_ARM_SECS: f64 = 4.0;

/// How long the "added N" confirmation stays up. The queue lives in the
/// other windows, so without a flash a bulk add gives no feedback at all.
const FLASH_SECS: f64 = 2.5;

/// Rows drawn per frame. The full list can be thousands of items; egui has
/// no virtualisation here, so the filter is how you reach the rest.
const MAX_ROWS: usize = 300;

/// Which tab the browser window is showing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Library,
    Queue,
}

/// One live tile, wherever it lives. Gathered across every wall window
/// before the per-screen loop, because a `Screen` cannot see its siblings.
pub struct WallTile {
    /// Index of the owning window in `screens`, and the monitor it is on.
    pub window: usize,
    pub monitor: usize,
    pub tile_id: usize,
    pub title: String,
    pub paused: bool,
    pub muted: bool,
    pub audio_enabled: bool,
    pub volume: i64,
    pub time_pos: f64,
    pub duration: f64,
}

/// Per-window browser state. Each library window browses independently.
pub struct LibraryState {
    pub tab: Tab,
    /// Index into the configured sources; None until one is picked (or the
    /// source list is empty).
    pub source: Option<usize>,
    pub search: String,
    pub order: QueueOrder,
    /// Transient confirmation of the last queue action.
    pub flash: Option<(String, std::time::Instant)>,
    /// "clear queue" armed at this instant. Emptying the queue is a big,
    /// silent action, so it takes two clicks — same pattern as the
    /// transport's delete button.
    pub clear_armed: Option<std::time::Instant>,
}

impl Default for LibraryState {
    fn default() -> LibraryState {
        LibraryState {
            tab: Tab::default(),
            source: None,
            search: String::new(),
            order: QueueOrder::Name,
            flash: None,
            clear_armed: None,
        }
    }
}

impl LibraryState {
    fn flash(&mut self, msg: String) {
        self.flash = Some((msg, std::time::Instant::now()));
    }
}

/// A read-only look at the shared queue. The browser never mutates it
/// directly — every change goes out as an [`OverlayAction`], because the
/// queue is owned by the event loop.
pub struct QueueView<'a> {
    pub items: &'a [MediaItem],
    /// Index of the item the wall is currently on. Everything at or before
    /// it is history and cannot be reordered or removed.
    pub current: usize,
}

impl QueueView<'_> {
    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn contains(&self, id: &str) -> bool {
        self.items.iter().any(|i| i.id == id)
    }
}

/// Draws the browser window: a tab bar, then whichever tab is showing.
/// Every mutation leaves as an [`OverlayAction`] — the queue and the tiles
/// are owned by the event loop, not by this window.
#[allow(clippy::too_many_arguments)]
pub fn draw(
    ctx: &egui::Context,
    state: &mut LibraryState,
    sources: &[Source],
    catalog: &mut SourceCatalog,
    thumbs: &mut ThumbCache,
    queue: &QueueView,
    wall: &[WallTile],
    actions: &mut Vec<OverlayAction>,
) {
    catalog.poll();
    if let Some((_, at)) = &state.flash {
        if at.elapsed().as_secs_f64() > FLASH_SECS {
            state.flash = None;
        }
    }

    egui::TopBottomPanel::top("browser-bar").show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut state.tab, Tab::Library, "library");
            ui.selectable_value(&mut state.tab, Tab::Queue, "queue");
            ui.separator();
            let playing = wall.iter().filter(|t| !t.paused).count();
            ui.weak(format!(
                "{} in the queue · {playing}/{} tile(s) playing",
                queue.len(),
                wall.len()
            ));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if state.tab == Tab::Library
                    && ui
                        .button("refresh")
                        .on_hover_text("re-list this source")
                        .clicked()
                {
                    if let Some(src) = state.source.and_then(|i| sources.get(i)) {
                        catalog.refresh(src);
                    }
                }
            });
        });
    });

    match state.tab {
        Tab::Library => draw_library(ctx, state, sources, catalog, thumbs, queue, actions),
        Tab::Queue => draw_queue(ctx, state, queue, wall, actions),
    }
}

/// The queue tab: what is playing right now across every wall window, and
/// the queue those tiles are fed from.
fn draw_queue(
    ctx: &egui::Context,
    state: &mut LibraryState,
    queue: &QueueView,
    wall: &[WallTile],
    actions: &mut Vec<OverlayAction>,
) {
    egui::CentralPanel::default().show(ctx, |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.add_space(4.0);
            ui.label(egui::RichText::new("now playing").strong());
            ui.weak("every tile on every wall window");
            ui.add_space(4.0);
            if wall.is_empty() {
                ui.weak("no tiles — open a wall window with the status bar's +");
            }
            for t in wall {
                ui.horizontal(|ui| {
                    if ui
                        .button(if t.paused { "▶" } else { "⏸" })
                        .on_hover_text(if t.paused { "resume" } else { "pause" })
                        .clicked()
                    {
                        actions.push(OverlayAction::TogglePause { tile_id: t.tile_id });
                    }
                    if ui
                        .button(if t.muted { "🔇" } else { "🔊" })
                        .on_hover_text(if t.audio_enabled {
                            "mute / unmute"
                        } else {
                            "audio is off — restart with --audio"
                        })
                        .clicked()
                    {
                        actions.push(OverlayAction::MuteButton { tile_id: t.tile_id });
                    }
                    let mut vol = t.volume;
                    if ui
                        .add_enabled(
                            t.audio_enabled && !t.muted,
                            egui::Slider::new(&mut vol, 0..=100)
                                .show_value(false)
                                .fixed_decimals(0),
                        )
                        .changed()
                    {
                        actions.push(OverlayAction::SetVolume { tile_id: t.tile_id, volume: vol });
                    }
                    ui.weak(format!("w{} · m{}", t.window, t.monitor));
                    ui.weak(format!("{} / {}", fmt_time(t.time_pos), fmt_time(t.duration)));
                    ui.label(truncate(&t.title, 70));
                });
            }

            ui.add_space(8.0);
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("queue ({})", queue.len())).strong());
                if ui
                    .button("shuffle")
                    .on_hover_text("re-shuffle what is still to play")
                    .clicked()
                {
                    actions.push(OverlayAction::QueueShuffle);
                }
                if ui
                    .button("A–Z")
                    .on_hover_text("sort what is still to play by name")
                    .clicked()
                {
                    actions.push(OverlayAction::QueueSortName);
                }
                // Clear: armed on the first click, done on the second.
                if state.clear_armed.is_some_and(|at| at.elapsed().as_secs_f64() > CLEAR_ARM_SECS) {
                    state.clear_armed = None;
                }
                let armed = state.clear_armed.is_some();
                let btn = if armed {
                    egui::Button::new(
                        egui::RichText::new(format!("click again to clear {}", queue.len()))
                            .color(egui::Color32::WHITE),
                    )
                    .fill(egui::Color32::from_rgb(150, 50, 50))
                } else {
                    egui::Button::new("clear queue")
                };
                if ui
                    .add_enabled(!queue.items.is_empty(), btn)
                    .on_hover_text("empty the queue — tiles keep playing what they already loaded")
                    .clicked()
                {
                    if armed {
                        state.clear_armed = None;
                        state.flash(format!("cleared {} item(s)", queue.len()));
                        actions.push(OverlayAction::QueueClear);
                    } else {
                        state.clear_armed = Some(std::time::Instant::now());
                    }
                }
                if let Some((msg, _)) = &state.flash {
                    ui.colored_label(egui::Color32::from_rgb(140, 220, 140), msg);
                }
            });
            ui.weak("items already played (and the current one) stay put — only what is pending reorders");
            ui.add_space(4.0);

            if queue.items.is_empty() {
                ui.weak("the queue is empty — add something from the library tab");
                return;
            }
            // The first pending item has nothing to swap up into.
            let first_pending = queue.current + 1;
            for (i, m) in queue.items.iter().enumerate().take(MAX_ROWS) {
                let pending = i > queue.current;
                ui.horizontal(|ui| {
                    if i == queue.current {
                        ui.colored_label(egui::Color32::from_rgb(140, 220, 140), "▶");
                    } else {
                        ui.weak(format!("{:>3}", i + 1));
                    }
                    // Only the pending tail can be reordered or removed —
                    // PlaybackQueue enforces it, so the buttons match.
                    ui.add_enabled_ui(pending, |ui| {
                        if ui
                            .add_enabled(i > first_pending, egui::Button::new("⏶").small())
                            .on_hover_text("move up")
                            .clicked()
                        {
                            actions.push(OverlayAction::QueueReorder { from: i, to: i - 1 });
                        }
                        if ui
                            .add_enabled(i + 1 < queue.items.len(), egui::Button::new("⏷").small())
                            .on_hover_text("move down")
                            .clicked()
                        {
                            actions.push(OverlayAction::QueueReorder { from: i, to: i + 1 });
                        }
                        if ui
                            .add(egui::Button::new("✖").small())
                            .on_hover_text("remove from the queue")
                            .clicked()
                        {
                            actions.push(OverlayAction::QueueRemove { index: i });
                        }
                    });
                    if pending {
                        ui.label(truncate(&m.title, 80));
                    } else {
                        ui.weak(truncate(&m.title, 80));
                    }
                });
            }
            if queue.items.len() > MAX_ROWS {
                ui.add_space(6.0);
                ui.weak(format!("{} more not shown", queue.items.len() - MAX_ROWS));
            }
        });
    });
}

/// The library tab: sources down the left, that source's items on the
/// right with a live filter and an order.
#[allow(clippy::too_many_arguments)]
fn draw_library(
    ctx: &egui::Context,
    state: &mut LibraryState,
    sources: &[Source],
    catalog: &mut SourceCatalog,
    thumbs: &mut ThumbCache,
    queue: &QueueView,
    actions: &mut Vec<OverlayAction>,
) {
    if state.source.is_none() && !sources.is_empty() {
        state.source = Some(0);
    }
    if state.source.is_some_and(|i| i >= sources.len()) {
        state.source = (!sources.is_empty()).then_some(0);
    }

    egui::SidePanel::left("library-sources")
        .resizable(true)
        .default_width(220.0)
        .show(ctx, |ui| {
            ui.add_space(6.0);
            ui.label(egui::RichText::new("sources").strong());
            ui.weak("configured in settings (S)");
            ui.add_space(6.0);
            if sources.is_empty() {
                ui.weak("none — add one in the settings panel");
            }
            for (i, src) in sources.iter().enumerate() {
                let selected = state.source == Some(i);
                if ui.selectable_label(selected, src.label()).clicked() {
                    state.source = Some(i);
                    state.search.clear();
                }
            }
        });

    egui::CentralPanel::default().show(ctx, |ui| {
        let Some(src) = state.source.and_then(|i| sources.get(i)).cloned() else {
            ui.centered_and_justified(|ui| {
                ui.weak("no source selected");
            });
            return;
        };

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("filter");
            ui.add(
                egui::TextEdit::singleline(&mut state.search)
                    .hint_text("part of a title")
                    .desired_width(260.0),
            );
            if !state.search.is_empty() && ui.button("✖").on_hover_text("clear").clicked() {
                state.search.clear();
            }
            ui.separator();
            ui.label("order");
            for (order, label) in [
                (QueueOrder::Name, "name"),
                (QueueOrder::Date, "date"),
                (QueueOrder::Size, "size"),
            ] {
                if ui.selectable_label(state.order == order, label).clicked() {
                    state.order = order;
                }
            }
        });
        ui.add_space(4.0);

        let listing = catalog.get(&src);
        let (items, status): (Vec<&MediaItem>, Option<String>) = match listing {
            Listing::Loading => (Vec::new(), Some("listing…".to_owned())),
            Listing::Failed(e) => (Vec::new(), Some(format!("couldn't list this source: {e}"))),
            Listing::Ready(all) => {
                let needle = state.search.trim().to_lowercase();
                let mut v: Vec<&MediaItem> = all
                    .iter()
                    .filter(|m| needle.is_empty() || m.title.to_lowercase().contains(&needle))
                    .collect();
                match state.order {
                    QueueOrder::Name | QueueOrder::Shuffle => {
                        v.sort_by_key(|m| m.title.to_lowercase())
                    }
                    QueueOrder::Date => v.sort_by_key(|m| std::cmp::Reverse(m.modified.unwrap_or(0))),
                    QueueOrder::Size => v.sort_by_key(|m| std::cmp::Reverse(m.size.unwrap_or(0))),
                }
                (v, None)
            }
        };

        if let Some(msg) = status {
            ui.add_space(10.0);
            ui.weak(msg);
            return;
        }

        // Bulk add — of the matches when filtering, else the whole source,
        // exactly like the Quest app's "Add N to queue".
        ui.horizontal(|ui| {
            let n = items.len();
            let label = if state.search.trim().is_empty() {
                format!("add all {n} to queue")
            } else {
                format!("add {n} match{} to queue", if n == 1 { "" } else { "es" })
            };
            if ui.add_enabled(n > 0, egui::Button::new(label)).clicked() {
                let add: Vec<MediaItem> = items.iter().map(|m| (*m).clone()).collect();
                state.flash(format!("added {} to the queue", add.len()));
                actions.push(OverlayAction::QueueAdd { items: add });
            }
            if let Some((msg, _)) = &state.flash {
                ui.colored_label(egui::Color32::from_rgb(140, 220, 140), msg);
            }
        });
        ui.add_space(4.0);
        ui.separator();

        let shown = items.len().min(MAX_ROWS);
        egui::ScrollArea::vertical().show(ui, |ui| {
            for m in items.iter().take(MAX_ROWS) {
                row(ui, m, thumbs, queue, actions, state);
            }
            if items.len() > shown {
                ui.add_space(6.0);
                ui.weak(format!(
                    "{} more not shown — narrow the filter",
                    items.len() - shown
                ));
            }
        });
    });
}

/// mm:ss / h:mm:ss, matching the wall transport.
fn fmt_time(t: f64) -> String {
    if !t.is_finite() || t < 0.0 {
        return "--:--".into();
    }
    let s = t as u64;
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}")
    } else {
        format!("{m}:{sec:02}")
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

/// One item row: poster, title, size, and the two queue actions.
fn row(
    ui: &mut egui::Ui,
    m: &MediaItem,
    thumbs: &mut ThumbCache,
    queue: &QueueView,
    actions: &mut Vec<OverlayAction>,
    state: &mut LibraryState,
) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(64.0, 36.0), egui::Sense::hover());
        match thumbs.get(ui.ctx(), &m.id, m.thumb_url.as_deref()) {
            Some(tex) => {
                ui.painter().image(
                    tex.id(),
                    rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
            None => {
                ui.painter()
                    .rect_filled(rect, egui::Rounding::same(3.0), egui::Color32::from_gray(28));
            }
        }

        let queued = queue.contains(&m.id);
        if ui
            .add(egui::Button::new("+").small())
            .on_hover_text("append to the end of the queue")
            .clicked()
        {
            state.flash(format!("queued \"{}\"", m.title));
            actions.push(OverlayAction::QueueAppend { item: m.clone() });
        }
        if ui
            .add(egui::Button::new("▶").small())
            .on_hover_text("play now on the first tile of the first wall window")
            .clicked()
        {
            actions.push(OverlayAction::PlayNow { item: m.clone() });
        }

        let title = if queued {
            egui::RichText::new(format!("{} · queued", m.title)).weak()
        } else {
            egui::RichText::new(&m.title)
        };
        ui.label(title);

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if let Some(sz) = m.size {
                ui.weak(human_size(sz));
            }
        });
    });
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{} {}", bytes, UNITS[0])
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn human_size_scales_and_keeps_bytes_exact() {
        assert_eq!(super::human_size(512), "512 B");
        assert_eq!(super::human_size(2048), "2.0 KB");
        assert_eq!(super::human_size(3_500_000_000), "3.3 GB");
    }
}
