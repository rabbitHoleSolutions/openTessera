//! Keyboard seeking, coalesced: a held arrow accumulates into a few real seeks
//! rather than one mpv seek per key repeat.

use std::time::{Duration, Instant};

use crate::overlay;
use crate::screen::Screen;

/// Seeks the tile under the cursor. The arrows used to move every tile on
/// the screen at once, which is almost never what you mean on a wall — the
/// controls act on the tile you are pointing at, and so should the keys.
/// With the cursor over no tile, nothing moves.
/// Quiet time after the last arrow keypress before the accumulated seek is
/// issued. Below X's auto-repeat interval (~40ms), so a held key keeps
/// accumulating and a released one flushes on the next frame.
pub const SEEK_SETTLE: Duration = Duration::from_millis(200);

/// Longest a held arrow key goes without the video actually moving. The
/// transport shows the target position the whole time, so this only governs
/// how often mpv is asked to do the expensive part.
pub const SEEK_MAX_HOLD: Duration = Duration::from_millis(600);

/// Deltas smaller than this are not worth a demuxer flush.
pub const SEEK_EPSILON: f64 = 0.05;

/// What to do with an accumulating seek this frame. Split out from
/// [`flush_seek`] so the timing rules can be tested without a display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekStep {
    /// Keep accumulating; nothing to issue yet.
    Wait,
    /// Issue the seek; the accumulator is finished.
    SeekAndEnd,
    /// Issue the seek but keep accumulating — the key is still down and a
    /// long hold should not go silent.
    SeekAndHold,
    /// Nothing accumulated and the key is up: drop the accumulator.
    End,
    /// Nothing accumulated but the key is still down. Happens when the
    /// long-hold timer comes round before another key repeat has landed;
    /// seeking by zero is a real flush for no movement.
    Hold,
}

/// Adds to the seek the user is spelling out on the tile under the cursor,
/// rather than seeking now. See [`overlay::PendingSeek`] for why.
pub fn queue_seek(
    screens: &mut [Screen],
    screen_idx: usize,
    overlay: &mut overlay::OverlayState,
    secs: f64,
) {
    let screen = &screens[screen_idx];
    let monitor_index = screen.monitor_index;
    let Some((x, y)) = screen.cursor else { return };
    let Some(id) = screen.tile_at(x, y) else {
        println!("screen {monitor_index}: seek ignored — cursor is not over a tile");
        return;
    };
    // The cursor moved to another tile mid-hold: whatever is pending belongs
    // to the tile it was aimed at, so it goes out before the new one starts.
    if overlay.pending_seek.is_some_and(|ps| ps.tile_id != id) {
        flush_seek(screens, overlay, true);
    }
    let now = Instant::now();
    match overlay.pending_seek {
        Some(ref mut ps) => {
            ps.delta += secs;
            ps.last_input = now;
        }
        None => {
            overlay.pending_seek = Some(overlay::PendingSeek {
                tile_id: id,
                delta: secs,
                last_input: now,
                last_flush: now,
            })
        }
    }
}

pub fn seek_step(delta: f64, since_input: Duration, since_flush: Duration, force: bool) -> SeekStep {
    let settled = since_input >= SEEK_SETTLE;
    let held_long = since_flush >= SEEK_MAX_HOLD;
    if !(force || settled || held_long) {
        return SeekStep::Wait;
    }
    let done = force || settled;
    match (delta.abs() < SEEK_EPSILON, done) {
        (true, true) => SeekStep::End,
        (true, false) => SeekStep::Hold,
        (false, true) => SeekStep::SeekAndEnd,
        (false, false) => SeekStep::SeekAndHold,
    }
}

/// Issues the accumulated seek when the key has settled, when a long hold
/// has gone too long without the video moving, or when `force` (key
/// released). Called once a frame.
pub fn flush_seek(screens: &mut [Screen], overlay: &mut overlay::OverlayState, force: bool) {
    let Some(ps) = overlay.pending_seek else { return };
    let now = Instant::now();
    let step = seek_step(
        ps.delta,
        now.duration_since(ps.last_input),
        now.duration_since(ps.last_flush),
        force,
    );
    if step == SeekStep::Wait {
        return;
    }
    if matches!(step, SeekStep::SeekAndEnd | SeekStep::SeekAndHold) {
        for screen in screens.iter_mut() {
            let monitor_index = screen.monitor_index;
            if let Some(tile) = screen.tiles.iter_mut().find(|t| t.id == ps.tile_id) {
                let from = tile.time_pos();
                tile.seek(ps.delta, false);
                // Show the destination straight away. mpv overwrites this
                // with the real position within a frame or two, and without
                // it the readout snaps back to `from` until the seek lands.
                if from >= 0.0 {
                    tile.last_time_pos = (from + ps.delta).max(0.0);
                }
                println!(
                    "screen {monitor_index} tile {}: seek {}{:.0}s from {}",
                    ps.tile_id,
                    if ps.delta >= 0.0 { "+" } else { "" },
                    ps.delta,
                    fmt_time(from)
                );
                break;
            }
        }
    }
    overlay.pending_seek = match step {
        SeekStep::SeekAndEnd | SeekStep::End => None,
        // Still held: start a fresh delta and keep the accumulator alive.
        _ => Some(overlay::PendingSeek { delta: 0.0, last_flush: now, ..ps }),
    };
}

pub fn fmt_time(t: f64) -> String {
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
    #[test]
    fn held_arrow_coalesces_into_few_seeks() {
        use super::{seek_step, SeekStep, SEEK_MAX_HOLD, SEEK_SETTLE};
        use std::time::Duration;
        let none = Duration::ZERO;

        // Mid-repeat: input is still arriving and the hold is young.
        assert_eq!(seek_step(30.0, none, none, false), SeekStep::Wait);

        // Key released: go now rather than waiting out the settle window.
        assert_eq!(seek_step(30.0, none, none, true), SeekStep::SeekAndEnd);

        // Quiet for the settle window: the user let go (or tapped once).
        assert_eq!(seek_step(10.0, SEEK_SETTLE, none, false), SeekStep::SeekAndEnd);

        // Still held, but the video has not moved for too long.
        assert_eq!(seek_step(120.0, none, SEEK_MAX_HOLD, false), SeekStep::SeekAndHold);

        // A long-hold tick that arrives before any new repeat must not turn
        // into a zero-length seek — that is a demuxer flush for no movement.
        assert_eq!(seek_step(0.0, none, SEEK_MAX_HOLD, false), SeekStep::Hold);
        assert_eq!(seek_step(0.0, SEEK_SETTLE, none, false), SeekStep::End);
    }
}
