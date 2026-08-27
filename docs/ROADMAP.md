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
- Settings persistence: ui_scale, cache_mb, max_streams (~/.config/opentessera/settings.conf)
- Stability: fd-limit raise at startup + fds status line (full-wall fd exhaustion
  wedged nvidia-drm, 2026-08-26), nvdec-copy default (CUDA/GL interop segfault
  fix, commit c385501), fatal-signal backtrace handler
- Audio: per-tile mute, --audio/--audio-device, runtime enable
- Stability: nvdec-copy default (CUDA/GL interop segfault fix, commit c385501),
  fatal-signal backtrace handler
