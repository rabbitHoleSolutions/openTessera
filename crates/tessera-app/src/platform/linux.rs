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

/// Where the pointer is over `window` right now, asked of the OS. Not
/// needed here: under X11 the event loop's own cursor keeps up during an
/// XDND drag (ROADMAP §21 — tested with a real GTK source, drops landed on
/// the tile under the pointer), so None leaves it alone. If a toolkit or WM
/// turns out to withhold motion mid-drag, XQueryPointer would answer this.
pub fn pointer_in_window(_window: &Window) -> Option<(f32, f32)> {
    None
}

/// Floats a window above every other one (true) or puts it back among
/// them. The caller has already set its decorations, which this keeps.
/// Returns whether the WM let go of the window and is taking it back, in
/// which case anything else asked of the WM (fullscreen) has to wait.
///
/// `_NET_WM_STATE_ABOVE` alone isn't enough under a tiling WM: pop-shell
/// keeps an "above" window in its tile, so a floating wall just grew into
/// the space its title bar left. Pop-shell has no way for a client to ask
/// to float; it decides when a window is mapped, and tiles only NORMAL,
/// non-transient ones. So the window is withdrawn and mapped again as a
/// DIALOG, which every EWMH WM floats (and, unlike UTILITY, mutter keeps
/// in the dock and Alt-Tab), and back as NORMAL to unfloat — pop-shell
/// then tiles it again. On Wayland the handle isn't Xlib and only the
/// level is set (which winit doesn't do there either).
pub fn set_floating(window: &Window, floating: bool) -> bool {
    let remapped = retype(window, if floating { "_NET_WM_WINDOW_TYPE_DIALOG" } else { "_NET_WM_WINDOW_TYPE_NORMAL" });
    // After the remap: a withdrawn window's state is the WM's to clear,
    // and it does.
    window.set_window_level(if floating {
        winit::window::WindowLevel::AlwaysOnTop
    } else {
        winit::window::WindowLevel::Normal
    });
    remapped
}

/// Withdraws `window`, sets its `_NET_WM_WINDOW_TYPE`, and maps it again
/// where it was. Through winit's own Display, so these requests stay in
/// order with the ones winit sends. False if there is no X11 window to do
/// it to.
fn retype(window: &Window, type_atom: &str) -> bool {
    use raw_window_handle::{HasRawDisplayHandle, HasRawWindowHandle, RawDisplayHandle, RawWindowHandle};
    use std::ffi::CString;
    use x11_dl::xlib;
    let (RawDisplayHandle::Xlib(d), RawWindowHandle::Xlib(w)) = (window.raw_display_handle(), window.raw_window_handle())
    else {
        return false;
    };
    let dpy = d.display as *mut xlib::Display;
    let xwin = w.window;
    if dpy.is_null() || xwin == 0 {
        return false;
    }
    let Ok(x) = xlib::Xlib::open() else { return false };
    // Where the WM has it now, frame included — what a NorthWest-gravity
    // client position means once it is managed again.
    let pos = window.outer_position().ok();
    unsafe {
        let root = (x.XDefaultRootWindow)(dpy);
        (x.XWithdrawWindow)(dpy, xwin, d.screen);
        // Mapping again only counts as a new window once the WM has let go
        // of this one (unmanaged it and reparented it back to the root);
        // mapped any earlier, it reappears inside the dying frame. That
        // takes the WM a few ms; it is a keypress, so wait for it, bounded.
        for _ in 0..100 {
            (x.XSync)(dpy, xlib::False);
            let (mut r, mut parent, mut kids, mut n) = (0, 0, std::ptr::null_mut(), 0);
            if (x.XQueryTree)(dpy, xwin, &mut r, &mut parent, &mut kids, &mut n) != 0 && !kids.is_null() {
                (x.XFree)(kids as *mut _);
            }
            if parent == root {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let name = |s: &str| {
            let c = CString::new(s).unwrap();
            (x.XInternAtom)(dpy, c.as_ptr(), xlib::False)
        };
        let value = name(type_atom);
        (x.XChangeProperty)(
            dpy,
            xwin,
            name("_NET_WM_WINDOW_TYPE"),
            xlib::XA_ATOM,
            32,
            xlib::PropModeReplace,
            &value as *const xlib::Atom as *const u8,
            1,
        );
        // Without a position hint mutter places a newly mapped window
        // itself (centred on the monitor), so it would jump on every T.
        if let Some(p) = pos {
            let mut hints: xlib::XSizeHints = std::mem::zeroed();
            let mut supplied = 0;
            (x.XGetWMNormalHints)(dpy, xwin, &mut hints, &mut supplied);
            hints.flags |= xlib::USPosition | xlib::PPosition;
            hints.x = p.x;
            hints.y = p.y;
            (x.XSetWMNormalHints)(dpy, xwin, &mut hints);
            (x.XMoveWindow)(dpy, xwin, p.x, p.y);
        }
        (x.XMapRaised)(dpy, xwin);
        (x.XFlush)(dpy);
    }
    true
}
