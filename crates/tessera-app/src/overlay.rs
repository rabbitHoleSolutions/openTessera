//! On-screen overlay UI (egui): status bar, per-tile chips, and a per-tile
//! hover queue with thumbnails (Quest-immersive style: the queue appears at
//! the bottom of the tile under the cursor; clicking an entry plays it on
//! that tile).
//!
//! Visibility: the caller computes `visible` each frame (pinned via Tab, or
//! any user input within the last few seconds). Every frame a full egui pass
//! runs — with an empty UI when hidden — so egui's input state stays fresh
//! and stale widgets can never swallow clicks while the overlay is off. UI
//! interactions are recorded as [`OverlayAction`]s and executed by the
//! caller after the frame, because pause/resume must go through the global
//! budget.

use tessera_core::Rect;

use crate::thumbs::ThumbCache;

#[derive(Default)]
pub struct OverlayState {
    /// Computed each frame by the caller: pinned OR recent user input.
    pub visible: bool,
    /// Tab toggles; when true the overlay never auto-hides.
    pub pinned: bool,
    pub actions: Vec<OverlayAction>,
    /// A click awaiting its frame-time verdict: if the position lands on an
    /// egui layer, the UI owns it; otherwise it executes as a raw video-wall
    /// click. Event-time consumption can't be trusted — egui's notion of the
    /// pointer position lags one frame behind.
    pub pending_click: Option<PendingClick>,
    /// Queue strip drag in progress: the source entry's queue index.
    pub queue_drag: Option<usize>,
    /// Delete confirmation armed on a tile's transport: (tile id, armed at).
    /// Clicking the trash a second time within CONFIRM_SECS deletes the
    /// tile's current video and advances it.
    pub confirm_delete: Option<(usize, std::time::Instant)>,
    /// Transient on-screen feedback (shown a few seconds even while the
    /// rest of the overlay is hidden).
    pub notice: Option<(String, std::time::Instant)>,
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
    /// Queue entry clicked: play this item on the hovered tile now.
    PlayOnTile { tile_id: usize, index: usize },
    /// Queue entry right-clicked: remove it from the pending queue.
    QueueRemove { index: usize },
    /// Transport trash clicked twice: delete the tile's current video at its
    /// source and advance to the next queue item.
    DeleteCurrent { tile_id: usize },
    /// Transport "+": add a tile to this screen, taking the next queue item.
    AddPanel,
    /// Transport fullscreen icon / F key: zoom this tile to fill its screen
    /// (toggles back).
    FullscreenToggle { tile_id: usize },
    /// Transport "−": remove this tile from its screen (other screens are
    /// untouched).
    RemovePanel { tile_id: usize },
    /// Queue drag reorder: insert `from` before `to`.
    QueueReorder { from: usize, to: usize },
    /// Reshuffle the pending queue tail.
    QueueShuffle,
    /// Sort the pending queue tail by name.
    QueueSortName,
    PauseAll,
    ResumeAll,
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

/// One pending queue entry as shown in the hover queue.
pub struct QueueEntry {
    pub index: usize,
    pub id: String,
    pub title: String,
    pub thumb_url: Option<String>,
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

/// Thumb box the entries are fitted into (egui points).
const THUMB_MAX: egui::Vec2 = egui::Vec2::new(84.0, 56.0);
/// Placeholder size for entries without a thumbnail.
const THUMB_PLACEHOLDER: egui::Vec2 = egui::Vec2::new(74.0, 56.0);

#[allow(clippy::too_many_arguments)]
pub fn draw(
    ctx: &egui::Context,
    monitor_index: usize,
    pinned: bool,
    tiles: &[TileUiInfo],
    cells: &[Rect],
    queue: &[QueueEntry],
    budget: &BudgetUi,
    hover: Option<(usize, Rect)>,
    thumbs: &mut ThumbCache,
    actions: &mut Vec<OverlayAction>,
    queue_drag: &mut Option<usize>,
    confirm_delete: &mut Option<(usize, std::time::Instant)>,
) {
    const CONFIRM_SECS: f64 = 4.0;
    let ppp = ctx.pixels_per_point();

    // Top status bar: budget, streams, evictions, play/pause-all.
    egui::TopBottomPanel::top("tessera_status").show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.strong(format!("openTessera — screen {monitor_index}"));
            if pinned {
                ui.weak("· pinned");
            }
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

    // Empty screen: one big "+" so the wall can be repopulated.
    if tiles.is_empty() {
        egui::Area::new(egui::Id::new("tessera-empty-add"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                let btn = egui::Button::new(egui::RichText::new("+").size(64.0))
                    .min_size(egui::vec2(140.0, 140.0));
                if ui.add(btn).on_hover_text("add a tile (next from queue)").clicked() {
                    actions.push(OverlayAction::AddPanel);
                }
            });
    }

    // Per-tile info chips, anchored at each tile's top-left corner.
    for (tile, cell) in tiles.iter().zip(cells.iter()) {
        if cell.w <= 0.0 || cell.h <= 0.0 {
            continue; // hidden while another tile is zoomed
        }
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
                            // Fullscreen this tile (painter-drawn corner-bracket
                            // icon: no font dependency).
                            let (zrect, zresp) = ui.allocate_exact_size(
                                egui::vec2(20.0, 16.0),
                                egui::Sense::click(),
                            );
                            {
                                let p = ui.painter_at(zrect);
                                let col = if zresp.hovered() {
                                    egui::Color32::WHITE
                                } else {
                                    egui::Color32::from_gray(180)
                                };
                                let r = zrect.shrink(2.0);
                                let arm = 5.0;
                                let st = egui::Stroke::new(1.6, col);
                                let lt = r.left_top();
                                let rt = r.right_top();
                                let lb = r.left_bottom();
                                let rb = r.right_bottom();
                                p.line_segment([lt, egui::pos2(r.left() + arm, r.top())], st);
                                p.line_segment([lt, egui::pos2(r.left(), r.top() + arm)], st);
                                p.line_segment([rt, egui::pos2(r.right() - arm, r.top())], st);
                                p.line_segment([rt, egui::pos2(r.right(), r.top() + arm)], st);
                                p.line_segment([lb, egui::pos2(r.left() + arm, r.bottom())], st);
                                p.line_segment([lb, egui::pos2(r.left(), r.bottom() - arm)], st);
                                p.line_segment([rb, egui::pos2(r.right() - arm, r.bottom())], st);
                                p.line_segment([rb, egui::pos2(r.right(), r.bottom() - arm)], st);
                            }
                            if zresp.clicked() {
                                actions.push(OverlayAction::FullscreenToggle { tile_id: tile.id });
                            }
                            zresp.on_hover_text("fullscreen this tile (F)");
                            // Panel management: remove this tile / add one.
                            if b(ui, "−", 20.0).on_hover_text("remove this tile").clicked() {
                                actions.push(OverlayAction::RemovePanel { tile_id: tile.id });
                            }
                            if b(ui, "+", 20.0).on_hover_text("add a tile (next from queue)").clicked() {
                                actions.push(OverlayAction::AddPanel);
                            }
                            // Delete current video (click twice to confirm).
                            let armed = matches!(confirm_delete, Some((id, _)) if *id == tile.id);
                            let mut btn = egui::Button::new(
                                egui::RichText::new(if armed { "🗑?" } else { "🗑" }).small(),
                            );
                            if armed {
                                btn = btn.fill(egui::Color32::from_rgb(200, 60, 60));
                            }
                            let del = ui.add_sized([24.0, 16.0], btn);
                            if del.clicked() {
                                if armed {
                                    actions.push(OverlayAction::DeleteCurrent { tile_id: tile.id });
                                    *confirm_delete = None;
                                } else {
                                    *confirm_delete = Some((tile.id, std::time::Instant::now()));
                                }
                            }
                            del.on_hover_text(if armed {
                                "click again to DELETE this video from its source and move on"
                            } else {
                                "delete this video (file / library) and play the next"
                            });
                        });
                    });
            });
    }

    // Per-tile hover queue: anchored at the bottom of the tile the cursor is
    // over. Click a thumbnail to play it on that tile right now; right-click
    // removes it from the queue; drag reorders; shuffle / A–Z re-sort.
    if let Some((_, armed_at)) = confirm_delete {
        if armed_at.elapsed().as_secs_f64() > CONFIRM_SECS {
            *confirm_delete = None;
        }
    }
    if let Some((tile_id, cell)) = hover {
        if !queue.is_empty() {
            let strip_w = ((cell.w / ppp) - 12.0).max(140.0);
            let strip_h = THUMB_MAX.y + 18.0;
            let pos = egui::pos2(
                cell.x / ppp + 6.0,
                (cell.y + cell.h) / ppp - strip_h - 6.0,
            );
            egui::Area::new(egui::Id::new(format!("tile-queue-{tile_id}")))
                .order(egui::Order::Foreground)
                .fixed_pos(pos)
                .show(ctx, |ui| {
                    egui::Frame::none()
                        .fill(egui::Color32::from_black_alpha(185))
                        .rounding(egui::Rounding::same(6.0))
                        .inner_margin(egui::Margin::symmetric(6.0, 5.0))
                        .stroke(egui::Stroke::new(1.0, egui::Color32::from_black_alpha(120)))
                        .show(ui, |ui| {
                            ui.set_width(strip_w);
                            ui.horizontal(|ui| {
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
                                            ui.add_space(4.0);
                                        }
                                        let tex = thumbs.get(ui.ctx(), &entry.id, entry.thumb_url.as_deref());
                                        let size = match &tex {
                                            Some(t) => fit(t.size_vec2()),
                                            None => THUMB_PLACEHOLDER,
                                        };
                                        let (rect, resp) = ui.allocate_exact_size(
                                            size,
                                            egui::Sense::click_and_drag(),
                                        );
                                        let painter = ui.painter_at(rect);
                                        if let Some(t) = &tex {
                                            painter.image(
                                                t.id(),
                                                rect,
                                                egui::Rect::from_min_max(
                                                    egui::pos2(0.0, 0.0),
                                                    egui::pos2(1.0, 1.0),
                                                ),
                                                egui::Color32::WHITE,
                                            );
                                        } else {
                                            painter.rect_filled(
                                                rect,
                                                egui::Rounding::same(3.0),
                                                egui::Color32::from_black_alpha(210),
                                            );
                                            painter.text(
                                                rect.center(),
                                                egui::Align2::CENTER_CENTER,
                                                "▶",
                                                egui::FontId::proportional(18.0),
                                                egui::Color32::from_gray(170),
                                            );
                                        }
                                        // Interaction feedback: lighten + white border on hover.
                                        if resp.hovered() {
                                            painter.rect_filled(rect, egui::Rounding::same(3.0), egui::Color32::from_white_alpha(28));
                                            painter.rect_stroke(
                                                rect,
                                                egui::Rounding::same(3.0),
                                                egui::Stroke::new(2.0, egui::Color32::WHITE),
                                            );
                                        } else if pos == 0 {
                                            // Next-to-play highlight.
                                            painter.rect_stroke(
                                                rect,
                                                egui::Rounding::same(3.0),
                                                egui::Stroke::new(1.5, egui::Color32::from_rgb(120, 190, 255)),
                                            );
                                        }
                                        if resp.hovered() {
                                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                                        }
                                        if resp.clicked() {
                                            actions.push(OverlayAction::PlayOnTile { tile_id, index: entry.index });
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
                                        if let Some(from) = *queue_drag {
                                            if from != entry.index && resp.hovered() {
                                                painter.hline(
                                                    rect.left()..=rect.right(),
                                                    rect.bottom(),
                                                    egui::Stroke::new(2.0, egui::Color32::LIGHT_BLUE),
                                                );
                                            }
                                        }
                                        rects.push((entry.index, rect));
                                        resp.on_hover_text(format!(
                                            "{}\nclick: play here · right-click: remove from queue · drag: reorder",
                                            entry.title
                                        ));
                                    }
                                });
                                if let Some(from) = drop_from {
                                    *queue_drag = None;
                                    if let Some(p) = ctx.input(|i| i.pointer.hover_pos()) {
                                        for (idx, r) in &rects {
                                            if *idx != from && r.contains(p) {
                                                actions.push(OverlayAction::QueueReorder { from, to: *idx });
                                                break;
                                            }
                                        }
                                    }
                                }
                            });
                        });
                });
        }
    }
}

/// Transient feedback toast, top-center. Drawn even while the rest of the
/// overlay is hidden, fading out over the last second.
pub fn draw_notice(ctx: &egui::Context, notice: &(String, std::time::Instant)) {
    const SHOW_SECS: f64 = 4.0;
    let age = notice.1.elapsed().as_secs_f64();
    if age > SHOW_SECS {
        return;
    }
    let alpha = if age > SHOW_SECS - 1.0 {
        ((SHOW_SECS - age) as f32).clamp(0.0, 1.0)
    } else {
        1.0
    };
    egui::Area::new(egui::Id::new("tessera-notice"))
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 44.0))
        .show(ctx, |ui| {
            egui::Frame::none()
                .fill(egui::Color32::from_black_alpha((215.0 * alpha) as u8))
                .rounding(egui::Rounding::same(6.0))
                .inner_margin(egui::Margin::symmetric(12.0, 7.0))
                .stroke(egui::Stroke::new(1.0, egui::Color32::from_black_alpha((120.0 * alpha) as u8)))
                .show(ui, |ui| {
                    ui.label(egui::RichText::new(&notice.0).color(egui::Color32::from_white_alpha((255.0 * alpha) as u8)));
                });
        });
}

/// Fits a thumbnail into the thumb box, preserving aspect.
fn fit(size: egui::Vec2) -> egui::Vec2 {
    if size.x <= 0.0 || size.y <= 0.0 {
        return THUMB_PLACEHOLDER;
    }
    let s = (THUMB_MAX.x / size.x).min(THUMB_MAX.y / size.y);
    egui::vec2(size.x * s, size.y * s)
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
