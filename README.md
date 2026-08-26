# openTessera

Open-source desktop multi-stream video wall. A sister application to the Meta
Quest app **Tessera**: many simultaneous video streams, arranged as tiles, with
library curation and organization tools. Targets Linux first (this project was
built on Pop!_OS/GNOME), macOS next.

## Current status

Phase 1 skeleton — playback foundation:

- Multi-monitor fullscreen: one borderless fullscreen window per monitor, each
  with its own GL context and its own playback queue (or a single windowed
  window)
- N tiles per screen via libmpv render API, GPU-direct (frames never touch CPU
  memory), hardware decode (NVDEC/VA-API/VideoToolbox via mpv)
- Grid layout solver (coverage-scoring rows/cols search, ported from the Quest
  app's LayoutSolver) with per-tile aspect letterboxing
- Decode budget controller: weighted macroblocks/sec admission with
  codec weights, max-streams gate, real probe data fed back on load,
  OLDEST_STARTED eviction (the oldest playing stream is paused to admit
  a new one or to sustain the user's stream count), and an adaptive
  ceiling driven by real decoder frame-drop telemetry (sustained drops
  shrink the effective ceiling and evict the worst offender; clean
  windows recover it). A single stream is always admissible alone.
  Budget is global across screens; resuming a paused tile re-requests
  admission and may evict.
- Cursor-based wrapping playback queue per screen (name/date/size/shuffle
  ordering, pending-tail re-sorting that preserves the current item)
- Overlay UI (egui, Tab to toggle, `--overlay` to start visible): status bar
  with live budget/stream/eviction counters and a play/pause-all button,
  per-tile info chips (title, play state, position/duration, mute; click the
  chip to pause/resume the tile through the budget), and an up-next queue
  strip. UI interactions execute with the same budget-aware semantics as
  clicks on the video itself.
- Transport: per-tile chip transport row (prev / -10s / +10s / next buttons
  plus a scrub bar), keyboard Left/Right seeks ±10s on the focused screen,
  N/P load the next/previous queue item on each tile.
- Queue management (Quest-immersive style): hovering a tile shows its
  screen's queue as a thumbnail strip along the bottom of that tile — click
  a thumbnail to play it on that tile immediately, right-click to remove it
  from the queue, drag to reorder, shuffle / A–Z buttons re-sort the pending tail. Hovered thumbnails highlight.
  Thumbnails come from the media servers (Jellyfin primary art, Plex
  thumbs) via a background fetch+decode cache; local files show a
  placeholder.
- Panel management: each tile's transport row has "−" (remove that tile —
  its GL resources and stream are freed and the grid re-solves) and "+"
  (add a tile to this screen, taking the next queue item; honors the
  decode budget, `--tiles` cap, default 12, and budget-aware eviction).
  Other screens are unaffected. Failed actions explain themselves in a
  transient on-screen toast (budget full, tiles cap, empty queue, ...).
- Fullscreen: "F" (or the corner-bracket icon on a tile's transport) zooms
  that tile to fill its screen — in windowed mode the window also goes
  borderless-fullscreen on its monitor, so other monitors and windows keep
  working; in multi-screen mode the other screens keep playing untouched.
  F/Esc toggles back; Esc only quits when nothing is zoomed. `Shift-F`
  toggles the window itself between windowed and borderless fullscreen
  (the whole wall layout, no tile zoom) — also per-screen.
- An emptied screen (all tiles removed) shows a large centered "+" to add
  a tile back.
- Delete from transport: each tile's transport row has a 🗑 button that
  deletes the tile's current video at its source — the local file, or the
  server-side item (Jellyfin `DELETE /Items/{id}`, Plex
  `/library/metadata/{id}/delete`) — and advances the tile to the next
  queue item. Click twice to confirm (the button arms red for 4 seconds).
- Overlay auto-hide: the UI (status bar, chips, hover queues) shows on any
  user input and hides after 5 seconds of inactivity; Tab pins/unpins it
  (`--overlay` starts pinned).
- Click a tile to pause/resume it, Space to pause/resume a screen, Esc to quit

Phase 0 benchmarking (see git history / docs when added) measured, on a
Ryzen 7 9800X3D + RTX 5080: 12x 4K30 H.264 streams at 1.000x realtime using
2.4% of total machine CPU with libmpv render API + NVDEC. The alternative
(vlcj/vmem callback path) could not hardware-decode on NVIDIA at all and
saturated the machine at 8x 4K — hence the Rust + libmpv stack decision.

## Requirements

- Rust (stable)
- libmpv 1.x (`libmpv.so.1`; on Debian/Ubuntu: `mpv` or `libmpv1`; dev headers
  are NOT required — the C ABI is declared in `tessera-mpv` and loaded at
  runtime). Set `TESSERA_MPV_LIB` to point at a specific library file.
- For hardware decode: NVIDIA (NVDEC), Mesa (VA-API), or macOS (VideoToolbox)
  — mpv negotiates via `--hwdec auto-safe`

## Build

```
cargo build --release
```

## Run

```
# 4 tiles on every monitor, fullscreen
opentessera --tiles 4 /path/to/videos/

# windowed, single screen
opentessera --windowed 1600x900 /path/to/file.mkv

# only monitor 2, advance through a directory instead of looping
opentessera --screens 2 --advance /path/to/videos/

# a dozen streams of the same source
opentessera --tiles 12 vid1.mkv vid1.mkv vid1.mkv vid1.mkv ...

# everything in the Plex "Movies" library
opentessera --plex --plex-library Movies

# local files plus two remote servers, one shared queue
opentessera --jellyfin --jellyfin-library Movies --plex /path/to/local/
```

- Remote sources: Plex and Jellyfin. `--plex` / `--jellyfin` add a server's
  movie libraries (optionally restricted with `--plex-library` /
  `--jellyfin-library`); items stream over HTTP via each server's
  direct-play endpoint (original file bytes, range requests — no
  transcoding), so mpv treats them exactly like local files, seeking
  included. Credentials live in `~/.config/opentessera/sources.conf`
  (`plex_url`/`plex_token`/`jellyfin_url`/`jellyfin_key`) with
  `TESSERA_*` env-var overrides. Local paths and remote sources can be
  freely mixed in one launch.

Controls: `Esc` exit fullscreen if zoomed, otherwise quit · `Tab`
pin/unpin the overlay UI (auto-hides after 5s idle) · `Space` pause/resume
all tiles on the focused screen · `F` fullscreen the tile under the cursor ·
click a tile to pause/resume it · hover a tile to show its queue strip
(click a thumbnail to play it there) · with `--audio`: right-click a tile
to unmute/mute it
(all tiles start muted — a wall of simultaneous streams should be
selectively audible, not a cacophony), `Up`/`Down` adjust volume of unmuted
tiles on the focused screen.

Seeks and file loads are issued asynchronously (mpv's async command API), and
quit tears down all mpv instances in parallel. Known limitation: a seek
still briefly hitches the other tiles on its screen — mpv's render API
blocks on the seeking core (~250ms), and tiles render sequentially on one
thread. Fixing it needs per-tile render threads; `TESSERA_PROFILE=1` logs
blocking render/property phases when investigating. Status (playback rate per tile, composited fps, hwdec mode,
decoder drops, budget utilization) prints every 5 seconds; adjust with
`--status-interval`.

Env vars: `TESSERA_VSYNC=1` enable vsync (off by default — an X11 swap stall
was observed with vsync on occluded windows), `TESSERA_MPV_LIB` path to libmpv,
`TESSERA_MPV_LOG=v` verbose mpv logs, `TESSERA_DUMP_DIR` screenshot dumps each
status interval (add `TESSERA_DUMP_OVERLAY=1` to include the overlay UI).

## Workspace layout

| Crate | Responsibility |
|---|---|
| `tessera-mpv` | libmpv render API wrapper (dlopen, hand-declared C ABI) |
| `tessera-core` | MediaItem/SourceKind model, PlaybackQueue, layout solver, decode budget |
| `tessera-app` | Binary: screens/windows, GL compositor, tiles, CLI |

## Roadmap

1. ~~Phase 1: skeleton — multi-monitor windows, mpv tiles, GL compositor~~
2. Phase 2: multi-tile UX — per-screen queue strips, transport controls,
   budget-driven eviction (pause oldest on overload), egui overlay
3. Phase 3: sources — local FS, Jellyfin, Plex, SMB (localhost range-bridge
   pattern from the Quest app)
4. Phase 4: library — SQLite persistence, watch state, resume points
5. Phase 5: curation UI tiles — browser/search/tags/collections as tiles amid
   the stream tiles

## License

GPL-3.0-or-later (see LICENSE). libmpv is GPLv2+.
