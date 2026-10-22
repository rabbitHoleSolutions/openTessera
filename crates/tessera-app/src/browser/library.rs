//! Library tab: sources down the left, that source's items on the right.
//!
//! Shape follows the Quest app's BrowserScreen — a live filter, a sort
//! order, and the two distinct ways to queue things: bulk "add N to queue"
//! (which folds into the queue's order) and a per-row "+" (which appends
//! without rewriting the order). See `PlaybackQueue::add_items` vs
//! `append_items`.
//!
//! v1 lists each source flat rather than walking a folder tree: `Source`
//! has no directory model yet, and search over a flat list covers most of
//! "find things and queue them". Folder navigation is the obvious next
//! step and nothing here assumes its absence.

use tessera_core::{MediaItem, QueueOrder};

use super::{flash_label, human_size, BrowserCtx, BrowserState, QueueView, MAX_ROWS};
use crate::catalog::Listing;
use crate::overlay::{OverlayAction, UiSettings};
use crate::thumbs::ThumbCache;

/// The library tab's filtered, ordered rows — indices into the source's
/// listing — and the inputs they were derived from.
///
/// This exists because the derivation used to run inside the egui pass,
/// every frame: filter the whole listing, then
/// `sort_by_key(|m| m.title.to_lowercase())`, which allocates a `String`
/// per comparison. A few thousand titles is tens of thousands of
/// allocations per frame, sixty times a second, to reproduce a list that
/// only changes when the user types in the filter box or clicks an order.
#[derive(Default)]
pub struct RowCache {
    key: Option<RowKey>,
    rows: Vec<usize>,
}

#[derive(PartialEq, Eq)]
struct RowKey {
    source: usize,
    /// Catalog generation, so a re-listing invalidates the view even when
    /// it happens to come back the same length.
    generation: u64,
    search: String,
    order: QueueOrder,
}

impl RowCache {
    /// Rebuilds the row list if any input moved; a no-op otherwise.
    fn ensure(&mut self, key: RowKey, items: &[MediaItem]) {
        if self.key.as_ref() == Some(&key) {
            return;
        }
        let needle = key.search.trim().to_lowercase();
        let mut rows: Vec<usize> = (0..items.len())
            .filter(|&i| needle.is_empty() || items[i].title.to_lowercase().contains(&needle))
            .collect();
        match key.order {
            // sort_by_cached_key, not sort_by_key: one lowercased title per
            // item rather than one per comparison.
            QueueOrder::Name | QueueOrder::Shuffle => {
                rows.sort_by_cached_key(|&i| items[i].title.to_lowercase())
            }
            QueueOrder::Date => {
                rows.sort_by_key(|&i| std::cmp::Reverse(items[i].modified.unwrap_or(0)))
            }
            QueueOrder::Size => {
                rows.sort_by_key(|&i| std::cmp::Reverse(items[i].size.unwrap_or(0)))
            }
        }
        self.rows = rows;
        self.key = Some(key);
    }

    fn rows(&self) -> &[usize] {
        &self.rows
    }

    /// Dropped when there is nothing to show, so a stale list cannot be
    /// drawn against a listing that has gone away.
    fn clear(&mut self) {
        self.key = None;
        self.rows.clear();
    }
}

pub fn draw(
    ctx: &egui::Context,
    state: &mut BrowserState,
    settings: &UiSettings,
    queue: &QueueView,
    services: &mut BrowserCtx,
    actions: &mut Vec<OverlayAction>,
) {
    let sources = &settings.sources;
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
            ui.weak("manage them in the sources tab (S)");
            ui.add_space(6.0);
            if sources.is_empty() {
                ui.weak("none yet");
                if ui.button("add a source").clicked() {
                    state.tab = super::Tab::Sources;
                }
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

        // Read before `get`, which borrows the catalog mutably.
        let generation = services.catalog.generation();
        let listing = services.catalog.get(&src);
        let (items, status): (&[MediaItem], Option<String>) = match listing {
            Listing::Loading => (&[], Some("listing…".to_owned())),
            Listing::Failed(e) => (&[], Some(format!("couldn't list this source: {e}"))),
            Listing::Ready(all) => (all.as_slice(), None),
        };

        if let Some(msg) = status {
            state.rows.clear();
            ui.add_space(10.0);
            ui.weak(msg);
            return;
        }

        state.rows.ensure(
            RowKey {
                source: state.source.unwrap_or(0),
                generation,
                search: state.search.clone(),
                order: state.order,
            },
            items,
        );

        // Bulk add — of the matches when filtering, else the whole source,
        // exactly like the Quest app's "Add N to queue".
        ui.horizontal(|ui| {
            let n = state.rows.rows().len();
            let label = if state.search.trim().is_empty() {
                format!("add all {n} to queue")
            } else {
                format!("add {n} match{} to queue", if n == 1 { "" } else { "es" })
            };
            if ui.add_enabled(n > 0, egui::Button::new(label)).clicked() {
                let add: Vec<MediaItem> =
                    state.rows.rows().iter().map(|&i| items[i].clone()).collect();
                let added = add.len();
                actions.push(OverlayAction::QueueAdd { items: add });
                state.flash(format!("added {added} to the queue"));
            }
            flash_label(ui, state);
        });
        ui.add_space(4.0);
        ui.separator();

        let rows = state.rows.rows();
        let total = rows.len();
        let shown = total.min(MAX_ROWS);
        // A row is the 36pt thumbnail plus the layout's own spacing. Passed
        // to show_rows so egui lays out only what is on screen: drawing all
        // 300 meant asking the thumbnail cache for 300 posters at once,
        // which fetched and uploaded every one of them whether or not it
        // was ever scrolled to.
        let row_h = 36.0 + ui.spacing().item_spacing.y;
        let mut flash: Option<String> = None;
        egui::ScrollArea::vertical().show_rows(ui, row_h, shown, |ui, range| {
            for &i in &rows[range] {
                row(ui, &items[i], services.thumbs, queue, actions, &mut flash);
            }
        });
        if total > shown {
            ui.add_space(6.0);
            ui.weak(format!("{} more not shown — narrow the filter", total - shown));
        }
        if let Some(msg) = flash {
            state.flash(msg);
        }
    });
}

/// A play button the size of the "+" beside it, its triangle painted.
///
/// "▶" as text comes from egui's emoji fallback font, whose advance and
/// baseline don't match the UI font's: it sat low and right of centre in
/// its button, and wider than the "+".
fn play_button(ui: &mut egui::Ui, size: egui::Vec2) -> egui::Response {
    let resp = ui.add(egui::Button::new("").small().min_size(size));
    let col = ui.style().interact(&resp).fg_stroke.color;
    let r = resp.rect;
    let h = r.height() * 0.5;
    let w = h * 0.87;
    // Centred on the triangle's centroid, which is a third of the way in
    // from its flat side; centring the bounding box looks left-heavy.
    let c = r.center();
    let left = c.x - w / 3.0;
    ui.painter().add(egui::Shape::convex_polygon(
        vec![egui::pos2(left, c.y - h / 2.0), egui::pos2(left + w, c.y), egui::pos2(left, c.y + h / 2.0)],
        col,
        egui::Stroke::NONE,
    ));
    resp
}

/// One item row: poster, title, size, and the two queue actions.
fn row(
    ui: &mut egui::Ui,
    m: &MediaItem,
    thumbs: &mut ThumbCache,
    queue: &QueueView,
    actions: &mut Vec<OverlayAction>,
    // Set instead of calling `BrowserState::flash` directly: the row list
    // is borrowed from the same `BrowserState` for the whole loop.
    flash: &mut Option<String>,
) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(64.0, 36.0), egui::Sense::hover());
        match thumbs.get(ui.ctx(), m) {
            Some(tex) => {
                // Fitted inside the slot, not stretched to it: a server
                // poster is 2:3 and a frame from a file is the film's own
                // shape, so filling a 16:9 slot squashed every poster. The
                // slot stays fixed so titles still line up row to row.
                let size = tex.size_vec2();
                let s = (rect.width() / size.x.max(1.0)).min(rect.height() / size.y.max(1.0));
                let img = egui::Rect::from_center_size(rect.center(), size * s);
                ui.painter()
                    .rect_filled(rect, egui::Rounding::same(3.0), egui::Color32::from_gray(18));
                ui.painter().image(
                    tex.id(),
                    img,
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
        let plus = ui
            .add(egui::Button::new("+").small())
            .on_hover_text("append to the end of the queue");
        if plus.clicked() {
            *flash = Some(format!("queued \"{}\"", m.title));
            actions.push(OverlayAction::QueueAppend { item: m.clone() });
        }
        if play_button(ui, plus.rect.size())
            .on_hover_text("play now on the first tile of the first wall window")
            .clicked()
        {
            actions.push(OverlayAction::PlayNow { item: m.clone() });
        }
        // With the row's other buttons, not at its right end: there it
        // sat under the scroll bar, which widens on hover and took the
        // click.
        if ui
            .add(egui::Button::new("i").small())
            .on_hover_text("what is this file? (path, size, format)")
            .clicked()
        {
            actions.push(OverlayAction::ItemInfo { item: m.clone() });
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
            // Two copies of one film list under one title; the frame size
            // is usually what tells them apart before anything is opened.
            if let (Some(w), Some(h)) = (m.width, m.height) {
                ui.weak(tessera_core::info::resolution_label(w, h));
            }
        });
    });
}
