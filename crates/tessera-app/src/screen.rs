use std::sync::Arc;

use glow::HasContext;
use glutin::prelude::*;
use tessera_core::{DecodeBudget, PlaybackQueue, Rect, StreamSpec};
use winit::window::Window;

use crate::compositor::Compositor;
use crate::overlay::{self, BudgetUi, OverlayState, TileUiInfo, UiSettings};
use crate::thumbs::ThumbCache;
use crate::tile::{Tile, TileEvent};

/// The shared services a screen's UI draws from, none of which a `Screen`
/// can reach on its own: source listings, every wall tile there is (a
/// Screen cannot see its siblings), and the sign-in worker. Handed in from
/// the event loop each frame.
pub struct Services<'a> {
    pub catalog: &'a mut crate::catalog::SourceCatalog,
    pub wall_tiles: &'a [crate::browser::WallTile],
    pub auth: &'a mut crate::auth::Auth,
    /// Stable plex.tv device identifier for this install.
    pub plex_client_id: &'a str,
}

/// What a window is for. A library window has no tiles and no video: it
/// still owns a GL context (egui needs one) and rides the same per-screen
/// tick, render and close paths, so everything about window management —
/// creation, W, closing, drag routing — works on it unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenRole {
    /// A video wall: tiles fed from the shared queue.
    Wall,
    /// The browser: library, queue, sources and config — everything
    /// app-wide, in the one window that exists once.
    Browser,
}

pub struct Screen {
    pub window: Arc<Window>,
    pub surface: glutin::surface::Surface<glutin::surface::WindowSurface>,
    pub context: glutin::context::PossiblyCurrentContext,
    pub gl: Arc<glow::Context>,
    pub egui: egui_glow::EguiGlow,
    pub compositor: Compositor,
    pub monitor_index: usize,
    pub role: ScreenRole,
    /// Browser state, used only when `role` is Browser. Each browser
    /// window browses independently.
    pub browser: crate::browser::BrowserState,
    /// egui pixels-per-point for THIS window. Seeded from the persisted
    /// default; the gear panel edits it per window, because one scale
    /// across monitors of very different pixel densities cannot be right
    /// for all of them.
    pub ui_scale: f32,
    /// Uncommitted UI-scale slider draft while the control is held. The
    /// window that owns the panel owns the draft, so unlike the old shared
    /// field it needs no window tag to stay out of another screen's way.
    pub scale_drag: Option<f32>,
    pub tiles: Vec<Tile>,
    /// Pointer position in this window (top-left pixel origin) as of the
    /// last CursorMoved; None until the pointer has been in this window.
    /// Lives on the Screen rather than in a Vec indexed by screen
    /// position: windows are created and closed at runtime, and a parallel
    /// Vec would silently desync.
    pub cursor: Option<(f32, f32)>,
    layout: Vec<Rect>,
    /// Tile currently zoomed to fill the screen (F / transport icon).
    pub zoomed: Option<usize>,
    /// Tile currently pinned (P / transport pin button): it takes most of
    /// the screen while the other tiles share a bottom strip. Zoom (F)
    /// takes precedence over the pin while active.
    pub pinned: Option<usize>,
    /// The monitor this screen was created on; used to re-assert fullscreen
    /// when the WM displaces fullscreen windows (observed with pop-shell
    /// when another window leaves fullscreen).
    pub intended_monitor: Option<winit::monitor::MonitorHandle>,
    /// The wall wants this window fullscreen (fullscreen screens at
    /// startup, W toggles back). Windowed screens (--windowed, W toggle)
    /// are left alone by fix_displaced_walls.
    pub should_fullscreen: bool,
    /// Window geometry saved by W (window-mode toggle) before expanding to
    /// fullscreen; restored when the window becomes a window again.
    pub windowed_restore: Option<(winit::dpi::PhysicalPosition<i32>, winit::dpi::PhysicalSize<u32>)>,
    /// Window geometry saved by F (tile zoom) when IT took the window
    /// fullscreen; restored when the zoom exits. Kept separate so W and F
    /// never clobber each other's restore state.
    pub zoom_restore: Option<(winit::dpi::PhysicalPosition<i32>, winit::dpi::PhysicalSize<u32>)>,
    last_status_tp: Vec<f64>,
    pub frames_drawn: u64,
    pub thumbs: ThumbCache,
    pending_screenshot: Option<(std::path::PathBuf, String)>,
    /// Frame grabs asked for over the instance socket (MCP), served at the
    /// next tick — the only point where the right GL state exists.
    pub captures: Vec<Capture>,
}

/// An RGB8 readback in glReadPixels order: bottom row first.
pub struct RawImage {
    pub w: u32,
    pub h: u32,
    pub rgb: Vec<u8>,
}

pub enum CaptureWhat {
    /// The whole window, before or after the overlay is drawn.
    Window { overlay: bool },
    /// One tile's video, at the resolution it is decoded to.
    Tile(usize),
}

pub struct Capture {
    pub what: CaptureWhat,
    /// None if the tile is gone or has no frame yet.
    pub reply: std::sync::mpsc::Sender<Option<RawImage>>,
}

impl Screen {
    pub fn new(
        window: Arc<Window>,
        surface: glutin::surface::Surface<glutin::surface::WindowSurface>,
        context: glutin::context::PossiblyCurrentContext,
        gl: Arc<glow::Context>,
        egui: egui_glow::EguiGlow,
        monitor_index: usize,
    ) -> Screen {
        let compositor = Compositor::new(&gl);
        let size = window.inner_size();
        Screen {
            window,
            surface,
            context,
            gl,
            egui,
            compositor,
            monitor_index,
            role: ScreenRole::Wall,
            browser: crate::browser::BrowserState::default(),
            ui_scale: 1.0,
            scale_drag: None,
            tiles: Vec::new(),
            cursor: None,
            layout: Vec::new(),
            zoomed: None,
            pinned: None,
            intended_monitor: None,
            should_fullscreen: false,
            windowed_restore: None,
            zoom_restore: None,
            last_status_tp: Vec::new(),
            frames_drawn: 0,
            thumbs: ThumbCache::new(),
            pending_screenshot: None,
            captures: Vec::new(),
        }
        .with_layout(size.width as i32, size.height as i32)
    }

    /// The layout actually used for compositing and hit-testing: the grid,
    /// or — while a tile is zoomed — one full-screen cell for that tile and
    /// zero-size cells for the others, or — while a tile is pinned — one
    /// tall cell for it with the rest sharing a bottom strip.
    pub fn effective_layout(&self) -> Vec<Rect> {
        match self.zoomed {
            Some(zid) => {
                let size = self.window.inner_size();
                let (w, h) = (size.width as f32, size.height as f32);
                self.tiles
                    .iter()
                    .map(|t| {
                        if t.id == zid {
                            Rect { x: 0.0, y: 0.0, w, h }
                        } else {
                            Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 }
                        }
                    })
                    .collect()
            }
            None => match self.pinned {
                // A stale pin id (removed/moved tile) falls back to the grid.
                Some(pid) if self.tiles.iter().any(|t| t.id == pid) => {
                    let size = self.window.inner_size();
                    let (w, h) = (size.width as f32, size.height as f32);
                    let others = self.tiles.len() - 1;
                    let gap = 8.0;
                    if others == 0 {
                        return vec![Rect { x: 0.0, y: 0.0, w, h }];
                    }
                    let strip_h = (h * 0.2).clamp(96.0, 384.0);
                    let main_h = h - strip_h - gap;
                    let strip_each = (w - gap * (others - 1) as f32) / others as f32;
                    let mut cells = Vec::with_capacity(self.tiles.len());
                    let mut strip_i = 0usize;
                    for t in &self.tiles {
                        if t.id == pid {
                            cells.push(Rect { x: 0.0, y: 0.0, w, h: main_h });
                        } else {
                            cells.push(Rect {
                                x: strip_i as f32 * (strip_each + gap),
                                y: main_h + gap,
                                w: strip_each,
                                h: strip_h,
                            });
                            strip_i += 1;
                        }
                    }
                    cells
                }
                _ => self.layout.clone(),
            },
        }
    }

    fn with_layout(mut self, w: i32, h: i32) -> Screen {
        self.relayout(w, h);
        self
    }

    pub fn relayout(&mut self, w: i32, h: i32) {
        self.layout = tessera_core::layout::solve_grid(self.tiles.len(), w as f32, h as f32, 16.0 / 9.0, 8.0);
    }

    pub fn add_tile(&mut self, tile: Tile) {
        self.tiles.push(tile);
        self.last_status_tp.push(-1.0);
        let size = self.window.inner_size();
        self.relayout(size.width as i32, size.height as i32);
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        let (nw, nh) = (std::num::NonZeroU32::new(w), std::num::NonZeroU32::new(h));
        if let (Some(w), Some(h)) = (nw, nh) {
            self.surface.resize(&self.context, w, h);
        }
        self.relayout(w as i32, h as i32);
    }

    /// One render pass: poll tile events, render new frames into tile FBOs,
    /// composite to the window, draw the overlay, swap. Handles queue
    /// advancement on EOF.
    pub fn tick(
        &mut self,
        budget: &mut DecodeBudget,
        queue: &mut PlaybackQueue,
        services: &mut Services,
        advance: bool,
        overlay: &mut OverlayState,
        settings: &mut UiSettings,
    ) {
        self.context.make_current(&self.surface).ok();

        let mut ended: Vec<usize> = Vec::new();
        for tile in self.tiles.iter_mut() {
            for ev in tile.tick(&self.gl) {
                match ev {
                    TileEvent::EndOfFile => ended.push(tile.id),
                    TileEvent::Reconnected => {
                        println!(
                            "screen {} tile {}: \"{}\" is back",
                            self.monitor_index, tile.id, tile.item.title
                        );
                        overlay.notice = Some((
                            format!("{}: reconnected", tile.item.title),
                            std::time::Instant::now(),
                        ));
                    }
                    TileEvent::Failed { reason } => {
                        // Said out loud, not just logged: the tile has gone
                        // blank and the user needs to know it is the stream
                        // that failed, not the app. A camera whose URL is
                        // wrong is otherwise indistinguishable from one that
                        // is simply slow.
                        println!(
                            "screen {} tile {}: {reason} — \"{}\"",
                            self.monitor_index, tile.id, tile.item.title
                        );
                        overlay.notice = Some((
                            format!("{}: {reason}", tile.item.title),
                            std::time::Instant::now(),
                        ));
                    }
                    // Zero-size means mpv has not seen a frame yet (live
                    // streams, mostly). Admitting on that would weigh the
                    // stream at nothing, so the placeholder stands until
                    // the real size arrives with the first frame.
                    TileEvent::Loaded { width, height, .. } if width == 0 || height == 0 => {
                        println!(
                            "screen {} tile {}: opened, size not yet known — holding the budget estimate",
                            self.monitor_index, tile.id
                        );
                    }
                    TileEvent::Loaded { width, height, fps, codec } => {
                        let spec = StreamSpec {
                            width,
                            height,
                            fps,
                            codec: tessera_core::Codec::from_name(&codec),
                        };
                        let util = budget.update(tile.id, spec);
                        println!(
                            "screen {} tile {}: loaded {}x{}@{:.0} {} (budget {:.0}%)",
                            self.monitor_index,
                            tile.id,
                            width,
                            height,
                            fps,
                            codec,
                            util * 100.0
                        );
                    }
                }
            }
        }

        if advance {
            for id in ended {
                if let Some(next) = queue.advance() {
                    let next = next.clone();
                    if let Some(tile) = self.tiles.iter_mut().find(|t| t.id == id) {
                        tile.item = next.clone();
                        if let Err(e) = tile.load(&next) {
                            eprintln!("tile {}: load failed: {e}", tile.id);
                        }
                    }
                }
            }
        }

        let size = self.window.inner_size();
        let (w, h) = (size.width as i32, size.height as i32);
        unsafe {
            self.gl.disable(glow::SCISSOR_TEST);
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.gl.viewport(0, 0, w, h);
            self.gl.clear_color(0.04, 0.04, 0.05, 1.0);
            self.gl.clear(glow::COLOR_BUFFER_BIT);
        }
        // Solved once and reused: the compositor, the hover hit-test and
        // the overlay all want the same cells, and each call rebuilt (and
        // reallocated) the Vec.
        let cells = self.effective_layout();
        for (tile, cell) in self.tiles.iter_mut().zip(cells.iter()) {
            if cell.w <= 0.0 || cell.h <= 0.0 {
                continue; // hidden while another tile is zoomed
            }
            if let Some(tex) = tile.texture() {
                let aspect = tile.video_aspect();
                self.compositor.draw(&self.gl, tex, cell, aspect, w, h);
            }
        }
        let dump_overlay = std::env::var("TESSERA_DUMP_OVERLAY").map(|v| v != "0").unwrap_or(false);
        if !dump_overlay {
            self.take_screenshot();
        }
        self.serve_captures(false);
        // Overlay (egui). A full pass runs every frame — with an empty UI when
        // hidden — so egui input state stays fresh and stale widgets can't
        // swallow clicks. Painted after the screenshot read so dumps stay
        // pure video.
        let monitor_index = self.monitor_index;
        // Everything below feeds `overlay::draw`, so it is built only when
        // that will run: a wall window whose chrome is on screen. The
        // overlay auto-hides after five seconds of no input, which is the
        // steady state, and a browser window never draws any of it.
        let draws_overlay = self.role == ScreenRole::Wall && overlay.visible;
        let hover = if !draws_overlay
            || overlay.tile_drag.map(|d| d.window_id) == Some(self.window.id())
        {
            // Hide the hover queue strip while rearranging tiles on this
            // screen: it would sit right where the user is dropping.
            None
        } else {
            self.cursor.and_then(|(x, y)| {
                self.tiles
                    .iter()
                    .zip(cells.iter())
                    .find(|(_, cell)| cell.contains(x, y))
                    .map(|(t, cell)| (t.id, *cell))
            })
        };
        // The chip — the only consumer of the track lists — is drawn for the
        // hovered or dragged tile alone, so every other tile can carry empty
        // Vecs instead of a deep clone of both track lists every frame.
        let chip_tile = hover
            .map(|(id, _)| id)
            .or_else(|| overlay.tile_drag.filter(|d| d.window_id == self.window.id()).map(|d| d.from_tile));
        let tiles_ui: Vec<TileUiInfo> = if !draws_overlay {
            Vec::new()
        } else {
            self.tiles
                .iter()
                .map(|t| TileUiInfo {
                    id: t.id,
                    pending_seek: match overlay.pending_seek {
                        Some(ps) if ps.tile_id == t.id => ps.delta,
                        _ => 0.0,
                    },
                    title: t.item.title.clone(),
                    paused: t.is_paused(),
                    muted: t.is_muted(),
                    audio_enabled: t.audio_enabled(),
                    volume: t.volume(),
                    time_pos: t.last_time_pos,
                    duration: t.duration,
                    audio_tracks: if chip_tile == Some(t.id) { t.audio_tracks.clone() } else { Vec::new() },
                    subtitle_tracks: if chip_tile == Some(t.id) { t.subtitle_tracks.clone() } else { Vec::new() },
                    live: t.item.is_live(),
                    reconnecting: t.reconnecting(),
                    // Only asked of live tiles, and only while the overlay
                    // is drawing — an mpv property read per tile per frame
                    // is exactly what the snapshot gating exists to avoid.
                    behind: if t.item.is_live() { t.behind_live() } else { 0.0 },
                    pinned: self.pinned == Some(t.id),
                    solo: overlay.solo_id == Some(t.id),
                })
                .collect()
        };
        let queue_entries: Vec<overlay::QueueEntry> = if !draws_overlay {
            Vec::new()
        } else {
            queue
                .pending(12)
                .iter()
                .map(|(i, m)| overlay::QueueEntry { index: *i, item: (*m).clone() })
                .collect()
        };
        let tile_drag = overlay.tile_drag.filter(|d| d.window_id == self.window.id());
        let budget_ui = BudgetUi {
            utilization: budget.utilization(),
            used_equivalents: budget.used_equivalents(),
            ceiling_equivalents: budget.ceiling_equivalents(),
            adaptive: budget.adaptive(),
            active: budget.active_count(),
            max_streams: budget.max_streams(),
            evictions: budget.total_evictions(),
        };
        let visible = overlay.visible;
        let pinned = overlay.pinned;
        let mut queue_drag = overlay.queue_drag;
        let mut confirm_delete = overlay.confirm_delete;
        let mut actions = Vec::new();
        let pending_here = overlay
            .pending_click
            .filter(|pc| pc.window_id == self.window.id());
        let mut click_on_ui = false;
        let window = self.window.clone();
        let window_id = window.id();
        // Split borrows: the egui pass closure needs the thumbnail cache
        // while the EguiGlow itself is borrowed by run().
        let egui = &mut self.egui;
        let thumbs = &mut self.thumbs;
        let browser_state = &mut self.browser;
        let role = self.role;
        let mut ui_scale = self.ui_scale;
        let mut scale_drag = self.scale_drag;
        let mut scale_committed = false;
        // An empty wall is a dead end unless it says where to go: with the
        // sources form now in another window, a first run has no visible
        // path to adding one.
        let empty_hint = if role != ScreenRole::Wall || !self.tiles.is_empty() {
            None
        } else if settings.sources.is_empty() {
            Some(overlay::EmptyWall::AddSource)
        } else if queue.items().is_empty() {
            Some(overlay::EmptyWall::FillQueue)
        } else {
            Some(overlay::EmptyWall::AddTile)
        };
        // The browser only reads the queue; taking a view keeps the queue
        // itself out of the egui closure, which already borrows plenty.
        let queue_view = crate::browser::QueueView {
            items: queue.items(),
            current: queue.current_index(),
        };
        let notice = overlay.notice.clone();
        // The help/settings panels live on exactly one screen — the one
        // that opened them. Every screen used to draw its own copy against
        // the same shared state, which left several egui contexts fighting
        // over one set of widgets.
        let drop_hint = overlay.drop_target == Some(window_id);
        let mut help_open = overlay.help_open == Some(window_id);
        let mut settings_open = overlay.settings_open == Some(window_id);
        let window_fullscreen = crate::platform::is_fullscreen(&self.window);
        egui.run(&window, |ctx| {
            // Scale the whole overlay UI (zoom multiplies the native ppp
            // and persists across frames). `ui_scale` only moves when the
            // scale slider is released, so this is a no-op mid-drag.
            // Whether it is absolute or relative to the display's own
            // scale is the platform's call (Retina).
            crate::platform::apply_ui_scale(ctx, ui_scale);
            if role == ScreenRole::Browser {
                // A browser window has no wall to overlay, and its UI must
                // not auto-hide the way the wall chrome does.
                let mut bctx = crate::browser::BrowserCtx {
                    catalog: services.catalog,
                    thumbs,
                    auth: services.auth,
                    plex_client_id: services.plex_client_id,
                    wall: services.wall_tiles,
                };
                crate::browser::draw(ctx, browser_state, settings, &queue_view, &mut bctx, &mut actions);
            } else if visible {
                overlay::draw(ctx, monitor_index, pinned, window_fullscreen, &tiles_ui, &cells, &queue_entries, &budget_ui, hover, tile_drag, overlay.track_panel, thumbs, &mut actions, &mut queue_drag, &mut confirm_delete, empty_hint);
            }
            if help_open {
                overlay::draw_help(ctx, &mut help_open);
            }
            if settings_open {
                overlay::draw_settings(ctx, &mut ui_scale, &mut scale_drag, &mut scale_committed, &mut settings_open, &mut actions);
            }
            if drop_hint {
                overlay::draw_drop_hint(ctx, role == ScreenRole::Browser);
            }
            if let Some(n) = &notice {
                overlay::draw_notice(ctx, n);
            }
            if let Some(pc) = pending_here {
                // Frame-fresh verdict with the exact click position: if any
                // painted layer owns it, the raw click is dropped (widgets
                // respond via their own actions instead). The implicit
                // full-screen Background layer does NOT count — egui adds it
                // every pass whether we painted a central panel or not.
                let ppp = ctx.pixels_per_point();
                click_on_ui = matches!(
                    ctx.layer_id_at(egui::pos2(pc.x / ppp, pc.y / ppp)),
                    Some(layer) if layer.order != egui::Order::Background
                );
            }
        });
        self.ui_scale = ui_scale;
        self.scale_drag = scale_drag;
        if scale_committed {
            // Keyed on the monitor the window is on *now*, not the one it
            // was created on: drag a window to another display and adjust
            // it there, and it is that display you meant.
            if let Some(name) = self.window.current_monitor().and_then(|m| m.name()) {
                settings.monitor_scale.insert(name, ui_scale);
            } else {
                // No name to key on (rare; some backends give none) —
                // fall back to moving the global default so the change is
                // not silently forgotten.
                settings.ui_scale = ui_scale;
            }
        }
        overlay.queue_drag = queue_drag;
        // Only the owning screen may close a panel (its ✕ button); the
        // others never drew it and have nothing to say about it.
        if !help_open && overlay.help_open == Some(window_id) {
            overlay.help_open = None;
        }
        if !settings_open && overlay.settings_open == Some(window_id) {
            overlay.settings_open = None;
        }
        overlay.confirm_delete = confirm_delete;
        if click_on_ui {
            overlay.pending_click = None;
        }
        egui.paint(&window);
        overlay.actions.append(&mut actions);
        unsafe {
            // egui's painter may leave scissor enabled; don't let it clip the
            // next frame's clears and draws.
            self.gl.disable(glow::SCISSOR_TEST);
        }
        if dump_overlay {
            self.take_screenshot();
        }
        self.serve_captures(true);
        self.surface.swap_buffers(&self.context).ok();
        self.frames_drawn += 1;
    }

    /// Answers the captures that can be taken at this point of the frame:
    /// before the overlay, a bare window and any tile; after it, a window
    /// with the overlay.
    fn serve_captures(&mut self, overlay_drawn: bool) {
        let mut keep = Vec::new();
        for c in std::mem::take(&mut self.captures) {
            let image = match c.what {
                CaptureWhat::Window { overlay } if overlay == overlay_drawn => Some(self.read_window()),
                CaptureWhat::Tile(id) if !overlay_drawn => {
                    Some(self.tiles.iter().find(|t| t.id == id).and_then(|t| t.read_fbo(&self.gl)))
                }
                _ => None,
            };
            match image {
                Some(img) => {
                    let _ = c.reply.send(img);
                }
                None => keep.push(c),
            }
        }
        self.captures = keep;
    }

    fn read_window(&self) -> Option<RawImage> {
        let size = self.window.inner_size();
        let (w, h) = (size.width, size.height);
        if w == 0 || h == 0 {
            return None;
        }
        let mut rgb = vec![0u8; (w as usize) * (h as usize) * 3];
        unsafe {
            self.gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
            self.gl.read_pixels(0, 0, w as i32, h as i32, glow::RGB, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(&mut rgb));
        }
        Some(RawImage { w, h, rgb })
    }

    /// Reads back the composite frame (plus per-tile FBOs) if a screenshot
    /// was requested. With TESSERA_DUMP_OVERLAY the read happens after the
    /// egui pass, so the dump shows the overlay as well.
    fn take_screenshot(&mut self) {
        let Some((dir, tag)) = self.pending_screenshot.take() else {
            return;
        };
        let size = self.window.inner_size();
        let (w, h) = (size.width as i32, size.height as i32);
        let mut buf = vec![0u8; (w as usize) * (h as usize) * 3];
        unsafe {
            self.gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
            self.gl.read_pixels(0, 0, w, h, glow::RGB, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(&mut buf));
        }
        let path = dir.join(format!("screen{}-{}.png", self.monitor_index, tag));
        match crate::png_util::write_png_rgb(&path, w, h, &buf) {
            Ok(()) => println!("screenshot: {}", path.display()),
            Err(e) => eprintln!("screenshot failed: {e}"),
        }
        for tile in self.tiles.iter() {
            let path = dir.join(format!("screen{}-tile{}-{}.png", self.monitor_index, tile.id, tag));
            if let Err(e) = tile.dump_fbo(&self.gl, &path) {
                eprintln!("tile fbo dump failed: {e}");
            } else {
                println!("screenshot: {}", path.display());
            }
        }
    }

    /// Requests a composited-frame screenshot (taken at the next tick).
    pub fn request_screenshot(&mut self, dir: std::path::PathBuf, tag: &str) {
        self.pending_screenshot = Some((dir, tag.to_string()));
    }

    /// Removes a tile from this screen: frees its GL resources and mpv
    /// instance, drops its status slot, and re-solves the grid so the
    /// remaining tiles fill the screen.
    pub fn remove_tile(&mut self, id: usize) -> bool {
        let Some(pos) = self.tiles.iter().position(|t| t.id == id) else {
            return false;
        };
        self.context.make_current(&self.surface).ok();
        let mut tile = self.tiles.remove(pos);
        tile.destroy_gl(&self.gl);
        self.last_status_tp.remove(pos);
        let size = self.window.inner_size();
        self.relayout(size.width as i32, size.height as i32);
        true
    }

    /// Swaps the grid cells of two tiles (same-screen drag-to-rearrange).
    /// Layout derives from the Vec order, so swapping entries swaps cells;
    /// FBOs belong to the tiles and move with them. The status-rate slots
    /// swap too so they keep tracking the same tiles.
    pub fn swap_tiles(&mut self, a: usize, b: usize) -> bool {
        let Some(pa) = self.tiles.iter().position(|t| t.id == a) else {
            return false;
        };
        let Some(pb) = self.tiles.iter().position(|t| t.id == b) else {
            return false;
        };
        if pa == pb {
            return false;
        }
        self.tiles.swap(pa, pb);
        self.last_status_tp.swap(pa, pb);
        true
    }

    /// Hit-tests the cursor (top-left pixel origin) and returns the tile id.
    pub fn tile_at(&self, x: f32, y: f32) -> Option<usize> {
        self.tiles
            .iter()
            .zip(self.effective_layout().iter())
            .find(|(_, cell)| cell.contains(x, y))
            .map(|(tile, _)| tile.id)
    }

    /// Pauses the tile with the given id. Returns false if not on this screen.
    pub fn pause_tile(&mut self, id: usize) -> bool {
        if let Some(tile) = self.tiles.iter_mut().find(|t| t.id == id) {
            tile.set_paused(true);
            true
        } else {
            false
        }
    }

    /// Resumes the tile with the given id (caller has already secured budget).
    pub fn resume_tile(&mut self, id: usize) -> bool {
        if let Some(tile) = self.tiles.iter_mut().find(|t| t.id == id) {
            tile.set_paused(false);
            true
        } else {
            false
        }
    }

    /// Right-click: toggles audio on the tile under the cursor. Returns the
    /// tile id if one was hit.
    pub fn click_mute(&mut self, x: f32, y: f32) -> Option<usize> {
        let cells = self.effective_layout();
        for (tile, cell) in self.tiles.iter_mut().zip(cells.iter()) {
            if cell.contains(x, y) {
                if !tile.audio_enabled() {
                    println!(
                        "screen {} tile {}: audio disabled (turn it on in the config tab, C)",
                        self.monitor_index, tile.id
                    );
                    return Some(tile.id);
                }
                let muted = tile.toggle_mute();
                println!(
                    "screen {} tile {}: {}",
                    self.monitor_index,
                    tile.id,
                    if muted { "muted".to_string() } else { format!("audio on (vol {}%)", tile.volume()) }
                );
                return Some(tile.id);
            }
        }
        None
    }

    /// Adjusts volume on every unmuted tile of this screen.
    pub fn adjust_volume(&mut self, delta: i64) {
        for tile in self.tiles.iter_mut() {
            if tile.audio_enabled() && !tile.is_muted() {
                tile.set_volume(tile.volume() + delta);
                println!("screen {} tile {}: vol {}%", self.monitor_index, tile.id, tile.volume());
            }
        }
    }

    pub fn status(&mut self, dt: f64) {
        let mut parts = Vec::new();
        for (i, tile) in self.tiles.iter_mut().enumerate() {
            let tp = tile.last_time_pos;
            let rate = if self.last_status_tp[i] >= 0.0 && tp >= 0.0 {
                ((tp - self.last_status_tp[i]).max(0.0) / dt * 100.0).round() / 100.0
            } else {
                0.0
            };
            self.last_status_tp[i] = tp;
            let fps = tile.frames as f64 / dt;
            tile.frames = 0;
            parts.push(format!("  tile {} {:>5.2}x {:>5.1}fps {}", tile.id, rate, fps, tile.status()));
        }
        println!(
            "screen {} ({}x{}) frames {}:",
            self.monitor_index,
            self.window.inner_size().width,
            self.window.inner_size().height,
            self.frames_drawn
        );
        self.frames_drawn = 0;
        for p in parts {
            println!("{p}");
        }
    }

    /// Frees all GL resources (must run on this screen's GL context) and
    /// hands the tiles back so their mpv instances can be terminated in
    /// parallel (see main's shutdown path) — serial mpv teardown is what
    /// made quitting slow.
    pub fn destroy(&mut self) -> Vec<Tile> {
        self.context.make_current(&self.surface).ok();
        for tile in self.tiles.iter_mut() {
            tile.destroy_gl(&self.gl);
        }
        self.compositor.destroy(&self.gl);
        self.egui.destroy();
        std::mem::take(&mut self.tiles)
    }
}
