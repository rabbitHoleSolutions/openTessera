//! Linux (X11/Wayland) and other free unixes. The behaviour here is what the
//! app was built on; keep it that way unless the change is meant for Linux.

use std::path::PathBuf;

use winit::event::{KeyEvent, MouseButton};
use winit::keyboard::ModifiersState;
use winit::monitor::MonitorHandle;
use winit::platform::x11::WindowBuilderExtX11;
use winit::window::{Fullscreen, Window, WindowBuilder};

use super::Chord;

/// `nvdec-copy`: NVDEC decoding with a frame copy rather than the CUDA/GL
/// zero-copy interop, which segfaults when decoders re-init mid-playback.
pub const DEFAULT_HWDEC: &str = "nvdec-copy";

/// The `--hwdec` paragraph of `--help`, continuation lines pre-indented.
pub const HWDEC_HELP: &str = "mpv hardware decode mode (default nvdec-copy — hardware
                     decode without the CUDA/GL zero-copy interop, which
                     segfaults in mpv 0.34 when many decoders re-initialize;
                     use auto-safe for zero-copy at your own risk)";

/// Tiling WMs (pop-shell) ignore a fresh window's fullscreen request and
/// relocate fullscreen windows when another leaves fullscreen; the event
/// loop re-anchors them on a short timer (see `fix_displaced_walls`).
pub const WM_SETTLE: bool = true;

/// Startup tiles are created before the event loop runs. (macOS has to
/// wait for the first tick, which is when Finder's open-file arrives.)
pub const DEFER_STARTUP_TILES: bool = false;

/// WM_CLASS, and the name the desktop entry matches with StartupWMClass.
/// Set explicitly rather than left to default from the binary name, so
/// renaming the binary can't silently break the dock association.
const APP_ID: &str = "tessera";

/// WM_CLASS, so the desktop entry's StartupWMClass matches and the dock
/// groups our windows under the installed icon.
pub fn window_builder(wb: WindowBuilder) -> WindowBuilder {
    wb.with_name(APP_ID, APP_ID)
}

/// A window that starts out as a borderless fullscreen wall on `monitor`.
pub fn fullscreen_window_builder(wb: WindowBuilder, monitor: &MonitorHandle) -> WindowBuilder {
    wb.with_decorations(false)
        .with_fullscreen(Some(Fullscreen::Borderless(Some(monitor.clone()))))
}

/// Nothing to do: the builder already asked for fullscreen.
pub fn finish_fullscreen_window(_window: &Window, _monitor: &MonitorHandle) {}

pub fn enter_fullscreen(window: &Window, monitor: Option<MonitorHandle>) {
    window.set_fullscreen(Some(Fullscreen::Borderless(monitor)));
}

pub fn exit_fullscreen(window: &Window) {
    window.set_fullscreen(None);
}

pub fn is_fullscreen(window: &Window) -> bool {
    window.fullscreen().is_some()
}

/// App-wide chrome is the WM's business here.
pub fn sync_fullscreen_chrome(_any_fullscreen: bool) {}

/// No cap beyond the hard limit.
pub fn fd_limit_cap() -> Option<libc::rlim_t> {
    None
}

/// Open descriptors, for the status line — fd exhaustion is invisible
/// otherwise.
pub fn fd_count() -> usize {
    std::fs::read_dir("/proc/self/fd").map(|d| d.count()).unwrap_or(0)
}

/// The UI scale is an absolute pixels-per-point: X11 reports every monitor
/// at scale 1, so the per-monitor value is the whole story.
pub fn apply_ui_scale(ctx: &egui::Context, scale: f32) {
    ctx.set_pixels_per_point(scale);
}

/// No OS chords: every key reaches the wall's hotkeys, as it always has.
pub fn key_chord(_event: &KeyEvent, _mods: ModifiersState) -> Option<Chord> {
    None
}

/// Right-click is only ever a real right button.
pub fn is_secondary_click(_button: MouseButton, _mods: ModifiersState) -> bool {
    false
}

/// Files arrive on the command line; there is no runtime open event.
pub fn install_open_handler() {}

pub fn take_opened_paths() -> Vec<PathBuf> {
    Vec::new()
}
