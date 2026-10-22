//! The browser window: everything that is *global* to the app, in tabs.
//!
//! Splitting by scope rather than by kind is deliberate. A wall window's
//! gear panel holds only settings belonging to that window; anything shared
//! by the whole app lives here, in the one window that exists once. That
//! kills a whole bug class: every `Screen` runs its own egui context over
//! the same `UiSettings`, so a widget drawn on several screens has several
//! contexts fighting over one value (see `OverlayState::help_open` for the
//! window-tagging that used to be needed). A control drawn in exactly one
//! window needs none of it.
//!
//! Tabs: library and queue are the media; sources and config are the
//! setup. Sources is data — which servers exist, with credentials — while
//! config is preference, so they stay apart rather than becoming one
//! "settings" pile.

mod config;
mod library;
mod queue;
mod sources;

use tessera_core::{MediaItem, QueueOrder};

use crate::catalog::SourceCatalog;
use crate::overlay::{OverlayAction, UiSettings};
use crate::thumbs::ThumbCache;

pub(crate) use tessera_core::info::human_size;

/// How long a confirmation flash stays up. The queue lives in the other
/// windows, so without a flash a bulk add gives no feedback at all.
pub(crate) const FLASH_SECS: f64 = 2.5;

/// Rows drawn per frame. The full list can be thousands of items; egui has
/// no virtualisation here, so the filter is how you reach the rest.
pub(crate) const MAX_ROWS: usize = 300;

/// Which tab the browser window is showing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Library,
    Queue,
    Sources,
    Config,
}

impl Tab {
    fn label(&self) -> &'static str {
        match self {
            Tab::Library => "library",
            Tab::Queue => "queue",
            Tab::Sources => "sources",
            Tab::Config => "config",
        }
    }
}

/// One live tile, wherever it lives. Gathered across every wall window
/// before the per-screen loop, because a `Screen` cannot see its siblings.
pub struct WallTile {
    /// Index of the owning window in `screens`, and the monitor it is on.
    pub window: usize,
    pub monitor: usize,
    pub tile_id: usize,
    pub title: String,
    pub paused: bool,
    pub muted: bool,
    pub audio_enabled: bool,
    pub volume: i64,
    pub time_pos: f64,
    pub duration: f64,
}

/// Per-window browser state. Each browser window browses independently.
///
/// The add-source form lives here rather than on `UiSettings` for the same
/// reason the tabs do: it is transient per-window editing state, and on
/// shared settings it had to be hand-excluded from equality so a half-typed
/// password could not trigger a save.
pub struct BrowserState {
    pub tab: Tab,
    /// Index into the configured sources; None until one is picked (or the
    /// source list is empty).
    pub source: Option<usize>,
    pub search: String,
    pub order: QueueOrder,
    /// Transient confirmation of the last queue action.
    pub flash: Option<(String, std::time::Instant)>,
    /// "clear queue" armed at this instant. Emptying the queue is a big,
    /// silent action, so it takes two clicks — same pattern as the
    /// transport's delete button.
    pub clear_armed: Option<std::time::Instant>,
    /// Source removal armed on this row index, at this instant. Same two
    /// -click rule, and for a sharper reason: the ✖ sits against every row
    /// of the list, one stray click discards a server's credentials, and
    /// re-adding means signing in again.
    pub remove_armed: Option<(usize, std::time::Instant)>,
    /// Add-source form.
    pub add: sources::AddForm,
    /// The library tab's filtered, ordered row list, rebuilt only when its
    /// inputs move. See [`library::RowCache`].
    pub rows: library::RowCache,
}

impl Default for BrowserState {
    fn default() -> BrowserState {
        BrowserState {
            tab: Tab::default(),
            source: None,
            search: String::new(),
            order: QueueOrder::Name,
            flash: None,
            clear_armed: None,
            remove_armed: None,
            add: sources::AddForm::default(),
            rows: library::RowCache::default(),
        }
    }
}

impl BrowserState {
    pub(crate) fn flash(&mut self, msg: String) {
        self.flash = Some((msg, std::time::Instant::now()));
    }
}

/// A read-only look at the shared queue. The browser never mutates it
/// directly — every change goes out as an [`OverlayAction`], because the
/// queue is owned by the event loop.
pub struct QueueView<'a> {
    pub items: &'a [MediaItem],
    /// Index of the item the wall is currently on. Everything at or before
    /// it is history and cannot be reordered or removed. None until
    /// anything has started: then the whole queue is pending.
    pub current: Option<usize>,
}

impl QueueView<'_> {
    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn contains(&self, id: &str) -> bool {
        self.items.iter().any(|i| i.id == id)
    }
}

/// Everything the browser draws from that it does not own. Bundled rather
/// than passed loose: four tabs each want a different slice of it, and the
/// list grows every time a tab does.
pub struct BrowserCtx<'a> {
    pub catalog: &'a mut SourceCatalog,
    pub thumbs: &'a mut ThumbCache,
    pub auth: &'a mut crate::auth::Auth,
    pub plex_client_id: &'a str,
    pub wall: &'a [WallTile],
}

/// Draws the browser window: a tab bar, then whichever tab is showing.
/// Every mutation of the wall or the queue leaves as an [`OverlayAction`] —
/// both are owned by the event loop, not by this window. Settings are the
/// exception: they are edited in place, and the event loop diffs them after
/// the frame exactly as it does for the gear panel.
pub fn draw(
    ctx: &egui::Context,
    state: &mut BrowserState,
    settings: &mut UiSettings,
    queue_view: &QueueView,
    services: &mut BrowserCtx,
    actions: &mut Vec<OverlayAction>,
) {
    services.catalog.poll();
    if let Some((_, at)) = &state.flash {
        if at.elapsed().as_secs_f64() > FLASH_SECS {
            state.flash = None;
        }
    }

    egui::TopBottomPanel::top("browser-bar").show(ctx, |ui| {
        ui.horizontal(|ui| {
            for tab in [Tab::Library, Tab::Queue, Tab::Sources, Tab::Config] {
                ui.selectable_value(&mut state.tab, tab, tab.label());
            }
            ui.separator();
            let playing = services.wall.iter().filter(|t| !t.paused).count();
            ui.weak(format!(
                "{} in the queue · {playing}/{} tile(s) playing",
                queue_view.len(),
                services.wall.len()
            ));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Same gear as the wall's status bar, meaning the same
                // thing: this window's own settings. Without it a browser
                // window would have no way to set its own UI scale — and
                // it is the text-heavy window that most wants one.
                if crate::overlay::gear_button(ui, 1.0).clicked() {
                    actions.push(OverlayAction::ToggleSettings);
                }
                if state.tab == Tab::Library
                    && ui
                        .button("refresh")
                        .on_hover_text("re-list this source")
                        .clicked()
                {
                    if let Some(src) = state.source.and_then(|i| settings.sources.get(i)) {
                        services.catalog.refresh(src);
                    }
                }
            });
        });
    });

    match state.tab {
        Tab::Library => library::draw(ctx, state, settings, queue_view, services, actions),
        Tab::Queue => queue::draw(ctx, state, settings, queue_view, services.wall, actions),
        Tab::Sources => sources::draw(ctx, state, settings, services),
        Tab::Config => config::draw(ctx, settings),
    }
}

/// mm:ss / h:mm:ss, matching the wall transport.
pub(crate) fn fmt_time(t: f64) -> String {
    if !t.is_finite() || t < 0.0 {
        return "--:--".into();
    }
    let s = t as u64;
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}")
    } else {
        format!("{m}:{sec:02}")
    }
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

/// Green confirmation text, shown next to whatever caused it.
pub(crate) fn flash_label(ui: &mut egui::Ui, state: &BrowserState) {
    if let Some((msg, _)) = &state.flash {
        ui.colored_label(egui::Color32::from_rgb(140, 220, 140), msg);
    }
}
