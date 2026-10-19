//! Windows and monitors: creating a screen's window and GL context, fullscreen,
//! and working out which monitor a window is really on.

use std::sync::Arc;

use glutin::prelude::*;
use raw_window_handle::HasRawWindowHandle;
use winit::window::WindowBuilder;

use crate::cli::{Args, ScreenSel};
use crate::platform;
use crate::screen::{Screen, ScreenRole};

/// The window/taskbar icon, baked into the binary so a window looks right
/// even when the app is run straight out of target/release with nothing
/// installed. The desktop entry uses the installed hicolor icons instead.
pub const WINDOW_ICON_PNG: &[u8] = include_bytes!("../../../assets/window-icon-64.png");

/// Decodes the embedded icon. A failure here is cosmetic — the window just
/// gets the WM's default — so it never stops startup.
pub fn window_icon() -> Option<winit::window::Icon> {
    let img = image::load_from_memory(WINDOW_ICON_PNG).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    winit::window::Icon::from_rgba(img.into_raw(), w, h).ok()
}

/// Where a window opened at runtime should go. None of these means "near
/// the pointer, as a window" — the "+" button's behaviour.
#[derive(Clone, Copy, Debug, Default)]
pub struct Placement {
    /// Index into `available_monitors()`, as `--monitor` and the startup
    /// "monitor N:" lines number them.
    pub monitor: Option<usize>,
    /// Start as a fullscreen wall (what W does) rather than a window.
    pub fullscreen: bool,
}

/// Opens a plain (decorated, non-fullscreen) window at runtime, centred on
/// `on` when a monitor is given and clamped to it. The GL config is the one
/// every other window shares, so [`create_screen`] can build on it.
pub fn open_window<T>(
    target: &winit::event_loop::EventLoopWindowTarget<T>,
    gl_config: &glutin::config::Config,
    size: (u32, u32),
    on: Option<&winit::monitor::MonitorHandle>,
) -> Option<Arc<winit::window::Window>> {
    let (mut w, mut h) = size;
    let mut wb = app_window().with_title("Tessera");
    if let Some(m) = on {
        let (mp, ms) = (m.position(), m.size());
        w = w.min(ms.width);
        h = h.min(ms.height);
        wb = wb.with_position(winit::dpi::PhysicalPosition::new(
            mp.x + (ms.width.saturating_sub(w)) as i32 / 2,
            mp.y + (ms.height.saturating_sub(h)) as i32 / 2,
        ));
    }
    wb = wb.with_inner_size(winit::dpi::PhysicalSize::new(w, h));
    match glutin_winit::finalize_window(target, wb, gl_config) {
        Ok(win) => Some(Arc::new(win)),
        Err(e) => {
            eprintln!("warning: new window failed: {e}");
            None
        }
    }
}

/// Builds a [`Screen`] around an existing window: a GL context and surface
/// on the shared config, an egui context, and the compositor. Startup and
/// runtime "new window" both come through here so the two can't drift.
/// Returns None (having explained itself) if any GL step fails.
pub fn create_screen<T>(
    target: &winit::event_loop::EventLoopWindowTarget<T>,
    display: &glutin::display::Display,
    gl_config: &glutin::config::Config,
    window: Arc<winit::window::Window>,
    monitor_index: usize,
) -> Option<Screen> {
    let raw = window.raw_window_handle();
    let context_attrs = glutin::context::ContextAttributesBuilder::new().build(Some(raw));
    let context = match unsafe { display.create_context(gl_config, &context_attrs) } {
        Ok(c) => c,
        Err(e) => {
            eprintln!("warning: GL context for monitor {monitor_index} failed: {e}");
            return None;
        }
    };
    let size = window.inner_size();
    let one = std::num::NonZeroU32::new(1).unwrap();
    let (nw, nh) = (
        std::num::NonZeroU32::new(size.width).unwrap_or(one),
        std::num::NonZeroU32::new(size.height).unwrap_or(one),
    );
    let surface_attrs =
        glutin::surface::SurfaceAttributesBuilder::<glutin::surface::WindowSurface>::new()
            .build(window.raw_window_handle(), nw, nh);
    let surface = match unsafe { display.create_window_surface(gl_config, &surface_attrs) } {
        Ok(s) => s,
        Err(e) => {
            eprintln!("warning: GL surface for monitor {monitor_index} failed: {e}");
            return None;
        }
    };
    let context = match context.make_current(&surface) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("warning: make_current for monitor {monitor_index} failed: {e}");
            return None;
        }
    };
    let vsync = std::env::var("TESSERA_VSYNC").map(|v| v != "0").unwrap_or(false);
    surface
        .set_swap_interval(
            &context,
            if vsync {
                glutin::surface::SwapInterval::Wait(std::num::NonZeroU32::new(1).unwrap())
            } else {
                glutin::surface::SwapInterval::DontWait
            },
        )
        .ok();
    // egui_glow takes an Arc, though the context never leaves this thread.
    #[allow(clippy::arc_with_non_send_sync)]
    let gl = Arc::new(unsafe {
        glow::Context::from_loader_function(|s| {
            let c = std::ffi::CString::new(s).unwrap();
            display.get_proc_address(&c) as *const std::os::raw::c_void
        })
    });
    // EguiGlow compiles its shaders now, so the GL context must be current.
    let egui = egui_glow::EguiGlow::new(target, gl.clone(), None, None);
    Some(Screen::new(window, surface, context, gl, egui, monitor_index))
}

pub fn window_builder(args: &Args, monitors: &[winit::monitor::MonitorHandle], index: usize) -> WindowBuilder {
    let monitor = &monitors[index];
    match &args.screens {
        ScreenSel::Windowed(w, h) => {
            let wb = app_window().with_title("Tessera");
            // Asked for a monitor: centre the window on it. Without one the
            // window manager places it, as before.
            match args.monitor {
                Some(_) => {
                    let (mp, ms) = (monitor.position(), monitor.size());
                    let (w, h) = ((*w).min(ms.width), (*h).min(ms.height));
                    wb.with_inner_size(winit::dpi::PhysicalSize::new(w, h)).with_position(
                        winit::dpi::PhysicalPosition::new(
                            mp.x + (ms.width - w) as i32 / 2,
                            mp.y + (ms.height - h) as i32 / 2,
                        ),
                    )
                }
                None => wb.with_inner_size(winit::dpi::PhysicalSize::new(*w, *h)),
            }
        }
        _ => platform::fullscreen_window_builder(
            app_window().with_title(format!("Tessera — screen {index}")),
            monitor,
        ),
    }
}

/// A WindowBuilder carrying this app's identity: WM_CLASS (so the desktop
/// entry's StartupWMClass matches and the dock groups our windows) and the
/// embedded icon.
pub fn app_window() -> WindowBuilder {
    platform::window_builder(WindowBuilder::new()).with_window_icon(window_icon())
}

/// Inner window rect in global screen coordinates (x0, y0, x1, y1).
pub fn window_rect(s: &Screen) -> Option<(i32, i32, i32, i32)> {
    let p = s.window.inner_position().ok()?;
    let sz = s.window.inner_size();
    Some((p.x, p.y, p.x + sz.width as i32, p.y + sz.height as i32))
}

/// A request aimed at the app rather than at a window means "come to the
/// front" — that is what a second click on the dock icon is asking for, so
/// even a bare re-launch does this. Raised on arrival, not once listed: a
/// folder that takes a while to walk should not leave the user wondering
/// whether anything heard them.
pub fn raise_front(screens: &[Screen]) {
    let front = screens
        .iter()
        .position(|s| s.role == ScreenRole::Wall)
        .or(if screens.is_empty() { None } else { Some(0) });
    if let Some(i) = front {
        screens[i].window.focus_window();
    }
}

/// Brings the OS's app-wide fullscreen chrome (macOS menu bar and Dock) in
/// line with whether any window is currently a fullscreen wall.
pub fn sync_fullscreen_chrome(screens: &[Screen]) {
    platform::sync_fullscreen_chrome(screens.iter().any(|s| platform::is_fullscreen(&s.window)));
}

/// Toggles the window itself between windowed and borderless-fullscreen on
/// its current monitor — the whole wall layout, no tile zoom. The focused
/// window only; other screens are untouched.
/// Toggles one screen's window between a fullscreen wall and a normal
/// decorated window. Expanding only calls set_fullscreen (never touches
/// decorations — changing them mid-transition makes pop-shell re-tile the
/// window back over the fullscreen switch); window-ifying turns decorations
/// on and restores the saved geometry (or a sensible default).
///
/// Returns whether the WM needs watching afterwards (see
/// [`fix_displaced_walls`]) — only ever on a platform that has one to fight.
pub fn toggle_window_mode(screens: &mut [Screen], idx: usize) -> bool {
    let screen = &mut screens[idx];
    let window = screen.window.clone();
    let monitor_index = screen.monitor_index;
    let mut schedule_fix = false;
    if platform::is_fullscreen(&window) {
        schedule_fix = true;
        // Fullscreen wall -> normal window.
        screen.zoomed = None;
        screen.should_fullscreen = false;
        platform::exit_fullscreen(&window);
        window.set_decorations(true);
        let restore = screen
            .windowed_restore
            .take()
            .or(screen.zoom_restore.take());
        match restore {
            Some((pos, size)) => {
                window.set_outer_position(pos);
                let _ = window.request_inner_size(size);
                screen.resize(size.width, size.height);
            }
            None => {
                if let Some(m) = window.current_monitor() {
                    let ms = m.size();
                    let (w, h) = (ms.width.min(1600), ms.height.min(900));
                    let _ = window.request_inner_size(winit::dpi::PhysicalSize::new(w, h));
                    let mp = m.position();
                    let x = mp.x + (ms.width.saturating_sub(w)) as i32 / 2;
                    let y = mp.y + (ms.height.saturating_sub(h)) as i32 / 2;
                    window.set_outer_position(winit::dpi::PhysicalPosition::new(x, y));
                    screen.resize(w, h);
                }
            }
        }
        println!("screen {monitor_index}: window mode");
    } else {
        // Normal window -> fullscreen wall. Aim at the intended monitor:
        // winit moves the window there and grabs focus, which pop-shell
        // needs before it honors fullscreen for a window it has tiled.
        screen.should_fullscreen = true;
        // A floating window stops floating: a wall above every other
        // window would sit over the next app you switch to. Its missing
        // title bar is left alone here (see above) and comes back with the
        // rest of the window on the way out of fullscreen.
        let mut remapped = false;
        if screen.floating {
            screen.floating = false;
            remapped = platform::set_floating(&window, false);
            screen.refocus_at = Some(std::time::Instant::now() + FLOAT_REFOCUS);
        }
        if screen.windowed_restore.is_none() {
            if let Ok(pos) = window.outer_position() {
                let size = window.inner_size();
                screen.windowed_restore = Some((pos, size));
            }
        }
        // Aim at the monitor the window is on NOW, not the one it was
        // created on: dragging a window to another screen and hitting W is
        // the whole point of per-window fullscreen.
        let here = monitor_under_window(&window);
        if let Some((i, m)) = &here {
            screen.monitor_index = *i;
            screen.intended_monitor = Some(m.clone());
        }
        let monitor = here
            .map(|(_, m)| m)
            .or_else(|| screen.intended_monitor.clone())
            .or_else(|| window.current_monitor());
        if remapped && screen.intended_monitor.is_some() {
            // The WM is still taking the window back from unfloating and
            // would drop a fullscreen request made now. `should_fullscreen`
            // is set, so `fix_displaced_walls` makes it once the WM settles.
            println!("screen {}: fullscreen wall, once the window is back", screen.monitor_index);
            return true;
        }
        platform::enter_fullscreen(&window, monitor);
        println!("screen {}: fullscreen wall", screen.monitor_index);
    }
    schedule_fix && platform::WM_SETTLE
}

/// Toggles a screen's zoomed/fullscreen state. `tile_id` is the tile to
/// zoom onto (None just exits zoom). In windowed mode the window itself
/// also goes borderless-fullscreen on its current monitor — other monitors
/// and windows are unaffected. In multi-screen mode the windows are already
/// fullscreen, so only the tile zoom applies.
pub fn toggle_fullscreen(screens: &mut [Screen], idx: usize, tile_id: Option<usize>) {
    let screen = &mut screens[idx];
    if screen.zoomed.is_some() {
        screen.zoomed = None;
        // Only restore the window if the ZOOM took it fullscreen (W's
        // fullscreen is undone by W, not by un-zooming).
        if let Some((pos, size)) = screen.zoom_restore.take() {
            platform::exit_fullscreen(&screen.window);
            screen.window.set_outer_position(pos);
            let _ = screen.window.request_inner_size(size);
            screen.resize(size.width, size.height);
        }
    } else if let Some(id) = tile_id {
        screen.zoomed = Some(id);
        // A floating window zooms within itself: it is a small window on
        // purpose, and taking the monitor over would undo that.
        if !platform::is_fullscreen(&screen.window) && !screen.floating {
            // Windowed window: also take over the whole monitor.
            if let Ok(pos) = screen.window.outer_position() {
                let size = screen.window.inner_size();
                screen.zoom_restore = Some((pos, size));
            }
            let monitor = screen.window.current_monitor();
            platform::enter_fullscreen(&screen.window, monitor);
        }
    }
}

/// How long after floating a window asks for focus back. Floating can make
/// the WM let go of the window and take it back (it does under X11; see
/// `platform::set_floating`), and focus goes elsewhere meanwhile. Asked for
/// at once, the WM refuses: the window isn't on screen again until it has
/// drawn a frame. Long enough for that, short enough to beat the next key.
const FLOAT_REFOCUS: std::time::Duration = std::time::Duration::from_millis(250);

/// Toggles one window between its ordinary self and floating: no title
/// bar, above every other window, still movable and resizable —
/// picture-in-picture for a wall. With the OS frame gone, the status bar's
/// text moves it and its edges resize it (`Screen::floating_grab`, handled
/// in the event loop before egui sees the press).
///
/// Floating is a window-sized thing, so a fullscreen wall becomes a window
/// first and a zoom that took over the monitor is undone. Returns whether
/// the WM needs watching afterwards, as [`toggle_window_mode`] does.
pub fn toggle_floating(screens: &mut [Screen], idx: usize) -> bool {
    let mut settle = false;
    // Where a window coming out of fullscreen is headed. Read here, not off
    // the window afterwards: the WM may not have moved it yet, and under
    // X11 a floating window that read its geometry too soon came back as
    // a monitor-sized float in the monitor's corner.
    let mut headed = None;
    if !screens[idx].floating {
        if let Some(r) = screens[idx].zoom_restore {
            headed = Some(r);
            toggle_fullscreen(screens, idx, None);
        }
        if platform::is_fullscreen(&screens[idx].window) {
            headed = headed.or(screens[idx].windowed_restore);
            settle = toggle_window_mode(screens, idx);
        }
    }
    let screen = &mut screens[idx];
    screen.floating = !screen.floating;
    let window = screen.window.clone();
    // The picture keeps its size: macOS keeps the *frame* when the title
    // bar goes, so the content would otherwise grow by the bar's height
    // (and shrink by it on the way back).
    let size = headed.map_or_else(|| window.inner_size(), |(_, size)| size);
    window.set_decorations(!screen.floating);
    platform::set_floating(&window, screen.floating);
    screen.refocus_at = Some(std::time::Instant::now() + FLOAT_REFOCUS);
    if let Some((pos, _)) = headed {
        window.set_outer_position(pos);
    }
    let _ = window.request_inner_size(size);
    println!(
        "screen {}: {}",
        screen.monitor_index,
        if screen.floating { "floating on top" } else { "ordinary window" }
    );
    settle
}

/// Snaps the walls back onto their intended monitors. Two displacements
/// are fixed, both from pop-shell: (a) a tiling WM grabs freshly mapped
/// windows, ignoring their fullscreen request and tiling them onto the
/// focused monitor until they get focus; (b) it relocates other fullscreen
/// windows (piling them onto the primary) when one window leaves
/// fullscreen. Either way the cure is the same single call:
/// set_fullscreen(Borderless(Some(monitor))) — winit moves the window to
/// the monitor origin AND grabs input focus, which pop-shell needs before
/// it honors fullscreen. Runs on a short timer at startup and after mode
/// toggles, retrying while displaced windows keep being found.
/// (Detection uses the real window rect — winit's current_monitor() is
/// unreliable here.)
pub fn fix_displaced_walls(screens: &mut [Screen]) -> bool {
    let mut fixed_any = false;
    for screen in screens.iter_mut() {
        if !screen.should_fullscreen {
            continue;
        }
        let Some(m) = screen.intended_monitor.clone() else { continue };
        let Some(centre) = window_centre(&screen.window) else { continue };
        let inside = monitor_contains(&m, centre);
        // Non-fullscreen (tiled by the WM) or fullscreen on the wrong
        // monitor: re-assert fullscreen aimed at the intended monitor.
        if platform::is_fullscreen(&screen.window) && inside {
            continue;
        }
        let window = screen.window.clone();
        platform::enter_fullscreen(&window, Some(m));
        println!("screen {}: re-anchored to its monitor", screen.monitor_index);
        fixed_any = true;
    }
    fixed_any
}

/// What `tessera_core::monitor` needs to know about each monitor, in
/// `available_monitors()` order.
pub fn monitor_infos(monitors: impl Iterator<Item = winit::monitor::MonitorHandle>) -> Vec<tessera_core::monitor::MonitorInfo> {
    monitors
        .map(|m| {
            let (p, s) = (m.position(), m.size());
            tessera_core::monitor::MonitorInfo { name: m.name(), x: p.x, y: p.y, w: s.width, h: s.height }
        })
        .collect()
}

/// The monitor a window is actually on, found by containment of the
/// window's centre in each monitor's rect, with the index into
/// `available_monitors()` (the same order `monitors` is built from in
/// main). winit's `current_monitor()` is unreliable here — see
/// [`fix_displaced_walls`].
pub fn monitor_under_window(
    window: &winit::window::Window,
) -> Option<(usize, winit::monitor::MonitorHandle)> {
    let c = window_centre(window)?;
    window
        .available_monitors()
        .enumerate()
        .find(|(_, m)| monitor_contains(m, c))
}

/// Centre of a window in desktop coordinates, or None if the WM won't
/// report its position yet (freshly mapped windows).
pub fn window_centre(window: &winit::window::Window) -> Option<(i32, i32)> {
    let pos = window.outer_position().ok()?;
    let size = window.inner_size();
    Some((pos.x + size.width as i32 / 2, pos.y + size.height as i32 / 2))
}

/// Whether a point lies in a monitor's rect. Half-open on the far edges so
/// adjacent monitors can't both claim a point on their shared border.
pub fn monitor_contains(monitor: &winit::monitor::MonitorHandle, p: (i32, i32)) -> bool {
    let (mp, ms) = (monitor.position(), monitor.size());
    rect_contains((mp.x, mp.y, ms.width, ms.height), p)
}

/// The containment test itself, free of winit types so it can be tested.
pub fn rect_contains(rect: (i32, i32, u32, u32), p: (i32, i32)) -> bool {
    let (x, y, w, h) = rect;
    p.0 >= x && p.0 < x + w as i32 && p.1 >= y && p.1 < y + h as i32
}

#[cfg(test)]
mod tests {
    /// A real three-monitor layout: DP-2 5120x2160 at +2160+1027, DP-0
    /// 3440x1440 at +7280+1435, DP-4 2160x3840 at +0+0.
    const MONITORS: [(i32, i32, u32, u32); 3] =
        [(2160, 1027, 5120, 2160), (7280, 1435, 3440, 1440), (0, 0, 2160, 3840)];

    fn monitor_at(p: (i32, i32)) -> Option<usize> {
        MONITORS.iter().position(|r| super::rect_contains(*r, p))
    }

    #[test]
    fn window_centre_picks_the_monitor_it_sits_on() {
        // A window dragged onto DP-0 must resolve to monitor 1, not the
        // one it was created on — this is what W-to-fullscreen relies on.
        assert_eq!(monitor_at((8261, 2369)), Some(1));
        assert_eq!(monitor_at((4000, 2000)), Some(0));
        assert_eq!(monitor_at((1000, 1000)), Some(2));
    }

    #[test]
    fn monitor_rects_are_half_open_so_borders_are_unambiguous() {
        // DP-4 spans x 0..2160; DP-2 starts at x 2160. The shared column
        // belongs to exactly one of them.
        assert_eq!(monitor_at((2159, 1500)), Some(2));
        assert_eq!(monitor_at((2160, 1500)), Some(0));
        // DP-2's right edge is 2160+5120 = 7280, where DP-0 begins.
        assert_eq!(monitor_at((7279, 2000)), Some(0));
        assert_eq!(monitor_at((7280, 2000)), Some(1));
    }

    #[test]
    fn a_point_off_every_monitor_resolves_to_none() {
        assert_eq!(monitor_at((20000, 20000)), None);
        assert_eq!(monitor_at((-5, -5)), None);
    }
}
