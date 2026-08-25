//! On-screen overlay UI (egui): status bar, per-tile chips, up-next strip.
//!
//! The overlay is toggled with Tab. Every frame a full egui pass runs (with
//! an empty UI when hidden) so egui's input state stays fresh and stale
//! widgets can never swallow clicks while the overlay is off. UI interactions
//! are recorded as [`OverlayAction`]s and executed by the caller after the
//! frame, because pause/resume must go through the global budget.

use tessera_core::Rect;

#[derive(Default)]
pub struct OverlayState {
    pub visible: bool,
    pub actions: Vec<OverlayAction>,
    /// A click awaiting its frame-time verdict: if the position lands on an
    /// egui layer, the UI owns it; otherwise it executes as a raw video-wall
    /// click. Event-time consumption can't be trusted — egui's notion of the
    /// pointer position lags one frame behind.
    pub pending_click: Option<PendingClick>,
    /// Queue strip drag in progress: the source entry's queue index.
    pub queue_drag: Option<usize>,
}

#[derive(Debug)]
pub enum OverlayAction {
    /// Click on a tile chip: pause if playing, resume (budget-aware) if paused.
    TogglePause { tile_id: usize },
    ToggleMute { tile_id: usize },
    /// Scrub bar released (or clicked): seek to an absolute position.
    SeekTo { tile_id: usize, to: f64 },
    /// Relative seek, e.g. +/-10s buttons.
    Skip { tile_id: usize, secs: f64 },
    /// Load the next/previous queue item on this tile.
    Next { tile_id: usize },
    Prev { tile_id: usize },
    PauseAll,
    ResumeAll,
    /// Queue strip: make this pending item load next (moves it to the front
    /// of the pending tail).
    QueueJumpNext { index: usize },
    /// Queue strip: remove this pending item from the queue.
    QueueRemove { index: usize },
    /// Queue strip: drag reorder — insert `from` before `to`.
    QueueReorder { from: usize, to: usize },
    /// Queue strip: reshuffle the pending tail.
    QueueShuffle,
    /// Queue strip: sort the pending tail by name.
    QueueSortName,
}

/// One pending queue entry as shown in the up-next strip.
pub struct QueueEntry {
    pub index: usize,
    pub title: String,
}

#[derive(Debug, Clone, Copy)]
pub struct PendingClick {
    pub window_id: winit::window::WindowId,
    pub x: f32,
    pub y: f32,
    pub right: bool,
}

/// Snapshot of one tile, taken before the egui pass (avoids borrowing the
/// Screen while egui runs).
pub struct TileUiInfo {
    pub id: usize,
    pub title: String,
    pub paused: bool,
    pub muted: bool,
    pub audio_enabled: bool,
    pub time_pos: f64,
    pub duration: f64,
}

#[derive(Clone, Copy)]
pub struct BudgetUi {
    pub utilization: f64,
    pub used_mbps: f64,
    pub ceiling_mbps: f64,
    pub adaptive: f64,
    pub active: usize,
    pub max_streams: usize,
    pub evictions: u64,
}

pub fn draw(
    ctx: &egui::Context,
    monitor_index: usize,
    tiles: &[TileUiInfo],
    cells: &[Rect],
    queue: &[QueueEntry],
    budget: &BudgetUi,
    actions: &mut Vec<OverlayAction>,
    queue_drag: &mut Option<usize>,
) {
    let ppp = ctx.pixels_per_point();

    // Top status bar: budget, streams, evictions, play/pause-all.
    egui::TopBottomPanel::top("tessera_status").show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.strong(format!("openTessera — screen {monitor_index}"));
            ui.separator();
            let color = if budget.utilization > 0.9 {
                egui::Color32::from_rgb(240, 100, 100)
            } else if budget.utilization > 0.7 {
                egui::Color32::from_rgb(240, 200, 100)
            } else {
                egui::Color32::from_rgb(140, 220, 140)
            };
            ui.colored_label(
                color,
                format!(
                    "budget {:.0}% ({:.0}/{:.0} mbps)",
                    budget.utilization * 100.0,
                    budget.used_mbps,
                    budget.ceiling_mbps
                ),
            );
            if budget.adaptive < 1.0 {
                ui.colored_label(
                    egui::Color32::from_rgb(240, 160, 100),
                    format!("adaptive ceiling {:.0}%", budget.adaptive * 100.0),
                );
            }
            ui.label(format!("streams {}/{}", budget.active, budget.max_streams));
            ui.label(format!("evictions {}", budget.evictions));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let any_playing = tiles.iter().any(|t| !t.paused);
                let label = if any_playing { "⏸ pause all" } else { "▶ play all" };
                if ui.small_button(label).clicked() {
                    actions.push(if any_playing { OverlayAction::PauseAll } else { OverlayAction::ResumeAll });
                }
            });
        });
    });

    // Bottom strip: the pending queue tail. Interactive: click an entry to
    // make it load next, right-click to remove it, drag to reorder.
    egui::TopBottomPanel::bottom("tessera_queue").show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.strong("Up next");
            if queue.is_empty() {
                ui.weak("(queue loops)");
            }
            ui.separator();
            if ui.small_button("🔀").clicked() {
                actions.push(OverlayAction::QueueShuffle);
            }
            if ui.small_button("A–Z").clicked() {
                actions.push(OverlayAction::QueueSortName);
            }
            ui.separator();
            let mut drop_from: Option<usize> = None;
            let mut rects: Vec<(usize, egui::Rect)> = Vec::new();
            egui::ScrollArea::horizontal().show(ui, |ui| {
                for (pos, entry) in queue.iter().enumerate() {
                    if pos > 0 {
                        ui.weak("·");
                    }
                    let rich = if pos == 0 {
                        egui::RichText::new(&entry.title).strong()
                    } else {
                        egui::RichText::new(&entry.title).weak()
                    };
                    let resp = ui.add(
                        egui::Label::new(rich).sense(egui::Sense::click_and_drag()),
                    );
                    if resp.clicked() {
                        actions.push(OverlayAction::QueueJumpNext { index: entry.index });
                    }
                    if resp.clicked_by(egui::PointerButton::Secondary) {
                        actions.push(OverlayAction::QueueRemove { index: entry.index });
                    }
                    if resp.dragged() {
                        *queue_drag = Some(entry.index);
                    }
                    if resp.drag_stopped() {
                        drop_from = *queue_drag;
                    }
                    // Drop-target highlight while a strip drag is live.
                    if let Some(from) = *queue_drag {
                        if from != entry.index && resp.hovered() {
                            let r = resp.rect;
                            ui.painter().hline(
                                r.left()..=r.right(),
                                r.bottom(),
                                egui::Stroke::new(2.0, egui::Color32::LIGHT_BLUE),
                            );
                        }
                    }
                    rects.push((entry.index, resp.rect));
                    resp.on_hover_text("click: play next · right-click: remove · drag: reorder");
                }
            });
            // Drag released: if the pointer is over another entry, reorder.
            if let Some(from) = drop_from {
                *queue_drag = None;
                if let Some(pos) = ctx.input(|i| i.pointer.hover_pos()) {
                    for (idx, r) in &rects {
                        if *idx != from && r.contains(pos) {
                            actions.push(OverlayAction::QueueReorder { from, to: *idx });
                            break;
                        }
                    }
                }
            }
        });
    });

    // Per-tile info chips, anchored at each tile's top-left corner.
    for (tile, cell) in tiles.iter().zip(cells.iter()) {
        egui::Area::new(egui::Id::new(format!("tile-chip-{}", tile.id)))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(cell.x / ppp + 8.0, cell.y / ppp + 8.0))
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(egui::Color32::from_black_alpha(170))
                    .rounding(egui::Rounding::same(4.0))
                    .inner_margin(egui::Margin::symmetric(8.0, 5.0))
                    .stroke(egui::Stroke::new(1.0, egui::Color32::from_black_alpha(120)))
                    .show(ui, |ui| {
                        ui.set_max_width(460.0);
                        ui.horizontal(|ui| {
                            let state = if tile.paused { "⏸" } else { "▶" };
                            let title = truncate(&tile.title, 38);
                            let color = if tile.paused {
                                egui::Color32::from_rgb(200, 120, 120)
                            } else {
                                egui::Color32::WHITE
                            };
                            let label = ui.add(
                                egui::Label::new(
                                    egui::RichText::new(format!("{state}  {title}"))
                                        .strong()
                                        .color(color),
                                )
                                .sense(egui::Sense::click()),
                            );
                            if label.hovered() {
                                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            }
                            if label.clicked() {
                                actions.push(OverlayAction::TogglePause { tile_id: tile.id });
                            }
                            if tile.audio_enabled {
                                let icon = if tile.muted { "🔇" } else { "🔊" };
                                if ui.small_button(icon).clicked() {
                                    actions.push(OverlayAction::ToggleMute { tile_id: tile.id });
                                }
                            }
                        });
                        // Transport row: prev, -10s, +10s, next, scrub, time.
                        ui.horizontal(|ui| {
                            let b = |ui: &mut egui::Ui, glyph: &str, w: f32| {
                                ui.add_sized(
                                    [w, 16.0],
                                    egui::Button::new(egui::RichText::new(glyph).small()),
                                )
                            };
                            if b(ui, "⏮", 22.0).clicked() {
                                actions.push(OverlayAction::Prev { tile_id: tile.id });
                            }
                            if b(ui, "⏪", 22.0).clicked() {
                                actions.push(OverlayAction::Skip { tile_id: tile.id, secs: -10.0 });
                            }
                            if b(ui, "⏩", 22.0).clicked() {
                                actions.push(OverlayAction::Skip { tile_id: tile.id, secs: 10.0 });
                            }
                            if b(ui, "⏭", 22.0).clicked() {
                                actions.push(OverlayAction::Next { tile_id: tile.id });
                            }
                            if tile.duration > 0.0 {
                                let mut pos = tile.time_pos.clamp(0.0, tile.duration);
                                let sresp = ui.add_sized(
                                    [110.0, 14.0],
                                    egui::Slider::new(&mut pos, 0.0..=tile.duration)
                                        .show_value(false),
                                );
                                if (sresp.clicked() || sresp.drag_stopped())
                                    && (pos - tile.time_pos).abs() > 0.5
                                {
                                    actions.push(OverlayAction::SeekTo { tile_id: tile.id, to: pos });
                                }
                            } else {
                                ui.add_space(110.0);
                            }
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} / {}",
                                    fmt_time(tile.time_pos),
                                    fmt_time(tile.duration)
                                ))
                                .small()
                                .weak(),
                            );
                        });
                    });
            });
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let cut: String = s.chars().take(max - 1).collect();
        format!("{cut}…")
    }
}

fn fmt_time(t: f64) -> String {
    if t < 0.0 {
        return "--:--".into();
    }
    let total = t as i64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}
