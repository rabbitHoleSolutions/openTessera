# openTessera Roadmap

Feature tracking across sessions. Update status as work completes.
Effort estimates: `S` (< half session), `M` (~1 session), `L` (multi-session).

---

## 1. Tile drag-to-rearrange

Status: **implemented** (Phase 1 + Phase 2).

### Phase 1: Same-screen drag (do this first)

Drag a tile onto another tile to **swap** their cells (decision: swap semantics,
not insert/shift).

**Design**
- Press-and-hold or left-button drag past a small threshold (~8px) so plain
  clicks (pause/resume) still work — same pattern as the queue-strip drag.
- While dragging: outline source cell, highlight hovered target cell.
- Drop on a tile: swap. Drop on empty space / outside grid: no-op.
- Esc cancels the drag.

**Implementation notes**
- Add `tile_drag: Option<TileDrag>` to `OverlayState` (sibling of
  `queue_drag: Option<usize>` in overlay.rs).
- No GL/mpv changes: layout derives from `Screen::tiles` Vec order
  (`solve_grid` by index). Swapping two Vec entries swaps their cells; FBOs
  belong to tiles and move with them.
- Reuse the deferred click machinery (`pending_click`) for press detection and
  hit-testing.
- Draw feedback via egui `painter` (source outline + target highlight).

**Effort**: M

### Phase 2: Cross-screen drag (implemented 2026-08-26)

A tile's mpv render context is bound to its screen's GL context, so moving
between monitors requires teardown/rebuild — implemented exactly that way.

**What shipped**
- Drop on another screen's window = move: the tile is appended to the
  target screen and both grids re-flow. Release keeps its X button grab,
  so the source window keeps reporting motion with out-of-bounds
  coordinates — the drop position is resolved in global coordinates and
  hit-tested against every screen's window rect.
- Snapshot/rebuild (`ResumeState` in tile.rs): item, time-pos (`start=`),
  paused, volume, mute, and selected aid/sid are applied to the fresh mpv
  instance before load; `start` is reset on FileLoaded so queue advances
  aren't affected. The tile keeps its id.
- Budget: a playing move re-requests admission; on rejection the move
  still happens but paused (the source instance already released its
  slot). Paused tiles move without a slot, as everywhere else.
- Queue decision: only the playing item moves; per-screen queues stay.
- Target-screen guards: no move while target is zoomed (F) or at its
  --tiles limit (toast explains).
- Supporting changes: `--screens` takes a list (`0,2`); `--populate N`
  opens N tiles per screen at startup while `--tiles` stays the max
  (default: populate = max); `-`/`+` hotkeys remove the tile under the
  cursor / add a tile on the focused screen (same logic as the transport
  buttons, extracted into helpers); the startup WM-settle is armed so
  pop-shell mis-placement of the 2nd+ fullscreen windows gets re-anchored
  (retries 8× at 1s — placement still occasionally needs an app restart
  when the WM wins the fight).

**Effort**: M–L (actual: ~1 session)

---

## 2. Visual volume control

Status: **implemented** — inline slider + % label in the chip row, scroll
wheel over a tile = ±5 on that tile (throttled: winit/X11 doubles button
wheel events), Up/Down still screen-wide.

Per-tile volume with a visible control (each tile is its own mpv instance;
`Tile::set_volume/volume` already exist — tile.rs).

**Design** (decision: slider + scroll wheel)
- Inline slider in the tile's chip row, shown when the tile is unmuted or
  hovered; % label alongside. Mute button stays as-is.
- Scroll wheel over a tile = volume ±5 on that tile (always active).
- Up/Down keys remain screen-wide (adjust unmuted tiles of the screen).
- Clamp 0–100.
- Optional later: "audio focus" concept (single unmuted stream); per-tile
  knobs don't preclude it.

**Implementation notes**
- `egui::Slider` in chip row next to mute button; live-set on drag.
- `WindowEvent::MouseWheel` in main.rs → `tile_at` for hovered tile →
  `set_volume`; feedback via chip label or brief toast.

**Effort**: S

---

## 3. Audio track selector

Status: **implemented** (with #4) — no popup (user decision): "A" button in
the chip row (only when >1 audio track) expands a track row inside the same
panel, below the transport. Off + one button per track; current selection
filled. Track caches live on the tile, refreshed on file load and whenever
the row opens.

**Design**
- "A" button in tile chip row — only shown when the file has >1 audio track.
- Click opens a compact popup listing tracks from mpv's `track-list`:
  `Off / <lang> <codec> <title> (default)`.
- Selection sets mpv `aid` property to the track id (or `no`); current
  selection checkmarked.

**Implementation notes**
- Needs a tessera-mpv accessor for `track-list` (easiest: `get_property`
  returning JSON string → serde_json parse; fields: id, type, title, lang,
  default, selected).
- `Tile::set_audio_track(id_or_no)` → `set aid <v>`.
- New `OverlayAction::SetAudioTrack { tile, track }` variant.
- Popup uses the existing dialog pattern (`Order::Tooltip`), like
  help/settings.
- Query `track-list` lazily when the popup opens (re-query after file loads).
- Cache per-tile track counts for button visibility.

**Effort**: M (shared with #4)

---

## 4. Subtitle track selector

Status: **implemented** (with #3) — "S" button whenever subtitle tracks
exist; same inline row, sets mpv `sid`; subtitles render through the
existing render-API path (baked into the video frame).

**Design**
- "S" button in tile chip row — only shown when the file has subtitle tracks.
- Same popup/checkmark UX as audio tracks; sets mpv `sid` property.

**Implementation notes**
- Shares all machinery with #3; only the property name differs (`sid` vs
  `aid`). `Tile::set_subtitle_track(id_or_no)`.
- Subtitles render through the existing mpv render API path automatically
  (baked into the video frame) — no extra GL work.
- `OverlayAction::SetSubtitleTrack { tile, track }`.

**Effort**: ~0 once #3 lands

---

## 5. Pin + solo

Status: **implemented** (2026-08-26)

Pin (P key or 📌 transport button): the pinned tile takes most of the
screen; the others share a bottom strip (20% of height, 96–384px, 8px
gaps). Computed live in `Screen::effective_layout` (like zoom), so resizes
just work; zoom (F) takes precedence while active.

- Dragging a strip tile onto the pinned cell re-pins the dragged tile
  (drop-on-pin = feature it) instead of swapping.
- Pin clears when the tile is removed or moved cross-screen (target
  screen didn't ask for the pin); stale ids fall back to the grid.
- Plain `p` = pin toggle on the tile under the cursor; **Shift+P** is
  previous now (was plain P).

Solo (🎤 chip button): unmute that tile, mute every other stream on every
screen (audio focus). Solo highlight clears on any manual mute change.
Needs audio enabled (the default; off with --no-audio); toast explains otherwise.

**Effort**: S–M

---

## 6. Sources in settings

Status: **implemented** (2026-08-26), **moved to the browser window** in §12.

The gear panel had a sources section: list with remove buttons, plus an
add form (kind selector: folder / plex / jellyfin; path-or-url field,
token/key + optional library filter for servers). Sources persist as one
JSON line (`sources = [...]`) in settings.conf — saved on every settings
change alongside ui_scale/cache_mb/max_streams/audio — and are listed at
startup, so the wall runs with zero CLI arguments. CLI positional paths
and --plex/--jellyfin still work and are merged (deduplicated) with the
saved sources. Camera sources will join `tessera_core::sources::Source`
(a serde-tagged enum) when they land.

Two bugs found and fixed along the way, both of which had made the
settings/help panels unusable by mouse:
- The panels were drawn at `Order::Tooltip`, and egui never hit-tests
  tooltip-order layers — every control in them was inert. Now
  `Order::Foreground` like the chip panels.
- App hotkeys fired while typing in text fields (a "/" in a folder path
  opened the help panel mid-typing, covering the form). Keyboard handling
  now yields to egui when it wants keyboard input.

**Effort**: M

---

## 7. Per-window fullscreen + windows on demand

Status: **implemented** (2026-08-27)

The wall is no longer "all monitors or nothing". One ordinary window is the
default; `--max` is the opt-in all-monitors-fullscreen video-wall mode
(`--screens 0,2` still names specific monitors).

- Status-bar "+" opens another window at runtime — its own GL context,
  surface, egui context and tiles, built through the same `create_screen`
  path as startup. It opens with one tile (`NEW_WINDOW_TILES`): at startup
  nothing is playing so filling is free, but adding to a live wall would
  evict other windows' streams via OLDEST_STARTED.
- `W` toggles one window between windowed and fullscreen **on the monitor it
  is currently on**, derived from the window's centre rather than the
  monitor it was created on — so drag-to-another-screen-then-W works. (The
  containment test is unit-tested against the real 3-monitor layout;
  `current_monitor()` stays unusable under pop-shell.)
- Closing a window closes just that window; the last one quits.
- A plain window starts at `DEFAULT_WINDOW_TILES` (4, matching the Quest
  dock) rather than filling to `--tiles`; `--max` still fills.

### Queue model

One `PlaybackQueue` shared by every window, owned by `main` next to the
budget — the round-robin per-screen split is gone (it had no answer for a
window created later). Tiles are slots fed from the one queue, as in the
Quest app's `DockGridManager`. **Next:** a settings toggle for
per-window vs global queues, alongside the library browser / queue builder
ported from the Quest app.

### Bugs found and fixed on the way

- `mpv_render_context_free` frees GL objects, so tearing tiles down on
  worker threads (no GL context current) segfaults. Teardown is now
  two-phase: render contexts go on the main thread with the owning screen's
  context current (`Tile::split_for_teardown`), then the mpv cores are
  terminated in parallel. `shutdown` had the same latent hazard.
- The status bar was a `TopBottomPanel`, i.e. egui's Background layer, which
  the click router treats as "not UI" — so every status-bar click also fell
  through and paused the tile behind the bar. It is an `Area` at
  `WALL_ORDER` now.
- The status bar pushed its buttons off the right edge of a narrow window;
  controls are laid out first now and the status text clips instead.

**Effort**: M (actual: ~1 session)

---

## 8. Library browser + queue building

Status: **v1 implemented** (2026-08-27)

Ported the shape of the Quest app's `BrowserScreen`, adapted to the desktop:
the browser is **its own window** (decision: window, not overlay panel or
wall tile) opened with `L` or the status-bar list icon. `ScreenRole` on
`Screen` distinguishes it from a wall window — no tiles, no video, but the
same GL context, tick, W, drag-routing and close paths, so window management
works on it unchanged.

- `catalog.rs`: per-source listings fetched on a background worker and
  cached, polled each frame. `Source::list()` blocks (HTTP for servers, an
  FS walk for folders) so it can never run on the render thread. Startup
  enumeration stays separate — the wall plays immediately (decision: keep
  auto-filling at launch; the browser refines rather than gates).
- Live title filter, name/date/size ordering, per-row thumbnail (shared
  `ThumbCache`), size, and a "queued" marker.
- The two Quest queue semantics, now in `tessera-core`:
  `PlaybackQueue::add_items` folds into the queue's order (bulk "add N to
  queue" — a shuffled queue interleaves), `append_items` appends untouched
  (the per-row "+"). Unit-tested, including that neither disturbs the
  cursor.
- Row "▶" plays now on the first wall window's first tile.

**v1 lists each source flat** (decision), rather than the Quest app's
`DirListing` folder tree: `Source` has no directory model, and search over a
flat list covers most of "find things and queue them".

### Queue tab (2026-08-27)

The browser window has two tabs — `L` opens it on library, `Q` on queue;
both keys focus the existing window and just switch its tab.

- **now playing**: every tile on every wall window (a `Screen` can't see its
  siblings, so `main` snapshots them into `library::WallTile` before the
  per-screen loop). Pause/resume, mute and volume per tile, reusing the
  existing tile-id-addressed actions.
- **queue**: the shared queue with the current item marked, ⏶/⏷ reorder and
  ✖ remove on pending items only (matching what `PlaybackQueue` will
  actually do), plus shuffle / A–Z on the pending tail and "clear queue"
  (`PlaybackQueue::clear`, arm-then-confirm like the transport's delete).

Bug found here: **auto-repeat fired every hotkey continuously.** Toggles
(W, F, L/Q, S, +/-) now ignore `KeyEvent::repeat`; only the continuous keys
(seek, volume arrows) still repeat. Held keys were storming the wall — and
anything that moves focus mid-press can swallow the key release, after which
X repeats indefinitely.

### Next

- Folder navigation (`DirListing` per adapter: directories for local,
  libraries/collections for Plex and Jellyfin), with the Quest app's
  recursive per-folder search and Subfolders toggle.
- Search history with a settle window (Quest: `SearchHistory`).
- Per-window vs global queue toggle — the north star noted when the shared
  queue landed. It is inherently per-window, so it belongs in the gear panel
  next to UI scale (see §12), not in the config tab.
- Row virtualisation: the list is capped at 300 rows and tells you when it
  truncates; egui has no virtualisation here.

**Effort**: M (actual: ~half session for v1)

---

## 11. Queue retention, and controls that follow the cursor

Status: **implemented** (2026-08-27)

- **Queue retention** is a setting (default on). On, the queue and its cursor
  round-trip through `~/.config/opentessera/queue.json`; off, every launch
  starts empty, and turning it off deletes the saved file rather than leaving
  a queue to resurrect months later. Written on a 20s timer and at shutdown —
  the cursor moves on every auto-advance, so saving per mutation would rewrite
  hundreds of items constantly.
- A bare launch **no longer enumerates every source at startup**. Nothing used
  that list except the initial queue, so with retention it is dead weight; the
  library browser lists on demand. Startup is now instant instead of waiting on
  an HTTP walk of every server. Positional paths still enumerate, since those
  mean "play this".
- **Only the hovered tile shows its chip.** One per tile meant any mouse
  movement lit up the whole wall, and every control there targets a single
  tile anyway.
- **Arrow-key seek follows the cursor** instead of moving every tile on the
  screen at once.
- **The scrub bar takes the tile's width.** egui draws a Slider's rail at
  `spacing.slider_width` (100 by default) and ignores the room `add_sized`
  gives it, so widening the row alone did nothing — the width has to be set
  on the spacing.

### Delete now routes to the right server

`MediaItem` carries the label of the `Source` that listed it, stamped in
`Source::list` so no listing path can forget. `delete_item` resolves that
against the configured sources instead of re-reading `sources.conf`, which
could only ever name one server per kind. Previously deletes worked by
coincidence — one Jellyfin entry in `sources.conf` that happened to match —
and a second server would have sent the delete to the wrong place. Local
`sources.conf` stays as the fallback for items listed via `--plex` /
`--jellyfin`, which never had a configured `Source`.

---

## 12. Settings split by scope: global in the browser, per-window in the gear

Status: **implemented** (2026-08-27)

Everything app-wide moved out of the wall's gear panel and into the browser
window as tabs; the gear now holds only what belongs to the window you
clicked it in.

**Why, beyond tidiness.** Each `Screen` runs its own egui context over one
shared `UiSettings`, so a global control drawn in the gear was drawn once per
window and the last screen in the loop won. That is the hazard behind
`help_open` / `settings_open` being `Option<WindowId>` and the old
window-tagged `scale_drag`. A control drawn in the one window that exists
once needs none of that scaffolding. `UiSettings` is now purely persisted
global state, so its hand-written `PartialEq` — which had to remember to
exclude every transient field or a half-typed password would trigger a save —
is simply derived.

**The window**

| Tab | Holds | Key |
|---|---|---|
| library | (unchanged) | `L` |
| queue | now-playing, the queue, and "remember the queue between runs" next to the thing it remembers | `Q` |
| sources | the list and the add form, with the LAN-discovery / password / plex.tv-link flows given a window instead of a 420px column over video | `S` |
| config | max concurrent streams, cache per stream, audio | `C` |

`library.rs` became `browser/`: `mod.rs` (tab bar, shared state, helpers)
plus one file per tab. `overlay.rs` lost ~330 lines.

**Per-window UI scale.** The premise that there were no per-window settings
was wrong by one, and it was the one that mattered: a single `ui_scale`
across a 5120x2160, a 3440x1440 and a 2160x3840 portrait monitor cannot be
right for all three. `Screen` now owns `ui_scale` and its drag draft;
committing a window's slider also updates the persisted value, which is the
starting scale for windows opened later and for the next launch. Because the
panel is drawn on exactly one window, the draft needed no `WindowId` tag.

Scales are remembered per monitor in `UiSettings::monitor_scale`, keyed on
`MonitorHandle::name()` — `WindowId` does not survive a restart and a monitor
index shifts when displays are plugged in. A window seeds from its monitor's
remembered scale, falling back to `ui_scale`; committing writes under the
monitor the window is on *now*, so dragging a window to another display and
adjusting it there saves against that display. Moving a window does not
re-scale it — changing the UI out from under a drag is worse than a stale
scale. The config tab lists what is remembered with a per-monitor "forget",
since otherwise a scale saved on a rarely-used display is invisible.

`--ui-scale` became a run-only override instead of writing into the
persisted settings, where it silently overwrote the saved value on every
debugging launch.

**Fallout fixed on the way**

- The browser window had no gear, so its own scale was unreachable — and it
  is the text-heavy window that most wants one. The painter-drawn gear is now
  shared between the wall's status bar and the browser's tab bar.
- L/Q/S/C focused an existing browser window but the status-bar button always
  opened another. Both now go through `OverlayAction::NewLibraryWindow`, which
  focuses first.
- An empty wall was a dead end once the sources form left the gear. It now
  says which of three things is true — no sources, empty queue, or no tiles —
  and offers the one action that fits. The big "+" was a separate centred
  Area, so on a first run it painted on top of the explanation; they are one
  state now.
- `ui.with_layout` takes all remaining space, which in an `Area` is the rest
  of the screen. The old panel was tall enough to hide it; the short one
  stretched to full window height with the close button stranded at the
  bottom. `allocate_ui_with_layout` with an explicit height fixes it.
- The startup banner printed the `--audio` CLI flag while tiles took
  `settings.audio`, so a settings.conf with audio on still announced
  `audio=false`.

**Effort**: M

---

## 13. Budget display in units a person can read

Status: **implemented** (2026-08-27)

The status bar read `budget 4% (881280/23500800 mbps)`. The arithmetic was
right and the label was not: the budget is denominated in *weighted
macroblocks per second*, abbreviated "mbps" in the source, which on screen
reads as twenty-three terabits of video.

`MBPS_1080P30` is now `MACROBLOCKS_1080P30`, the accessors say `load`
(`used_load`, `ceiling_load`, `effective_ceiling_load`) for the raw unit,
and `used_equivalents()` / `ceiling_equivalents()` divide through to
1080p30-equivalent streams for display. The bar now reads
`budget 4% (3.6/96 streams)`, and every log line and toast that quoted the
raw figure was converted with it.

**Effort**: S

---

## 14. Held-arrow seeking coalesces

Status: **implemented** (2026-08-31)

Holding an arrow key stuttered: X auto-repeats at roughly 25/sec and every
repeat became its own `seek` command, so the video ground through dozens of
demuxer flushes a second (and, for Plex/Jellyfin sources, dozens of fresh
HTTP range requests). Other players queue up the distance and jump in
sensible intervals.

Deltas now accumulate in `OverlayState::pending_seek` and go out as one seek
when the key settles (200ms of quiet), on key release, or every 600ms of
continuous holding — whichever comes first. Measured: a 2.5s hold went from
roughly 60 seeks to 4.

Two details make it feel immediate rather than laggy:

- The transport renders `time_pos + pending_seek`, so the clock and scrub
  handle move at key-repeat rate even though the video moves in batches. On
  flush the tile's `last_time_pos` is set optimistically to the destination,
  so the readout does not snap backwards while the seek lands.
- A long-hold tick that arrives before another key repeat would otherwise
  issue a zero-length seek — a real flush for no movement. `seek_step`
  returns `Hold` for that case instead.

Moving the cursor to another tile mid-hold flushes what is pending to the
tile it was aimed at before the new one starts accumulating.

The timing rules live in `seek_step`, separate from the tile plumbing, so
they are unit-tested without a display.

**Effort**: S

---

## 15. Controls scale to what contains them

Status: **implemented** (2026-08-31)

The chip is laid out around a transport row that wants roughly 476 points.
On a twelve-tile wall at a UI scale of 3, a cell is about 285 points across —
so the controls covered the video they were annotating, and spilled into the
neighbouring tile.

The chip and the hover queue strip now shrink to fit: `chip_scale` takes the
smaller of "fits the cell's width" and "takes no more than 42% of its
height", with a floor of 0.55 (below that the buttons stop being hittable,
and an unusable control is worse than a crowded one). A cell with room to
spare returns exactly 1.0, so nothing changes on the sizes that were already
fine.

Everything scales through `scale_style`, which multiplies the Ui's fonts and
spacing. The first two attempts used egui's layer transform
(`Context::set_transform_layer`) instead, which is tempting because it scales
hit-testing for free — and wrong for anything that can reach the edge of its
window. `Area::content_ui` intersects both max_rect and clip_rect with
`screen_rect` *before* the transform runs, and `Layers::drain` then transforms
the clip rect along with the shapes. The status bar, laid out at `width / k`,
was clipped back to the window and then shrunk, ending short of the right edge
with "pause all" sliced in half. The chip hit the same wall on the rightmost
column. Style scaling has no such limit; the cost is that explicit pixel sizes
take the factor at their call sites.

Two further defects surfaced from the same case:

- The transport row's fixed width (`ROW_FIXED`, was `TRAILING`) counted only
  the controls *after* the scrub bar, not the four seek buttons before it. The
  row had therefore always been about 110pt wider than the `avail` it was
  handed — harmless on a big tile, but on the rightmost column the excess ran
  past the window edge and the trash button was sliced off.
- The scale floor was a flat 0.55, which at a target scale of 3 stopped the
  chip shrinking long before it fit its cell. Floors belong in physical
  pixels: `CHIP_MIN_CONTROL_PX` is 14px, so a high target scale can shrink
  much further and still leave a hittable button.

`chip_width` now bounds the chip to its cell outright, so the worst case is
clipping at the tile's own edge rather than the window's.

Window-level chrome — the status bar, and the help and settings panels —
scales to the window on the same principle (`window_scale`), but by a
different mechanism. The layer transform cannot be used for anything that
spans its window: `Area::content_ui` intersects both max_rect and clip_rect
with the screen rect *before* the transform runs, so a bar laid out at
`width / k` gets clipped back to the window and then shrunk, ending short of
the right edge with "pause all" sliced in half. That is what the first
attempt did. `scale_style` multiplies the Ui's fonts and spacing instead,
which has no such limit; the cost is that painter-drawn icons carry explicit
sizes and have to take the factor as an argument.

This also fixed a standing bug: the settings panel is 400 points wide, and on
a WM-tiled window at a target scale of 3 (297 points) it clipped off the
edge, taking its close button with it. It now fits.

The setting is now "target UI scale" in both the gear panel and the config
tab, since it is the size the UI aims for rather than the size it always is.

**Effort**: S

---

## 16. macOS

Status: **implemented, first pass** (2026-09-11) — built and run on an M1 Max
(macOS 26, three 1x displays), Homebrew mpv 0.41. Personal-use bundle only;
distribution is not done (see Next).

Everything OS-specific now lives in `tessera-app/src/platform/` — `linux.rs`
holds the behaviour the app was built on, moved rather than reinterpreted, and
`macos.rs` the counterpart. The rest of the app names no OS. Outside it: the
libmpv candidate list in `tessera-mpv`, and the bundle in `packaging/macos/`.

What differs, and why:

- **libmpv**: `libmpv.2.dylib`, tried inside the bundle
  (`Contents/Frameworks`), on the dyld path, then under Homebrew/MacPorts —
  a Finder-launched app has none of those prefixes on any search path. Linux
  now falls back to `libmpv.so.2` when `.so.1` is absent (mpv ≥ 0.35).
- **hwdec**: `videotoolbox`, zero-copy. The NVIDIA interop crash behind
  `nvdec-copy` doesn't exist here; 4K 10-bit HEVC plays with zero drops.
- **Fullscreen** is "simple" fullscreen — re-framed over the monitor on the
  current desktop, no Space, no animation — the closest match to Linux
  borderless, and robust when `--max` changes several windows at once. The
  menu bar and Dock auto-hide while any wall is fullscreen; that state is
  app-wide, and winit's per-window save/restore of it left the menu bar hidden
  whenever two walls came out of fullscreen in the "wrong" order, so it is
  recomputed from the windows every frame. The pop-shell settle timer is off.
- **Finder opens** ("Open With", double-click, drop on the Dock icon) arrive
  as an Apple Event, not argv; winit 0.29 has no hook, so
  `application:openURLs:` is added to its delegate class at runtime. At launch
  the file means what a command-line path means — it replaces the queue — so
  startup tiles wait for the first tick, when the event has landed. While
  running, it is the same request as a hand-off from a second launch (#17)
  and goes through the same `open_listed`: plays on the first wall tile,
  extras queued. (It used to also queue the file it was playing, so next
  played it again.)
- **Drag-and-drop** onto a window (implemented 2026-09-21). winit registers
  every window for `NSFilenamesPboardType` and accepts the drag, so the
  cursor already promised a drop that nothing handled — `DroppedFile` was
  never matched. Built on both platforms at once; the merged version is #21,
  which plays on the tile under the pointer. Listing runs off-thread, since a
  dropped folder is a directory walk — this side's contribution, now shared
  by every way a file can arrive.
- **Keys**: Cmd chords are never read as hotkeys. Cmd+W closes the window
  (the last one quits), Ctrl+Cmd+F toggles fullscreen like W, Cmd+Q comes
  through winit's menu — and the OS quit now gets the same teardown and
  queue save as Esc (`LoopExiting`). Ctrl-click is a right-click.
- **UI scale** multiplies the display's own scale (`set_zoom_factor`), so 1.0
  on a Retina display is normal size rather than half. On a 1x display it is
  the same absolute scale as Linux.
- **fd limit**: the hard limit reads `unlimited`, which setrlimit rejects, so
  raising to it failed silently and a Dock launch kept launchd's 256. Capped
  at `kern.maxfilesperproc` now. `/dev/fd` stands in for `/proc/self/fd`.

Found on the way, fixed for both:

- **Frame pacing.** The loop re-armed `WaitUntil(now + 16ms)` on every
  callback, including the one after the frame's work, so the period was the
  work *plus* 16ms — 45Hz at four tiles on the M1, 60fps video dropping to
  match. The deadline is now anchored to the frame's start.
- **Smooth scrolling** (trackpads, Magic Mouse; also Wayland) stepped the
  volume on every pixel delta — dozens per swipe. It accumulates into notches
  now. X11 only ever sends line deltas, so the Linux X11 path is unchanged.

Next:

- Distribution: copy libmpv and its dylib closure into `Contents/Frameworks`
  (e.g. dylibbundler), sign with a Developer ID, notarize. The loader already
  looks there first.
- Untested by hand so far: W/F/`--max` interactively, cross-window tile drag,
  Cmd+W / Ctrl+Cmd+F, ctrl-click, trackpad volume, mixed Retina + 1x
  displays (window placement converts through each monitor's scale factor;
  the monitor-under-window test mixes physical coordinates across displays).
- Keep the display awake while playing (IOPMAssertion here; Linux has the
  same gap — mpv's own inhibit belongs to its VO, which `vo=libmpv` skips).

**Effort**: M

---

## 17. Single instance, and the hand-off from later launches

Status: **implemented** (2026-09-03)

A second launch used to become a second app, which quietly broke two
invariants that are written down as global but are really *per process*:

- **One decode budget.** Two processes each admit up to the full ceiling
  against one GPU, so admission control stops meaning anything.
- **One queue, one settings file.** `save_settings` writes the whole file
  from memory and never re-reads the disk, so a process holding a stale
  copy silently erases whatever another added. Reproduced: with sources A
  and B loaded, a second writer adding C, then toggling *queue retention*
  in the first — an unrelated setting — left A and B and destroyed C. This
  is believed to be how a configured source went missing in real use.

`instance.rs` claims a Unix socket before any expensive setup; a later
launch hands over its paths, asks the running instance to come to the
front, and exits 0 (measured at ~1ms). Design notes:

- **Connect-first, never a lock file.** A lock can be inherited from a
  process that died holding it; a dead instance simply refuses connections,
  which is the signal wanted. A `kill -9` leaves the socket file behind and
  the next launch reclaims it — verified.
- **Keyed on the config directory**, not the uid. Isolating HOME isolates
  the instance, which is what lets a sandboxed run coexist with the real
  app instead of fighting it for the socket.
- **Paths are absolutised by the sender**, whose cwd is the relevant one.
- **The accept loop is a worker thread** feeding a channel drained in the
  event loop — blocking work stays off the render thread.
- `TESSERA_ALLOW_MULTI=1` opts out, for development only.

Handed-off files play immediately rather than queueing quietly, matching
the rule positional paths already follow: opening a video has to play that
video. They are also moved to the head of the pending queue, which is what
makes the no-tile branch deterministic — the window it opens comes up on
the file that was just asked for, not on whatever the cursor pointed at.

### Next: where new videos go

The policy lives in exactly one place, `open_listed`, which already has
both outcomes wired: play on an existing tile, or open a wall window for it
(today the second only fires when there is no wall window to use). A setting to
choose between them — "play in an existing stream" vs. "give it its own
window", the VLC/Celluloid distinction — is a branch at that call site plus
a `UiSettings` field, not new machinery.

**Effort**: S (for the setting)

---

## 9. Credential and key management

Status: **not started** — deliberately deferred, see below. File modes
are now tightened (2026-08-31); everything else here stands.

Credentials currently live in plaintext in three places, not two:
`~/.config/opentessera/sources.conf` (`plex_token`, `jellyfin_key`); the
`sources = [...]` JSON line of `settings.conf`, since sources moved into
the settings panel; and — the one this entry used to miss — **`queue.json`**,
because a Plex/Jellyfin direct-play URI *is* the token: every persisted
item carries `?api_key=…` or `?X-Plex-Token=…` in its `uri`, and again in
`thumb_url`. The friendlier sign-in flows (#10) do not change any of that —
they only mean the user never has to *handle* the token; one still ends up
on disk.

This is knowingly fine for now (single developer, one machine, services
reachable only on the LAN) and knowingly not fine as soon as any of those
stops being true. It should be designed properly rather than patched in:

- **Where secrets live.** Out of `settings.conf` and into either the
  platform secret store (Secret Service / `libsecret` on Linux, Keychain on
  macOS) or a separate, mode-0600 credentials file — with the plaintext
  path kept as an explicit opt-out for headless boxes.
- **What is stored.** Prefer a revocable, per-device token over an account
  password; never persist passwords. Record which server minted it so it
  can be invalidated.
- **File permissions and migration.** *Partly done (2026-08-31).* Both
  token-bearing files the app writes — `settings.conf` and `queue.json` —
  now go through `write_private` in `main.rs`: created 0600, and existing
  files chmod'd on the next write, so an install predating the change is
  tightened rather than left behind. `sources.conf` is hand-written and
  untouched. This is the cheap half; a migration still needs to move the
  secrets out of these files entirely, at which point the mode stops being
  the only thing protecting them.
- **Logging and screenshots.** `Source::label()` is printed at startup, put
  in window titles and drawn in the settings/library panels. Audit that no
  path can surface a token, including `TESSERA_DUMP_DIR` screenshots and
  panic backtraces.
- **Scope.** Camera sources are planned for the same `Source` enum and will
  bring their own credentials; the design should cover them.

**Effort**: M

---

## 10. Friendlier server sign-in

Status: **implemented** (2026-08-27); library picker added 2026-09-01

### Library picker (2026-09-01)

Signing in was friendly; saying *which library* was not. Both flows ended
by committing a source scoped to whatever sat in a free-text "library" box
— a name the user had to type before ever being shown one. Worse, the add
tore the form down (`AddForm::clear` + `Auth::reset`), so a second library
from the same server meant walking through the whole plex.tv link again.
One library per sign-in, name typed from memory.

Choosing a server now *lists* its libraries instead of committing:

- Tick any number → one `Source` per library, which is what makes them
  browse separately (distinct `label()` → distinct catalog entries).
- "add all as one source" keeps the whole-server behaviour.
- "back" returns to the server list without re-linking.
- The pasted-token/API-key paths get "browse libraries" too, so they are
  not second-class.
- Adding a batch reports duplicates without aborting the rest; only a clean
  add closes the form, so the picker stays up for the other libraries.

`ServerLibraries` + `ServerKind` in `auth.rs` carry both flows through one
worker request and one picker widget; only the `Source` a tick becomes
differs.

**Which libraries are offered** is decided differently per server, and the
difference is load-bearing:

- **Plex** — `type == "movie"` on `/library/sections` is authoritative.
- **Jellyfin** — `CollectionType` is *not*. A real server here reports
  `CollectionType: null` for a library holding 30 movies, while `Playlists`
  is properly typed and holds none; filtering on the type offers exactly
  the wrong one of the two. So each folder is counted with
  `Limit=0` (`TotalRecordCount`, no items) and offered if it holds movies
  or declares itself a movie library and is merely empty.

Counts are shown either way — `_sx2 (4279 movies)` vs `Movies (30 movies)`
is the whole decision, and it is invisible without them.

Still open: only movie libraries are offered, because `Source::list` only
serves movies. TV libraries need a listing path before they can be picked.

Typing an API key or `X-Plex-Token` is the least friendly part of adding a
source. All three of these already exist in the Quest app and port over:

1. **Jellyfin username/password** — `AuthenticateByName` returns an access
   token, so the user types what they already know instead of digging a key
   out of the admin dashboard.
2. **Discovery** — Jellyfin answers a UDP broadcast on 7359
   (`JellyfinDiscovery.kt`); Plex's resource list returns server addresses
   once linked. Turns "type an IP" into "pick from a list".
3. **Plex plex.tv link** — mint a PIN, show a 4-character code, poll until
   the user enters it at plex.tv/link, then list the account's servers
   (`PlexLinkClient.kt`). No token, no IP typing. The manual token path
   stays as the offline fallback.

All three shipped, in `tessera-core::signin` (blocking, with the fiddly
parsing split out and unit-tested) behind `tessera-app::auth` (a worker, so
none of it touches the render thread — the Plex poll can run for minutes).
The settings panel's add-source form grew a sign-in section per server kind;
the raw token/API-key path stays as the fallback, since it is the only one
that works with no internet or a locked-down server.

Notes from the port:
- Plex candidate URLs are ordered LAN `http://ip:port` first, then the LAN
  `plex.direct` HTTPS URIs, then remote — and relay connections are dropped
  outright, because they proxy through plex.tv and can never direct-play.
  Each server's first *reachable* candidate is probed before it is offered.
- `plex-client-id` is persisted in the config dir. plex.tv keys the PIN and
  the account's device list on it, so it must not change between creating
  and checking a PIN, nor across runs.
- Jellyfin sign-in exchanges the password for an access token once and
  never stores the password.

Whatever these produce still lands on disk, so #9 is the follow-up, not an
alternative.

---

## 18. Chapter markings in the transport

Status: **not started**

Celluloid draws chapter boundaries on its seek bar and lets you jump
between them; a two-hour film with chapters is far easier to navigate than
one long undifferentiated rail. Nothing about this is wall-specific, which
is rather the point — the transport is already a real transport, and this
is the obvious missing piece of it.

### Where the data comes from

mpv already has it, and the pattern for reading it already exists here:

- `chapter-list` — a node property, so requesting it as a string returns
  JSON: an array of `{ "title": ..., "time": <seconds> }`. This is exactly
  how `Tile::refresh_tracks` reads `track-list`, and should be read the
  same way, into the same kind of cache on `Tile`.
- `chapters` — the count; `chapter` — the current index, and **settable**,
  which gives prev/next for free without arithmetic on timestamps.

Refresh where `refresh_tracks()` is already called: on `FileLoaded`. A
chapterless file (and every live stream) simply yields an empty list and
nothing is drawn.

### Drawing it

The scrub bar is an `egui::Slider` whose rail is drawn at
`spacing.slider_width`, which the transport sets to `scrub_w` — so the rail
rect is known and marks can be painted over it from the `Response`. Two
things to respect, both already documented invariants:

- **Marks are explicit pixel sizes, so they take `k` at the call site.**
  The chip is style-scaled, not layer-scaled, and style scaling does not
  reach painter-drawn geometry (see §15).
- **The chapter list rides in `TileUiInfo`, cloned only for the chip
  tile.** The track lists already work this way after the per-frame
  snapshot work — cloning a chapter list for every tile every frame is the
  same waste in a new coat.

Worth having beyond the marks themselves: the current chapter's title
beside the time readout, and click-a-mark to seek. Keys for prev/next
chapter would have to go through `OverlayState::pending_seek`'s coalescing
if they are made repeatable, or be declared non-repeating like every other
toggle (§14).

**Effort**: S–M

---

## 19. Security cameras as a source

Status: **implemented** (2026-09-03)

A live camera is, to this app, a media item that happens never to end.
`Source::Cameras` joins the enum beside Plex and Jellyfin, `list()` returns
one `MediaItem` per stream, and a camera is queued, tiled, pinned, soloed
and dragged between screens exactly like a film. No new plumbing in the
queue, the layout or the budget — which was the test of whether `Source`
was drawn in the right place.

### Streams directly, not a vendor API

The original sketch here was a UniFi Protect client. Probing real hardware
killed it, and rightly:

- Protect's API needs an API key **and** a self-signed-certificate bypass —
  the controller presents `CN=unifi.local` on `192.168.1.1`, so strict
  verification fails and hostname verification cannot succeed either. That
  means a custom rustls verifier, a new direct dependency, and a
  vendor-specific parser.
- All of which buys is a URL that mpv was always going to open anyway.

So a camera source is a list of stream URLs. It works with any camera that
speaks RTSP — or MJPEG over HTTP, or HLS — with no key, no certificate
handling, and no per-vendor client to maintain.

**Discovery still does the tedious half.** Ubiquiti devices answer a UDP
broadcast on port 10001 with their name, model and address and **no
credentials at all**, so the scan names the cameras and only the stream
path has to be supplied. Verified against real hardware: three cameras
(two G4 Domes, a G6 Turret) and the controller. Two pitfalls found by doing
it rather than reading about it:

- A gateway reports *every* interface, WAN included, so trusting the
  payload's address listed a public IP alongside the cameras as though it
  were another device. A device is reachable at the address it answered
  from; that is the only one worth having, and using it also collapses a
  multi-interface device's several replies into one entry.
- **A Protect-adopted camera does not serve RTSP itself** — 554 and 7441
  are both refused on the camera. Its stream comes from the controller, at
  `rtsps://<controller>:7441/<alias>`, with RTSP enabled per camera in
  Protect. The form says so, because the guessed `rtsp://<address>:554/`
  prefill is right for a generic camera and wrong for these.

### What a stream with no end actually broke

1. **mpv options stopped being global.** `TileOptions` was one struct
   applied identically to every tile, and its 512 MiB demuxer cache is
   exactly backwards for a live feed — that is not resilience, it is
   latency. Options now key off the item: live gets `profile=low-latency`,
   `rtsp-transport=tcp`, `network-timeout=10`, an 8 MiB cache and no
   readahead; recorded gets what it always had. `set_cache_mb` skips live
   tiles, so changing the global cache setting cannot hand back the latency
   the live profile just removed.
2. **Looping is meaningless** and is no longer set for live items — mpv
   would only ever act on it after an EOF that never arrives.
3. **The decode budget was being told the stream weighs nothing.** A live
   stream reports `0x0` at file-load: mpv has the container open but has
   not seen a frame, and the low-latency profile deliberately probes as
   little as possible. `StreamSpec { width: 0, height: 0 }` weighs zero, so
   a wall of cameras would have admitted itself for free — undermining
   admission control in exactly the case it exists for. Two halves to the
   fix: a zero-size `Loaded` no longer updates the budget (the placeholder
   stands), and `ensure_texture` now reports a size *change*, so the first
   real frame re-reports the stream and the budget corrects itself.
   Observed end to end: `0x0` → "holding the budget estimate" →
   `loaded 640x360@30`.
4. **Delete is refused.** There is no file to remove and the trash sits
   right there on the transport next to a live feed. The compiler demanded
   this the moment `SourceKind::Camera` was added, which is the enum
   earning its keep.

`SourceKind::is_live()` is where the distinction lives, rather than
guessing from a URI scheme, so a future source cannot forget to declare it.

### Verified

Discovery found the real cameras through the real UI; a source was
assembled, persisted and re-listed with each camera appearing in the
library beside films, with the same queue and play buttons. Playback was
tested against a genuinely endless MPEG-TS over UDP (the Protect streams
need an alias this build has no way to obtain): it ran at `1.00x 15.0fps
[640x360] drops=0`, with the budget correcting itself as above.

On macOS (2026-10-16), by hand: every Protect camera played over the
`rtsps://<controller>:7441/<alias>?enableSrtp` URL copied from its Protect
settings (Advanced → RTSP). Playback is fine; setup is the clunky part —
one URL per camera, copied out of Protect and pasted into the scan's empty
field, because adopted cameras serve nothing themselves and discovery
cannot learn the alias.

### A stream that will not open must say so

Found in use, and three separate defects behind one symptom — cameras
queued, the queue advanced, and the last frame of the *previous* item just
sat there:

1. **A failed load was invisible.** `load()` is async, so a bad stream
   surfaces as an end-of-file event carrying an error code, which was
   `eprintln!`d and otherwise dropped. The tile kept its old texture, so a
   failure was indistinguishable from a freeze — the queue had moved on and
   the picture had not. Now the frame is cleared and the reason is said out
   loud: *"Carport: could not open the stream (unknown format)"*.
   `tessera_mpv::error_name` is public for this.
2. **`load()` did not re-apply per-item options.** They were set once, at
   tile *creation*, from whatever that tile first held — but one queue
   mixes films and cameras freely. A camera loaded into a tile built for a
   film inherited a 512 MiB demuxer cache, which on a live feed is not
   resilience but latency. The options that differ are now re-derived on
   every load, in both directions.
3. **The prefill was a wrong answer that looked like a right one.** Taking
   a discovered camera filled in `rtsp://<camera>:554/`, which is a fair
   guess for a generic camera and simply false for a Ubiquiti one: those
   refuse 554, because the stream comes from the controller. It was
   accepted, saved and queued, and only failed at playback. A "UVC" model
   now gets an empty field, an amber "needs the controller's url" beside
   it, and instructions naming the Protect setting to turn on.

The through-line: the app knew the stream had failed and had the model
string that predicted it would, and told the user neither.

### Pause makes a camera fall behind, so resume rejoins

A paused live tile keeps demuxing, so it comes back late. Measured on a 4K
Protect feed, the lag tracks pause time 1:1 and then stops:

| paused | behind on resume |
|---|---|
| 5s | 5.03s |
| 10s | 10.02s |
| 20s / 40s / 60s | 10.26s |

The ceiling is the 8 MiB live cache at that camera's bitrate — pause longer
and mpv simply stops reading. Ten seconds is a lot on a security camera.

**Resuming a live tile now rejoins at the edge**, because on a camera
"play" means "show me now", not "show me what I missed". Seeking cannot do
it: mpv *accepts* a forward seek on a live stream, returns success, and
changes nothing — measured identical before and after. A reload does, and
took 0.07s on a LAN camera, taking 10.09s of lag to 0.00s.

The transport says so too. A live tile has no duration to scrub and nothing
to seek, so that space carries a **LIVE** indicator instead: green at the
edge, amber and clickable with the lag on it (`LIVE −13s`) when behind — for
drift that arrives without an explicit pause, such as a network stall. The
seek buttons and the time readout are hidden for live tiles, both being
meaningless.

(The first cut of that label led with `●` U+25CF, which is not in egui's
bundled fonts and rendered as tofu — the house rule about checking a glyph
before trusting it, ignored and then immediately paid for.)

### Reconnect, and why an error event is not enough

*Done (2026-09-03).* A live tile that drops now retries on its own, backing
off 1s → 2s → 5s → 10s → 20s → 30s and then holding there forever. It never
gives up: a wall is a thing you leave running, and a camera that comes back
on Monday should simply be there again. Only the first drop raises a
notice; the rest are the retry loop doing its job.

**The naive version does not work, and testing is the only way to find
that out.** Reconnecting on `EndFile` with an error covers a stream that
refuses to *open* — but killing a source mid-play produced no end-of-file,
no error, and no event of any kind. mpv simply stopped, and the tile sat at
`0.00x 0.0fps` indefinitely. The most ordinary camera failure there is —
one that stops sending — is silent.

So the trigger is the clock, not an event: a live tile whose `time-pos`
stops advancing for [`STALL_TIMEOUT`] has dropped, whatever it did or did
not report. Detection cannot be instant, because after the source goes away
mpv plays out its buffer — up to the ~10s live cache — so the clock keeps
moving for a while first; the timeout is measured from when it stops.

Two things the stall clock has to be told about, or it reads them as drops:
a fresh load restarts it (or a reconnect is judged on the silence that
preceded it), and so does resuming, since a paused tile's clock has not
moved for as long as it was paused.

Verified end to end against a killable source: kill it → *"stopped sending
— retry 1 in 1s"* → the backoff escalates → bring it back → *"Flaky Cam is
back"*, playing at 1.00x with no drops and no intervention. The transport
shows `reconnecting…` in red throughout.
- **Substream selection.** Protect exposes high/medium/low. A wall of
  sixteen wants low, the pinned one wants high — switching channel on
  pin/unpin remains the good idea it was, but it needs per-camera URLs for
  each quality, which the current form would make tedious.
- ~~**Snapshot thumbnails.**~~ *Done (2026-09-03).* No vendor snapshot
  endpoint and no auth header needed after all: the same `vo=image` frame
  extraction §20 uses for files works on any URL mpv can open, including
  `rtsps://`, in 0.34s on a real camera. Refresh is hourly and
  **opportunistic** — the TTL is only checked when something actually draws
  the thumbnail, so cameras nobody is looking at cost nothing. Verified by
  shortening the TTL: the entry regenerated on schedule while the library
  was open and stopped the moment the tab was switched away. A stale
  thumbnail is refreshed *while still on screen*, and a refresh that fails
  keeps the old picture rather than dropping to a placeholder — a camera
  that is briefly unreachable still shows what it watches.
- **ONVIF WS-Discovery** (UDP multicast 239.255.255.250:3702) would find
  non-Ubiquiti cameras the same way, and can report the RTSP path itself —
  which would remove the one manual step left.
- The scan lists every Ubiquiti device, not just cameras; a switch or an AP
  will appear in the list too.

**Effort**: L — done, minus the gaps above.

---

## 20. Thumbnails for local files

Status: **implemented** (2026-09-03)

Plex and Jellyfin hand over a poster URL; a file on disk had nothing, so
every local item drew a grey placeholder. A frame out of the file is the
obvious substitute.

**Extraction uses libmpv, not ffmpeg.** `vo=image` writes a frame to a
directory and `frames=1` stops after the first — no display, no GL context,
no external tool, and libmpv is already a hard dependency. Measured at
**0.37s for a 77 GB 4K HEVC remux on cold storage**, so one worker keeps up
with what a scrolling list asks for. The worker takes its own `MpvLib`
handle (a dlopen of a library already in the process) rather than threading
the main one through `Screen::new` and `create_screen`. These instances
never get a render context, so the GL-thread teardown rule does not apply
to them.

**The cache is central and fingerprinted.** `$XDG_CACHE_HOME/opentessera/
thumbs`, one JPEG per fingerprint — explicitly *not* beside the user's
media, which was the stated requirement, and not in the config directory
either, because this is regenerable and config is not. Deleting the
directory is supported and costs a re-extract.

The fingerprint is FNV-1a over **path, size and mtime**. Content-hashing 77
GB to decide whether to draw a 9 KB image is absurd, and those three are
already on every local `MediaItem`, so a cache *lookup* costs no I/O — which
is what makes it usable while scrolling. Re-encoding in place moves the
mtime and re-extracts; moving the file does too. For a cache that is the
right trade.

Writes go to a scratch directory and are renamed into place, so a cache hit
always means "complete" — a killed process leaves a stray directory, never a
truncated JPEG that would be served forever.

Verified: four clips extracted correct distinct frames; a restart reused
them byte-identically with no regeneration; `touch` on a file produced a new
fingerprint and a new entry; and nothing whatsoever was written next to the
media.

### Known gaps

- **The disk cache never shrinks.** A changed file orphans its old entry.
  The in-memory texture cache has an LRU bound; the disk one wants a sweep
  (age, or total size) — worth doing before anyone points a library of tens
  of thousands of files at it.
- Extraction is single-worker, shared with remote poster fetches. Fine at
  0.37s each, but a slow network mount will queue behind itself.
- The sample point is a fixed 25%. Something smarter (scene detection, or
  skipping near-black frames) would occasionally do better.

---

## 21. Drag and drop

Status: **implemented** (2026-09-03)

Dragging a video from the file manager onto a window plays it; dragging
onto the browser window adds it to the queue instead. Multiple files play
the first and queue the rest, matching what a hand-off from a second launch
already does — both go through one function, so they cannot drift apart.

**Drops target the tile they land on.** winit reports `DroppedFile` with no
pointer position, so this reads `Screen::cursor`, which was expected to be
stale during an X11 drag (the source holds a pointer grab). Tested with a
real GTK XDND source rather than assumed: with the pointer parked over the
top-left tile beforehand, dropping on the bottom-right quadrant of a 2×2
wall loaded tile 3, and dropping on the top-right loaded tile 1. It works
here; it depends on the toolkit and WM delivering motion during the drag,
so it is a refinement rather than a guarantee — the drop still lands
somewhere sensible either way.

**macOS needed the pointer asked for.** A Finder drag sends no mouse-moved
events at all: AppKit runs a drag session, and winit 0.29 implements
`draggingEntered`/`performDragOperation` but not `draggingUpdated`. The
cursor was wherever it was before the drag — usually outside the window —
so every drop landed on the first tile. `platform::pointer_in_window` asks
AppKit instead (`mouseLocationOutsideOfEventStream`, into the content view,
scaled to physical pixels), each frame while a drag hovers and once more as
it lands; on Linux it returns None and the tracked cursor stands. The hint
now outlines the tile a drop will replace and sits inside it ("plays in
this tile"), rather than centred over the corners of a 2×2. Tested by hand
on macOS (2026-10-16): a file dropped on the lower-left tile of a four-tile
wall played there.

winit delivers one event per file with no grouping, so drops are
accumulated per window as they arrive and acted on together in the frame
block: a multi-file drop is one gesture and behaves like one.

A `HoveredFile` banner says what will happen before the user commits —
"drop to play" on a wall, "add to the queue" on the browser. It is
window-scoped (`OverlayState::drop_target`, an `Option<WindowId>`) for the
usual reason: every screen runs its own egui pass over one shared struct,
and a plain bool would light the hint on all of them.

Both that banner and the pre-existing empty-wall panel needed an explicit
`set_width`: `vertical_centered` inside an `Area` claims the rest of the
window, which drew the hint as a full-width band across the wall. Same trap
`with_layout` sets in the gear panel.

### Playing and queueing are alternatives

First cut got this wrong: every dropped file was appended to the queue *and*
the first was loaded onto a tile, so it played while also sitting at the
head of the pending tail — press next and the same film started again,
which reads as the wall being stuck on it. The comment even said "put it
next in line as well as playing it", which is the bug stated out loud.

The queue is now touched only on the branch that needs it:

| where it lands | queue |
|---|---|
| plays on a tile | only the *extras* from a multi-file drop |
| dropped on the browser | all of them; nothing plays |
| nowhere to play | all of them, first moved to the head, then a window opens on it |

This is the split `OverlayAction::PlayNow` already drew — a tile owns the
item it is playing, and the queue holds what is still to come. Verified:
dropping onto a wall with a three-item queue then pressing next four times
walks the original queue and the dropped file never reappears; a
three-file drop plays the first and queues exactly two.

### One path for every arrival (2026-10-08)

A hand-off from a later launch, a macOS Finder open while running, and a
drop are the same request with a different target, and now take one path:
the paths are listed on a worker by `list_off_thread` (any of them can be a
folder, and a directory walk stalls a frame), then `open_listed` acts. The
tile under the pointer is read when the drop arrives, not when the listing
lands. A wall window with no tiles gets a new tile — the window dropped on,
or for a hand-off the first wall window — rather than a second window being
opened beside it.

### Known gaps

- Only files are accepted. A dropped URL (`text/uri-list` pointing at
  http) would be a reasonable thing to feed mpv directly.

---

## 22. Audio on by default, and a wall that starts ready

Status: **implemented** (2026-10-08)

- **Audio is on unless asked off.** `--audio` used to be needed for any
  sound at all; now `--no-audio` (or the config tab) turns it off, and
  `--audio` remains to override a saved "off".
- **The first tile is heard; the ones beside it are not.** Opening one
  video should just play it, sound included, but a wall of N streams is
  noise. The rule is decided at tile creation: a tile starts unmuted only
  if no other tile exists anywhere. Moving a tile between windows keeps
  its own mute state.
- **No "+" before the first video.** A launch with nothing queued opens an
  empty wall; the first thing queued from the browser now starts a tile on
  it. Only until the first tile exists — closing every tile later is a
  choice, and the wall does not refill behind the user. "Play now" on an
  empty wall starts a tile too, instead of a toast.

Found on the way: the queue read a fresh cursor (`None`) two ways.
`advance`/`upcoming` treated it as already on item 0, so the first tile
added by "+" played item 1; startup filled from `current()` and left the
cursor on an item nothing had played, so "+" after it skipped one. `None`
now means nothing has started and item 0 is next, everywhere; startup fills
through `upcoming` like everything else, and `restore` puts the cursor just
before the saved item so a restored queue still resumes on it.
`spawn_tile` no longer advances the queue — callers taking from it do — so
an item that was never queued (play now, an opened file) can't consume
someone else's place.

---

## 23. MCP interface (`tessera mcp`)

Status: **implemented** (2026-10-08) — see `docs/MCP.md`

Asked for by Tia, a voice assistant. A stdio shim over the
single-instance socket, so a client needs one line of config and no port or
token. Seven player tools (status, open, control, subtitles, frame, queue,
close), coarse and defaulting to the focused tile, replying in short
speakable text. `--dev` adds six more aimed at a sandboxed instance —
launch/quit/log, on-demand screenshots, a UI state dump, and synthetic
pointer input through the real event path — which replace the
`TESSERA_DUMP_DIR` + xdotool routine for checking a change.

Monitors are also named by connector (`DP-4`) or position (`left`,
`middle`, `right`; `top`/`bottom` only when stacked) — `tessera_core::
monitor`, suggested by Tia's agent so clients don't each keep a table. A
windowed *move* doesn't stick under pop-shell (tried: plain move, and a
fullscreen hop — both snapped back to the tile slot); `tessera_window`
reports where the window really is.

Monitors (2026-10-08): `--monitor N` / `--fullscreen` on the command
line, `tessera_window` (list monitors and windows; fullscreen, windowed,
move, new), and `monitor`/`fullscreen` on `tessera_open` — which opens a
window on that monitor when none is there, with the file at the head of
the queue so it is what that window plays. Status reports each tile's
monitor, read from where the window actually is (it can be dragged).
Runtime windows take a `Placement`; a fullscreen one is re-asserted by the
WM settle timer, as at startup. Pin was already `tessera_control pin`.

Crop and zoom (`tessera_control` crop/zoom, 2026-10-08): a per-tile
`View` in core, drawn by the compositor as a UV sub-rectangle fitted at the
cropped aspect — which is what makes cropping a letterbox fill a 21:9
screen rather than just shrink the picture. `crop auto` runs ffmpeg
cropdetect at three points and keeps the least aggressive box. The view is
tied to the item it was set for and travels in `ResumeState`. No on-screen
control yet; it's an MCP feature first.

`tessera_sources` (list; add a folder) followed on 2026-10-08, and with it
a fix to folder sources themselves: listing only read the folder's top
level, so a library laid out as `Movies/`, `Shows/<show>/...` showed
nothing. `collect_items` now walks recursively, skipping hidden entries
(macOS `._x.mkv` AppleDouble files carry a video extension), NAS trash and
thumbnail folders, and symlink loops; `m2v`, `vob` and `3gp` were added to
the recognised extensions.

Built on the way: `SourceKind::Url` (http(s) opened directly; not
deletable), `audio-client-name=Tessera`, on-demand GL captures on `Screen`,
and core modules for title parsing, subtitle cues and time notation, all
tested headlessly. Testing it found `PlaybackQueue::pending(usize::MAX)`
wrapping to an empty range in release builds.

### Next

- **Events**: a streaming socket request the shim turns into MCP
  notifications (playback ended, now playing, paused, tile closed).
- **Subtitles for Plex/Jellyfin items** via the servers' own subtitle
  endpoints; today only the line on screen is available for streamed items.
- **Keys in dev input**: winit's `KeyEvent` can't be built outside winit, so
  this needs the hotkey handler factored out of the event match first.

**Effort**: S each

---

## Implemented features (for context)

- Phase 2 budget: eviction (OLDEST_STARTED), adaptive ceiling, single-stream rule
- egui overlay: status bar, chips, transport, hover queue strips + thumbnails,
  auto-hide (configurable: 1s idle, then a 1.5s fade) + Tab pin, toasts, help + settings panels
- Transport: scrub, ±10s, next/prev, keyboard; async seeks/loads
- Tile drag-to-rearrange: same-screen drag-to-swap; cross-screen drag moves
  the tile (snapshot/rebuild with position, paused, volume, tracks intact) —
  all of #1
- Visual volume: per-tile chip slider + scroll wheel (#2)
- Track selectors: inline A/S rows for audio + subtitles, off included (#3, #4)
- Pin + solo: P/📌 pins a tile big with a bottom strip of the others; 🎤
  solos audio (mutes all other streams) (#5)
- Sources: local, Plex, Jellyfin (direct-play URIs), library filters,
  delete-at-source
- Panel management: +/- tiles (transport buttons and -/+ hotkeys), per-window
  W toggle, F tile zoom, --populate vs --tiles, --screens monitor list
- Settings persistence: ui_scale, cache_mb, max_streams, audio, sources
  (~/.config/opentessera/settings.conf; sources edited in the gear panel)
- Stability: fd-limit raise at startup + fds status line (full-wall fd exhaustion
  wedged nvidia-drm, 2026-08-26), nvdec-copy default (CUDA/GL interop segfault
  fix, commit c385501), fatal-signal backtrace handler
- Audio: per-tile mute, --audio/--audio-device, runtime enable
- Stability: nvdec-copy default (CUDA/GL interop segfault fix, commit c385501),
  fatal-signal backtrace handler
