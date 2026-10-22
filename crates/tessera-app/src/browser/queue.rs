//! Queue tab: what is playing right now across every wall window, and the
//! queue those tiles are fed from.
//!
//! "remember the queue between runs" lives here rather than in config: it
//! is a fact about this list, and it is the tab you are on when you wonder
//! whether the list will still be here tomorrow.

use super::{flash_label, fmt_time, truncate, BrowserState, QueueView, WallTile, MAX_ROWS};
use crate::overlay::{OverlayAction, UiSettings};

/// How long "clear queue" stays armed before it disarms itself.
const CLEAR_ARM_SECS: f64 = 4.0;

pub fn draw(
    ctx: &egui::Context,
    state: &mut BrowserState,
    settings: &mut UiSettings,
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
                            "audio is off — turn it on in the config tab"
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
                    if ui
                        .add(egui::Button::new("i").small())
                        .on_hover_text("what is this file? (path, size, format, live decode)")
                        .clicked()
                    {
                        actions.push(OverlayAction::TileInfo { tile_id: t.tile_id });
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
                flash_label(ui, state);
            });
            ui.checkbox(&mut settings.retain_queue, "remember the queue between runs")
                .on_hover_text(
                    "off: every launch starts with an empty queue (or just the file you opened)",
                );
            ui.weak("items already played (and the current one) stay put — only what is pending reorders");
            ui.add_space(4.0);

            if queue.items.is_empty() {
                ui.weak("the queue is empty — add something from the library tab");
                return;
            }
            // The first pending item has nothing to swap up into.
            let first_pending = queue.current.map_or(0, |c| c + 1);
            for (i, m) in queue.items.iter().enumerate().take(MAX_ROWS) {
                let pending = i >= first_pending;
                ui.horizontal(|ui| {
                    if Some(i) == queue.current {
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
                            // i + 2, not i + 1: `to` names the item to land
                            // in front of, so "before the one already after
                            // me" is a no-op. For the second-to-last row
                            // that index is len(), which move_pending takes
                            // to mean the end.
                            actions.push(OverlayAction::QueueReorder { from: i, to: i + 2 });
                        }
                        if ui
                            .add(egui::Button::new("✖").small())
                            .on_hover_text("remove from the queue")
                            .clicked()
                        {
                            actions.push(OverlayAction::QueueRemove { index: i });
                        }
                    });
                    // Outside the pending-only block: what an item is
                    // is worth asking whether or not it has played.
                    if ui
                        .add(egui::Button::new("i").small())
                        .on_hover_text("what is this file? (path, size, format)")
                        .clicked()
                    {
                        actions.push(OverlayAction::ItemInfo { item: m.clone() });
                    }
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
