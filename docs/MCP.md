# MCP interface

`tessera mcp` is an MCP server on stdio for the running app. It was built to
the brief in `MCP_NOTES_FROM_TIA.md` (Tia, the voice assistant, is its first
client); this page is the reference, and records which parts of that brief
are done.

## Connecting

```jsonc
// any MCP client's server map
"tessera": { "command": "tessera", "args": ["mcp"] }
```

`tessera` must be on the client's PATH (`./packaging/install.sh` puts it in
`~/.local/bin`). There is no port and no token: the shim talks to the running
app over the single-instance socket, which is keyed on the config directory,
so it reaches whichever tessera owns `~/.config/opentessera` for that user.
If tessera isn't running, `tessera_open` starts it on the files asked for
(the same as opening them from a file manager); every other tool says so and
returns immediately. Nothing waits on a dead socket.

Check it with the MCP Inspector:
`npx @modelcontextprotocol/inspector tessera mcp` (or `--cli ... --method tools/list`).

## Tools

All take an optional `tile`; leaving it out means the **focused tile**: the
only tile, else the one filling a window (F), else the only one with sound,
else the one under the pointer. When that is still ambiguous the error lists
the tiles to choose from. Times are `HH:MM:SS` in every reply and accepted
back in every argument.

| tool | does | read-only |
|---|---|---|
| `tessera_status` | per tile: parsed title (show/season/episode or film/year), position, duration, chapter, paused/muted/volume, file, focused/pinned/fullscreen; queue summary | yes |
| `tessera_open` | play files, folders or URLs; first plays (focused tile, `tile`, or `new_tile`), rest queue; `unmuted`; starts tessera if needed | no |
| `tessera_control` | play, pause, toggle, seek, volume, mute, unmute, next, previous, pin, fullscreen — returns the new state | no |
| `tessera_subtitles` | `now` (last 60s + next 10s, `-- now --` marked), `search` (timestamps to seek to), `tracks`, `select` | yes |
| `tessera_frame` | the tile's current frame, or one `at` another time, as a ~768px JPEG; `save` writes to `~/Pictures/Tessera` | no |
| `tessera_queue` | `list`, `add` (same path rules as open), `next`, `previous` | no |
| `tessera_close` | remove a tile (the file is untouched) | no |

Seek takes seconds (`90`), `HH:MM:SS`, relative (`+30`, `-10`), `next
chapter` / `previous chapter`, `chapter 3`, or part of a chapter title.
Volume takes 0–130 or `+10`/`-10`.

Replies are short sentences and `[HH:MM:SS] line` lists, not JSON — Tia
speaks them.

## Safety

- Every tool is `destructiveHint: false`. None deletes or moves a file; the
  delete in tessera's own UI is not exposed.
- Local paths must resolve (after symlinks and `..`) inside a configured
  folder source or `~/Videos`; anything else is refused with a message that
  says how to allow it. URLs are limited to http(s) and streaming schemes.
- Status redacts query strings from paths: a Plex/Jellyfin direct-play URL
  carries its access token there.
- Frames are saved with `create_new`, so a grab never overwrites anything.

## How it is built

- `mcp.rs` — the stdio server: JSON-RPC, tool schemas, reply formatting, and
  the slow work the app must not do: ffmpeg subtitle extraction (cached in
  `~/.cache/opentessera/subs`) and frames at another time.
- `instance.rs` — the socket. A line with `{"rpc": ...}` is an API call; one
  thread per connection, so a frame grab never blocks a hand-off.
- `rpc.rs` — the API. Parsed on the connection thread; anything that touches
  tiles, the queue or GL becomes a `Call` answered by the event loop on its
  next frame (~16ms). Listing folders and encoding JPEGs stay on the
  connection thread. Controls dispatch through `handle_overlay_action`, so
  they go through the decode budget exactly as the UI's buttons do.
- Subtitles: text tracks only (SRT, ASS, WebVTT, mov_text — ffmpeg converts
  them all to SRT); image tracks (PGS, VobSub) are reported as unreadable.
  Indexing an embedded track reads the file once (~1s for 2.6GB locally).
- mpv's `audio-client-name` is `Tessera`, so the sound server (and Tia)
  sees the streams by name. A single opened video starts with sound; tiles
  added beside it start muted.

## Not yet

- **Events** (playback ended, now playing, paused, tile closed). Status is
  cheap enough to poll meanwhile (~10ms). The natural shape is a second
  socket request that streams lines, which the shim turns into MCP
  notifications.
- **Subtitles inside streamed items** (Plex/Jellyfin): extracting them would
  download the whole file. Both servers expose subtitle streams of their
  own; using those is the way. The line currently on screen is still
  reported.

## Development mode

`tessera mcp --dev [--home DIR]` adds tools for developing tessera itself,
and aims *every* tool at a sandboxed instance (its own HOME under
`$XDG_RUNTIME_DIR/tessera-dev` by default) so the user's settings are never
touched. The repo's `.mcp.json` registers it for Claude Code sessions here.

| tool | does |
|---|---|
| `tessera_dev_launch` | start the sandbox windowed (`--ui-scale 1 --overlay`), with `files`, extra `args`, a `settings` file, `fresh` HOME; `~/Videos` is linked in |
| `tessera_dev_quit` | quit as Esc does; returns the log tail |
| `tessera_dev_log` | the sandbox's stdout/stderr, optionally filtered |
| `tessera_dev_screenshot` | a PNG of one window, overlay included, on demand |
| `tessera_dev_state` | windows, tile rects, overlay flags, budget, queue cursor, settings as JSON |
| `tessera_dev_input` | synthetic `move`/`click`/`right_click`/`down`/`up`/`drag`/`scroll`/`wait` steps |

Synthetic input goes through an `EventLoopProxy` user event that becomes an
ordinary `WindowEvent` at the top of the event loop, so it takes the real
path — egui, the click router, tile drags — paced a frame apart. Only an
instance started with `TESSERA_DEV_INPUT=1` (which `tessera_dev_launch`
sets) accepts it. Keys aren't synthesised (winit's `KeyEvent` can't be
constructed outside winit); `xdotool key` still works for those.
