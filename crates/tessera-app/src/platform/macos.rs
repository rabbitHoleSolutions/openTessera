//! macOS: AppKit through winit, plus the two things winit 0.29 doesn't
//! expose — Finder's open-document event and the app-wide menu bar/Dock
//! state that fullscreen walls share.

use std::ffi::CStr;
use std::os::raw::c_char;
use std::path::PathBuf;
use std::sync::Mutex;

use objc2::runtime::{AnyClass, AnyObject, Sel};
use objc2::{class, msg_send, sel};
use winit::dpi::Position;
use winit::event::{ElementState, KeyEvent, MouseButton};
use winit::keyboard::{Key, ModifiersState};
use winit::monitor::MonitorHandle;
use winit::platform::macos::WindowExtMacOS;
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::window::{Window, WindowBuilder};

use super::Chord;

/// VideoToolbox, zero-copy: mpv maps the decoder's IOSurfaces straight into
/// GL textures. The NVIDIA interop crash that made Linux copy doesn't exist
/// here.
pub const DEFAULT_HWDEC: &str = "videotoolbox";

/// The `--hwdec` paragraph of `--help`, continuation lines pre-indented.
pub const HWDEC_HELP: &str = "mpv hardware decode mode (default videotoolbox —
                     zero-copy VideoToolbox decode; videotoolbox-copy or
                     no if a codec misbehaves)";

/// No tiling WM to fight: fullscreen here is our own frame change, and it
/// lands where it was asked to.
pub const WM_SETTLE: bool = false;

/// A file opened from Finder arrives as an Apple Event during the event
/// loop's launch, not on the command line — so startup tiles wait for the
/// first tick, when it is known whether the queue is the saved one or
/// "just this file".
pub const DEFER_STARTUP_TILES: bool = true;

/// Nothing to add: a bundle's identity lives in its Info.plist, and
/// per-window icons don't exist on macOS (the Dock shows the bundle's).
pub fn window_builder(wb: WindowBuilder) -> WindowBuilder {
    wb
}

/// A window placed over `monitor`, made fullscreen by
/// [`finish_fullscreen_window`] once it exists. winit's builder-time
/// fullscreen is the native kind, which gives each wall its own Space and a
/// slide animation — and several windows making that transition at once
/// (`--max`) is exactly where it gets unreliable.
pub fn fullscreen_window_builder(wb: WindowBuilder, monitor: &MonitorHandle) -> WindowBuilder {
    let sf = monitor.scale_factor();
    wb.with_position(Position::Logical(monitor.position().to_logical(sf)))
        .with_inner_size(winit::dpi::Size::Logical(monitor.size().to_logical(sf)))
}

pub fn finish_fullscreen_window(window: &Window, monitor: &MonitorHandle) {
    enter_fullscreen(window, Some(monitor.clone()));
}

/// "Simple" fullscreen: the window is re-framed to cover the monitor on the
/// desktop it is already on — no new Space, no animation — which is the
/// Linux borderless behaviour the rest of the app assumes.
pub fn enter_fullscreen(window: &Window, monitor: Option<MonitorHandle>) {
    if window.fullscreen().is_some() {
        // Native fullscreen (the green button). It is a wall already.
        return;
    }
    let target = monitor.or_else(|| window.current_monitor());
    if window.simple_fullscreen() {
        if target.is_none() || window.current_monitor() == target {
            return;
        }
        // A simple-fullscreen window can't be moved; come out first.
        window.set_simple_fullscreen(false);
    }
    // set_simple_fullscreen covers whatever screen the window is on, so it
    // has to be put on the right one first. Positioned in logical points,
    // which is what Cocoa's global coordinates are: a physical position
    // would be converted with the window's *current* scale factor, and land
    // on the wrong display when a Retina and a 1x monitor are both attached.
    if let Some(m) = &target {
        let sf = m.scale_factor();
        window.set_outer_position(Position::Logical(m.position().to_logical(sf)));
    }
    window.set_simple_fullscreen(true);
}

pub fn exit_fullscreen(window: &Window) {
    if window.fullscreen().is_some() {
        window.set_fullscreen(None);
    }
    window.set_simple_fullscreen(false);
}

pub fn is_fullscreen(window: &Window) -> bool {
    window.simple_fullscreen() || window.fullscreen().is_some()
}

const PRESENTATION_AUTO_HIDE_DOCK: usize = 1 << 0;
const PRESENTATION_AUTO_HIDE_MENU_BAR: usize = 1 << 2;
const PRESENTATION_FULL_SCREEN: usize = 1 << 10;

/// Hides the menu bar and Dock while any wall is fullscreen, and brings them
/// back once none is. Called every frame.
///
/// winit can't be left to do this: the presentation options are app-wide,
/// but each window saves "the options before me" on entering simple
/// fullscreen and restores them on leaving. With two walls, whichever
/// leaves last restores the *hidden* state — the menu bar stays gone with
/// nothing fullscreen — and a wall that is simply closed restores nothing.
/// So the truth is recomputed from the windows themselves.
pub fn sync_fullscreen_chrome(any_fullscreen: bool) {
    let want = if any_fullscreen {
        PRESENTATION_AUTO_HIDE_DOCK | PRESENTATION_AUTO_HIDE_MENU_BAR
    } else {
        0
    };
    unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        let have: usize = msg_send![app, presentationOptions];
        // Native fullscreen owns the options while it is up; AppKit throws
        // if they are changed underneath it.
        if have & PRESENTATION_FULL_SCREEN != 0 || have == want {
            return;
        }
        let _: () = msg_send![app, setPresentationOptions: want];
    }
}

/// The kernel's per-process ceiling. The hard limit reads as `unlimited`,
/// and setrlimit refuses that outright — so raising to it, as Linux does,
/// fails silently and leaves a Dock-launched app at launchd's 256.
pub fn fd_limit_cap() -> Option<libc::rlim_t> {
    let mut v: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>();
    let r = unsafe {
        libc::sysctlbyname(
            c"kern.maxfilesperproc".as_ptr(),
            &mut v as *mut libc::c_int as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    // OPEN_MAX: the documented ceiling if the sysctl can't be read.
    Some(if r == 0 && v > 0 { v as libc::rlim_t } else { 10240 })
}

/// There is no /proc; /dev/fd lists the same thing.
pub fn fd_count() -> usize {
    std::fs::read_dir("/dev/fd").map(|d| d.count()).unwrap_or(0)
}

/// The UI scale multiplies the display's own: a Retina display already
/// renders two pixels per point, and 1.0 there should mean "normal size",
/// not "half size". On a 1x display this is the same absolute scale Linux
/// uses, so per-monitor values mean the same thing on both.
pub fn apply_ui_scale(ctx: &egui::Context, scale: f32) {
    ctx.set_zoom_factor(scale);
}

/// Cmd chords belong to the OS, never to the wall: Cmd+W closes the window
/// and Ctrl+Cmd+F is the standard fullscreen toggle. Anything else with Cmd
/// held is swallowed so it can't be read as the bare letter. (Cmd+Q, Cmd+H
/// and friends are taken by winit's default menu before we see them.)
pub fn key_chord(event: &KeyEvent, mods: ModifiersState) -> Option<Chord> {
    if !mods.super_key() {
        return None;
    }
    if event.state != ElementState::Pressed || event.repeat {
        return Some(Chord::Swallow);
    }
    // With Ctrl held the logical key is a control character, so match the
    // key the layout would have produced unmodified.
    match event.key_without_modifiers().as_ref() {
        Key::Character(c) if c.eq_ignore_ascii_case("w") && !mods.control_key() => {
            Some(Chord::CloseWindow)
        }
        Key::Character(c) if c.eq_ignore_ascii_case("f") && mods.control_key() => {
            Some(Chord::ToggleFullscreen)
        }
        _ => Some(Chord::Swallow),
    }
}

/// Ctrl-click is a right-click on a Mac.
pub fn is_secondary_click(button: MouseButton, mods: ModifiersState) -> bool {
    button == MouseButton::Left && mods.control_key()
}

/// Paths Finder asked us to open, waiting for the event loop to pick up.
static OPENED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Teaches winit's application delegate `application:openURLs:`, which is
/// how AppKit hands over files opened from Finder ("Open With", a double
/// click, a drop on the Dock icon) — both the one that launched the app and
/// any opened while it runs. winit 0.29 has no hook for it, so the method is
/// added to its delegate class at runtime.
///
/// Must run after `EventLoop::new()` (which registers the class and installs
/// the delegate) and before `run()` (when the launch event is delivered).
pub fn install_open_handler() {
    let Some(cls) = AnyClass::get("WinitApplicationDelegate") else {
        eprintln!("warning: no winit app delegate — files opened from Finder will be ignored");
        return;
    };
    type OpenUrls = unsafe extern "C" fn(*mut AnyObject, Sel, *mut AnyObject, *mut AnyObject);
    let imp: OpenUrls = open_urls;
    unsafe {
        let added = objc2::ffi::class_addMethod(
            cls as *const AnyClass as *mut objc2::ffi::objc_class,
            sel!(application:openURLs:).as_ptr(),
            Some(std::mem::transmute::<OpenUrls, unsafe extern "C" fn()>(imp)),
            c"v@:@@".as_ptr(),
        );
        if added == objc2::ffi::NO {
            eprintln!("warning: could not hook application:openURLs: — Finder opens will be ignored");
            return;
        }
        // AppKit may cache which optional methods its delegate implements
        // when the delegate is set, so set it again now that there is one
        // more. winit holds its own strong reference, so the brief nil is
        // harmless.
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        let delegate: *mut AnyObject = msg_send![app, delegate];
        let _: () = msg_send![app, setDelegate: std::ptr::null_mut::<AnyObject>()];
        let _: () = msg_send![app, setDelegate: delegate];
    }
}

unsafe extern "C" fn open_urls(_this: *mut AnyObject, _cmd: Sel, _app: *mut AnyObject, urls: *mut AnyObject) {
    if urls.is_null() {
        return;
    }
    let count: usize = msg_send![urls, count];
    let mut paths = Vec::with_capacity(count);
    for i in 0..count {
        let url: *mut AnyObject = msg_send![urls, objectAtIndex: i];
        let is_file: bool = msg_send![url, isFileURL];
        if !is_file {
            continue;
        }
        let path: *mut AnyObject = msg_send![url, path];
        if path.is_null() {
            continue;
        }
        let utf8: *const c_char = msg_send![path, UTF8String];
        if !utf8.is_null() {
            paths.push(PathBuf::from(CStr::from_ptr(utf8).to_string_lossy().into_owned()));
        }
    }
    if let Ok(mut opened) = OPENED.lock() {
        opened.extend(paths);
    }
}

/// Drains the paths Finder has handed over since the last call.
pub fn take_opened_paths() -> Vec<PathBuf> {
    OPENED.lock().map(|mut v| std::mem::take(&mut *v)).unwrap_or_default()
}

/// AppKit's point/size/rect, laid out as CoreGraphics declares them (all
/// `f64` on 64-bit). Declared here because objc2 0.4 has no Foundation
/// types of its own, and these are the only three we need.
#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

unsafe impl objc2::Encode for CGPoint {
    const ENCODING: objc2::Encoding = objc2::Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
}

unsafe impl objc2::Encode for CGSize {
    const ENCODING: objc2::Encoding = objc2::Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
}

unsafe impl objc2::Encode for CGRect {
    const ENCODING: objc2::Encoding = objc2::Encoding::Struct("CGRect", &[CGPoint::ENCODING, CGSize::ENCODING]);
}

/// Where the pointer is over `window` right now, in physical pixels from
/// the top-left of its content — the frame `CursorMoved` reports in — asked
/// of AppKit rather than remembered from the last event.
///
/// A file dragged in from Finder crosses the window without a single
/// mouse-moved event: AppKit is running a drag session, and winit 0.29
/// implements `draggingEntered`/`performDragOperation` but not
/// `draggingUpdated`, so `DroppedFile` arrives with no position and the
/// event loop's cursor is wherever the pointer was before the drag — often
/// outside the window. Without this a drop always landed on the first tile.
pub fn pointer_in_window(window: &Window) -> Option<(f32, f32)> {
    use raw_window_handle::{HasRawWindowHandle, RawWindowHandle};
    let RawWindowHandle::AppKit(handle) = window.raw_window_handle() else { return None };
    let view = handle.ns_view as *mut AnyObject;
    if view.is_null() {
        return None;
    }
    unsafe {
        let ns_window: *mut AnyObject = msg_send![view, window];
        if ns_window.is_null() {
            return None;
        }
        // Window coordinates (origin bottom-left), then the content view's.
        let p: CGPoint = msg_send![ns_window, mouseLocationOutsideOfEventStream];
        let p: CGPoint = msg_send![view, convertPoint: p, fromView: std::ptr::null_mut::<AnyObject>()];
        let bounds: CGRect = msg_send![view, bounds];
        let flipped: bool = msg_send![view, isFlipped];
        let y = if flipped { p.y } else { bounds.size.height - p.y };
        let scale = window.scale_factor();
        Some(((p.x * scale) as f32, (y * scale) as f32))
    }
}

/// Floats a window above every other one (true) or puts it back among
/// them. The caller has already set its decorations. A window level is
/// the whole story here: there is no tiling WM to talk out of tiling it,
/// so the window never leaves and nothing has to wait (false).
pub fn set_floating(window: &Window, floating: bool) -> bool {
    window.set_window_level(if floating {
        winit::window::WindowLevel::AlwaysOnTop
    } else {
        winit::window::WindowLevel::Normal
    });
    false
}
