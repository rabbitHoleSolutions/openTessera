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

        let listing = services.catalog.get(&src);
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
            flash_label(ui, state);
        });
        ui.add_space(4.0);
        ui.separator();

        let shown = items.len().min(MAX_ROWS);
        egui::ScrollArea::vertical().show(ui, |ui| {
            for m in items.iter().take(MAX_ROWS) {
                row(ui, m, services.thumbs, queue, actions, state);
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

/// One item row: poster, title, size, and the two queue actions.
fn row(
    ui: &mut egui::Ui,
    m: &MediaItem,
    thumbs: &mut ThumbCache,
    queue: &QueueView,
    actions: &mut Vec<OverlayAction>,
    state: &mut BrowserState,
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
