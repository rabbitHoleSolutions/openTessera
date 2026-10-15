//! Wall actions: everything a click, a hotkey or the API can do to tiles, the
//! queue and the decode budget. They take a [`Wall`] — see its doc comment.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use glutin::prelude::*;
use tessera_core::{Admission, DecodeBudget, PlaybackQueue, QueueOrder};

use crate::overlay::{self, OverlayAction, OverlayState};
use crate::screen::{Screen, ScreenRole};
use crate::seek::fmt_time;
use crate::tile::{Tile, TileOptions};
use crate::windowing::{toggle_fullscreen, toggle_window_mode, window_rect};
use crate::notify;

/// Everything a wall action mutates, in one place.
///
/// Nearly every action needs most of it: pause/resume goes through the
/// global [`DecodeBudget`], adding a tile pulls from the shared
/// [`PlaybackQueue`] and needs the mpv library plus the tile options, and
/// anything that fails reports through the [`OverlayState`]. Passing them
/// individually had `handle_overlay_action` at eleven parameters.
///
/// Borrowed, not owned — build one for as long as the actions run, then let
/// it go so the frame's other code can use the same state.
pub struct Wall<'a> {
    pub screens: &'a mut Vec<Screen>,
    pub budget: &'a mut DecodeBudget,
    /// The one queue every window's tiles are fed from.
    pub queue: &'a mut PlaybackQueue,
    /// The configured sources, so a delete can be routed to the server the
    /// item actually came from.
    pub sources: &'a [tessera_core::sources::Source],
    pub overlay: &'a mut OverlayState,
    pub mpv_lib: &'a std::sync::Arc<tessera_mpv::MpvLib>,
    pub tile_opts: &'a TileOptions,
    pub next_tile_id: &'a mut usize,
    /// The --tiles cap, per window.
    pub max_tiles: usize,
    /// Deferred WM re-anchor check; armed by anything that moves a window.
    pub wm_settle_at: &'a mut Option<(Instant, u32)>,
}

/// Executes one queued overlay interaction. `screen_idx` is the screen whose
/// overlay produced the action.
pub fn handle_overlay_action(wall: &mut Wall, action: OverlayAction, screen_idx: usize) {
    match action {
        OverlayAction::TogglePause { tile_id } => {
            let playing = wall.screens
                .iter()
                .any(|s| s.tiles.iter().any(|t| t.id == tile_id && !t.is_paused()));
            if playing {
                pause_and_release(wall.screens, wall.budget, tile_id, "user pause");
            } else {
                resume_tile(wall.screens, wall.budget, tile_id);
            }
        }
        OverlayAction::MuteButton { tile_id } => {
            wall.overlay.solo_id = None;
            for screen in wall.screens.iter_mut() {
                if let Some(tile) = screen.tiles.iter_mut().find(|t| t.id == tile_id) {
                    if tile.audio_enabled() {
                        let muted = tile.toggle_mute();
                        println!(
                            "screen {} tile {tile_id}: {}",
                            screen.monitor_index,
                            if muted { "muted".to_string() } else { format!("audio on (vol {}%)", tile.volume()) }
                        );
                    } else {
                        notify(wall.overlay, "audio is off — turn it on in the config tab (C)".into());
                    }
                    return;
                }
            }
        }
        OverlayAction::Solo { tile_id } => {
            // Audio focus: unmute this tile, mute every other stream on
            // every screen.
            let mut target_ok = false;
            let mut muted_others = 0usize;
            for screen in wall.screens.iter_mut() {
                for tile in screen.tiles.iter_mut() {
                    if !tile.audio_enabled() {
                        continue;
                    }
                    if tile.id == tile_id {
                        target_ok = true;
                        tile.set_muted(false);
                        println!(
                            "screen {} tile {tile_id}: solo — audio on (vol {}%)",
                            screen.monitor_index,
                            tile.volume()
                        );
                    } else if !tile.is_muted() {
                        tile.set_muted(true);
                        muted_others += 1;
                    }
                }
            }
            if !target_ok {
                notify(wall.overlay, "audio is off — turn it on in the config tab (C)".into());
            } else {
                if muted_others > 0 {
                    println!("solo: muted {muted_others} other stream(s)");
                }
                wall.overlay.solo_id = Some(tile_id);
            }
        }
        OverlayAction::GoLive { tile_id } => {
            for screen in wall.screens.iter_mut() {
                if let Some(tile) = screen.tiles.iter_mut().find(|t| t.id == tile_id) {
                    tile.go_live();
                    println!("screen {} tile {tile_id}: rejoined the live stream", screen.monitor_index);
                    return;
                }
            }
        }
        OverlayAction::TogglePin { tile_id } => {
            if let Some(si) = wall.screens.iter().position(|s| s.tiles.iter().any(|t| t.id == tile_id)) {
                toggle_pin(wall.screens, si, tile_id);
            }
        }
        OverlayAction::SetVolume { tile_id, volume } => {
            for screen in wall.screens.iter_mut() {
                if let Some(tile) = screen.tiles.iter_mut().find(|t| t.id == tile_id) {
                    tile.set_volume(volume);
                    println!("screen {} tile {tile_id}: vol {}%", screen.monitor_index, tile.volume());
                    return;
                }
            }
        }
        OverlayAction::ToggleTrackPanel { tile_id, kind } => {
            wall.overlay.track_panel = if wall.overlay.track_panel == Some((tile_id, kind)) {
                None
            } else {
                Some((tile_id, kind))
            };
            // Re-read the track cache so the row reflects the current
            // selection (covers files loaded since the last refresh).
            for screen in wall.screens.iter_mut() {
                if screen.tiles.iter().any(|t| t.id == tile_id) {
                    let monitor_index = screen.monitor_index;
                    if let Some(tile) = screen.tiles.iter_mut().find(|t| t.id == tile_id) {
                        tile.refresh_tracks();
                        println!(
                            "screen {monitor_index} tile {tile_id}: {} audio, {} subtitle track(s)",
                            tile.audio_tracks.len(),
                            tile.subtitle_tracks.len()
                        );
                    }
                    break;
                }
            }
        }
        OverlayAction::SetAudioTrack { tile_id, track } => {
            for screen in wall.screens.iter_mut() {
                if let Some(tile) = screen.tiles.iter_mut().find(|t| t.id == tile_id) {
                    tile.set_audio_track(track);
                    let label = track
                        .map(|id| {
                            tile.audio_tracks
                                .iter()
                                .find(|t| t.id == id)
                                .map(|t| t.describe())
                                .unwrap_or_else(|| format!("#{id}"))
                        })
                        .unwrap_or_else(|| "off".into());
                    println!("screen {} tile {tile_id}: audio track -> {label}", screen.monitor_index);
                    return;
                }
            }
        }
        OverlayAction::SetSubtitleTrack { tile_id, track } => {
            for screen in wall.screens.iter_mut() {
                if let Some(tile) = screen.tiles.iter_mut().find(|t| t.id == tile_id) {
                    tile.set_subtitle_track(track);
                    let label = track
                        .map(|id| {
                            tile.subtitle_tracks
                                .iter()
                                .find(|t| t.id == id)
                                .map(|t| t.describe())
                                .unwrap_or_else(|| format!("#{id}"))
                        })
                        .unwrap_or_else(|| "off".into());
                    println!("screen {} tile {tile_id}: subtitle track -> {label}", screen.monitor_index);
                    return;
                }
            }
        }
        OverlayAction::SeekTo { tile_id, to } => {
            for screen in wall.screens.iter_mut() {
                if let Some(tile) = screen.tiles.iter_mut().find(|t| t.id == tile_id) {
                    tile.seek(to, true);
                    println!(
                        "screen {} tile {tile_id}: seek to {} ({})",
                        screen.monitor_index,
                        fmt_time(to),
                        tile.item.title
                    );
                    return;
                }
            }
        }
        OverlayAction::Skip { tile_id, secs } => {
            for screen in wall.screens.iter_mut() {
                if let Some(tile) = screen.tiles.iter_mut().find(|t| t.id == tile_id) {
                    // NB: time-pos is read before the seek — mpv applies the
                    // command asynchronously, so a post-command read can be
                    // stale.
                    let from = tile.time_pos();
                    tile.seek(secs, false);
                    println!(
                        "screen {} tile {tile_id}: seek {}{secs:.0}s from {} ({})",
                        screen.monitor_index,
                        if secs >= 0.0 { "+" } else { "" },
                        fmt_time(from),
                        tile.item.title
                    );
                    return;
                }
            }
        }
        OverlayAction::Next { tile_id } => {
            tile_next(wall.screens, wall.queue, tile_id, true);
        }
        OverlayAction::Prev { tile_id } => {
            tile_next(wall.screens, wall.queue, tile_id, false);
        }
        OverlayAction::PlayOnTile { tile_id, index } => {
            for si in 0..wall.screens.len() {
                if !wall.screens[si].tiles.iter().any(|t| t.id == tile_id) {
                    continue;
                }
                let monitor_index = wall.screens[si].monitor_index;
                let item = wall.queue.items().get(index).cloned();
                if let Some(item) = item {
                    wall.queue.remove_pending(index);
                    if let Some(tile) = wall.screens[si].tiles.iter_mut().find(|t| t.id == tile_id) {
                        tile.item = item.clone();
                        if let Err(e) = tile.load(&item) {
                            eprintln!("screen {monitor_index} tile {tile_id}: load failed: {e}");
                        } else {
                            println!("screen {monitor_index} tile {tile_id}: now playing \"{}\"", item.title);
                        }
                    }
                }
                break;
            }
        }
        OverlayAction::ToggleHelp => {
            // The panel opens on the screen whose button was clicked; a
            // click on another screen's button moves it there.
            let win = wall.screens[screen_idx].window.id();
            wall.overlay.help_open = if wall.overlay.help_open == Some(win) { None } else { Some(win) };
        }
        OverlayAction::QueueAdd { items } => {
            let n = items.len();
            wall.queue.add_items(items);
            println!("library: added {n} item(s) to the queue ({} total)", wall.queue.len());
        }
        OverlayAction::QueueClear => {
            let n = wall.queue.len();
            wall.queue.clear();
            println!("queue: cleared ({n} item(s))");
            notify(wall.overlay, format!("queue cleared ({n} item(s))"));
        }
        OverlayAction::QueueAppend { item } => {
            println!("library: queued \"{}\" ({} total)", item.title, wall.queue.len() + 1);
            wall.queue.append_items(vec![item]);
        }
        OverlayAction::PlayNow { item } => {
            // The first wall window's first tile — a library window has no
            // tiles of its own to play on.
            let target = wall
                .screens
                .iter()
                .find(|s| s.role == ScreenRole::Wall && !s.tiles.is_empty())
                .map(|s| (s.monitor_index, s.tiles[0].id));
            match target {
                Some((monitor_index, tile_id)) => {
                    for screen in wall.screens.iter_mut() {
                        if let Some(tile) = screen.tiles.iter_mut().find(|t| t.id == tile_id) {
                            tile.item = item.clone();
                            if let Err(e) = tile.load(&item) {
                                eprintln!("screen {monitor_index} tile {tile_id}: load failed: {e}");
                            } else {
                                println!(
                                    "library: screen {monitor_index} tile {tile_id}: now playing \"{}\"",
                                    item.title
                                );
                            }
                            break;
                        }
                    }
                }
                // An empty wall window gets a tile rather than an error —
                // the item is not queued, so the queue is not advanced.
                None => match wall.screens.iter().position(|s| s.role == ScreenRole::Wall) {
                    Some(si) => {
                        if can_add_tile(wall, si) {
                            spawn_tile(wall, si, item);
                        }
                    }
                    None => notify(wall.overlay, "no wall window to play on — open one with + first".into()),
                },
            }
        }
        // Intercepted by the event loop before they get here: opening a
        // window needs the EventLoopWindowTarget and the shared GL config,
        // and must happen outside the loop iterating `screens` by index.
        OverlayAction::NewWindow | OverlayAction::NewLibraryWindow { .. } => {}
        OverlayAction::ToggleSettings => {
            let win = wall.screens[screen_idx].window.id();
            wall.overlay.settings_open = if wall.overlay.settings_open == Some(win) { None } else { Some(win) };
            println!(
                "settings panel {} (screen {})",
                if wall.overlay.settings_open.is_some() { "open" } else { "closed" },
                wall.screens[screen_idx].monitor_index
            );
        }
        OverlayAction::ToggleWindowMode => {
            if toggle_window_mode(wall.screens, screen_idx) {
                *wall.wm_settle_at = Some((Instant::now() + Duration::from_millis(600), 4));
            }
        }
        OverlayAction::AddPanel => {
            add_tile_action(wall, screen_idx);
        }
        OverlayAction::RemovePanel { tile_id } => {
            remove_tile_action(wall.screens, wall.budget, wall.overlay, tile_id);
        }
        OverlayAction::FullscreenToggle { tile_id } => {
            if wall.screens[screen_idx].zoomed.is_some() {
                toggle_fullscreen(wall.screens, screen_idx, None);
            } else {
                toggle_fullscreen(wall.screens, screen_idx, Some(tile_id));
            }
        }
        OverlayAction::DeleteCurrent { tile_id } => {
            delete_current(wall.screens, wall.queue, wall.sources, screen_idx, tile_id, wall.overlay);
        }
        OverlayAction::QueueRemove { index } => {
            let monitor_index = wall.screens[screen_idx].monitor_index;
            let q = &mut *wall.queue;
            if let Some(m) = q.items().get(index).cloned() {
                q.remove_pending(index);
                println!("screen {monitor_index}: queue: removed \"{}\" ({} left)", m.title, q.len());
            }
        }
        OverlayAction::QueueReorder { from, to } => {
            let monitor_index = wall.screens[screen_idx].monitor_index;
            wall.queue.move_pending(from, to);
            println!("screen {monitor_index}: queue: reordered");
        }
        OverlayAction::QueueShuffle => {
            let monitor_index = wall.screens[screen_idx].monitor_index;
            wall.queue.set_order(QueueOrder::Shuffle);
            println!("screen {monitor_index}: queue: shuffled");
        }
        OverlayAction::QueueSortName => {
            let monitor_index = wall.screens[screen_idx].monitor_index;
            wall.queue.set_order(QueueOrder::Name);
            println!("screen {monitor_index}: queue: sorted by name");
        }
        OverlayAction::PauseAll => {
            let ids: Vec<usize> = wall.screens[screen_idx].tiles.iter().map(|t| t.id).collect();
            for id in ids {
                pause_and_release(wall.screens, wall.budget, id, "user pause");
            }
        }
        OverlayAction::ResumeAll => {
            let ids: Vec<usize> = wall.screens[screen_idx].tiles.iter().map(|t| t.id).collect();
            for id in ids {
                resume_tile(wall.screens, wall.budget, id);
            }
        }
    }
}

/// Adds a tile to a screen from its queue — the transport "+" button and
/// the "+" hotkey. Enforces the zoomed and --tiles limits.
pub fn add_tile_action(wall: &mut Wall, screen_idx: usize) {
    if !can_add_tile(wall, screen_idx) {
        return;
    }
    // Peek at what the next advance would yield rather than taking it: a
    // rejected add used to consume a queue position anyway, so pressing "+"
    // against a full budget silently skipped an item. The cursor moves only
    // once the tile actually exists, below.
    let item = match wall.queue.upcoming(1).first().map(|m| (*m).clone()) {
        Some(m) => m,
        None => {
            notify(wall.overlay, "not adding: queue is empty".into());
            return;
        }
    };
    if spawn_tile(wall, screen_idx, item) {
        // Only now: the item is really playing somewhere.
        wall.queue.advance();
    }
}

/// Whether this screen can take another tile, explaining (as a toast) why
/// not: browser windows have none, a zoomed wall is showing one, and
/// --tiles caps the rest.
pub fn can_add_tile(wall: &mut Wall, screen_idx: usize) -> bool {
    if wall.screens[screen_idx].role == ScreenRole::Browser {
        notify(wall.overlay, "this is the browser window — open a wall window for tiles".into());
        return false;
    }
    if wall.screens[screen_idx].zoomed.is_some() {
        notify(wall.overlay, "exit fullscreen (F) before adding tiles".into());
        return false;
    }
    if wall.screens[screen_idx].tiles.len() >= wall.max_tiles {
        notify(wall.overlay, format!("not adding: already at the {}-tile limit", wall.max_tiles));
        return false;
    }
    true
}

/// Starts a tile playing `item` on a screen, through the decode budget.
/// Returns whether it did. It leaves the queue alone: an item taken from
/// the queue is the caller's to advance past, and one that was never in it
/// (play now, an opened file) must not consume someone else's place.
pub fn spawn_tile(wall: &mut Wall, screen_idx: usize, item: tessera_core::MediaItem) -> bool {
    let monitor_index = wall.screens[screen_idx].monitor_index;
    let id = *wall.next_tile_id;
    match request_admission(wall.screens, wall.budget, id, &tessera_core::StreamSpec::placeholder()) {
        Admission::Admitted | Admission::AdmittedEvicting { .. } => {
            wall.screens[screen_idx].context.make_current(&wall.screens[screen_idx].surface).ok();
            let muted = wall.screens.iter().any(|s| !s.tiles.is_empty());
            match Tile::new(wall.mpv_lib, id, &item, wall.tile_opts, None, muted) {
                Ok(tile) => {
                    wall.screens[screen_idx].add_tile(tile);
                    *wall.next_tile_id += 1;
                    println!(
                        "screen {monitor_index}: added tile {id} \"{}\" ({} tiles)",
                        item.title,
                        wall.screens[screen_idx].tiles.len()
                    );
                    true
                }
                Err(e) => {
                    wall.budget.release(id);
                    notify(wall.overlay, format!("not adding: tile creation failed: {e}"));
                    false
                }
            }
        }
        Admission::RejectedBudget { would_use, ceiling } => {
            notify(
                wall.overlay,
                format!(
                    "not adding: decode budget full ({:.1}/{:.0} streams) — raise it in config (C)",
                    would_use / tessera_core::budget::MACROBLOCKS_1080P30,
                    ceiling / tessera_core::budget::MACROBLOCKS_1080P30
                ),
            );
            false
        }
        Admission::RejectedMaxStreams { max } => {
            notify(wall.overlay, format!("not adding: max concurrent streams ({max}) — raise it in config (C)"));
            false
        }
    }
}

/// Removes a tile by id, wherever it lives — the transport "−" button and
/// the "−" hotkey. Frees its budget slot and exits zoom if it was zoomed.
pub fn remove_tile_action(
    screens: &mut [Screen],
    budget: &mut DecodeBudget,
    overlay: &mut overlay::OverlayState,
    tile_id: usize,
) {
    if overlay.track_panel.map(|(id, _)| id) == Some(tile_id) {
        overlay.track_panel = None;
    }
    if overlay.solo_id == Some(tile_id) {
        overlay.solo_id = None;
    }
    for si in 0..screens.len() {
        if screens[si].tiles.iter().any(|t| t.id == tile_id) {
            let monitor_index = screens[si].monitor_index;
            let was_zoomed = screens[si].zoomed == Some(tile_id);
            if screens[si].pinned == Some(tile_id) {
                screens[si].pinned = None;
            }
            budget.release(tile_id);
            if screens[si].remove_tile(tile_id) {
                println!(
                    "screen {monitor_index}: removed tile {tile_id} ({} left)",
                    screens[si].tiles.len()
                );
            }
            if was_zoomed {
                toggle_fullscreen(screens, si, None);
            }
            break;
        }
    }
}

/// Pauses a tile (wherever it lives) and releases its budget slot. The
/// invariant: budget.active == the set of playing tiles.
pub fn pause_and_release(screens: &mut [Screen], budget: &mut DecodeBudget, id: usize, reason: &str) {
    budget.release(id);
    for screen in screens.iter_mut() {
        if screen.pause_tile(id) {
            println!("screen {} tile {id}: paused ({reason})", screen.monitor_index);
            return;
        }
    }
}

/// Requests admission, evicting (pausing) older streams as needed until the
/// request fits or only the requesting stream would remain. Mirrors the Quest
/// retry loop: the oldest stream is paused to maintain the stream count.
pub fn request_admission(
    screens: &mut [Screen],
    budget: &mut DecodeBudget,
    id: usize,
    spec: &tessera_core::StreamSpec,
) -> Admission {
    loop {
        match budget.request(id, spec) {
            Admission::Admitted => return Admission::Admitted,
            Admission::AdmittedEvicting { evicted } => {
                pause_and_release(screens, budget, evicted, "evicted: oldest stream");
                return Admission::AdmittedEvicting { evicted };
            }
            rejected @ (Admission::RejectedBudget { .. } | Admission::RejectedMaxStreams { .. }) => {
                // The single-stream rule guarantees admission once everything
                // else is evicted; stop short of evicting the last stream
                // only when the request itself is the lone candidate.
                if budget.active_count() > 1 {
                    if let Some(victim) = budget.evict_oldest() {
                        pause_and_release(screens, budget, victim, "evicted: oldest stream");
                        continue;
                    }
                }
                return rejected;
            }
        }
    }
}

/// Resumes a paused tile, securing budget first (evicting the oldest playing
/// stream if necessary). Prints the outcome.
pub fn resume_tile(screens: &mut [Screen], budget: &mut DecodeBudget, id: usize) {
    let spec = budget.spec(id).cloned().unwrap_or_else(tessera_core::StreamSpec::placeholder);
    match request_admission(screens, budget, id, &spec) {
        Admission::Admitted | Admission::AdmittedEvicting { .. } => {
            for screen in screens.iter_mut() {
                if screen.resume_tile(id) {
                    println!("screen {} tile {id}: playing", screen.monitor_index);
                    return;
                }
            }
        }
        Admission::RejectedBudget { would_use, ceiling } => {
            println!(
                "tile {id}: staying paused — budget {:.1}/{:.0} streams",
                would_use / tessera_core::budget::MACROBLOCKS_1080P30,
                ceiling / tessera_core::budget::MACROBLOCKS_1080P30
            );
        }
        Admission::RejectedMaxStreams { max } => {
            println!("tile {id}: staying paused — max streams ({max})");
        }
    }
}

/// Executes a raw (non-UI) left click on the video wall: pause/resume the
/// tile under the press position, budget-aware.
pub fn raw_tile_click(screens: &mut [Screen], budget: &mut DecodeBudget, pc: overlay::PendingClick) {
    let Some(idx) = screens.iter().position(|s| s.window.id() == pc.window_id) else {
        return;
    };
    if let Some(id) = screens[idx].tile_at(pc.x, pc.y) {
        let playing = screens
            .iter()
            .any(|s| s.tiles.iter().any(|t| t.id == id && !t.is_paused()));
        if playing {
            pause_and_release(screens, budget, id, "user pause");
        } else {
            resume_tile(screens, budget, id);
        }
    }
}

/// Pins/unpins a tile: the pinned tile takes most of the screen while the
/// others share a bottom strip (the P key and the transport pin button).
pub fn toggle_pin(screens: &mut [Screen], screen_idx: usize, tile_id: usize) {
    let monitor_index = screens[screen_idx].monitor_index;
    if screens[screen_idx].pinned == Some(tile_id) {
        screens[screen_idx].pinned = None;
        println!("screen {monitor_index}: unpinned tile {tile_id}");
    } else {
        screens[screen_idx].pinned = Some(tile_id);
        println!("screen {monitor_index}: pinned tile {tile_id}");
    }
}

/// Drop handler for a tile drag. Released over another screen's window:
/// move the tile there. Released over its own screen: swap the dragged
/// tile with the tile under the pointer. Dropping on empty space, outside
/// the grid, or back on the source cell is a no-op.
pub fn finish_tile_drag(
    screens: &mut [Screen],
    budget: &mut DecodeBudget,
    mpv_lib: &std::sync::Arc<tessera_mpv::MpvLib>,
    tile_opts: &TileOptions,
    overlay: &mut overlay::OverlayState,
    drag: overlay::TileDrag,
    max_tiles: usize,
) {
    let Some(src) = screens.iter().position(|s| s.window.id() == drag.window_id) else {
        return;
    };
    // Global pointer position: drag.pos lives in pos_window's coordinate
    // space (physical px, possibly past that window's bounds while a
    // button grab routes motion to the drag source).
    let global = screens
        .iter()
        .position(|s| s.window.id() == drag.pos_window)
        .and_then(|i| screens[i].window.inner_position().ok())
        .map(|p| (p.x as f32 + drag.pos.0, p.y as f32 + drag.pos.1));
    let target = global.and_then(|(gx, gy)| {
        screens.iter().position(|s| {
            window_rect(s)
                .is_some_and(|(x0, y0, x1, y1)| gx >= x0 as f32 && gx < x1 as f32 && gy >= y0 as f32 && gy < y1 as f32)
        })
    });
    if let Some(dst) = target.filter(|&dst| dst != src) {
        move_tile_cross_screen(screens, budget, mpv_lib, tile_opts, overlay, src, dst, drag.from_tile, max_tiles);
        return;
    }
    // Same-screen drop: pos must be in the source window's coordinates
    // (a foreign pos_window means the pointer is over another window).
    if drag.pos_window != drag.window_id {
        return;
    }
    let Some(to) = screens[src].tile_at(drag.pos.0, drag.pos.1) else {
        return;
    };
    if to == drag.from_tile {
        return;
    }
    let monitor_index = screens[src].monitor_index;
    // Dropping a strip tile onto the pinned cell features the dragged tile
    // instead of swapping Vec positions (the big cell follows the pin id).
    if screens[src].pinned == Some(to) {
        toggle_pin(screens, src, drag.from_tile);
        return;
    }
    if screens[src].swap_tiles(drag.from_tile, to) {
        println!("screen {monitor_index}: swapped tiles {} and {}", drag.from_tile, to);
    }
}

/// Cross-screen drag drop (Phase 2 of #1): move a tile to another screen.
/// A tile's mpv render context is bound to its screen's GL context, so the
/// instance cannot survive the move — the tile is snapshotted (item,
/// position, paused, volume, mute, track picks), destroyed on the source
/// screen and rebuilt on the target, which appends it and re-flows its
/// grid. Only the playing item moves; the per-screen queues stay put.
#[allow(clippy::too_many_arguments)]
pub fn move_tile_cross_screen(
    screens: &mut [Screen],
    budget: &mut DecodeBudget,
    mpv_lib: &std::sync::Arc<tessera_mpv::MpvLib>,
    tile_opts: &TileOptions,
    overlay: &mut overlay::OverlayState,
    src: usize,
    dst: usize,
    tile_id: usize,
    max_tiles: usize,
) {
    let src_monitor = screens[src].monitor_index;
    let dst_monitor = screens[dst].monitor_index;
    if screens[dst].zoomed.is_some() {
        notify(overlay, "not moving: target screen is zoomed (F)".into());
        return;
    }
    if screens[dst].tiles.len() >= max_tiles {
        notify(overlay, format!("not moving: target screen is at the {max_tiles}-tile limit"));
        return;
    }
    let (item, mut resume) = {
        let Some(tile) = screens[src].tiles.iter_mut().find(|t| t.id == tile_id) else {
            return;
        };
        (tile.item.clone(), tile.snapshot_resume())
    };
    // The pin doesn't travel with a cross-screen move (the target screen
    // didn't ask for it); clear it so the layout doesn't reference a
    // missing tile.
    if screens[src].pinned == Some(tile_id) {
        screens[src].pinned = None;
    }
    if !resume.paused {
        // The rebuilt instance needs the slot the old one holds.
        budget.release(tile_id);
        let rejected = match request_admission(screens, budget, tile_id, &tessera_core::StreamSpec::placeholder()) {
            Admission::Admitted | Admission::AdmittedEvicting { .. } => None,
            Admission::RejectedBudget { would_use, ceiling } => {
                Some(format!(
                    "decode budget full ({:.1}/{:.0} streams)",
                    would_use / tessera_core::budget::MACROBLOCKS_1080P30,
                    ceiling / tessera_core::budget::MACROBLOCKS_1080P30
                ))
            }
            Admission::RejectedMaxStreams { max } => Some(format!("max concurrent streams ({max})")),
        };
        if let Some(reason) = rejected {
            // Move anyway, but paused: the source instance already gave up
            // its budget slot.
            pause_and_release(screens, budget, tile_id, &reason);
            resume.paused = true;
        }
    }
    if !screens[src].remove_tile(tile_id) {
        return;
    }
    // Tile creation happens on the target screen's GL context.
    screens[dst].context.make_current(&screens[dst].surface).ok();
    match Tile::new(mpv_lib, tile_id, &item, tile_opts, Some(&resume), true) {
        Ok(tile) => {
            screens[dst].add_tile(tile);
            println!(
                "screen {src_monitor}: moved tile {tile_id} \"{}\" to screen {dst_monitor} (resume at {:.1}s{})",
                item.title,
                resume.start,
                if resume.paused { ", paused" } else { "" }
            );
            println!(
                "screen {dst_monitor}: tile {tile_id} added ({} tiles)",
                screens[dst].tiles.len()
            );
        }
        Err(e) => {
            budget.release(tile_id);
            notify(overlay, format!("not moving: tile creation failed: {e}"));
        }
    }
}

/// Deletes the tile's current video at its source (local file, or the
/// server's delete API), drops it from the queue, and advances the tile to
/// the next item.
pub fn delete_current(
    screens: &mut [Screen],
    queue: &mut PlaybackQueue,
    sources: &[tessera_core::sources::Source],
    screen_idx: usize,
    tile_id: usize,
    overlay: &mut OverlayState,
) {
    let Some(tile) = screens[screen_idx].tiles.iter().find(|t| t.id == tile_id) else {
        return;
    };
    let item = tile.item.clone();
    let monitor_index = screens[screen_idx].monitor_index;
    match tessera_core::sources::delete_item(&item, sources) {
        Ok(()) => println!(
            "screen {monitor_index} tile {tile_id}: deleted \"{}\" ({})",
            item.title,
            item.kind.label()
        ),
        Err(e) => {
            println!("delete failed: {e}");
            overlay.notice = Some((format!("delete failed: {e}"), Instant::now()));
            return;
        }
    }
    queue.remove_item(&item.id);
    tile_next(screens, queue, tile_id, true);
}

/// Loads the next (or previous) queue item onto the tile, advancing the
/// screen's shared queue. Returns false if the tile was not found.
pub fn tile_next(screens: &mut [Screen], queue: &mut PlaybackQueue, tile_id: usize, forward: bool) -> bool {
    let Some(screen) = screens.iter_mut().find(|s| s.tiles.iter().any(|t| t.id == tile_id)) else {
        return false;
    };
    let item = if forward { queue.advance() } else { queue.go_back() }.cloned();
    if let Some(item) = item {
        let monitor_index = screen.monitor_index;
        if let Some(tile) = screen.tiles.iter_mut().find(|t| t.id == tile_id) {
            tile.item = item.clone();
            if let Err(e) = tile.load(&item) {
                eprintln!("screen {monitor_index} tile {tile_id}: load failed: {e}");
            } else {
                println!(
                    "screen {monitor_index} tile {tile_id}: {} \"{}\"",
                    if forward { "next" } else { "prev" },
                    item.title
                );
            }
        }
    }
    true
}

/// Where a batch of files should land.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenTarget {
    /// No window in particular — a hand-off from another launch, or an OS
    /// open ("Open With" on macOS) while running. Plays on the first wall
    /// window that has a tile.
    Anywhere,
    /// Dropped onto this window: play there, on `tile` if the drop landed
    /// on one. A browser window queues instead, since it has no tiles to
    /// play on.
    Window { window: winit::window::WindowId, tile: Option<usize> },
    /// A tile of its own beside whatever is playing (MCP `new_tile`), on
    /// this window, else the first wall window.
    NewTile(Option<winit::window::WindowId>),
}

/// Files on their way to the wall, being listed off the render thread.
pub type Listed = (OpenTarget, Vec<tessera_core::MediaItem>);

/// Lists `paths` on a worker and sends the result to `open_listed`'s
/// channel: a dropped or handed-off folder is a directory walk, which must
/// not stall a frame.
pub fn list_off_thread(tx: &std::sync::mpsc::Sender<Listed>, target: OpenTarget, paths: Vec<PathBuf>) {
    let tx = tx.clone();
    std::thread::spawn(move || {
        let _ = tx.send((target, tessera_core::media::collect_items(&paths)));
    });
}

/// Acts on files that arrived while running — handed over by a later
/// launch, opened by the OS, or dropped onto a window — once
/// `list_off_thread` has listed them.
///
/// "Play that file" rather than "quietly append it" is the same rule the
/// command line already follows — see the positional-paths note in `main`.
/// Double-clicking a video in the file manager has to result in that video
/// playing, whether or not an instance was already up.
///
/// Returns true when there was nowhere to play it and the caller should
/// open a wall window; the file is left at the head of the pending queue,
/// so the tile that window opens with is the one just asked for.
pub fn open_listed(wall: &mut Wall, target: OpenTarget, items: Vec<tessera_core::MediaItem>) -> bool {
    if items.is_empty() {
        notify(wall.overlay, "nothing playable in what was opened".into());
        return false;
    }
    let count = items.len();

    // Dropping on the browser window means "add these", not "play these" —
    // it has no tiles, and the queue tab is the thing you were looking at.
    if let OpenTarget::Window { window, .. } = target {
        if wall.screens.iter().any(|s| s.window.id() == window && s.role == ScreenRole::Browser) {
            wall.queue.append_items(items);
            notify(wall.overlay, format!("added {count} item(s) to the queue"));
            return false;
        }
    }

    // Where it will play is decided BEFORE the queue is touched, because
    // that is what decides whether the file belongs in the queue at all.
    // A drop plays on the window it landed on — on the tile under the
    // pointer, else its first, else a new tile if it has none. Anything
    // else takes the first wall tile, else a new tile on an empty wall
    // window, else a new window.
    enum Spot {
        Replace(usize, usize),
        Spawn(usize),
    }
    let dropped_on = match target {
        OpenTarget::Window { window, tile } => wall
            .screens
            .iter()
            .position(|s| s.window.id() == window && s.role == ScreenRole::Wall)
            .map(|si| (si, tile)),
        OpenTarget::Anywhere | OpenTarget::NewTile(_) => None,
    };
    let spot = match dropped_on {
        _ if matches!(target, OpenTarget::NewTile(_)) => {
            let OpenTarget::NewTile(on) = target else { unreachable!() };
            on.and_then(|id| wall.screens.iter().position(|s| s.window.id() == id && s.role == ScreenRole::Wall))
                .or_else(|| wall.screens.iter().position(|s| s.role == ScreenRole::Wall))
                .map(Spot::Spawn)
        }
        Some((si, tile)) => Some(match wall.screens[si].tiles.first() {
            // The tile under the pointer was read when the drop arrived;
            // it may have closed while the listing ran.
            Some(t0) => Spot::Replace(
                si,
                tile.filter(|id| wall.screens[si].tiles.iter().any(|t| t.id == *id)).unwrap_or(t0.id),
            ),
            None => Spot::Spawn(si),
        }),
        None => wall
            .screens
            .iter()
            .position(|s| s.role == ScreenRole::Wall && !s.tiles.is_empty())
            .map(|si| Spot::Replace(si, wall.screens[si].tiles[0].id))
            .or_else(|| wall.screens.iter().position(|s| s.role == ScreenRole::Wall).map(Spot::Spawn)),
    };

    let mut items = items;
    if let Some(Spot::Spawn(si)) = spot {
        // A new tile for the first, the extras queued — the same split as
        // replacing one, below. If the tile can't be made (budget, --tiles)
        // the file goes to the head of the queue instead of being lost.
        let first = items.remove(0);
        wall.queue.append_items(items);
        if !(can_add_tile(wall, si) && spawn_tile(wall, si, first.clone())) {
            let first_idx = wall.queue.len();
            wall.queue.append_items(vec![first]);
            wall.queue.move_pending_to_front(first_idx);
        }
        wall.screens[si].window.focus_window();
        return false;
    }
    let Some(Spot::Replace(si, tile_id)) = spot else {
        // No wall window at all, so the file goes in the queue — at the
        // head of the pending tail, which is what the new window's first
        // tile pulls from, so it comes up on the file that was asked for
        // rather than on whatever was next.
        let first_idx = wall.queue.len();
        wall.queue.append_items(items);
        wall.queue.move_pending_to_front(first_idx);
        println!("opened: {count} file(s) — no wall window yet, opening one");
        return true;
    };

    // Playing it and queueing it are alternatives, never both. A file on a
    // tile that is *also* sitting at the head of the queue starts again the
    // moment you press next, which reads as the wall being stuck on it.
    // This is the split `OverlayAction::PlayNow` already draws: the tile
    // owns the item it is playing, and the queue holds what is still to
    // come. Only the extras from a multi-file drop are queued.
    let first = items.remove(0);
    let queued = items.len();
    wall.queue.append_items(items);

    let monitor_index = wall.screens[si].monitor_index;
    if let Some(tile) = wall.screens[si].tiles.iter_mut().find(|t| t.id == tile_id) {
        tile.item = first.clone();
        if let Err(e) = tile.load(&first) {
            eprintln!("screen {monitor_index} tile {tile_id}: load failed: {e}");
        } else {
            println!(
                "opened: screen {monitor_index} tile {tile_id}: now playing \"{}\"{}",
                first.title,
                if queued > 0 { format!(" (+{queued} queued)") } else { String::new() }
            );
        }
    }
    wall.screens[si].window.focus_window();
    false
}

/// Closes one window: frees its tiles' budget slots and GL resources, drops
/// the [`Screen`] (and with it the GL context, surface and egui context),
/// and terminates its mpv instances in parallel — serial teardown is what
/// made quitting slow. Overlay state pointing at the closed window or its
/// tiles is cleared, since none of it can be resolved any more.
pub fn close_screen(
    screens: &mut Vec<Screen>,
    budget: &mut DecodeBudget,
    overlay: &mut OverlayState,
    idx: usize,
) {
    let mut screen = screens.remove(idx);
    let window_id = screen.window.id();
    let monitor_index = screen.monitor_index;
    let tiles = screen.destroy();
    for tile in &tiles {
        budget.release(tile.id);
    }
    let gone: Vec<usize> = tiles.iter().map(|t| t.id).collect();
    if overlay.pending_click.map(|p| p.window_id) == Some(window_id) {
        overlay.pending_click = None;
    }
    if overlay.tile_drag.map(|d| d.window_id) == Some(window_id) {
        overlay.tile_drag = None;
    }
    if overlay.help_open == Some(window_id) {
        overlay.help_open = None;
    }
    if overlay.drop_target == Some(window_id) {
        overlay.drop_target = None;
    }
    if overlay.settings_open == Some(window_id) {
        overlay.settings_open = None;
    }
    if overlay.solo_id.is_some_and(|id| gone.contains(&id)) {
        overlay.solo_id = None;
    }
    if overlay.confirm_delete.is_some_and(|(id, _)| gone.contains(&id)) {
        overlay.confirm_delete = None;
    }
    if overlay.track_panel.is_some_and(|(id, _)| gone.contains(&id)) {
        overlay.track_panel = None;
    }
    println!(
        "closed window (monitor {monitor_index}, {} tile(s)); {} window(s) left",
        gone.len(),
        screens.len()
    );
    // Render contexts go first, here, while `screen.destroy()`'s
    // make_current is still in effect (see Tile::split_for_teardown); only
    // then may the GL context itself be dropped.
    let cores: Vec<tessera_mpv::Mpv> =
        tiles.into_iter().map(|t| t.split_for_teardown()).collect();
    drop(screen);
    terminate_parallel(cores);
}

/// Terminates mpv cores in parallel — serial teardown is what made quitting
/// slow. Safe on any thread: the GL-bound render contexts are already gone
/// by this point (see [`Tile::split_for_teardown`]).
pub fn terminate_parallel(cores: Vec<tessera_mpv::Mpv>) {
    std::thread::scope(|scope| {
        for core in cores {
            scope.spawn(move || drop(core));
        }
    });
}

/// Tears the wall down: GL resources are freed per-screen on their own GL
/// contexts (fast), then every tile's mpv instance is terminated in
/// parallel — serial teardown was what made quitting slow.
pub fn shutdown(screens: &mut [Screen]) {
    let start = Instant::now();
    let mut cores = Vec::new();
    for screen in screens.iter_mut() {
        // Per screen, and without interleaving: destroy() makes THIS
        // screen's GL context current, and the render contexts it just
        // returned can only be freed while that is true.
        let tiles = screen.destroy();
        cores.extend(tiles.into_iter().map(|t| t.split_for_teardown()));
    }
    terminate_parallel(cores);
    println!("shutdown in {:.2}s", start.elapsed().as_secs_f64());
}
