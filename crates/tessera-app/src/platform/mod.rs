//! Everything that differs between operating systems, behind one surface.
//!
//! The rest of the app is written against these functions and constants and
//! never names an OS itself. Each backend provides exactly the same items:
//!
//! - `linux.rs` — X11/Wayland (and other free unixes). The original target;
//!   these are the behaviours the app was built and tuned on, moved here
//!   verbatim rather than reinterpreted.
//! - `macos.rs` — AppKit via winit, plus the few Cocoa calls winit doesn't
//!   expose (Finder open-document events, app-wide fullscreen chrome).
//!
//! If a new difference turns up, add it here as a named item in *both*
//! backends — a `cfg` sprinkled into main.rs is how the next port breaks.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(not(target_os = "macos"))]
mod linux;
#[cfg(not(target_os = "macos"))]
pub use linux::*;

/// An OS-level keyboard shortcut that outranks the wall's own hotkeys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chord {
    /// Close the focused window (the last one quits), exactly as if its
    /// close button had been pressed.
    CloseWindow,
    /// Toggle the window between windowed and fullscreen — what W does.
    ToggleFullscreen,
    /// Belongs to the OS or the menu bar; the wall must ignore it rather
    /// than read the letter as a hotkey.
    Swallow,
}

/// Raises the soft fd limit as far as this OS lets it go. Every tile's mpv
/// holds dozens of fds (stream socket, demuxer cache, event pipes, and —
/// with audio — an audio-server context), so a full wall easily exhausts a
/// default soft limit. That doesn't just fail loads: GL fence creation needs
/// an fd too, and exhausting them wedged the NVIDIA DRM driver hard enough
/// to take the whole desktop down (2026-08-26).
pub fn raise_fd_limit() {
    unsafe {
        let mut rl: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) != 0 {
            return;
        }
        let target = match fd_limit_cap() {
            Some(cap) => rl.rlim_max.min(cap),
            None => rl.rlim_max,
        };
        if rl.rlim_cur >= target {
            return;
        }
        let old = rl.rlim_cur;
        rl.rlim_cur = target;
        if libc::setrlimit(libc::RLIMIT_NOFILE, &rl) == 0 {
            let show = |v: libc::rlim_t| {
                if v == libc::RLIM_INFINITY { "unlimited".to_string() } else { v.to_string() }
            };
            println!("fd limit: raised soft limit {} -> {}", show(old), show(target));
        }
    }
}
