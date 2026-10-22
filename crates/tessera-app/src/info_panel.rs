//! The "i" panel: what is known about one item — where the file lives, how
//! big it is, what is in it — and, for a playing tile, what mpv is doing
//! with it.
//!
//! Opened from a tile chip, a library row or a queue row, all through
//! [`OverlayAction::ItemInfo`] / [`OverlayAction::TileInfo`]. Like help and
//! settings it is a singleton scoped to the window that opened it (see
//! `OverlayState::help_open` for why a shared panel must be), and it lives
//! on `OverlayState` rather than on a `Screen` so that a browser window can
//! show the live numbers of a tile in some other window.
//!
//! [`OverlayAction::ItemInfo`]: crate::overlay::OverlayAction::ItemInfo
//! [`OverlayAction::TileInfo`]: crate::overlay::OverlayAction::TileInfo

use std::sync::mpsc;
use std::time::{Duration, Instant};

use tessera_core::info::Section;
use tessera_core::sources::Source;
use tessera_core::MediaItem;

/// How often a playing tile's numbers are re-read from mpv. A handful of
/// property reads, and the panel is reading material, not a meter.
const LIVE_EVERY: Duration = Duration::from_millis(1000);
/// Live numbers older than this are dropped rather than shown: the tile
/// has gone (closed, or moved to a window that has not ticked yet).
const LIVE_STALE: Duration = Duration::from_millis(3000);
/// Logical width the panel lays out around, before window scaling.
const DESIGN_W: f32 = 560.0;

/// Where the server's answer has got to.
enum Details {
    Loading(mpsc::Receiver<Result<Vec<Section>, String>>),
    Ready(Vec<Section>),
    Failed(String),
}

pub struct InfoPanel {
    /// The window drawing it.
    pub window: winit::window::WindowId,
    pub item: MediaItem,
    /// The tile it was opened from, if any: the panel follows that tile to
    /// its next item and shows its live playback numbers.
    pub tile_id: Option<usize>,
    details: Details,
    live: Vec<Section>,
    live_at: Option<Instant>,
}

impl InfoPanel {
    /// Opens on `item`, with the server round trip already under way.
    ///
    /// A thread per open rather than a standing worker: this happens on a
    /// click, a few times a session, and a thread that outlives a closed
    /// panel only finds its receiver gone.
    pub fn open(
        window: winit::window::WindowId,
        item: MediaItem,
        tile_id: Option<usize>,
        sources: &[Source],
    ) -> InfoPanel {
        let (tx, rx) = mpsc::channel();
        let (it, srcs) = (item.clone(), sources.to_vec());
        std::thread::spawn(move || {
            let _ = tx.send(tessera_core::sources::item_details(&it, &srcs));
        });
        InfoPanel { window, item, tile_id, details: Details::Loading(rx), live: Vec::new(), live_at: None }
    }

    /// Takes delivery of the server's answer, if it has come.
    pub fn poll(&mut self) {
        if let Details::Loading(rx) = &self.details {
            match rx.try_recv() {
                Ok(Ok(sections)) => self.details = Details::Ready(sections),
                Ok(Err(e)) => self.details = Details::Failed(e),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.details = Details::Failed("the lookup stopped without an answer".into())
                }
            }
        }
    }

    /// True when the live numbers are due a re-read.
    pub fn live_due(&self) -> bool {
        self.live_at.is_none_or(|at| at.elapsed() >= LIVE_EVERY)
    }

    pub fn set_live(&mut self, live: Vec<Section>) {
        self.live = live;
        self.live_at = Some(Instant::now());
    }
}

/// Draws the panel. `open` goes false when it is closed.
pub fn draw(ctx: &egui::Context, panel: &InfoPanel, open: &mut bool) {
    let win = ctx.screen_rect();
    let k = crate::overlay::window_scale(win.width(), win.height(), DESIGN_W, 0.0, 0.0);
    egui::Area::new(egui::Id::new("tessera-info"))
        // PANEL_ORDER, never Tooltip: egui does not hit-test tooltip
        // layers, and the close button would be inert.
        .order(crate::overlay::PANEL_ORDER)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            egui::Frame::window(ui.style())
                .fill(egui::Color32::from_black_alpha(235))
                .inner_margin(egui::Margin::symmetric(20.0, 14.0))
                .show(ui, |ui| {
                    crate::overlay::scale_style(ui, k);
                    ui.set_width(520.0 * k);
                    ui.heading(&panel.item.title);
                    // Which configured source — two servers can hold the
                    // same film, and this is the one the panel describes.
                    ui.weak(panel.item.source.as_deref().unwrap_or(panel.item.kind.label()));
                    ui.add_space(6.0);
                    // A film can carry a dozen audio and subtitle tracks;
                    // the panel scrolls rather than running off the window.
                    egui::ScrollArea::vertical()
                        .max_height((win.height() * 0.7).max(120.0))
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            let fresh = panel.live_at.is_some_and(|at| at.elapsed() < LIVE_STALE);
                            if fresh {
                                sections(ui, &panel.live, "info-live");
                            }
                            match &panel.details {
                                Details::Ready(s) if !s.is_empty() => sections(ui, s, "info-details"),
                                // Nothing more to ask (a camera, a URL):
                                // the listing is the whole story.
                                Details::Ready(_) => sections(ui, &tessera_core::info::listing(&panel.item), "info-listing"),
                                Details::Loading(_) => {
                                    sections(ui, &tessera_core::info::listing(&panel.item), "info-listing");
                                    ui.weak("asking the source…");
                                }
                                Details::Failed(e) => {
                                    sections(ui, &tessera_core::info::listing(&panel.item), "info-listing");
                                    ui.colored_label(
                                        egui::Color32::from_rgb(240, 150, 150),
                                        format!("couldn't get more from the source: {e}"),
                                    );
                                }
                            }
                        });
                    ui.add_space(8.0);
                    // Not `with_layout`: in an Area that takes every pixel
                    // left in the window and stretches the frame with it.
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), 20.0 * k),
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            if ui.button("close").clicked() {
                                *open = false;
                            }
                            if panel.tile_id.is_some() {
                                ui.weak("follows the tile to its next video");
                            }
                        },
                    );
                });
        });
}

fn sections(ui: &mut egui::Ui, sections: &[Section], id: &str) {
    for (n, s) in sections.iter().enumerate() {
        if s.rows.is_empty() {
            continue;
        }
        ui.add_space(4.0);
        ui.label(egui::RichText::new(&s.title).strong());
        // One grid per section, so a shared minimum is what lines the
        // values up from one section to the next.
        egui::Grid::new(format!("{id}-{n}"))
            .num_columns(2)
            .min_col_width(72.0)
            .spacing(egui::vec2(14.0, 3.0))
            .show(ui, |ui| {
                for (label, value) in &s.rows {
                    ui.weak(label);
                    // Wrapping, so a long server path breaks rather than
                    // widening the panel off the window; selectable, so
                    // it can be copied.
                    ui.add(egui::Label::new(value).wrap(true).selectable(true));
                    ui.end_row();
                }
            });
    }
}
