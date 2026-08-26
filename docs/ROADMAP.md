# openTessera Roadmap

Feature tracking across sessions. Update status as work completes.
Effort estimates: `S` (< half session), `M` (~1 session), `L` (multi-session).

---

## 1. Tile drag-to-rearrange

Status: **Phase 1 implemented** — same-screen drag swaps tiles; Phase 2
(cross-screen) not started.

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

### Phase 2: Cross-screen drag (later)

A tile's mpv render context is bound to its screen's GL context, so moving
between monitors requires teardown/rebuild.

**Design**
- Drag to a screen edge → drop → tile moves to that screen's next free slot.
- Playback continuity: read `time-pos` before teardown, reload with
  `start=<pos>`; preserve paused state, volume, track selections.
- Decide queue behavior: move the tile's current queue entries to the target
  screen's queue, or just the playing item.

**Implementation notes**
- Serialize tile state (URI, time-pos, paused, volume, aid/sid selections) →
  destroy render context → rebuild `Tile` on target screen → `load()` at
  position. mpv instance does not survive; only its state snapshot does.

**Effort**: M–L

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

Status: **not started**

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

Status: **not started**

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

## Implemented features (for context)

- Phase 2 budget: eviction (OLDEST_STARTED), adaptive ceiling, single-stream rule
- egui overlay: status bar, chips, transport, hover queue strips + thumbnails,
  auto-hide (5s) + Tab pin, toasts, help + settings panels
- Transport: scrub, ±10s, next/prev, keyboard; async seeks/loads
- Tile drag-to-rearrange: same-screen drag-to-swap (Phase 1 of #1)
- Visual volume: per-tile chip slider + scroll wheel (#2)
- Sources: local, Plex, Jellyfin (direct-play URIs), library filters,
  delete-at-source
- Panel management: +/- tiles, per-window W toggle, F tile zoom
- Settings persistence: ui_scale, cache_mb, max_streams (~/.config/opentessera/settings.conf)
- Stability: fd-limit raise at startup + fds status line (full-wall fd exhaustion
  wedged nvidia-drm, 2026-08-26), nvdec-copy default (CUDA/GL interop segfault
  fix, commit c385501), fatal-signal backtrace handler
- Audio: per-tile mute, --audio/--audio-device, runtime enable
- Stability: nvdec-copy default (CUDA/GL interop segfault fix, commit c385501),
  fatal-signal backtrace handler
