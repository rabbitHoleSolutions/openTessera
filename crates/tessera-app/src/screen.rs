use std::sync::Arc;

use glow::HasContext;
use glutin::prelude::*;
use tessera_core::{DecodeBudget, PlaybackQueue, Rect, StreamSpec};
use winit::window::Window;

use crate::compositor::Compositor;
use crate::overlay::{self, BudgetUi, OverlayState, TileUiInfo};
use crate::thumbs::ThumbCache;
use crate::tile::{Tile, TileEvent};

pub struct Screen {
    pub window: Arc<Window>,
    pub surface: glutin::surface::Surface<glutin::surface::WindowSurface>,
    pub context: glutin::context::PossiblyCurrentContext,
    pub gl: Arc<glow::Context>,
    pub egui: egui_glow::EguiGlow,
    pub compositor: Compositor,
    pub monitor_index: usize,
    pub tiles: Vec<Tile>,
    pub queue: PlaybackQueue,
    layout: Vec<Rect>,
    /// Tile currently zoomed to fill the screen (F / transport icon).
    pub zoomed: Option<usize>,
    /// Window geometry saved before entering OS fullscreen in windowed mode.
    pub windowed_restore: Option<(winit::dpi::PhysicalPosition<i32>, winit::dpi::PhysicalSize<u32>)>,
    last_status_tp: Vec<f64>,
    pub frames_drawn: u64,
    pub thumbs: ThumbCache,
    pending_screenshot: Option<(std::path::PathBuf, String)>,
}

impl Screen {
    pub fn new(
        window: Arc<Window>,
        surface: glutin::surface::Surface<glutin::surface::WindowSurface>,
        context: glutin::context::PossiblyCurrentContext,
        gl: Arc<glow::Context>,
        egui: egui_glow::EguiGlow,
        monitor_index: usize,
        queue: PlaybackQueue,
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
            tiles: Vec::new(),
            queue,
            layout: Vec::new(),
            zoomed: None,
            windowed_restore: None,
            last_status_tp: Vec::new(),
            frames_drawn: 0,
            thumbs: ThumbCache::new(),
            pending_screenshot: None,
        }
        .with_layout(size.width as i32, size.height as i32)
    }

    /// The layout actually used for compositing and hit-testing: the grid,
    /// or — while a tile is zoomed — one full-screen cell for that tile and
    /// zero-size cells for the others.
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
            None => self.layout.clone(),
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
        advance: bool,
        overlay: &mut OverlayState,
        cursor: Option<(f32, f32)>,
    ) {
        self.context.make_current(&self.surface).ok();

        let mut ended: Vec<usize> = Vec::new();
        for tile in self.tiles.iter_mut() {
            for ev in tile.tick(&self.gl) {
                match ev {
                    TileEvent::EndOfFile => ended.push(tile.id),
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
                if let Some(next) = self.queue.advance() {
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
        let zoom_cells = self.effective_layout();
        for (tile, cell) in self.tiles.iter_mut().zip(zoom_cells.iter()) {
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
        // Overlay (egui). A full pass runs every frame — with an empty UI when
        // hidden — so egui input state stays fresh and stale widgets can't
        // swallow clicks. Painted after the screenshot read so dumps stay
        // pure video.
        let tiles_ui: Vec<TileUiInfo> = self
            .tiles
            .iter()
            .map(|t| TileUiInfo {
                id: t.id,
                title: t.item.title.clone(),
                paused: t.is_paused(),
                muted: t.is_muted(),
                audio_enabled: t.audio_enabled(),
                time_pos: t.last_time_pos,
                duration: t.duration,
            })
            .collect();
        let cells = self.effective_layout();
        let monitor_index = self.monitor_index;
        let queue_entries: Vec<overlay::QueueEntry> = self
            .queue
            .pending(12)
            .iter()
            .map(|(i, m)| overlay::QueueEntry {
                index: *i,
                id: m.id.clone(),
                title: m.title.clone(),
                thumb_url: m.thumb_url.clone(),
            })
            .collect();
        let hover = cursor.and_then(|(x, y)| {
            self.tiles
                .iter()
                .zip(self.effective_layout().iter())
                .find(|(_, cell)| cell.contains(x, y))
                .map(|(t, cell)| (t.id, *cell))
        });
        let budget_ui = BudgetUi {
            utilization: budget.utilization(),
            used_mbps: budget.used_mbps(),
            ceiling_mbps: budget.effective_ceiling_mbps(),
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
        // Split borrows: the egui pass closure needs the thumbnail cache
        // while the EguiGlow itself is borrowed by run().
        let egui = &mut self.egui;
        let thumbs = &mut self.thumbs;
        let notice = overlay.notice.clone();
        egui.run(&window, |ctx| {
            if visible {
                overlay::draw(ctx, monitor_index, pinned, &tiles_ui, &cells, &queue_entries, &budget_ui, hover, thumbs, &mut actions, &mut queue_drag, &mut confirm_delete);
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
        overlay.queue_drag = queue_drag;
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
        self.surface.swap_buffers(&self.context).ok();
        self.frames_drawn += 1;
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
                        "screen {} tile {}: audio disabled (start with --audio)",
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

    pub fn destroy(&mut self) {
        self.context.make_current(&self.surface).ok();
        for tile in self.tiles.iter_mut() {
            tile.destroy_gl(&self.gl);
        }
        self.tiles.clear();
        self.compositor.destroy(&self.gl);
        self.egui.destroy();
    }
}
