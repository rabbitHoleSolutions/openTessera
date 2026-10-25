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
cargo test --workspace         # 135 tests, mostly tessera-core (layout, budget, queue, sources, info, tracks)
cargo clippy --workspace --all-targets   # clean; keep it that way
cargo +1.99.0 clippy --workspace --all-targets -- -D warnings   # exactly what CI runs
./scripts/check-macos.sh       # type-check + lint the macOS build from Linux
tessera --version              # app version + which libmpv loads; needs no display
```

Releases: bump `version` in Cargo.toml, tag `vX.Y.Z`, push the tag. CI
(`release.yml`) builds the macOS DMG (`build-app.sh --dmg`, libmpv bundled)
and attaches it to a draft release to publish by hand. `rust-version`
(1.90) is held by CI's `msrv` job.

Running it needs a display, libmpv, and media. **The first choice for
checking a change is the `tessera-dev` MCP server** (`.mcp.json`; build
first, since it runs `target/release/tessera mcp --dev`). Its tools drive a
sandboxed instance with its own HOME, so the user's settings are safe:
`tessera_dev_launch` (files, flags, a settings.conf, `fresh`),
`tessera_dev_screenshot` (any window, overlay included, on demand),
`tessera_dev_state` (windows, tile rects, overlay flags, budget, queue),
`tessera_dev_input` (click/drag/scroll through the real event path — no
xdotool focus games), `tessera_dev_log`, `tessera_dev_quit` — plus the seven
player tools, aimed at the sandbox. It can't press keys; use `xdotool key`
for hotkeys under X11, `wtype` under Wayland (see "On COSMIC" below). See
`docs/MCP.md`. Its `files` must be paths the MCP server can see — it runs
outside the shell sandbox, whose `/tmp` is private, so put test media under
the gitignored `target/` (e.g. `target/test-media`), not the scratchpad.

Without it, launching **windowed** by hand is fine — never bare (it grabs
every monitor fullscreen):

```
TESSERA_DUMP_DIR=<scratch> TESSERA_DUMP_OVERLAY=1 \
  ./target/release/tessera --windowed 1400x900 --populate 2 --ui-scale 1 \
  --overlay --status-interval 3 &
```

That dumps a full-screen PNG (overlay included) every status interval, which
is readable directly. On an X11 session (Pop!_OS) with `xdotool`, the UI
can be driven: `xdotool search --classname tessera`, then
`xdotool mousemove --window <id> X Y` (window-relative, matches the PNG's
pixel coordinates 1:1 at `--ui-scale 1`). Caveats: bare `xdotool click` fires
press+release in one frame, which egui's drag-only widgets (sliders) ignore —
use `mousedown`/`sleep`/`mouseup`; and synthetic input can resolve a press
against a stale pointer position, so treat a weird one-off as suspect and
re-run before believing it. In practice a click only lands reliably after a
*real* motion event into the target: `mousemove` a few pixels away, pause,
`mousemove` onto the widget, pause, then click (a bare `click --window`
never reaches the button). And never `pkill -f` a pattern that also matches
your own shell command line — it kills the shell (exit 144); resolve PIDs
with `ps -eo pid,args | grep ... | grep -v grep` instead.

**Back up `~/.config/opentessera/settings.conf` first** — the app rewrites it
on any settings change, and clicking around in the gear panel can delete the
user's configured sources.

**On macOS** (Apple Silicon, Homebrew `rust` + `mpv`) the same windowed
launch and `TESSERA_DUMP_DIR` work unchanged; run with `HOME=<scratch>` to
keep the real config untouched. There is no xdotool, and `osascript` key
injection goes to whatever app is frontmost — don't aim Cmd chords at the
user's session. `./packaging/macos/build-app.sh` builds `target/Tessera.app`;
`open -n --env HOME=<scratch> --stdout <log> --stderr <log> -a
$PWD/target/Tessera.app <file>` exercises a real Finder-style launch (Apple
Event, launchd environment, fd limit 256), and a second `open -a ... <file>`
the running-instance path. `osascript -e 'tell application id
"io.github.rabbitholesolutions.opentessera" to quit'` is the Cmd+Q path.
`linux.rs` never compiles on a Mac — type-check it by stubbing its X11 import.
The reverse works without a Mac: `scripts/check-macos.sh` checks and lints
the whole workspace for `aarch64-apple-darwin` (it can't link or run).

**On COSMIC** (Wayland, Ubuntu 26.04, Intel iGPU) the app runs natively on
Wayland — winit picks it whenever `WAYLAND_DISPLAY` is set; force XWayland
with `env -u WAYLAND_DISPLAY` to tell a Wayland problem from a COSMIC one
(winit 0.29 ignores `WINIT_UNIX_BACKEND`).
`xdotool` can't reach a Wayland window. The MCP's input covers the mouse;
for keys, `wtype -k <key>` works (COSMIC offers the virtual-keyboard
protocol) but types into whatever has *keyboard focus*, and the user's
terminal takes focus back the moment they reply. So: ask the user to click
the Tessera title bar after a delay, send one probe key and check the
sandbox log (`/run/user/1000/tessera-dev/tessera.log`) shows it arrived
before sending more. COSMIC auto-tiles windows (per workspace), so a
window's size changes under you: re-read tile rects from
`tessera_dev_state` before aiming input.

Useful at runtime: `--windowed 1600x900` (one ordinary window),
`--screens 0` (single monitor), `--tiles N`, `--populate N`, `--overlay`
(start with the UI pinned), `--status-interval N`. Env: `TESSERA_MPV_LIB`,
`TESSERA_MPV_LOG=v`, `TESSERA_PROFILE=1`, `TESSERA_VSYNC=1`,
`TESSERA_DUMP_DIR`.

## Workspace

| Crate | Responsibility |
|---|---|
| `tessera-mpv` | libmpv render-API wrapper: `dlopen` + hand-declared C ABI (no dev headers). `MpvLib` / `Mpv` / `RenderContext`. |
| `tessera-core` | Pure logic, no I/O of its own beyond HTTP sources: `MediaItem`/`SourceKind`, `PlaybackQueue`, `solve_grid`, `DecodeBudget`, `title` (file name → show/season/episode/year), `subs` (SRT cues, search), `timecode` (`HH:MM:SS`, seek targets incl. chapters), `info` (the "i" panel's rows: Plex/Jellyfin detail parsers, size/bitrate/date formatting), `tracks` (the audio and subtitle tracks picked per item, and matching them back onto a file's tracks), `view` (per-tile crop/zoom), `monitor` (a monitor by number, connector or position). **This is where tests live** — put new logic here if it can be tested headlessly. |
| `tessera-app` | The binary. `main.rs` (startup + the event loop), `cli.rs` (arguments, usage text), `wall.rs` (`Wall` and every action on tiles/queue/budget, incl. `handle_overlay_action`), `windowing.rs` (window + GL context creation, fullscreen, which monitor a window is on), `seek.rs` (coalesced keyboard seeking), `persist.rs` (settings.conf, queue.json), `platform/` (everything OS-specific: `linux.rs`, `macos.rs`, same items in both), `screen.rs` (one window + GL context per `Screen`, wall or library), `tile.rs` (one mpv instance + FBO), `compositor.rs`, `overlay.rs` (wall egui), `browser/` (the browser window's egui: `mod.rs` tab bar + shared state, then one file per tab — `library.rs`, `queue.rs`, `sources.rs`, `config.rs`), `catalog.rs` (background source listing), `thumbs.rs`, `icons.rs` (painted button icons — transport, info, subtitles, play), `info_panel.rs` (the "i" panel, shared by tiles and browser rows), `png_util.rs`, `instance.rs` (the single-instance socket, which also carries the API), `rpc.rs` (the API: what each request does, on which thread), `mcp.rs` (`tessera mcp`: the stdio MCP server over that API). |

## Architecture invariants

These are the things that bite. Most bugs so far have been a violation of one
of them.

**Nothing outside `platform/` names an OS.** Fullscreen goes through
`platform::{enter,exit,is}_fullscreen` — never `window.set_fullscreen` /
`window.fullscreen()` directly, because macOS walls are *simple* fullscreen,
which `fullscreen()` doesn't report. A new difference becomes a named item in
both `linux.rs` and `macos.rs` (identical signatures); `linux.rs` holds the
behaviour the app was tuned on, so a change there is a Linux change. The only
`cfg` elsewhere is libmpv's candidate list in `tessera-mpv`.

**Frame pacing is anchored to the frame's start.** `next_frame_at` is set
when a frame begins and every callback re-arms `WaitUntil(next_frame_at)`.
Re-arming `now + 16ms` per callback (as it once did) makes the period the
frame's work *plus* 16ms, because the loop's last callback comes after the
work.

**Under Wayland a window draws when the compositor asks, not on the
timer.** The timer never lines up with the output's refresh (62.5 frames
a second at 59.97Hz judders film), so a Wayland `Screen` is
`compositor_paced`: it calls `pre_present_notify` before its swap, and
winit holds its `RedrawRequested` until the frame callback — which sets
`frame_due` and wakes the loop to draw at once. The per-screen loop skips
a window that isn't due, except after 50ms (`wants_frame`): a window on
another workspace gets no callbacks, and its tiles must still render.
X11 and macOS stay timer-paced. To compare, `env -u WAYLAND_DISPLAY`
forces X11.

**One `Screen` per window, created and destroyed at runtime.** Windows are
opened from the status bar's "+" and closed individually (`CloseRequested`
only quits when it's the last one). So a screen index is a *temporary*
handle: never store one across frames. `WindowId` is the durable identity —
`pending_click`, `TileDrag` and `cursor_over` all key on it, and
`screens.iter().position(|s| s.window.id() == id)` is the lookup. Anything
that used to be a `Vec` indexed in parallel with `screens` belongs on
`Screen` instead (that is why `Screen::cursor` exists).

**A window is a `Screen` with a `ScreenRole`.** `Wall` has tiles and video;
`Browser` has neither but keeps its GL context, egui context and its place
in every per-screen path (tick, render, W, close, drag routing). Adding a
window kind means a role, not a parallel window type — but check the role
wherever an action assumes tiles (`add_tile_action` refuses on a browser
window).

**The decode budget is denominated in weighted macroblocks/sec, not
megabits.** `MACROBLOCKS_1080P30` is 244,800; the default ceiling is 96 of
those. The accessors say `load` for that raw unit and `*_equivalents()` for
the 1080p30-count that the UI shows — the old name was `used_mbps`, and the
status bar duly printed "23500800 mbps". Never put a raw load figure in
front of a person.

**Global settings live in the browser window; per-window settings live in
the gear.** This is the rule that keeps the shared-`UiSettings` hazard below
from recurring: a control drawn in the one window that exists once cannot
have several egui contexts fighting over it. `UiSettings` is now *only*
persisted global state, so its `PartialEq` is derived — if you add a field
that must not trigger a save, it does not belong there (per-window state
goes on `Screen`, transient form state on `BrowserState`). L/Q/S/C and the
browser buttons all funnel through `OverlayAction::NewLibraryWindow`, which
focuses the existing browser window rather than opening a second; don't
reintroduce a second path that just creates one.

**Hotkeys ignore auto-repeat unless they're continuous.** Seek and volume
arrows repeat; every toggle checks `!key_event.repeat`. A held key otherwise
storms the wall, and anything that moves focus mid-press (L/Q focusing the
browser) can swallow the key release, after which X repeats forever. That
server-side stuck-key state survives app restarts — if hotkeys start firing
on their own during testing, `xdotool keyup <key>` clears it.

**Auto-repeating keys must coalesce whatever they drive.** X repeats at
roughly 25/sec, so a held arrow turned into 25 mpv seeks a second — each one
a demuxer flush, and a fresh HTTP range request for remote sources. Seeks
accumulate in `OverlayState::pending_seek` and go out when the key settles
(200ms), on release, or every 600ms of holding, whichever comes first; the
transport draws `time_pos + pending_seek` so the readout still moves at key
rate. `seek_step` holds the timing rules on their own so they can be tested
headlessly. Anything else expensive that ends up on a repeating key needs
the same treatment.

**Chrome scales to what contains it — always by `scale_style`, never by a
layer transform.** Tile chips scale to their cell (`chip_scale`); the status
bar and the panels scale to their window (`window_scale`). Both then call
`scale_style`, which multiplies the Ui's fonts and spacing.

egui's `set_transform_layer` looks like the better tool — it scales
hit-testing for free — and it is a trap for anything that can reach the edge
of its window. `Area::content_ui` intersects both its max_rect and its
clip_rect with `screen_rect` *before* the transform runs, and `Layers::drain`
then transforms the clip rect along with the shapes. A full-width status bar
laid out at `width / k` is clipped back to the window and *then* shrunk; a
chip on the rightmost column loses everything past the window edge. Both were
built, shipped and had to be undone. Style scaling has no such limit; the
cost is that explicit pixel sizes (painter-drawn icons, `min_size`,
`add_sized`) are not style and take the factor at their call sites.

**A chip never lays out wider than its cell, and the transport row's fixed
width must count *both* ends.** `chip_width` bounds the first; `ROW_FIXED`
the second. `ROW_FIXED` used to be `TRAILING` and counted only the controls
after the scrub bar, so the row was always ~110pt wider than the budget it
was handed — invisible on a big tile, and on the rightmost column it ran off
the window and lost the trash button.

**Scale floors belong in physical pixels, not in the scale factor.** At a
target scale of 3 a `k` of 0.3 still leaves a 14px button; a flat 0.55 floor
stopped the chip shrinking long before it fit its cell. See
`CHIP_MIN_CONTROL_PX`.

**egui Sliders ignore the width you give them.** The rail is drawn at
`ui.spacing().slider_width` (100 by default); `add_sized` allocates the room
but the slider does not fill it. Set `spacing_mut().slider_width` instead.

**`ui.with_layout` takes every pixel that is left.** In an `Area`, that is
the rest of the screen — so a right-aligned close button at the end of a
short panel stretches the panel's frame to the full window height. Content
used to hide it; the gear panel got short and it showed. Use
`allocate_ui_with_layout` with an explicit height for trailing rows.

**Blocking work never runs on the render thread.** `Source::list()` is an
HTTP round trip or an FS walk; `catalog.rs` puts it on a worker and polls
the results each frame, the same pattern `thumbs.rs` uses for posters. If
you reach for anything that can block inside an egui pass, it belongs in a
worker instead.

**egui_glow needs the `clipboard` feature, and it is not cosmetic.**
`egui_glow`'s default feature set is empty and it pulls `egui-winit` with
`default-features = false`, so without asking for `clipboard` there is no
`arboard` in the build at all — and egui-winit silently substitutes an
*in-app* clipboard. Copy in a browser, paste in Tessera, and you get
whatever was last copied inside Tessera instead. It looks like a paste bug
in the widget; it is a missing Cargo feature. `links` is the same story for
`Hyperlink`: without it a clicked link only logs a warning.

**Playback options key off the item, not the app.** `TileOptions` is
global, but what mpv is actually told is derived per item, because a live
camera and a film want different players: a 512 MiB demuxer cache on a
stream nobody can seek is latency, not resilience. `SourceKind::is_live()`
is the switch — declared on the kind, never guessed from a URI scheme — and
it also governs looping (meaningless with no EOF) and delete (nothing to
delete). Note a live stream reports `0x0` at file-load, so the budget must
NOT be updated from it: the placeholder stands until `ensure_texture`
reports the first real frame size, or a wall of cameras admits itself for
free.

**One process owns the app; a second launch hands off and exits.** The
queue and the decode budget are per-*process*, so a second instance is not
a second window — it is a second budget admitting a second ceiling's worth
of streams against one GPU, and a second writer of settings.conf/queue.json
that stamps its own stale copy over the file (this ate a configured source
in the wild). `instance.rs` claims a Unix socket, keyed on the **config
directory** so an isolated HOME is an isolated instance — which is how it
is tested, and how a sandboxed run coexists with the real app. Connect
-first, never a lock file: a lock can be inherited from a process that
died, whereas a dead instance refuses connections. Handed-off paths arrive
on a channel drained in the event loop (blocking work stays off the render
thread, as ever). `TESSERA_ALLOW_MULTI=1` opts out, for development only.

**The queue cursor is the item most recently started; `None` means nothing
has.** Then item 0 is next and the whole queue is pending. Take the next
item with `upcoming(1)` and `advance()` only once a tile really plays it
(`spawn_tile` doesn't advance — its callers do, since an item played from
outside the queue must not consume a place). Reading a fresh queue as
"already on item 0" made the first "+" play item 1.

**The API (`rpc.rs`) answers on the event loop; slow work stays on the
connection thread.** A socket request is parsed on its own connection
thread. Anything touching tiles, the queue or GL becomes a `Call` drained
once a frame (next to the hand-offs) and answered there — through
`handle_overlay_action` where an action exists, so the decode budget sees
an API pause exactly as it sees a click. Listing folders, encoding JPEGs and
pacing synthetic input happen on the connection thread before or after;
ffmpeg runs in the `tessera mcp` shim, never in the app. A GL readback is a
`Capture` queued on the `Screen` and served inside its `tick`, the only
point where the right context and framebuffer are current.

**What the API returns is read by a language model.** Errors say what to
do next ("several tiles are playing — say which with `tile`: 0 (...), 1
(...)"). Paths are redacted to before `?` — a Plex/Jellyfin direct-play URI
*is* its token; `subtitle_info` carries the real URI for the shim's ffmpeg
and the shim never prints it. Opening is limited to folder sources and
`~/Videos` (canonicalised, so `..` and symlinks can't escape), and no tool
deletes anything. Synthetic input is honoured only by an instance started
with `TESSERA_DEV_INPUT=1`.

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

**Under pop-shell, the WM owns where a windowed window sits.** It snaps a
tiled window back to its slot ~0.3s after the app moves it, and restores
the slot when fullscreen ends (so a fullscreen "hop" doesn't move it
either). Placement at creation sticks, and so does fullscreen on a monitor
(re-asserted by `fix_displaced_walls`). Report where a window *is*
(`monitor_under_window`), never where it was asked to go. Floating (T) is
the one way out of the tile: `platform::set_floating` withdraws the window
and maps it back as a DIALOG, which pop-shell leaves alone. The window is
away from the WM for a moment, so anything else asked of the WM right
after (fullscreen, focus) has to wait for it — see ROADMAP §24. COSMIC's
auto-tiling behaves the same way (a window is resized into a slot, and
gets its slot back when fullscreen ends), but T can't free a window there:
under Wayland it only drops the title bar (§30).

**Under Wayland a window doesn't know where it is, and winit lists each
monitor twice.** `outer_position()` always fails there, so anything saved
"to put back later" must not hinge on it — `Restore` keeps the size and an
*optional* position; gating the whole save on the position once left an F
zoom stuck fullscreen. And winit 0.29's `Window::available_monitors()`
returns every Wayland output twice; go through
`windowing::available_monitors`, which dedups, never the winit method on a
window (the event loop's list is fine).

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

**`--hwdec nvdec-copy,vaapi-copy` is the deliberate default.** Zero-copy
CUDA/GL interop segfaults on NVIDIA when decoders re-init mid-playback.
Don't "optimize" it back. NVDEC stays first so an NVIDIA box keeps the path
the app was tuned on; VA-API (Intel, AMD) is the fallback — alone,
`nvdec-copy` dropped them to software decoding silently. mpv < 0.36 can't
take a list; `tessera_mpv::hwdec_for_api` narrows it to the first entry.

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
  doubt. Prefer painter-drawn icons for anything critical (the gear, the
  window-mode icon and everything in `icons.rs` already are).
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
  `platform/linux.rs` sets WM_CLASS explicitly to `APP_ID` — the desktop
  entry's `StartupWMClass` must match it, or the dock shows a second generic
  entry instead of lighting up ours. On macOS the identity is the bundle ID
  in `packaging/macos/Info.plist`.
- Positional paths on the command line REPLACE the saved sources rather than
  merging with them: opening a video from the file manager has to play that
  video. `--plex`/`--jellyfin` still merge, being an explicit ask. A Finder
  open that *launches* the app on macOS means the same thing (hence
  `DEFER_STARTUP_TILES`); one that arrives while running plays now and queues.
