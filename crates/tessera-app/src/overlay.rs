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
use crate::tile::TrackInfo;

/// Layer order for the wall's own UI: status bar, tile chips + transport,
/// hover queue strips. Deliberately *below* [`PANEL_ORDER`] — egui lifts
/// the most recently interacted area to the top within a single order, so
/// a transport row and a dialog sharing one order swap places the moment
/// you click the row, and the transport ends up painted over the dialog.
const WALL_ORDER: egui::Order = egui::Order::Middle;
/// Layer order for the modal-ish panels (help, settings): above the wall UI
/// so they can't be buried by it. NOT `Order::Tooltip` — egui never
/// hit-tests tooltip-order layers, which silently makes every control in
/// them inert.
const PANEL_ORDER: egui::Order = egui::Order::Foreground;

/// Which track row is expanded below a tile's transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackKind {
    Audio,
    Subtitle,
}

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
    /// pointer position lags one frame behind. Left presses stay parked here
    /// until release (raw click) or until the pointer moves past the drag
    /// threshold (tile drag).
    pub pending_click: Option<PendingClick>,
    /// Queue strip drag in progress: the source entry's queue index.
    pub queue_drag: Option<usize>,
    /// Same-screen tile drag in progress (drag-to-rearrange).
    pub tile_drag: Option<TileDrag>,
    /// Delete confirmation armed on a tile's transport: (tile id, armed at).
    /// Clicking the trash a second time within CONFIRM_SECS deletes the
    /// tile's current video and advances it.
    pub confirm_delete: Option<(usize, std::time::Instant)>,
    /// Transient on-screen feedback (shown a few seconds even while the
    /// rest of the overlay is hidden).
    pub notice: Option<(String, std::time::Instant)>,
    /// Tile track-picker row expanded below the transport (one at a time).
    pub track_panel: Option<(usize, TrackKind)>,
    /// Tile whose audio is soloed (all other streams muted by the solo
    /// action). Cosmetic state for the button highlight; cleared by any
    /// manual mute change.
    pub solo_id: Option<usize>,
    /// Help panel open, on this window (? or / toggles it on the focused
    /// screen). Window-scoped, not a plain bool: the panels are modal-ish
    /// singletons, and drawing a copy on every screen means several egui
    /// contexts fighting over one shared state.
    pub help_open: Option<winit::window::WindowId>,
    /// Settings panel open, on the window whose gear was clicked. See
    /// [`OverlayState::help_open`] for why this is window-scoped.
    pub settings_open: Option<winit::window::WindowId>,
}

/// App-wide runtime settings (config + sources tabs of the browser window,
/// persisted to settings.conf).
///
/// Every field here is global and persisted — there is no transient widget
/// state left to hand-exclude, so `PartialEq` is derived and the event
/// loop's `settings != settings_before` diff cannot silently miss a new
/// field. Per-window state (the UI scale and its drag draft) lives on
/// `Screen`; add-form state lives on `BrowserState`.
#[derive(Clone, Debug, PartialEq)]
pub struct UiSettings {
    /// Starting egui pixels-per-point for new windows. Each window then
    /// carries its own `Screen::ui_scale`; committing a window's scale
    /// slider updates this default too, so the next launch starts there.
    pub ui_scale: f32,
    /// Demuxer cache size in MiB per stream.
    pub cache_mb: u32,
    /// Max concurrent playing streams (budget count cap; the mbps ceiling
    /// is the real decode-load guard).
    pub max_streams: u32,
    /// Audio pipeline enabled (tiles still start muted).
    pub audio: bool,
    /// Remember the queue between runs. Off means every launch starts with
    /// an empty queue — or just the file you opened — and you fill it from
    /// the library browser.
    pub retain_queue: bool,
    /// Media sources (folders / Plex / Jellyfin). Persisted as one JSON
    /// line in settings.conf.
    pub sources: Vec<tessera_core::sources::Source>,
}

impl Default for UiSettings {
    fn default() -> UiSettings {
        UiSettings {
            ui_scale: 1.0,
            cache_mb: 512,
            max_streams: tessera_core::budget::DEFAULT_MAX_STREAMS as u32,
            audio: false,
            retain_queue: true,
            sources: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub enum OverlayAction {
    /// Click on a tile chip: pause if playing, resume (budget-aware) if paused.
    TogglePause { tile_id: usize },
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
    /// "?" key or status-bar button: show/hide the help panel.
    ToggleHelp,
    /// "W" key or status-bar window button: toggle THIS screen's window
    /// between a fullscreen wall and a normal decorated window.
    ToggleWindowMode,
    /// Gear button: show/hide the settings panel.
    ToggleSettings,
    /// Library browser: add these items to the shared queue, folded into
    /// its current order.
    QueueAdd { items: Vec<tessera_core::MediaItem> },
    /// Library browser row "+": append one item to the end of the queue
    /// without re-ordering what is already pending.
    QueueAppend { item: tessera_core::MediaItem },
    /// Queue tab: empty the whole queue (arm-then-confirm in the UI).
    QueueClear,
    /// Library browser row "▶": play this item now.
    PlayNow { item: tessera_core::MediaItem },
    /// Status-bar library button (or L / Q): open a browser window on this
    /// tab.
    NewLibraryWindow { tab: crate::browser::Tab },
    /// Status-bar "+" button: open another window, on the monitor the
    /// cursor is on, with its own GL context and its own tiles. It draws
    /// from the same shared queue as every other window.
    NewWindow,
    /// Transport fullscreen icon / F key: zoom this tile to fill its screen
    /// (toggles back).
    FullscreenToggle { tile_id: usize },
    /// Transport "−": remove this tile from its screen (other screens are
    /// untouched).
    RemovePanel { tile_id: usize },
    /// Mute button on a tile chip: toggles mute, or explains why audio is
    /// unavailable (started without --audio).
    MuteButton { tile_id: usize },
    /// Solo button on a tile chip: unmute this tile, mute every other
    /// stream.
    Solo { tile_id: usize },
    /// Transport pin button / P key: this tile takes most of the screen,
    /// the others share a bottom strip (toggles back).
    TogglePin { tile_id: usize },
    /// Volume slider dragged on a tile chip (or wheel): set absolute volume.
    SetVolume { tile_id: usize, volume: i64 },
    /// A/S button: expand (or collapse) this tile's audio/subtitle track row.
    ToggleTrackPanel { tile_id: usize, kind: TrackKind },
    /// Track row: pick an audio track (None = off).
    SetAudioTrack { tile_id: usize, track: Option<i64> },
    /// Track row: pick a subtitle track (None = off).
    SetSubtitleTrack { tile_id: usize, track: Option<i64> },
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

/// A tile drag in progress. `window_id` is the drag source; `pos` is the
/// pointer in `pos_window`'s coordinate space (window pixels, top-left
/// origin). While an X button grab routes motion to the source window,
/// `pos` runs past that window's bounds — which is exactly what the
/// cross-screen drop detection needs.
#[derive(Debug, Clone, Copy)]
pub struct TileDrag {
    pub window_id: winit::window::WindowId,
    pub pos_window: winit::window::WindowId,
    pub from_tile: usize,
    pub pos: (f32, f32),
}

/// Snapshot of one tile, taken before the egui pass (avoids borrowing the
/// Screen while egui runs).
pub struct TileUiInfo {
    pub id: usize,
    pub title: String,
    pub paused: bool,
    pub muted: bool,
    pub audio_enabled: bool,
    pub volume: i64,
    pub time_pos: f64,
    pub duration: f64,
    pub audio_tracks: Vec<TrackInfo>,
    pub subtitle_tracks: Vec<TrackInfo>,
    /// This tile is pinned (takes most of the screen).
    pub pinned: bool,
    /// This tile's audio is soloed (all other streams muted).
    pub solo: bool,
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
    window_fullscreen: bool,
    tiles: &[TileUiInfo],
    cells: &[Rect],
    queue: &[QueueEntry],
    budget: &BudgetUi,
    hover: Option<(usize, Rect)>,
    tile_drag: Option<TileDrag>,
    track_panel: Option<(usize, TrackKind)>,
    thumbs: &mut ThumbCache,
    actions: &mut Vec<OverlayAction>,
    queue_drag: &mut Option<usize>,
    confirm_delete: &mut Option<(usize, std::time::Instant)>,
    empty_hint: Option<EmptyWall>,
) {
    const CONFIRM_SECS: f64 = 4.0;
    let ppp = ctx.pixels_per_point();


    // Top status bar: budget, streams, evictions, play/pause-all.
    //
    // An Area at WALL_ORDER, deliberately NOT a TopBottomPanel: panels are
    // painted into egui's Background layer, and the click router treats
    // Background as "not UI" (egui inserts a full-screen background layer
    // every pass whether we paint one or not). As a panel, every click on a
    // status-bar button ALSO fell through to the video wall and
    // paused/resumed the tile behind the bar.
    let status_bar = egui::Area::new(egui::Id::new("tessera_status"))
        .order(WALL_ORDER)
        .anchor(egui::Align2::LEFT_TOP, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            egui::Frame::none()
                .fill(ui.style().visuals.panel_fill)
                .inner_margin(egui::Margin::symmetric(8.0, 4.0))
                .show(ui, |ui| {
                    ui.set_width((ctx.screen_rect().width() - 16.0).max(0.0));
                    // Controls are laid out FIRST (right-aligned), so they
                    // always get their space; the status text takes what is
                    // left and clips. The other order pushed the buttons off
                    // the right edge of a narrow window — the common case now
                    // that one (often tiled) window is the default.
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            // Settings toggle: painter-drawn gear — no font dependency.
                            if gear_button(ui).clicked() {
                                actions.push(OverlayAction::ToggleSettings);
                            }
                            // Windowed-mode toggle: painter-drawn "window" icon (frame
                            // with a title bar) — no font dependency.
                            {
                                let fullhint = window_fullscreen;
                                let (rect, resp) = ui.allocate_exact_size(
                                    egui::vec2(18.0, 16.0),
                                    egui::Sense::click(),
                                );
                                let p = ui.painter_at(rect);
                                let col = if resp.hovered() {
                                    egui::Color32::WHITE
                                } else {
                                    egui::Color32::from_gray(180)
                                };
                                let r = rect.shrink(2.0);
                                let bar = egui::Rect::from_min_max(r.left_top(), egui::pos2(r.right(), r.top() + 5.0));
                                p.rect_filled(bar, egui::Rounding::same(1.5), col);
                                p.rect_stroke(r, egui::Rounding::same(1.5), egui::Stroke::new(1.4, col));
                                if resp.clicked() {
                                    actions.push(OverlayAction::ToggleWindowMode);
                                }
                                let full = fullhint;
                                resp.on_hover_text(if full {
                                    "window mode (W): this screen becomes a normal window"
                                } else {
                                    "fullscreen wall (W): this window fills its screen"
                                });
                            }
                            // Library window: painter-drawn window frame with
                            // list lines in it (no font dependency).
                            {
                                let (rect, resp) = ui
                                    .allocate_exact_size(egui::vec2(18.0, 16.0), egui::Sense::click());
                                let p = ui.painter_at(rect);
                                let col = if resp.hovered() {
                                    egui::Color32::WHITE
                                } else {
                                    egui::Color32::from_gray(180)
                                };
                                let r = rect.shrink(2.0);
                                let st = egui::Stroke::new(1.4, col);
                                p.rect_stroke(r, egui::Rounding::same(1.5), st);
                                for i in 0..3 {
                                    let y = r.top() + 3.0 + i as f32 * 3.0;
                                    p.line_segment(
                                        [egui::pos2(r.left() + 3.0, y), egui::pos2(r.right() - 3.0, y)],
                                        st,
                                    );
                                }
                                if resp.clicked() {
                                    actions.push(OverlayAction::NewLibraryWindow { tab: crate::browser::Tab::Library });
                                }
                                resp.on_hover_text("browser window: library (L) and queue (Q) tabs");
                            }
                            // New window: painter-drawn window frame with a "+" in it
                            // (same no-font-dependency rule as the gear).
                            {
                                let (rect, resp) =
                                    ui.allocate_exact_size(egui::vec2(18.0, 16.0), egui::Sense::click());
                                let p = ui.painter_at(rect);
                                let col = if resp.hovered() {
                                    egui::Color32::WHITE
                                } else {
                                    egui::Color32::from_gray(180)
                                };
                                let r = rect.shrink(2.0);
                                let st = egui::Stroke::new(1.4, col);
                                p.rect_stroke(r, egui::Rounding::same(1.5), st);
                                let c = r.center();
                                p.line_segment([egui::pos2(c.x - 3.5, c.y), egui::pos2(c.x + 3.5, c.y)], st);
                                p.line_segment([egui::pos2(c.x, c.y - 3.5), egui::pos2(c.x, c.y + 3.5)], st);
                                if resp.clicked() {
                                    actions.push(OverlayAction::NewWindow);
                                }
                                resp.on_hover_text("new window (its own tiles, same queue)");
                            }
                            if ui.small_button("?").on_hover_text("keyboard & mouse help").clicked() {
                                actions.push(OverlayAction::ToggleHelp);
                            }
                            let any_playing = tiles.iter().any(|t| !t.paused);
                            let label = if any_playing { "⏸ pause all" } else { "▶ play all" };
                            if ui.small_button(label).clicked() {
                                actions.push(if any_playing { OverlayAction::PauseAll } else { OverlayAction::ResumeAll });
                            }
                            // Status text, in the space the controls left.
                            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                ui.set_clip_rect(ui.available_rect_before_wrap());
                                ui.strong(format!("Tessera — screen {monitor_index}"));
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
                            });
                        });
                    });
                });
        });
    let bar_bottom = status_bar.response.rect.bottom();

    // Empty screen: exactly one thing to do, depending on why it is empty.
    // The big "+" and an explanatory panel used to be separate centred
    // Areas, so on a first run they were painted on top of each other.
    match empty_hint {
        Some(EmptyWall::AddTile) => {
            egui::Area::new(egui::Id::new("tessera-empty-add"))
                .order(WALL_ORDER)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    let btn = egui::Button::new(egui::RichText::new("+").size(64.0))
                        .min_size(egui::vec2(140.0, 140.0));
                    if ui.add(btn).on_hover_text("add a tile (next from queue)").clicked() {
                        actions.push(OverlayAction::AddPanel);
                    }
                });
        }
        Some(why) => draw_empty_wall(ctx, why, actions),
        None => {}
    }

    // Chips are drawn for the tile under the cursor ONLY. Drawing one per
    // tile meant any mouse movement lit up the whole wall, which is
    // unreadable at a dozen tiles — and there is no such thing as acting on
    // "all of them" anyway: every control here targets one tile.
    // A tile being dragged keeps its chip even as the cursor leaves it, so
    // the thing you are moving stays identified. Zoom and pin need no case
    // of their own: their cells are what the cursor is over anyway.
    let chip_for = |id: usize| -> bool {
        hover.map(|(h, _)| h == id) == Some(true)
            || tile_drag.map(|d| d.from_tile) == Some(id)
    };
    for (tile, cell) in tiles.iter().zip(cells.iter()) {
        if cell.w <= 0.0 || cell.h <= 0.0 {
            continue; // hidden while another tile is zoomed
        }
        if !chip_for(tile.id) {
            continue;
        }
        egui::Area::new(egui::Id::new(format!("tile-chip-{}", tile.id)))
            .order(WALL_ORDER)
            .fixed_pos(egui::pos2(
                cell.x / ppp + 8.0,
                // Stay clear of the status bar when the tile is in the top row.
                (cell.y / ppp + 8.0).max(bar_bottom + 6.0),
            ))
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(egui::Color32::from_black_alpha(170))
                    .rounding(egui::Rounding::same(4.0))
                    .inner_margin(egui::Margin::symmetric(8.0, 5.0))
                    .stroke(egui::Stroke::new(1.0, egui::Color32::from_black_alpha(120)))
                    .show(ui, |ui| {
                        // As wide as the tile allows (minus the 8px inset on
                        // each side), so the scrub bar is a real scrub bar
                        // rather than the stub a fixed 460px gave on a big
                        // tile. Still bounded, so a tiny tile stays sane.
                        let avail = (cell.w / ppp - 16.0).clamp(320.0, 900.0);
                        ui.set_max_width(avail);
                        ui.set_min_width(avail.min(460.0));
                        ui.horizontal(|ui| {
                            // Track pickers (A/S) sit at the row start so their
                            // position is stable. A only when the file has >1
                            // audio track; S whenever subtitles exist.
                            {
                                let b = |ui: &mut egui::Ui, glyph: &str, open: bool, tip: &str| {
                                    let mut btn = egui::Button::new(
                                        egui::RichText::new(glyph).small(),
                                    )
                                    .min_size(egui::vec2(18.0, 16.0));
                                    if open {
                                        btn = btn.fill(egui::Color32::from_rgb(70, 110, 150));
                                    }
                                    let r = ui.add(btn);
                                    let clicked = r.clicked();
                                    r.on_hover_text(tip);
                                    clicked
                                };
                                if tile.audio_tracks.len() > 1
                                    && b(
                                        ui,
                                        "A",
                                        track_panel == Some((tile.id, TrackKind::Audio)),
                                        "audio tracks",
                                    )
                                {
                                    actions.push(OverlayAction::ToggleTrackPanel {
                                        tile_id: tile.id,
                                        kind: TrackKind::Audio,
                                    });
                                }
                                if !tile.subtitle_tracks.is_empty()
                                    && b(
                                        ui,
                                        "S",
                                        track_panel == Some((tile.id, TrackKind::Subtitle)),
                                        "subtitle tracks",
                                    )
                                {
                                    actions.push(OverlayAction::ToggleTrackPanel {
                                        tile_id: tile.id,
                                        kind: TrackKind::Subtitle,
                                    });
                                }
                            }
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
                            // Inline volume slider: shown when the tile is
                            // unmuted (or the cursor is over the tile, so a
                            // muted tile's level can be pre-set). % label
                            // alongside; live-set on drag.
                            if tile.audio_enabled {
                                let tile_hovered = ctx
                                    .input(|i| i.pointer.hover_pos())
                                    .map(|p| cell.contains(p.x * ppp, p.y * ppp))
                                    .unwrap_or(false);
                                if !tile.muted || tile_hovered {
                                    let mut vol = tile.volume.clamp(0, 100) as f32;
                                    let sresp = ui.add_sized(
                                        [64.0, 14.0],
                                        egui::Slider::new(&mut vol, 0.0..=100.0).show_value(false),
                                    );
                                    if sresp.changed() {
                                        actions.push(OverlayAction::SetVolume {
                                            tile_id: tile.id,
                                            volume: vol.round() as i64,
                                        });
                                    }
                                    ui.label(
                                        egui::RichText::new(format!("{}%", vol.round() as i64)).small().weak(),
                                    );
                                    sresp.on_hover_text("volume (scroll wheel also works)");
                                }
                            }
                            // Always-visible mute button with state.
                            {
                                let (icon, fill) = if !tile.audio_enabled {
                                    // Audio pipeline off: gray speaker.
                                    (egui::RichText::new("🔇").weak(), None)
                                } else if tile.muted {
                                    (egui::RichText::new("🔇").color(egui::Color32::from_rgb(240, 110, 110)), Some(egui::Color32::from_rgba_unmultiplied(120, 40, 40, 90)))
                                } else {
                                    (egui::RichText::new("🔊").color(egui::Color32::from_rgb(130, 220, 140)), None)
                                };
                                let mut btn = egui::Button::new(icon.small()).min_size(egui::vec2(24.0, 16.0));
                                if let Some(f) = fill {
                                    btn = btn.fill(f);
                                }
                                let resp = ui.add(btn);
                                if resp.clicked() {
                                    actions.push(OverlayAction::MuteButton { tile_id: tile.id });
                                }
                                let tip = if !tile.audio_enabled {
                                    "audio is off — start opentessera with --audio".to_string()
                                } else if tile.muted {
                                    "muted — click to unmute (right-click video also works)".to_string()
                                } else {
                                    "playing — click to mute".to_string()
                                };
                                resp.on_hover_text(tip);
                            }
                            // Solo: unmute this tile, mute every other stream
                            // (audio focus).
                            {
                                let mut btn = egui::Button::new(egui::RichText::new("🎤").small())
                                    .min_size(egui::vec2(24.0, 16.0));
                                if tile.solo {
                                    btn = btn.fill(egui::Color32::from_rgb(70, 110, 150));
                                }
                                let resp = ui.add(btn);
                                if resp.clicked() {
                                    actions.push(OverlayAction::Solo { tile_id: tile.id });
                                }
                                resp.on_hover_text("solo audio: mute every other stream");
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
                            // The scrub bar takes whatever the rest of the row
                            // leaves. TRAILING is the fixed chrome after it
                            // (time, pin, fullscreen, -, +, trash) plus the
                            // four seek buttons before it; a fixed 110px stub
                            // made seeking a two-hour film hopeless.
                            const TRAILING: f32 = 330.0;
                            let scrub_w = (avail - TRAILING).max(110.0);
                            if tile.duration > 0.0 {
                                let mut pos = tile.time_pos.clamp(0.0, tile.duration);
                                // egui draws a Slider's rail at
                                // spacing.slider_width (100 by default) and
                                // ignores whatever room add_sized gives it —
                                // so widening the row alone does nothing.
                                ui.spacing_mut().slider_width = scrub_w;
                                let sresp = ui.add(
                                    egui::Slider::new(&mut pos, 0.0..=tile.duration)
                                        .show_value(false),
                                );
                                if (sresp.clicked() || sresp.drag_stopped())
                                    && (pos - tile.time_pos).abs() > 0.5
                                {
                                    actions.push(OverlayAction::SeekTo { tile_id: tile.id, to: pos });
                                }
                            } else {
                                ui.add_space(scrub_w);
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
                            // Pin this tile: it takes most of the screen, the
                            // rest share a bottom strip (P also toggles).
                            {
                                let mut btn =
                                    egui::Button::new(egui::RichText::new("📌").small());
                                if tile.pinned {
                                    btn = btn.fill(egui::Color32::from_rgb(70, 110, 150));
                                }
                                let resp = ui.add(btn);
                                if resp.clicked() {
                                    actions.push(OverlayAction::TogglePin { tile_id: tile.id });
                                }
                                resp.on_hover_text("pin this tile: it takes most of the screen (P)");
                            }
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
                            if b(ui, "-", 20.0).on_hover_text("remove this tile").clicked() {
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
                        // Expanded track row: off + one button per track, the
                        // current selection filled. Same panel, below the
                        // transport — no popup.
                        if let Some((pid, kind)) = track_panel {
                            if pid == tile.id {
                                let (tracks, is_audio) = match kind {
                                    TrackKind::Audio => (&tile.audio_tracks, true),
                                    TrackKind::Subtitle => (&tile.subtitle_tracks, false),
                                };
                                let sel_fill = egui::Color32::from_rgb(70, 110, 150);
                                ui.horizontal(|ui| {
                                    let mut off = egui::Button::new(egui::RichText::new("off").small())
                                        .min_size(egui::vec2(30.0, 16.0));
                                    if !tracks.iter().any(|t| t.selected) {
                                        off = off.fill(sel_fill);
                                    }
                                    if ui.add(off).clicked() {
                                        actions.push(if is_audio {
                                            OverlayAction::SetAudioTrack { tile_id: tile.id, track: None }
                                        } else {
                                            OverlayAction::SetSubtitleTrack { tile_id: tile.id, track: None }
                                        });
                                    }
                                    for t in tracks {
                                        let mut btn =
                                            egui::Button::new(egui::RichText::new(t.short_label()).small());
                                        if t.selected {
                                            btn = btn.fill(sel_fill);
                                        }
                                        let r = ui.add(btn);
                                        if r.clicked() {
                                            actions.push(if is_audio {
                                                OverlayAction::SetAudioTrack {
                                                    tile_id: tile.id,
                                                    track: Some(t.id),
                                                }
                                            } else {
                                                OverlayAction::SetSubtitleTrack {
                                                    tile_id: tile.id,
                                                    track: Some(t.id),
                                                }
                                            });
                                        }
                                        r.on_hover_text(t.describe());
                                    }
                                });
                            }
                        }
                    });
            });
    }

    // Tile drag-to-rearrange feedback: the source cell gets an outline; the
    // cell under the pointer highlights as the swap target. Drawn on the
    // debug layer so it sits above every panel.
    if let Some(drag) = tile_drag {
        let painter = ctx.debug_painter();
        let accent = egui::Color32::from_rgb(120, 190, 255);
        let stroke = egui::Stroke::new(2.0, accent);
        if let Some((_, src)) = tiles.iter().zip(cells.iter()).find(|(t, _)| t.id == drag.from_tile) {
            let r = px_rect(src, ppp);
            painter.rect_stroke(r, egui::Rounding::same(4.0), stroke);
        }
        if let Some((_, cell)) = tiles.iter().zip(cells.iter()).find(|(t, c)| {
            t.id != drag.from_tile && c.w > 0.0 && c.contains(drag.pos.0, drag.pos.1)
        }) {
            let r = px_rect(cell, ppp);
            painter.rect_filled(r, egui::Rounding::same(4.0), egui::Color32::from_rgba_unmultiplied(120, 190, 255, 36));
            painter.rect_stroke(r, egui::Rounding::same(4.0), stroke);
        }
        ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
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
                .order(WALL_ORDER)
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
                                if ui.small_button("A-Z").clicked() {
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

/// The gear icon, painter-drawn rather than a glyph: egui's bundled fonts
/// have no gear, and a missing glyph renders as a silent empty box. Shared
/// by the wall's status bar and the browser's tab bar so "gear = this
/// window's own settings" means the same thing in both.
pub fn gear_button(ui: &mut egui::Ui) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(18.0, 16.0), egui::Sense::click());
    let p = ui.painter_at(rect);
    let col = if resp.hovered() {
        egui::Color32::WHITE
    } else {
        egui::Color32::from_gray(180)
    };
    let c = rect.center();
    let r = 5.0;
    let st = egui::Stroke::new(1.4, col);
    p.circle_stroke(c, r, st);
    for i in 0..8 {
        let a = i as f32 * std::f32::consts::FRAC_PI_4;
        let dir = egui::vec2(a.cos(), a.sin());
        p.line_segment([c + dir * (r + 1.0), c + dir * (r + 3.5)], st);
    }
    resp.on_hover_text("this window's settings")
}

/// The gear panel: settings that belong to THIS window and nothing else.
///
/// Everything global — sources, the decode budget, cache, audio, queue
/// retention — lives in the browser window's tabs instead (S and C). The
/// split is not cosmetic: each `Screen` runs its own egui context over one
/// shared `UiSettings`, so a global control drawn here is drawn once per
/// window, and the last window in the loop wins. Window-scoped controls
/// have no such problem, which is why this panel exists at all.
///
/// `scale` is the caller's live value for this window and `drag` its
/// uncommitted draft — both owned by the `Screen`, so no window tagging is
/// needed to keep two monitors' panels apart.
pub fn draw_settings(
    ctx: &egui::Context,
    scale: &mut f32,
    drag: &mut Option<f32>,
    default_scale: &mut f32,
    open: &mut bool,
    actions: &mut Vec<OverlayAction>,
) {
    egui::Area::new(egui::Id::new("tessera-settings"))
        .order(PANEL_ORDER)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            egui::Frame::window(ui.style())
                .fill(egui::Color32::from_black_alpha(235))
                .inner_margin(egui::Margin::symmetric(20.0, 14.0))
                .show(ui, |ui| {
                    ui.set_width(360.0);
                    ui.heading("this window");
                    ui.add_space(8.0);
                    ui.label("UI scale");
                    // The slider edits a draft while the control is held;
                    // the overlay keeps rendering at the committed `scale`
                    // until release. Re-scaling mid-drag moves the slider
                    // out from under the pointer, which makes the whole UI
                    // flail while you try to aim.
                    let held = drag.is_some();
                    let mut draft = drag.unwrap_or(*scale);
                    let slider = ui.add(
                        egui::Slider::new(&mut draft, 0.75..=3.0)
                            .step_by(0.25)
                            .text("x"),
                    );
                    if slider.dragged() || slider.is_pointer_button_down_on() {
                        *drag = Some(draft);
                    } else if held {
                        // Released: commit for this window, and make it the
                        // starting point for windows opened later (and for
                        // the next launch — this is the persisted value).
                        *drag = None;
                        *scale = draft;
                        *default_scale = draft;
                    } else if slider.changed() {
                        // Keyboard adjustment (arrows on a focused slider):
                        // no drag to wait out, commit immediately.
                        *scale = draft;
                        *default_scale = draft;
                    }
                    ui.weak("applies to this window; also becomes the default for new ones");

                    ui.add_space(12.0);
                    ui.separator();
                    ui.add_space(6.0);
                    ui.weak("sources, streams, cache, audio and queue retention are");
                    ui.weak("app-wide — they live in the browser window:");
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("sources (S)").clicked() {
                            actions.push(OverlayAction::NewLibraryWindow {
                                tab: crate::browser::Tab::Sources,
                            });
                            *open = false;
                        }
                        if ui.button("config (C)").clicked() {
                            actions.push(OverlayAction::NewLibraryWindow {
                                tab: crate::browser::Tab::Config,
                            });
                            *open = false;
                        }
                    });

                    ui.add_space(10.0);
                    // allocate_ui_with_layout, not with_layout: the latter
                    // takes all the space left in the Area, which for a
                    // short panel means a window-tall frame with the close
                    // button stranded at the bottom.
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), 22.0),
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            if ui.button("close").clicked() {
                                *open = false;
                            }
                        },
                    );
                });
        });
}

/// Why a wall window has nothing on it. A first run shows an empty grid
/// and a gear that no longer contains sources, so the wall has to say
/// where to go — otherwise the only path to adding a source is a hotkey
/// nobody has been told about yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Named for the one action each state offers, since that is what the
/// panel renders.
pub enum EmptyWall {
    /// Nothing configured at all: the first thing to do is add a source.
    AddSource,
    /// Sources exist but the queue is empty — pick something to play.
    FillQueue,
    /// There is a queue; the user just removed every tile. A plain "+" is
    /// the whole story here, so no explanation is drawn.
    AddTile,
}

fn draw_empty_wall(ctx: &egui::Context, why: EmptyWall, actions: &mut Vec<OverlayAction>) {
    egui::Area::new(egui::Id::new("tessera-empty-wall"))
        .order(WALL_ORDER)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            egui::Frame::window(ui.style())
                .fill(egui::Color32::from_black_alpha(200))
                .inner_margin(egui::Margin::symmetric(28.0, 20.0))
                .show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        match why {
                            EmptyWall::AddSource => {
                                ui.heading("nothing to play yet");
                                ui.add_space(4.0);
                                ui.weak("add a folder, a Plex server or a Jellyfin server");
                                ui.add_space(10.0);
                                if ui.button("add a source  (S)").clicked() {
                                    actions.push(OverlayAction::NewLibraryWindow {
                                        tab: crate::browser::Tab::Sources,
                                    });
                                }
                            }
                            EmptyWall::FillQueue => {
                                ui.heading("the queue is empty");
                                ui.add_space(4.0);
                                ui.weak("pick something from the library to fill the wall");
                                ui.add_space(10.0);
                                if ui.button("open the library  (L)").clicked() {
                                    actions.push(OverlayAction::NewLibraryWindow {
                                        tab: crate::browser::Tab::Library,
                                    });
                                }
                            }
                            // Handled by the plain "+" in draw(); never reaches here.
                            EmptyWall::AddTile => {}
                        }
                        ui.add_space(6.0);
                        ui.weak("? for help");
                    });
                });
        });
}

/// Keyboard/mouse help panel, centered. Drawn regardless of the overlay's
/// auto-hide state (it stays until dismissed).
pub fn draw_help(ctx: &egui::Context, open: &mut bool) {
    egui::Area::new(egui::Id::new("tessera-help"))
        .order(PANEL_ORDER)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            egui::Frame::window(ui.style())
                .fill(egui::Color32::from_black_alpha(235))
                .inner_margin(egui::Margin::symmetric(20.0, 14.0))
                .show(ui, |ui| {
                    ui.set_width(520.0);
                    ui.heading("Tessera — keyboard & mouse");
                    ui.add_space(6.0);
                    egui::Grid::new("help-keys")
                        .num_columns(2)
                        .spacing(egui::vec2(16.0, 4.0))
                        .show(ui, |ui| {
                            let keys: &[(&str, &str)] = &[
                                ("Esc", "exit fullscreen; otherwise quit"),
                                ("Tab", "pin/unpin the overlay (auto-hides after 5s idle)"),
                                ("Space", "pause/resume all tiles on this screen"),
                                ("F", "fullscreen the tile under the cursor"),
                                ("W", "toggle THIS window: fullscreen wall <-> window (on whichever monitor it is currently on — drag it, then W)"),
                                ("Left / Right", "seek -/+10s on the tile under the cursor"),
                                ("Up / Down", "volume -/+5 on unmuted tiles of this screen"),
                                ("N / Shift+P", "next / previous queue item on each tile"),
                                ("P", "pin the tile under the cursor: it takes most of the screen, the rest share a bottom strip"),
                                ("- / +", "remove tile under cursor / add a tile"),
                                ("L / Q", "browser window, library / queue tab (opens it if there isn't one)"),
                                ("S / C", "browser window, sources / config tab — everything app-wide"),
                                ("?", "toggle this help"),
                                ("gear / top bar", "this window's own settings (UI scale)"),
                                ("+ / top bar", "new window: its own tiles, drawn from the same shared queue"),
                            ];
                            for (k, d) in keys {
                                ui.strong(*k);
                                ui.label(*d);
                                ui.end_row();
                            }
                        });
                    ui.add_space(4.0);
                    ui.separator();
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("mouse").strong());
                    egui::Grid::new("help-mouse")
                        .num_columns(2)
                        .spacing(egui::vec2(16.0, 4.0))
                        .show(ui, |ui| {
                            let mouse: &[(&str, &str)] = &[
                                ("click tile", "pause/resume that tile"),
                                ("drag tile", "drop on another tile to swap; drop on another screen to move it (Esc cancels)"),
                                ("wheel on tile", "that tile's volume +/-5 (Up/Down = all unmuted tiles)"),
                                ("A / S buttons", "audio / subtitle track row (expands below the transport)"),
                                ("🎤 / 📌 buttons", "solo audio (mute all others) / pin this tile big"),
                                ("right-click tile", "mute/unmute audio (needs --audio)"),
                                ("hover tile", "show the queue strip at its bottom"),
                                ("click thumb", "play that item on the hovered tile"),
                                ("right-click thumb", "remove it from the queue"),
                                ("drag thumb", "reorder the queue"),
                                ("transport row", "⏮ ⏪ ⏩ ⏭ seek · scrub bar · ⛶ fullscreen · -/+ tiles · 🗑 delete (click twice)"),
                            ];
                            for (k, d) in mouse {
                                ui.strong(*k);
                                ui.label(*d);
                                ui.end_row();
                            }
                        });
                    ui.add_space(8.0);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("close").clicked() {
                            *open = false;
                        }
                        ui.weak("? or / also closes");
                    });
                });
        });
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
        .order(egui::Order::Tooltip)
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

/// Converts a tessera pixel-space rect to egui points.
fn px_rect(r: &Rect, ppp: f32) -> egui::Rect {
    egui::Rect::from_min_max(
        egui::pos2(r.x / ppp, r.y / ppp),
        egui::pos2((r.x + r.w) / ppp, (r.y + r.h) / ppp),
    )
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
