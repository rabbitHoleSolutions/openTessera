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
Needs --audio; toast explains otherwise.

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

## 15. Tile controls scale to the tile

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

Implemented as an egui *layer transform* (`Context::set_transform_layer`)
rather than by multiplying twenty font sizes and paddings. egui transforms
hit-testing along with the shapes — `layer_id_at` included, which is what
the click router uses — so the buttons stay where they look. Verified both
directions: a click on the scaled transport's skip button seeks the right
tile, and a click in the space the chip no longer covers falls through to
the video and pauses it.

Two details the transform imposes: layout runs before it, so the chip's
width budget is divided by the scale to end up filling the tile; and the
transform is anchored at the chip's own corner, or it would drift toward the
window origin.

The setting is now "target UI scale" in both the gear panel and the config
tab, since it is the size the UI aims for rather than the size it always is.

**Effort**: S

---

## 9. Credential and key management

Status: **not started** — deliberately deferred, see below.

Credentials currently live in plaintext: `~/.config/opentessera/sources.conf`
(`plex_token`, `jellyfin_key`) and, since sources moved into the settings
panel, inside the `sources = [...]` JSON line of `settings.conf`. The
friendlier sign-in flows (#10) do not change that — they only mean the user
never has to *handle* the token; one still ends up on disk.

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
- **File permissions and migration.** `settings.conf` is written 0644 today
  and has held tokens; a migration needs to move existing secrets out AND
  tighten or rewrite the old file.
- **Logging and screenshots.** `Source::label()` is printed at startup, put
  in window titles and drawn in the settings/library panels. Audit that no
  path can surface a token, including `TESSERA_DUMP_DIR` screenshots and
  panic backtraces.
- **Scope.** Camera sources are planned for the same `Source` enum and will
  bring their own credentials; the design should cover them.

**Effort**: M

---

## 10. Friendlier server sign-in

Status: **implemented** (2026-08-27)

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

## Implemented features (for context)

- Phase 2 budget: eviction (OLDEST_STARTED), adaptive ceiling, single-stream rule
- egui overlay: status bar, chips, transport, hover queue strips + thumbnails,
  auto-hide (5s) + Tab pin, toasts, help + settings panels
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
