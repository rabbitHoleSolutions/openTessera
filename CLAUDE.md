# CLAUDE.md — working notes for openTessera

Desktop multi-stream video wall (Rust + libmpv + glutin/glow + egui). The
Meta Quest app **Tessera** (`../tessera`) is the sister project: same UX
ideas, no shared code. See `README.md` for the user-facing feature list and
`docs/ROADMAP.md` for feature tracking.

## Commands

```
cargo build --release          # binary: target/release/tessera
./packaging/install.sh         # user-local install (bin + icon + .desktop)
cargo check --workspace        # fast loop while editing
cargo test --workspace         # 43 tests, mostly tessera-core (layout, budget, queue, sources)
cargo clippy --workspace       # 7 pre-existing warnings; don't let the count grow
```

Running it needs a display, libmpv, and media. Launching it **windowed** for
verification is fine and useful — never bare (it grabs every monitor
fullscreen):

```
TESSERA_DUMP_DIR=<scratch> TESSERA_DUMP_OVERLAY=1 \
  ./target/release/tessera --windowed 1400x900 --populate 2 --ui-scale 1 \
  --overlay --status-interval 3 &
```

That dumps a full-screen PNG (overlay included) every status interval, which
is readable directly. The session is X11 with `xdotool` available, so the UI
can be driven: `xdotool search --classname tessera`, then
`xdotool mousemove --window <id> X Y` (window-relative, matches the PNG's
pixel coordinates 1:1 at `--ui-scale 1`). Caveats: bare `xdotool click` fires
press+release in one frame, which egui's drag-only widgets (sliders) ignore —
use `mousedown`/`sleep`/`mouseup`; and synthetic input can resolve a press
against a stale pointer position, so treat a weird one-off as suspect and
re-run before believing it.

**Back up `~/.config/opentessera/settings.conf` first** — the app rewrites it
on any settings change, and clicking around in the gear panel can delete the
user's configured sources.

Useful at runtime: `--windowed 1600x900` (one ordinary window),
`--screens 0` (single monitor), `--tiles N`, `--populate N`, `--overlay`
(start with the UI pinned), `--status-interval N`. Env: `TESSERA_MPV_LIB`,
`TESSERA_MPV_LOG=v`, `TESSERA_PROFILE=1`, `TESSERA_VSYNC=1`,
`TESSERA_DUMP_DIR`.

## Workspace

| Crate | Responsibility |
|---|---|
| `tessera-mpv` | libmpv render-API wrapper: `dlopen` + hand-declared C ABI (no dev headers). `MpvLib` / `Mpv` / `RenderContext`. |
| `tessera-core` | Pure logic, no I/O of its own beyond HTTP sources: `MediaItem`/`SourceKind`, `PlaybackQueue`, `solve_grid`, `DecodeBudget`. **This is where tests live** — put new logic here if it can be tested headlessly. |
| `tessera-app` | The binary. `main.rs` (CLI + event loop + all the cross-screen actions), `screen.rs` (one window + GL context per `Screen`, wall or library), `tile.rs` (one mpv instance + FBO), `compositor.rs`, `overlay.rs` (wall egui), `library.rs` (browser egui), `catalog.rs` (background source listing), `thumbs.rs`, `png_util.rs`. |

## Architecture invariants

These are the things that bite. Most bugs so far have been a violation of one
of them.

**One `Screen` per window, created and destroyed at runtime.** Windows are
opened from the status bar's "+" and closed individually (`CloseRequested`
only quits when it's the last one). So a screen index is a *temporary*
handle: never store one across frames. `WindowId` is the durable identity —
`pending_click`, `TileDrag` and `cursor_over` all key on it, and
`screens.iter().position(|s| s.window.id() == id)` is the lookup. Anything
that used to be a `Vec` indexed in parallel with `screens` belongs on
`Screen` instead (that is why `Screen::cursor` exists).

**A window is a `Screen` with a `ScreenRole`.** `Wall` has tiles and video;
`Library` has neither but keeps its GL context, egui context and its place
in every per-screen path (tick, render, W, close, drag routing). Adding a
window kind means a role, not a parallel window type — but check the role
wherever an action assumes tiles (`add_tile_action` refuses on a library
window).

**Hotkeys ignore auto-repeat unless they're continuous.** Seek and volume
arrows repeat; every toggle checks `!key_event.repeat`. A held key otherwise
storms the wall, and anything that moves focus mid-press (L/Q focusing the
browser) can swallow the key release, after which X repeats forever. That
server-side stuck-key state survives app restarts — if hotkeys start firing
on their own during testing, `xdotool keyup <key>` clears it.

**egui Sliders ignore the width you give them.** The rail is drawn at
`ui.spacing().slider_width` (100 by default); `add_sized` allocates the room
but the slider does not fill it. Set `spacing_mut().slider_width` instead.

**Blocking work never runs on the render thread.** `Source::list()` is an
HTTP round trip or an FS walk; `catalog.rs` puts it on a worker and polls
the results each frame, the same pattern `thumbs.rs` uses for posters. If
you reach for anything that can block inside an egui pass, it belongs in a
worker instead.

**One `PlaybackQueue`, shared by every window** — owned by `main` and passed
`&mut` alongside `budget`. A tile is a slot fed from it; windows opened at
runtime just pull from the same queue. (Mirrors the Quest app's
`DockGridManager`, which owns exactly one queue for both its modes.) A
per-window/global toggle is planned, so keep queue access going through the
single owner rather than reintroducing per-screen queues.

**mpv render contexts must be freed on the thread whose GL context is
current.** `mpv_render_context_free` frees GL objects; calling it from a
worker thread (where no context is current) segfaults inside mpv. Teardown
is therefore two-phase: `Tile::split_for_teardown` drops the `RenderContext`
on the main thread right after the owning screen's `make_current`, and hands
back the bare `Mpv` core, which is slow to terminate but touches no GL and
can go on any thread (`terminate_parallel`). Both `close_screen` and
`shutdown` must keep that order — and `shutdown` must do it *per screen*,
since each `destroy()` makes its own context current.

**Each `Screen` has its own GL context and its own egui context — but a
single shared `OverlayState` + `UiSettings`.** `main.rs` loops
`for screen_idx in 0..screens.len() { screens[i].tick(..., &mut overlay, &mut settings) }`,
so *every screen's egui pass writes the same state, and the last one wins*.
Anything a widget writes back must therefore be scoped to the window that owns
the interaction — see `OverlayState::help_open` / `settings_open`
(`Option<WindowId>`, drawn on one screen only) and `UiSettings::scale_drag`
(window-tagged draft). A plain `bool`/`Option<T>` written from a per-screen
widget is a multi-monitor bug waiting to happen.

**Wall actions take a `Wall<'a>`, not a parameter list.** It bundles the
state nearly every action touches — `screens`, `budget`, the shared `queue`,
`overlay`, `mpv_lib`, `tile_opts`, `next_tile_id`, `max_tiles`,
`wm_settle_at` — as borrows, built for as long as the actions run and then
dropped so the rest of the frame can use the same state. Add a field here
rather than growing a signature. Note `NewWindow` is intercepted by the
event loop before `handle_overlay_action`: creating a window needs the
`EventLoopWindowTarget` and the shared GL config, and must happen outside
the loop iterating `screens` by index.

**Widgets don't mutate the world; they push `OverlayAction`s.** Pause/resume
must go through the global `DecodeBudget`, which the drawing code can't borrow.
`main.rs` drains `overlay.actions` after each screen's `tick` and runs them
through `handle_overlay_action`, which gets `screen_idx` (hence the acting
window). Add a variant rather than reaching for a `&mut Screen` in `overlay.rs`.

**egui panels use `Order::Foreground`, never `Order::Tooltip`.** egui does not
hit-test tooltip-order layers, which silently makes every control in the panel
inert. There are comments to this effect at both panel sites; keep them.

**Nothing interactive may live in egui's Background layer.** The click
router (`pos_on_ui` / `click_on_ui`) treats Background as "not UI", because
egui inserts a full-screen background layer every pass whether we paint one
or not. `TopBottomPanel`/`SidePanel`/`CentralPanel` all paint into that
layer — so a button in one fires its action *and* lets the click fall
through to the video wall behind it. The status bar is an `Area` at
`WALL_ORDER` for exactly this reason; don't "simplify" it back to a panel.

**The status bar lays its controls out first (right-aligned), then the text.**
A single, WM-tiled window is the default now, so the bar is often narrow;
the other nesting order pushes the buttons off-screen instead of clipping
the status text.

**`ctx.set_pixels_per_point()` takes effect on the *next* frame** (it routes
through `set_zoom_factor`). Any UI-scale logic has to account for that one-frame
lag — which is part of why the scale slider commits on release rather than live.

**Clicks are resolved a frame late, on purpose.** egui's pointer position lags
one frame, so a raw click can't be classified at event time. Left presses park
in `OverlayState::pending_click`; the frame-fresh verdict comes from
`ctx.layer_id_at()` in `Screen::tick` (the implicit full-screen `Background`
layer does *not* count as UI). Release → plain click (pause/resume);
past `DRAG_SLOP_PX` → tile drag.

**App hotkeys must yield to egui when it wants the keyboard**
(`egui_ctx.wants_keyboard_input()`), or typing a `/` in a folder path opens the
help panel over the form.

**The decode budget is global across screens.** Any resume/add/move that starts
a stream re-requests admission and may evict (`OLDEST_STARTED`). Paused tiles
hold no slot. Helpers: `request_admission`, `resume_tile`, `pause_and_release`.

**Tiles are bound to their screen's GL context.** Cross-screen moves tear down
and rebuild the mpv instance from a `ResumeState` snapshot (item, time-pos,
paused, volume, mute, aid/sid). `context.make_current()` before touching GL for
a screen.

**`--hwdec nvdec-copy` is the deliberate default.** Zero-copy CUDA/GL interop
segfaults on NVIDIA when decoders re-init mid-playback. Don't "optimize" it back.

## Conventions

- **The repo is not rustfmt-formatted** (194 files-worth of diff). Match the
  surrounding style by hand; do **not** run `cargo fmt` across the tree.
- Comments explain *why*, especially the non-obvious platform/egui/mpv
  workarounds — the codebase leans on them heavily. Match that density.
- UI strings are lowercase ("settings", "max concurrent streams").
- **Check any new icon glyph against egui's bundled fonts before using it.**
  The Proportional family falls back Ubuntu-Light → NotoEmoji-Regular →
  emoji-icon-font *only* (Hack is monospace-only, so `⊗ ⨯ ⊘` are tofu in
  normal text). A missing glyph renders as an empty box with no warning —
  `✕` U+2715 did exactly that; `✖` U+2716 is the one that exists. Fonts live
  in `~/.cargo/registry/src/*/epaint-<ver>/fonts/`; parse their cmaps if in
  doubt. Prefer painter-drawn icons for anything critical (the gear and the
  window-mode icon already are).
- Two egui layer orders, both in `overlay.rs`: `WALL_ORDER` (status bar,
  chips, queue strips) below `PANEL_ORDER` (help, settings). They must stay
  distinct — egui raises the most recently interacted area to the top within
  an order, so same-order dialogs get painted over by whatever you click.
- Runtime feedback: `println!` for state changes the user might want in the log
  (`settings: ...`, `screen N tile M: ...`); `notify(&mut overlay, msg)` for a
  transient on-screen toast when a user action fails (budget full, tiles cap).
- Settings persist to `~/.config/opentessera/settings.conf` via `save_settings`,
  triggered by a `settings != settings_before` diff each frame. Transient
  widget state on `UiSettings` must be excluded from its hand-written
  `PartialEq`.
- Update `docs/ROADMAP.md` when a tracked feature lands (status + what shipped).
- The binary is `tessera`; the project and the config dir stay `openTessera`
  / `~/.config/opentessera` (renaming the dir would orphan real credentials).
  `main.rs` sets WM_CLASS explicitly to `APP_ID` — the desktop entry's
  `StartupWMClass` must match it, or the dock shows a second generic entry
  instead of lighting up ours.
- Positional paths on the command line REPLACE the saved sources rather than
  merging with them: opening a video from the file manager has to play that
  video. `--plex`/`--jellyfin` still merge, being an explicit ask.
