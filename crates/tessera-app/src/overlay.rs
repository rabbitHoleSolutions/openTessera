//! On-screen overlay UI (egui): status bar, per-tile chips, and a per-tile
//! hover queue with thumbnails (Quest-immersive style: the queue appears at
//! the bottom of the tile under the cursor; clicking an entry plays it on
//! that tile).
//!
//! Visibility: the caller computes `opacity` each frame (pinned via Tab, or
//! any user input within the last second or so, then a fade — see
//! [`chrome_opacity`]) and `visible` is just `opacity > 0`. Every frame a full egui pass
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
pub(crate) const PANEL_ORDER: egui::Order = egui::Order::Foreground;

/// Which track row is expanded below a tile's transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackKind {
    Audio,
    Subtitle,
}

#[derive(Default)]
pub struct OverlayState {
    /// Computed each frame by the caller: pinned OR recent user input,
    /// i.e. `opacity > 0`.
    pub visible: bool,
    /// How much of the wall chrome shows, 1 → 0 across the fade-out
    /// ([`chrome_opacity`]). The help and gear panels ignore it.
    pub opacity: f32,
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
    /// Points to scroll the help panel by on its next frame — the wheel
    /// and arrow keys while it is open, routed by the event loop. Negative
    /// infinity means "from the top", which is how it opens.
    pub help_scroll: f32,
    /// Settings panel open, on the window whose gear was clicked. See
    /// [`OverlayState::help_open`] for why this is window-scoped.
    pub settings_open: Option<winit::window::WindowId>,
    /// A seek being accumulated while an arrow key auto-repeats.
    pub pending_seek: Option<PendingSeek>,
    /// A file drag is hovering over this window, so it can say it will
    /// take the drop. Window-scoped for the usual reason: every screen
    /// runs its own egui pass over this one struct, and a plain bool would
    /// light up the hint on all of them.
    pub drop_target: Option<winit::window::WindowId>,
    /// The "i" panel, on the window that opened it (`InfoPanel::window`).
    /// Here rather than on a `Screen` because the tile it describes may
    /// live in another window: the browser's queue tab can open it on any
    /// tile, and the owning screen feeds it live numbers as it ticks.
    pub info: Option<crate::info_panel::InfoPanel>,
}

impl OverlayState {
    /// Opens the help panel on `win`, or closes it if it is already there.
    /// A press on another window's ? moves it. Opens at the top.
    pub fn toggle_help(&mut self, win: winit::window::WindowId) {
        self.help_open = if self.help_open == Some(win) { None } else { Some(win) };
        self.help_scroll = f32::NEG_INFINITY;
    }
}

/// A seek the user is still spelling out. Holding an arrow fires roughly 25
/// key repeats a second, and turning each into its own mpv seek stutters the
/// video through dozens of demuxer flushes (and, for remote sources, dozens
/// of fresh HTTP range requests). The deltas add up here and go out as one
/// seek when the key settles or the hold runs long — the same coalescing
/// every other player does.
///
/// Not window-scoped despite being shared state: seeking targets the tile
/// under the cursor, and there is one cursor. A second window's arrow keys
/// simply take over the accumulator, which is what changing target means.
#[derive(Debug, Clone, Copy)]
pub struct PendingSeek {
    pub tile_id: usize,
    /// Seconds still to apply, relative to the tile's current position.
    pub delta: f64,
    pub last_input: std::time::Instant,
    pub last_flush: std::time::Instant,
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
    /// Fallback *target* egui pixels-per-point for a window on a monitor
    /// with no remembered scale of its own. Each window then carries its own
    /// `Screen::ui_scale`, and per-tile controls scale down from there when
    /// the tile is too small to hold them (see [`chip_scale`]).
    pub ui_scale: f32,
    /// Remembered target UI scale per monitor, keyed on the monitor's name
    /// (`MonitorHandle::name()`, e.g. "DP-2"). A `WindowId` does not
    /// survive a restart and a monitor index shifts when displays are
    /// plugged or unplugged, so the name is the only durable handle.
    /// A `BTreeMap` rather than a `HashMap`: this is serialised on every
    /// settings save, and hash order would rewrite the line each time.
    pub monitor_scale: std::collections::BTreeMap<String, f32>,
    /// Demuxer cache size in MiB per stream.
    pub cache_mb: u32,
    /// Max concurrent playing streams (budget count cap; the decode-load
    /// ceiling is the real guard).
    pub max_streams: u32,
    /// Audio pipeline enabled (tiles still start muted).
    pub audio: bool,
    /// Pinning a tile (P) or filling the screen with it (F) solos its
    /// audio — but only while something is audible: an all-muted wall
    /// stays silent.
    pub audio_follows: bool,
    /// Remember the queue between runs. Off means every launch starts with
    /// an empty queue — or just the file you opened — and you fill it from
    /// the library browser.
    pub retain_queue: bool,
    /// Seconds without input before the wall's chrome (status bar, tile
    /// controls) starts to fade. The help and gear panels are not chrome
    /// and stay.
    pub hide_after_secs: f32,
    /// How long that fade takes, in seconds; 0 hides at once.
    pub fade_secs: f32,
    /// Media sources (folders / Plex / Jellyfin). Persisted as one JSON
    /// line in settings.conf.
    pub sources: Vec<tessera_core::sources::Source>,
}

impl Default for UiSettings {
    fn default() -> UiSettings {
        UiSettings {
            ui_scale: 1.0,
            monitor_scale: std::collections::BTreeMap::new(),
            cache_mb: 512,
            max_streams: tessera_core::budget::DEFAULT_MAX_STREAMS as u32,
            audio: true,
            audio_follows: true,
            retain_queue: true,
            hide_after_secs: 1.0,
            fade_secs: 1.5,
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
    /// Tile chip "i": show what is known about this tile's video, live
    /// playback numbers included (toggles).
    TileInfo { tile_id: usize },
    /// Library or queue row "i": show what is known about this item
    /// (toggles).
    ItemInfo { item: tessera_core::MediaItem },
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
    /// Float this window on top without a title bar, or bring it back
    /// (T, or the gear).
    ToggleFloating,
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
    /// unavailable (audio turned off: --no-audio, or the config tab).
    MuteButton { tile_id: usize },
    /// Solo button on a tile chip: unmute this tile, mute every other
    /// stream.
    Solo { tile_id: usize },
    /// Transport pin button / P key: this tile takes most of the screen,
    /// the others share a bottom strip (toggles back).
    TogglePin { tile_id: usize },
    /// "LIVE" button on a camera's transport: rejoin the stream at its edge.
    GoLive { tile_id: usize },
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
    /// Seconds of accumulated, not-yet-issued seek. The transport renders
    /// the position the user is heading for rather than the one mpv is
    /// still on, so a held arrow key reads as continuous motion even though
    /// the actual seeks are several hundred ms apart.
    pub pending_seek: f64,
    pub title: String,
    pub paused: bool,
    pub muted: bool,
    pub audio_enabled: bool,
    pub volume: i64,
    pub time_pos: f64,
    pub duration: f64,
    pub audio_tracks: Vec<TrackInfo>,
    pub subtitle_tracks: Vec<TrackInfo>,
    /// A live feed: no duration, nothing to scrub, and it can fall behind.
    pub live: bool,
    /// The feed dropped and is being retried.
    pub reconnecting: bool,
    /// Seconds behind the live edge (live tiles only).
    pub behind: f64,
    /// This tile is pinned (takes most of the screen).
    pub pinned: bool,
    /// This tile's audio is soloed (all other streams muted).
    pub solo: bool,
    /// The info panel is showing this tile.
    pub info_open: bool,
}

/// One pending queue entry as shown in the hover queue.
///
/// Carries the whole item rather than a few scraped fields: a local
/// thumbnail is fingerprinted from the path, size and mtime, and picking
/// those out one at a time only to reassemble them is worse than the clone.
/// Built only when the overlay actually draws (see `Screen::tick`).
pub struct QueueEntry {
    pub index: usize,
    pub item: tessera_core::MediaItem,
}

#[derive(Clone, Copy)]
pub struct BudgetUi {
    pub utilization: f64,
    /// Load in 1080p30-equivalent streams, and the ceiling in the same
    /// unit. NOT a bitrate — see `MACROBLOCKS_1080P30`.
    pub used_equivalents: f64,
    pub ceiling_equivalents: f64,
    pub adaptive: f64,
    pub active: usize,
    pub max_streams: usize,
    pub evictions: u64,
}

/// Thumb box the entries are fitted into (egui points).
const THUMB_MAX: egui::Vec2 = egui::Vec2::new(84.0, 56.0);

/// Everything on the transport row except the scrub bar: the four seek
/// buttons before it, plus the time, pin, fullscreen, -, + and trash after.
/// Measured rather than guessed — this used to count only the trailing half,
/// so the row was always about 110pt wider than the `avail` it was handed.
/// Harmless on a big tile; on the rightmost column the excess ran past the
/// window edge, where the Area's clip rect sliced it off.
const ROW_FIXED: f32 = 442.0;

/// Logical width the tile chip is laid out around: the fixed transport
/// controls plus a scrub bar worth having.
const CHIP_DESIGN_W: f32 = ROW_FIXED + 120.0;
/// ...and its height, for the same reason on a short, wide cell.
const CHIP_DESIGN_H: f32 = 62.0;
/// A chip may take at most this fraction of a cell's height before it stops
/// being an overlay and starts being the content.
const CHIP_MAX_H_FRACTION: f32 = 0.42;
/// Smallest a chip control may become, in *physical pixels*. The floor has
/// to be expressed this way rather than as a scale factor: at a target scale
/// of 3 a `k` of 0.3 still leaves a 14px button, perfectly hittable, while a
/// flat 0.55 floor stopped the chip shrinking long before it fit its cell —
/// which is what left the rightmost column's trash button sliced off.
const CHIP_MIN_CONTROL_PX: f32 = 14.0;
/// Nominal height of a chip control in points at k = 1.
const CHIP_CONTROL_H: f32 = 16.0;

/// Logical size the status bar is laid out around: its text and its six
/// controls, before anything has to clip.
const BAR_DESIGN_W: f32 = 720.0;
const BAR_DESIGN_H: f32 = 26.0;
/// The bar may take at most this fraction of a window's height. A single
/// row is a big share of a squat, WM-tiled window.
const BAR_MAX_H_FRACTION: f32 = 0.07;
/// Floor for window-level chrome. Higher than the per-tile floor: the status
/// bar and the panels are the things you read, not just poke at.
const WINDOW_MIN_SCALE: f32 = 0.6;

/// Logical widths the modal-ish panels are laid out around, margins included.
const HELP_DESIGN_W: f32 = 560.0;
const SETTINGS_DESIGN_W: f32 = 400.0;

/// How much to shrink window-level chrome so it fits the window.
///
/// Same idea as [`chip_scale`] one level up: the status bar and the panels
/// are sized for a window with room, and a narrow one (a WM tile, or any
/// window at a target scale of 3) does not have it. A panel that overflows
/// is worse than a small one — the settings panel used to clip off the left
/// edge of a narrow window, taking its controls with it.
///
/// `max_h_fraction` of 0 means "don't judge by height", which is what the
/// panels want: their height depends on their content.
pub(crate) fn window_scale(w_pts: f32, h_pts: f32, design_w: f32, design_h: f32, max_h_fraction: f32) -> f32 {
    let by_width = (w_pts - 16.0) / design_w;
    let by_height = if max_h_fraction > 0.0 && design_h > 0.0 {
        h_pts * max_h_fraction / design_h
    } else {
        f32::INFINITY
    };
    by_width.min(by_height).clamp(WINDOW_MIN_SCALE, 1.0)
}

/// How much to shrink a tile's controls so they fit the tile.
///
/// A twelve-tile wall gives each cell a few hundred pixels, and the chip is
/// laid out for a cell several times that — so at full size it covers the
/// video it is annotating. The whole chip is drawn at its natural size and
/// the *layer* is then scaled (`Context::set_transform_layer`), which scales
/// hit-testing with it; scaling each font and padding by hand would leave a
/// trail of literals to keep in step and get the click targets wrong.
///
/// Returns 1.0 whenever the cell is big enough, so nothing changes on the
/// sizes that were already fine.
/// The width a chip lays out in, given its cell. Never wider than the cell:
/// an `Area` is clipped to the window, so a chip that overflows its cell on
/// the rightmost column loses whatever crosses the window edge.
/// Opacity of the wall chrome `idle` seconds after the last input: solid
/// for `hide_after`, then out over `fade` on a smoothstep, which leaves the
/// first and last moments of the fade gentle instead of a linear ramp's
/// visible start and stop. Input mid-fade puts it straight back to 1 (the
/// caller resets `idle`), so there is no fade *in* to match.
pub fn chrome_opacity(idle: f32, hide_after: f32, fade: f32) -> f32 {
    let t = idle - hide_after;
    if t <= 0.0 {
        return 1.0;
    }
    if fade.is_nan() || fade <= 0.0 || t >= fade {
        return 0.0;
    }
    let x = t / fade;
    1.0 - x * x * (3.0 - 2.0 * x)
}

fn chip_width(cell_w_pts: f32) -> f32 {
    (cell_w_pts - 16.0).clamp(80.0, 900.0)
}

fn chip_scale(cell_w_pts: f32, cell_h_pts: f32, ppp: f32) -> f32 {
    // Measured against the room the chip actually gets (`chip_width`), not
    // the whole cell, or the 8pt inset on each side pushes it back over.
    let by_width = chip_width(cell_w_pts) / CHIP_DESIGN_W;
    let by_height = cell_h_pts * CHIP_MAX_H_FRACTION / CHIP_DESIGN_H;
    let floor = (CHIP_MIN_CONTROL_PX / (CHIP_CONTROL_H * ppp.max(0.1))).clamp(0.2, 1.0);
    by_width.min(by_height).clamp(floor, 1.0)
}

/// Scales a Ui's text and spacing in place. The one mechanism for shrinking
/// chrome, used by both [`chip_scale`] and [`window_scale`] callers.
///
/// egui's layer transform (`set_transform_layer`) looks like the tidier tool
/// — it scales hit-testing for free — but it cannot be used for anything
/// that can reach the edge of its window. `Area::content_ui` intersects both
/// its max_rect and its clip_rect with `screen_rect` *before* the transform
/// runs, and `Layers::drain` then transforms the clip rect along with the
/// shapes. A full-width status bar laid out at `width / k` is therefore
/// clipped back to the window and *then* shrunk, ending short of the right
/// edge with its last button sliced in half; a chip on the rightmost column
/// loses everything past the window edge the same way. Both were observed.
///
/// Style scaling has no such limit. The cost is that explicit pixel sizes —
/// painter-drawn icons, `min_size`, `add_sized` — are not style, so they
/// take the factor at their call sites.
pub(crate) fn scale_style(ui: &mut egui::Ui, k: f32) {
    if (k - 1.0).abs() < 0.01 {
        return;
    }
    let style = ui.style_mut();
    for font in style.text_styles.values_mut() {
        // A floor, or a very narrow window renders unreadable slivers.
        font.size = (font.size * k).max(7.0);
    }
    let sp = &mut style.spacing;
    sp.item_spacing *= k;
    sp.button_padding *= k;
    sp.interact_size *= k;
    sp.indent *= k;
    sp.icon_width *= k;
    sp.icon_width_inner *= k;
    sp.icon_spacing *= k;
    sp.slider_width *= k;
}

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
    opacity: f32,
    floating: bool,
) -> Option<egui::Rect> {
    const CONFIRM_SECS: f64 = 4.0;
    // A floating window's move handle: the status text's share of the bar,
    // which holds no controls. Returned in points for the caller to hit-test
    // presses against (see `Screen::floating_grab`).
    let mut move_handle = None;
    let ppp = ctx.pixels_per_point();


    // Top status bar: budget, streams, evictions, play/pause-all.
    //
    // An Area at WALL_ORDER, deliberately NOT a TopBottomPanel: panels are
    // painted into egui's Background layer, and the click router treats
    // Background as "not UI" (egui inserts a full-screen background layer
    // every pass whether we paint one or not). As a panel, every click on a
    // status-bar button ALSO fell through to the video wall and
    // paused/resumed the tile behind the bar.
    let win = ctx.screen_rect();
    let bar_k = window_scale(win.width(), win.height(), BAR_DESIGN_W, BAR_DESIGN_H, BAR_MAX_H_FRACTION);
    let status_bar = egui::Area::new(egui::Id::new("tessera_status"))
        .order(WALL_ORDER)
        .anchor(egui::Align2::LEFT_TOP, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            // Ui::set_opacity reaches every child Ui (they clone the
            // painter), so one call at the top of each wall Area fades all
            // of it. Not tooltips: egui paints those on their own layer.
            ui.set_opacity(opacity);
            egui::Frame::none()
                .fill(ui.style().visuals.panel_fill)
                .inner_margin(egui::Margin::symmetric(8.0, 4.0))
                .show(ui, |ui| {
                    // Style-scaled rather than layer-scaled: the bar spans the
                    // window, and an Area's content is clipped to the screen
                    // before any layer transform runs. See `scale_style`.
                    scale_style(ui, bar_k);
                    ui.set_width((win.width() - 16.0).max(0.0));
                    // Controls are laid out FIRST (right-aligned), so they
                    // always get their space; the status text takes what is
                    // left and clips. The other order pushed the buttons off
                    // the right edge of a narrow window — the common case now
                    // that one (often tiled) window is the default.
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            // Settings toggle: painter-drawn gear — no font dependency.
                            if gear_button(ui, bar_k).clicked() {
                                actions.push(OverlayAction::ToggleSettings);
                            }
                            // Windowed-mode toggle: painter-drawn "window" icon (frame
                            // with a title bar) — no font dependency.
                            {
                                let fullhint = window_fullscreen;
                                let (rect, resp) = ui.allocate_exact_size(
                                    egui::vec2(18.0, 16.0) * bar_k,
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
                                p.rect_stroke(r, egui::Rounding::same(1.5), egui::Stroke::new(1.4_f32, col));
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
                                    .allocate_exact_size(egui::vec2(18.0, 16.0) * bar_k, egui::Sense::click());
                                let p = ui.painter_at(rect);
                                let col = if resp.hovered() {
                                    egui::Color32::WHITE
                                } else {
                                    egui::Color32::from_gray(180)
                                };
                                let r = rect.shrink(2.0);
                                let st = egui::Stroke::new(1.4_f32, col);
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
                                    ui.allocate_exact_size(egui::vec2(18.0, 16.0) * bar_k, egui::Sense::click());
                                let p = ui.painter_at(rect);
                                let col = if resp.hovered() {
                                    egui::Color32::WHITE
                                } else {
                                    egui::Color32::from_gray(180)
                                };
                                let r = rect.shrink(2.0);
                                let st = egui::Stroke::new(1.4_f32, col);
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
                                if floating {
                                    move_handle = Some(ui.max_rect());
                                }
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
                                        "budget {:.0}% ({:.1}/{:.0} streams)",
                                        budget.utilization * 100.0,
                                        budget.used_equivalents,
                                        budget.ceiling_equivalents
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
                    ui.set_opacity(opacity);
                    let btn = egui::Button::new(egui::RichText::new("+").size(64.0))
                        .min_size(egui::vec2(140.0, 140.0));
                    if ui.add(btn).on_hover_text("add a tile (next from queue)").clicked() {
                        actions.push(OverlayAction::AddPanel);
                    }
                });
        }
        Some(why) => draw_empty_wall(ctx, why, actions, opacity),
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
        let k = chip_scale(cell.w / ppp, cell.h / ppp, ppp);
        let anchor = egui::pos2(
            cell.x / ppp + 8.0,
            // Stay clear of the status bar when the tile is in the top row.
            (cell.y / ppp + 8.0).max(bar_bottom + 6.0),
        );
        egui::Area::new(egui::Id::new(format!("tile-chip-{}", tile.id)))
            .order(WALL_ORDER)
            .fixed_pos(anchor)
            .show(ctx, |ui| {
                ui.set_opacity(opacity);
                scale_style(ui, k);
                egui::Frame::none()
                    .fill(egui::Color32::from_black_alpha(170))
                    .rounding(egui::Rounding::same(4.0 * k))
                    .inner_margin(egui::Margin::symmetric(8.0 * k, 5.0 * k))
                    .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_black_alpha(120)))
                    .show(ui, |ui| {
                        // As wide as the tile allows (minus the 8px inset on
                        // each side), so the scrub bar is a real scrub bar
                        // rather than the stub a fixed 460px gave on a big
                        // tile. Still bounded, so a tiny tile stays sane.
                        //
                        // Layout is in final coordinates (the chip is style-
                        // scaled, not layer-scaled), so this is simply the room
                        // the tile has — never more. There used to be a 320pt
                        // lower bound here, which on the rightmost column made
                        // the chip wider than its cell and ran it off the
                        // window edge, where the Area's clip rect sliced it.
                        // A cell too narrow for the controls now clips them at
                        // its own edge, which is bounded and legible.
                        let avail = chip_width(cell.w / ppp);
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
                                    .min_size(egui::vec2(18.0, 16.0) * k);
                                    if open {
                                        btn = btn.fill(egui::Color32::from_rgb(70, 110, 150));
                                    }
                                    let r = ui.add(btn);
                                    let clicked = r.clicked();
                                    r.on_hover_text(tip);
                                    clicked
                                };
                                // Plain "i", not U+2139: it reads the same
                                // and needs no font to have the glyph.
                                if b(ui, "i", tile.info_open, "what is this file? (size, path, format)") {
                                    actions.push(OverlayAction::TileInfo { tile_id: tile.id });
                                }
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
                                    // Same trap the scrub bar below documents:
                                    // egui draws the rail at
                                    // spacing.slider_width and ignores the
                                    // width add_sized allocated, so this came
                                    // out ~36pt wider than its slot and pushed
                                    // the mute and solo buttons toward the
                                    // chip edge.
                                    ui.spacing_mut().slider_width = 64.0 * k;
                                    let sresp = ui.add_sized(
                                        egui::vec2(64.0, 14.0) * k,
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
                                let mut btn = egui::Button::new(icon.small()).min_size(egui::vec2(24.0, 16.0) * k);
                                if let Some(f) = fill {
                                    btn = btn.fill(f);
                                }
                                let resp = ui.add(btn);
                                if resp.clicked() {
                                    actions.push(OverlayAction::MuteButton { tile_id: tile.id });
                                }
                                let tip = if !tile.audio_enabled {
                                    "audio is off — turn it on in the config tab (C)".to_string()
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
                                    .min_size(egui::vec2(24.0, 16.0) * k);
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
                                    egui::vec2(w, 16.0) * k,
                                    egui::Button::new(egui::RichText::new(glyph).small()),
                                )
                            };
                            if b(ui, "⏮", 22.0).clicked() {
                                actions.push(OverlayAction::Prev { tile_id: tile.id });
                            }
                            // Seeking a live feed is meaningless — mpv
                            // accepts it and does nothing — so a camera
                            // gets its transport space back instead.
                            if !tile.live {
                                if b(ui, "⏪", 22.0).clicked() {
                                    actions.push(OverlayAction::Skip { tile_id: tile.id, secs: -10.0 });
                                }
                                if b(ui, "⏩", 22.0).clicked() {
                                    actions.push(OverlayAction::Skip { tile_id: tile.id, secs: 10.0 });
                                }
                            }
                            if b(ui, "⏭", 22.0).clicked() {
                                actions.push(OverlayAction::Next { tile_id: tile.id });
                            }
                            // The scrub bar takes whatever the rest of the row
                            // leaves — a fixed stub made seeking a two-hour
                            // film hopeless on a big tile, and a stub that
                            // ignored the leading buttons overflowed a small
                            // one. See ROW_FIXED.
                            let scrub_w = (avail - ROW_FIXED * k).max(40.0 * k);
                            // Where the user is heading: mpv's position plus
                            // whatever seek is still accumulating. Duration is
                            // -1 until mpv has read the file, so the clamp has
                            // to be guarded — clamp(0.0, -1.0) panics.
                            let target = if tile.time_pos < 0.0 {
                                tile.time_pos // unknown; fmt_time renders "--:--"
                            } else if tile.duration > 0.0 {
                                (tile.time_pos + tile.pending_seek).clamp(0.0, tile.duration)
                            } else {
                                (tile.time_pos + tile.pending_seek).max(0.0)
                            };
                            if tile.live {
                                // A camera has no duration and nothing to
                                // scrub. What it has instead is a distance
                                // from now, which is the one number worth
                                // showing — and a way back.
                                const AT_EDGE: f64 = 1.5;
                                let behind = tile.behind;
                                let at_edge = behind < AT_EDGE;
                                // No leading "●": U+25CF is not in egui's
                                // bundled fonts and rendered as tofu. The
                                // colour already carries the state, and the
                                // house rule is to check a glyph before
                                // trusting it.
                                let label = if tile.reconnecting {
                                    "reconnecting…".to_owned()
                                } else if at_edge {
                                    "LIVE".to_owned()
                                } else {
                                    format!("LIVE  −{:.0}s", behind)
                                };
                                let mut btn = egui::Button::new(
                                    egui::RichText::new(label).small().color(
                                        if tile.reconnecting {
                                            egui::Color32::from_rgb(255, 220, 220)
                                        } else if at_edge {
                                            egui::Color32::from_rgb(130, 220, 140)
                                        } else {
                                            egui::Color32::from_rgb(255, 235, 200)
                                        },
                                    ),
                                );
                                if tile.reconnecting {
                                    btn = btn.fill(egui::Color32::from_rgb(150, 45, 45));
                                } else if !at_edge {
                                    btn = btn.fill(egui::Color32::from_rgb(150, 90, 30));
                                }
                                let resp = ui.add_sized(egui::vec2(96.0, 16.0) * k, btn);
                                if resp.clicked() {
                                    actions.push(OverlayAction::GoLive { tile_id: tile.id });
                                }
                                resp.on_hover_text(if tile.reconnecting {
                                    "the feed dropped — retrying; click to try now"
                                } else if at_edge {
                                    "playing live"
                                } else {
                                    "behind live — click to rejoin at the current moment"
                                });
                                ui.add_space((scrub_w - 96.0 * k).max(0.0));
                            } else if tile.duration > 0.0 {
                                let mut pos = target;
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
                                    && (pos - target).abs() > 0.5
                                {
                                    actions.push(OverlayAction::SeekTo { tile_id: tile.id, to: pos });
                                }
                            } else {
                                ui.add_space(scrub_w);
                            }
                            if !tile.live {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{} / {}",
                                        fmt_time(target),
                                        fmt_time(tile.duration)
                                    ))
                                    .small()
                                    .weak(),
                                );
                            }
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
                                egui::vec2(20.0, 16.0) * k,
                                egui::Sense::click(),
                            );
                            {
                                let p = ui.painter_at(zrect);
                                let col = if zresp.hovered() {
                                    egui::Color32::WHITE
                                } else {
                                    egui::Color32::from_gray(180)
                                };
                                let r = zrect.shrink(2.0 * k);
                                let arm = 5.0 * k;
                                let st = egui::Stroke::new(1.6 * k, col);
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
                            let del = ui.add_sized(egui::vec2(24.0, 16.0) * k, btn);
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
                                        .min_size(egui::vec2(30.0, 16.0) * k);
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
        let stroke = egui::Stroke::new(2.0_f32, accent);
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
            // Same treatment as the chip, and for the same reason.
            let k = chip_scale(cell.w / ppp, cell.h / ppp, ppp);
            let strip_w = ((cell.w / ppp) - 12.0).max(140.0 * k);
            let strip_h = (THUMB_MAX.y + 18.0) * k;
            let pos = egui::pos2(
                cell.x / ppp + 6.0,
                (cell.y + cell.h) / ppp - strip_h - 6.0,
            );
            egui::Area::new(egui::Id::new(format!("tile-queue-{tile_id}")))
                .order(WALL_ORDER)
                .fixed_pos(pos)
                .show(ctx, |ui| {
                    ui.set_opacity(opacity);
                    scale_style(ui, k);
                    egui::Frame::none()
                        .fill(egui::Color32::from_black_alpha(185))
                        .rounding(egui::Rounding::same(6.0 * k))
                        .inner_margin(egui::Margin::symmetric(6.0 * k, 5.0 * k))
                        .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_black_alpha(120)))
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
                                            ui.add_space(4.0 * k);
                                        }
                                        let tex = thumbs.get(ui.ctx(), &entry.item);
                                        let size = match &tex {
                                            Some(t) => fit(t.size_vec2(), k),
                                            None => THUMB_PLACEHOLDER * k,
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
                                                egui::Stroke::new(2.0_f32, egui::Color32::WHITE),
                                            );
                                        } else if pos == 0 {
                                            // Next-to-play highlight.
                                            painter.rect_stroke(
                                                rect,
                                                egui::Rounding::same(3.0),
                                                egui::Stroke::new(1.5_f32, egui::Color32::from_rgb(120, 190, 255)),
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
                                                    egui::Stroke::new(2.0_f32, egui::Color32::LIGHT_BLUE),
                                                );
                                            }
                                        }
                                        rects.push((entry.index, rect));
                                        resp.on_hover_text(format!(
                                            "{}\nclick: play here · right-click: remove from queue · drag: reorder",
                                            entry.item.title
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
    move_handle
}

/// The gear icon, painter-drawn rather than a glyph: egui's bundled fonts
/// have no gear, and a missing glyph renders as a silent empty box. Shared
/// by the wall's status bar and the browser's tab bar so "gear = this
/// window's own settings" means the same thing in both.
pub fn gear_button(ui: &mut egui::Ui, k: f32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(18.0, 16.0) * k, egui::Sense::click());
    let p = ui.painter_at(rect);
    let col = if resp.hovered() {
        egui::Color32::WHITE
    } else {
        egui::Color32::from_gray(180)
    };
    let c = rect.center();
    let r = 5.0 * k;
    let st = egui::Stroke::new(1.4 * k, col);
    p.circle_stroke(c, r, st);
    for i in 0..8 {
        let a = i as f32 * std::f32::consts::FRAC_PI_4;
        let dir = egui::vec2(a.cos(), a.sin());
        p.line_segment([c + dir * (r + 1.0 * k), c + dir * (r + 3.5 * k)], st);
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
/// needed to keep two monitors' panels apart. `committed` is set when the
/// user finishes a change, which is the caller's cue to remember it against
/// the monitor this window is on.
pub fn draw_settings(
    ctx: &egui::Context,
    scale: &mut f32,
    drag: &mut Option<f32>,
    committed: &mut bool,
    open: &mut bool,
    floating: bool,
    actions: &mut Vec<OverlayAction>,
) {
    let win = ctx.screen_rect();
    let k = window_scale(win.width(), win.height(), SETTINGS_DESIGN_W, 0.0, 0.0);
    egui::Area::new(egui::Id::new("tessera-settings"))
        .order(PANEL_ORDER)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            egui::Frame::window(ui.style())
                .fill(egui::Color32::from_black_alpha(235))
                .inner_margin(egui::Margin::symmetric(20.0, 14.0))
                .show(ui, |ui| {
                    scale_style(ui, k);
                    ui.set_width(360.0 * k);
                    ui.heading("this window");
                    ui.add_space(8.0);
                    ui.label("target UI scale");
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
                        // Released: commit for this window, and remember it
                        // for this monitor.
                        *drag = None;
                        *scale = draft;
                        *committed = true;
                    } else if slider.changed() {
                        // Keyboard adjustment (arrows on a focused slider):
                        // no drag to wait out, commit immediately.
                        *scale = draft;
                        *committed = true;
                    }
                    ui.weak("remembered for this monitor, so each display keeps its own");
                    ui.weak("a target, not a fixed size: a tile's controls shrink below it");
                    ui.weak("when the tile is too small to hold them at full size");

                    ui.add_space(10.0);
                    let mut float = floating;
                    if ui.checkbox(&mut float, "float on top (T)").changed() {
                        actions.push(OverlayAction::ToggleFloating);
                    }
                    ui.weak("no title bar, above every other window; drag the status");
                    ui.weak("bar to move it, and its edges to resize it");

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

fn draw_empty_wall(ctx: &egui::Context, why: EmptyWall, actions: &mut Vec<OverlayAction>, opacity: f32) {
    egui::Area::new(egui::Id::new("tessera-empty-wall"))
        .order(WALL_ORDER)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.set_opacity(opacity);
            egui::Frame::window(ui.style())
                .fill(egui::Color32::from_black_alpha(200))
                .inner_margin(egui::Margin::symmetric(28.0, 20.0))
                .show(ui, |ui| {
                    // See `draw_drop_hint`: centred layouts in an Area take
                    // the whole window unless told otherwise.
                    ui.set_width(DROP_HINT_W);
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

/// The help panel's egui id, which is also its layer's: the event loop
/// asks whether a press landed on the panel ([`pos_on_help`]) before
/// deciding that the press means "close it".
const HELP_ID: &str = "tessera-help";

/// Points to scroll the help panel per arrow press or wheel notch.
pub const HELP_LINE: f32 = 40.0;

/// Whether a window-pixel position is over the help panel.
pub fn pos_on_help(ctx: &egui::Context, x: f32, y: f32) -> bool {
    let ppp = ctx.pixels_per_point();
    ctx.layer_id_at(egui::pos2(x / ppp, y / ppp))
        == Some(egui::LayerId::new(PANEL_ORDER, egui::Id::new(HELP_ID)))
}

const KEYS: &[(&str, &str)] = &[
    ("Esc", "exit fullscreen; otherwise quit"),
    ("Tab", "pin/unpin the overlay (auto-hides when idle)"),
    ("Space", "pause/resume all tiles on this screen"),
    ("F", "fullscreen the tile under the cursor (its sound plays, the rest mute, unless all are muted)"),
    ("W", "toggle THIS window: fullscreen wall <-> window (on whichever monitor it is currently on — drag it, then W)"),
    ("T", "float THIS window on top, no title bar — drag the status bar to move it, the edges to resize"),
    ("Left / Right", "seek -/+10s on the tile under the cursor; hold to scan (the jump adds up and is applied in batches, so it doesn't stutter)"),
    ("Up / Down", "volume -/+5 on unmuted tiles of this screen"),
    ("N / Shift+P", "next / previous queue item on each tile"),
    ("P", "pin the tile under the cursor: it takes most of the screen, the rest share a bottom strip (sound follows, as F)"),
    ("- / +", "remove tile under cursor / add a tile"),
    ("L / Q", "browser window, library / queue tab (opens it if there isn't one)"),
    ("S / C", "browser window, sources / config tab — everything app-wide"),
    ("?", "toggle this help"),
    ("gear / top bar", "this window's own settings (UI scale)"),
    ("+ / top bar", "new window: its own tiles, drawn from the same shared queue"),
];

const MOUSE: &[(&str, &str)] = &[
    ("click tile", "pause/resume that tile"),
    ("drag tile", "drop on another tile to swap; drop on another screen to move it (Esc cancels)"),
    ("wheel on tile", "that tile's volume +/-5 (Up/Down = all unmuted tiles)"),
    ("i / A / S buttons", "what this file is (path, size, format, live decode) / audio / subtitle track row"),
    ("🎤 / 📌 buttons", "solo audio (mute all others) / pin this tile big"),
    ("right-click tile", "mute/unmute audio"),
    ("hover tile", "show the queue strip at its bottom"),
    ("click thumb", "play that item on the hovered tile"),
    ("right-click thumb", "remove it from the queue"),
    ("drag thumb", "reorder the queue"),
    ("transport row", "⏮ ⏪ ⏩ ⏭ seek · scrub bar · ⛶ fullscreen · -/+ tiles · 🗑 delete (click twice)"),
];

/// Keyboard/mouse help panel, centered. Drawn regardless of the overlay's
/// auto-hide state (it stays until dismissed).
///
/// It used to be a fixed 520pt grid that ran off any window shorter than
/// itself, with no visible way to scroll and nothing but ? to close it.
/// Now it fits the window — narrower means the descriptions wrap, shorter
/// means it scrolls, with the bar always showing — and while it is open it
/// owns the wheel and the arrow keys (the event loop routes them here as
/// `scroll_by`, in points). A click on it closes it; so does a click
/// anywhere else, or almost any key (see the event loop).
pub fn draw_help(ctx: &egui::Context, open: &mut bool, scroll_by: f32) {
    let win = ctx.screen_rect();
    let k = window_scale(win.width(), win.height(), HELP_DESIGN_W, 0.0, 0.0);
    // Never wider than the window: the floor in `window_scale` stops the
    // shrinking well before a narrow window is satisfied, and what is left
    // over is taken up by wrapping.
    let width = (520.0 * k).min((win.width() - 48.0).max(80.0));
    let area = egui::Area::new(egui::Id::new(HELP_ID))
        .order(PANEL_ORDER)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            egui::Frame::window(ui.style())
                .fill(egui::Color32::from_black_alpha(235))
                .inner_margin(egui::Margin::symmetric(20.0, 14.0))
                .show(ui, |ui| {
                    scale_style(ui, k);
                    ui.set_width(width);
                    ui.heading("Tessera — keyboard & mouse");
                    ui.add_space(6.0);
                    // What the frame, heading and footer take; the list
                    // gets the rest of the window.
                    let chrome = 120.0 * k;
                    let mut scroll = egui::ScrollArea::vertical()
                        .id_source("tessera-help-scroll")
                        .max_height((win.height() - chrome).max(60.0))
                        .auto_shrink([false, true])
                        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible);
                    if scroll_by != 0.0 {
                        let now = egui::scroll_area::State::load(ctx, ui.make_persistent_id("tessera-help-scroll"))
                            .map_or(0.0, |s| s.offset.y);
                        // egui clamps the far end itself once it knows
                        // the content's height.
                        scroll = scroll.vertical_scroll_offset((now + scroll_by).clamp(0.0, 1.0e7));
                    }
                    scroll.show(ui, |ui| {
                        help_rows(ui, KEYS, k);
                        ui.add_space(4.0);
                        ui.separator();
                        ui.add_space(4.0);
                        ui.label(egui::RichText::new("mouse").strong());
                        help_rows(ui, MOUSE, k);
                    });
                    ui.add_space(8.0);
                    // A row of fixed height: `with_layout` in an Area takes
                    // every pixel left in the window.
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), 20.0 * k),
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            if ui.button("close").clicked() {
                                *open = false;
                            }
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new("wheel or arrows scroll · click or any key closes").weak(),
                                )
                                .wrap(true),
                            );
                        },
                    );
                });
        });
    // A plain click anywhere on the panel closes it. A click, not a press:
    // egui reports none for a press that moved, so dragging the scroll bar
    // still scrolls.
    let r = area.response.rect;
    let clicked_here = ctx.input(|i| i.pointer.any_click() && i.pointer.interact_pos().is_some_and(|p| r.contains(p)));
    if clicked_here {
        *open = false;
    }
}

/// Key and description, the description wrapping into whatever width the
/// key column leaves.
fn help_rows(ui: &mut egui::Ui, rows: &[(&str, &str)], k: f32) {
    let key_w = 120.0 * k;
    for (key, what) in rows {
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(key_w, 0.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_width(key_w);
                    ui.add(egui::Label::new(egui::RichText::new(*key).strong()).wrap(true));
                },
            );
            ui.add(egui::Label::new(*what).wrap(true));
        });
        ui.add_space(2.0);
    }
}

/// Width the drop banner lays out in, before window scaling.
const DROP_HINT_W: f32 = 320.0;

/// "Drop it here" banner, shown while a file drag hovers the window.
/// Centred and unmissable, because the alternative is dropping a file and
/// wondering whether anything caught it.
pub fn draw_drop_hint(ctx: &egui::Context, queue_only: bool, cell: Option<Rect>) {
    let accent = egui::Color32::from_rgb(120, 190, 255);
    // With a tile to land on, the hint sits in that tile and outlines it,
    // so on a busy wall it is plain which one is about to be replaced.
    // Centred on the window it covered the corner of every tile in a 2x2.
    let target = cell.map(|c| px_rect(&c, ctx.pixels_per_point()));
    if let Some(r) = target {
        ctx.layer_painter(egui::LayerId::new(PANEL_ORDER, egui::Id::new("tessera-drop-cell")))
            .rect_stroke(r.shrink(2.0), egui::Rounding::same(4.0), egui::Stroke::new(3.0_f32, accent));
    }
    let offset = target.map_or(egui::Vec2::ZERO, |r| r.center() - ctx.screen_rect().center());
    egui::Area::new(egui::Id::new("tessera-drop-hint"))
        .order(PANEL_ORDER)
        .anchor(egui::Align2::CENTER_CENTER, offset)
        .show(ctx, |ui| {
            egui::Frame::window(ui.style())
                .fill(egui::Color32::from_black_alpha(225))
                .stroke(egui::Stroke::new(2.0_f32, accent))
                .inner_margin(egui::Margin::symmetric(28.0, 20.0))
                .show(ui, |ui| {
                    // Explicit width: `vertical_centered` claims whatever is
                    // left, and inside an Area that is the rest of the
                    // window — which drew this as a full-width band rather
                    // than a panel. Same trap `with_layout` sets (see the
                    // gear panel's trailing row).
                    ui.set_width(DROP_HINT_W);
                    ui.vertical_centered(|ui| {
                        ui.heading(if queue_only { "add to the queue" } else { "drop to play" });
                        ui.add_space(4.0);
                        ui.weak(if queue_only {
                            "dropped files are appended to the queue"
                        } else if target.is_some() {
                            "plays in this tile; anything extra joins the queue"
                        } else {
                            "plays here; anything extra joins the queue"
                        });
                    });
                });
        });
}

/// Transient feedback toast, top-center. Drawn even while the rest of the
/// overlay is hidden, fading out over the last second.
pub fn draw_notice(ctx: &egui::Context, notice: &(String, std::time::Instant)) {
    let show_secs = notice_secs(&notice.0);
    let age = notice.1.elapsed().as_secs_f64();
    if age > show_secs {
        return;
    }
    let alpha = if age > show_secs - 1.0 {
        ((show_secs - age) as f32).clamp(0.0, 1.0)
    } else {
        1.0
    };
    let win = ctx.screen_rect();
    // Along the bottom, not the top. Most notices answer a click on a tile
    // chip, and a toast dropped over the chip landed on the hover tooltip
    // of the very button that was clicked: the tooltip showed through it
    // and ran past its edge, which read as the message being clipped.
    egui::Area::new(egui::Id::new("tessera-notice"))
        .order(egui::Order::Tooltip)
        .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -24.0))
        .show(ctx, |ui| {
            egui::Frame::none()
                // Near-opaque: whatever is behind must not read as part of
                // the message.
                .fill(egui::Color32::from_black_alpha((240.0 * alpha) as u8))
                .rounding(egui::Rounding::same(6.0))
                .inner_margin(egui::Margin::symmetric(12.0, 7.0))
                .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_black_alpha((120.0 * alpha) as u8)))
                .show(ui, |ui| {
                    // A long message wraps into a block rather than
                    // spanning the window.
                    ui.set_max_width((560.0_f32).min(win.width() - 48.0).max(120.0));
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(&notice.0).color(egui::Color32::from_white_alpha((255.0 * alpha) as u8)),
                        )
                        .wrap(true),
                    );
                });
        });
}

/// How long a notice stays up: four seconds for a short one, longer for
/// one that takes longer to read (an explanation of why a delete failed is
/// not read in four), within reason.
fn notice_secs(msg: &str) -> f64 {
    (4.0 + msg.chars().count() as f64 / 25.0).min(12.0)
}

/// Converts a tessera pixel-space rect to egui points.
fn px_rect(r: &Rect, ppp: f32) -> egui::Rect {
    egui::Rect::from_min_max(
        egui::pos2(r.x / ppp, r.y / ppp),
        egui::pos2((r.x + r.w) / ppp, (r.y + r.h) / ppp),
    )
}

/// Fits a thumbnail into the thumb box, preserving aspect.
fn fit(size: egui::Vec2, k: f32) -> egui::Vec2 {
    if size.x <= 0.0 || size.y <= 0.0 {
        return THUMB_PLACEHOLDER * k;
    }
    let max = THUMB_MAX * k;
    let s = (max.x / size.x).min(max.y / size.y);
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

#[cfg(test)]
mod tests {
    use super::{chip_scale, chip_width, chrome_opacity, CHIP_CONTROL_H, CHIP_DESIGN_W, CHIP_MIN_CONTROL_PX};

    #[test]
    fn chrome_holds_then_fades_out_smoothly() {
        // Solid through the hold, gone once the fade has run.
        assert_eq!(chrome_opacity(0.0, 1.0, 1.5), 1.0);
        assert_eq!(chrome_opacity(1.0, 1.0, 1.5), 1.0);
        assert_eq!(chrome_opacity(2.5, 1.0, 1.5), 0.0);
        assert_eq!(chrome_opacity(9.0, 1.0, 1.5), 0.0);
        // Halfway through, half there; and never rising on the way down.
        assert!((chrome_opacity(1.75, 1.0, 1.5) - 0.5).abs() < 1e-6);
        let mut last = 1.0;
        for i in 0..=30 {
            let o = chrome_opacity(1.0 + i as f32 * 0.05, 1.0, 1.5);
            assert!(o <= last && (0.0..=1.0).contains(&o));
            last = o;
        }
        // No fade (or a nonsense one) is the old hard hide.
        assert_eq!(chrome_opacity(1.01, 1.0, 0.0), 0.0);
        assert_eq!(chrome_opacity(1.01, 1.0, f32::NAN), 0.0);
    }

    #[test]
    fn chip_scales_down_only_when_the_cell_is_small() {
        // A cell with room to spare is left exactly as it was — this is the
        // one property that keeps the change invisible on big walls.
        assert_eq!(chip_scale(1920.0, 1080.0, 1.0), 1.0);
        assert_eq!(chip_scale(CHIP_DESIGN_W + 16.0, 400.0, 1.0), 1.0);

        // Short but wide: scaled by height instead.
        assert!(chip_scale(1920.0, 80.0, 1.0) < 1.0);

        // The floor is a physical-pixel one, so a high target scale can shrink
        // much further than a low one before controls stop being hittable.
        let floor_at = |ppp: f32| CHIP_MIN_CONTROL_PX / (CHIP_CONTROL_H * ppp);
        assert!((chip_scale(10.0, 10.0, 3.0) - floor_at(3.0)).abs() < 0.001);
        assert!(chip_scale(10.0, 10.0, 3.0) < chip_scale(10.0, 10.0, 1.0));

        // The reported bug: a two-column wall at target scale 3 gives ~251pt
        // cells, and the chip has to fit the room it is given or its trailing
        // controls fall off the window edge.
        let cell = 251.0;
        let k = chip_scale(cell, 158.0, 3.0);
        assert!(
            CHIP_DESIGN_W * k <= chip_width(cell) + 0.5,
            "chip wants {} of {}",
            CHIP_DESIGN_W * k,
            chip_width(cell)
        );
    }

    #[test]
    fn chip_never_lays_out_wider_than_its_cell() {
        use super::chip_width;
        // The rightmost column is where this bites: an Area is clipped to the
        // window, so a chip wider than its cell loses whatever crosses the
        // edge — background intact, controls sliced off.
        for cell in [100.0f32, 200.0, 285.0, 476.0, 600.0, 1200.0, 3000.0] {
            let w = chip_width(cell) + 16.0; // + the 8pt inset each side
            assert!(w <= cell.max(96.0), "cell {cell} got chip {w}");
        }
        // Still bounded on a very wide cell, or the scrub bar becomes a
        // full-screen ruler.
        assert_eq!(chip_width(3000.0), 900.0);
    }

    #[test]
    fn window_chrome_scales_down_only_when_the_window_is_small() {
        use super::{
            window_scale, BAR_DESIGN_H, BAR_DESIGN_W, BAR_MAX_H_FRACTION, SETTINGS_DESIGN_W,
            WINDOW_MIN_SCALE,
        };

        // Roomy window: untouched.
        assert_eq!(
            window_scale(1700.0, 900.0, BAR_DESIGN_W, BAR_DESIGN_H, BAR_MAX_H_FRACTION),
            1.0
        );

        // Narrow: the status bar shrinks by width.
        let narrow = window_scale(550.0, 900.0, BAR_DESIGN_W, BAR_DESIGN_H, BAR_MAX_H_FRACTION);
        assert!(narrow < 1.0 && narrow > WINDOW_MIN_SCALE, "got {narrow}");

        // Narrower than the floor allows: the bar stops shrinking and its
        // text clips instead, which is already how it handles a tight window
        // (the controls are laid out first, so they survive).
        assert_eq!(
            window_scale(400.0, 900.0, BAR_DESIGN_W, BAR_DESIGN_H, BAR_MAX_H_FRACTION),
            WINDOW_MIN_SCALE
        );

        // Squat: one row is too big a share of the height even when wide.
        assert!(window_scale(1700.0, 200.0, BAR_DESIGN_W, BAR_DESIGN_H, BAR_MAX_H_FRACTION) < 1.0);

        // Panels pass max_h_fraction 0, so height never shrinks them — their
        // height is content, not a fixed row.
        assert_eq!(window_scale(1700.0, 120.0, SETTINGS_DESIGN_W, 0.0, 0.0), 1.0);

        // The panel that used to clip off a narrow window's edge now fits.
        let tight = window_scale(340.0, 800.0, SETTINGS_DESIGN_W, 0.0, 0.0);
        assert!(tight < 1.0, "got {tight}");
        assert!(340.0 - 16.0 >= SETTINGS_DESIGN_W * tight, "still overflows");

        assert_eq!(
            window_scale(10.0, 10.0, BAR_DESIGN_W, BAR_DESIGN_H, BAR_MAX_H_FRACTION),
            WINDOW_MIN_SCALE
        );
    }
}
