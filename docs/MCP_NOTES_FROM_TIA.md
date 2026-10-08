# MCP interface: notes from the Tia side

Tia (the voice assistant in `~/Development/Tia`) will be the first MCP client of tessera. These are
notes on what makes the interface easy for an agent to use, based on how Tia calls tools: Claude via
the Agent SDK, with her own tool servers approved automatically.

## Connection

- Expose MCP from the **running instance**. The easiest route for clients is a stdio shim,
  `tessera mcp`, that talks to the running app over the existing single-instance channel (keyed on
  the config directory). On the client side that's one line:
  `{ command: 'tessera', args: ['mcp'] }`. No network port and no token to manage.
- If tessera isn't running, the shim should either start it (`tessera_open` would do that
  naturally) or return a clear "tessera isn't running" error. It should never hang.
- Keep calls under about a second. Frame grabs can take longer, but put a timeout on them.

## Tool set

- About **8–12 coarse tools**, not dozens, with names prefixed `tessera_`.
- Each description should say **when to use the tool**, give **units** and include an **example**.
  The model chooses tools purely from those descriptions.
- Suggested set:
  - `tessera_status`: what's playing, per tile.
  - `tessera_open`: open files or folders.
  - `tessera_control`: play, pause, seek, volume, mute.
  - `tessera_subtitles`: read and search subtitles.
  - `tessera_frame`: capture a frame.
  - `tessera_queue`: list, add, next.
  - `tessera_close`: close a tile.

## Addressing tiles

- Give every tile a **stable id**. Status should return, for each tile:
  - id, path, parsed title (show, season, episode, year), position, duration and chapter
  - paused, muted, volume
  - screen, and whether it's focused or pinned
- Make the tile **optional** in every call. Default to the focused tile, or the only tile, so
  "pause it" works without a lookup first.

## Time

- Accept seconds, `HH:MM:SS`, relative values (`+30`, `-10`) and chapter names.
- Every control call should **return the resulting state** (new position, volume), so the agent
  can confirm without calling status again.

## Subtitles

- Embedded or external tracks.
- A **"where am I"** call: the cues from the last minute or two, plus the next few seconds, with
  timestamps.
- **Full-text search** across the whole track that returns timestamps, so "skip to where they say
  *rosebud*" is one search plus one seek.
- Track listing and selection.
- Return compact text like `[01:12:03] line`, not big JSON. Every token gets re-read, and Tia
  speaks the replies.

## Frames

- Return images as **MCP image content**: a downscaled JPEG, about 768px on the long side. A
  vision model can then answer "what's on screen" or "who is that".
- Optionally take a timestamp, and optionally save to a file.

## Audio

- **Start unmuted** when a single video is opened, or offer an explicit `unmuted` option on
  `tessera_open`. Today `--audio` enables audio but every tile starts muted (`tile.rs`, around line
  191).
- Set mpv's `audio-client-name` to **`Tessera`**. Tia currently sees the audio streams as
  `application.name: mpv`. A clear name lets Tia recognise, duck and visualise tessera's audio, as
  she already does for Spotify.
- Expose **per-tile volume** through MCP. That's more reliable than Tia adjusting system stream
  volumes.

## Events (optional, valuable later)

- A small local event stream for **playback ended, now playing, paused and tile closed**. Tia
  could then show tessera in her now-playing bar, react to "tell me when this episode ends", or
  follow it with the lights' music mode.
- MCP clients mostly work request/response, so keep `tessera_status` cheap enough to poll as well.

## Safety

- Mark tools with MCP annotations (`readOnlyHint`, `destructiveHint`). Tia approves her own servers
  automatically, so keep everything non-destructive.
- No deleting or moving files. Opening should be limited to the media folders and URLs.
- Errors should say what to do next, e.g. "No tile is playing; open something with
  tessera_open first."

## Testing

- Make sure it works in the MCP Inspector:
  `npx @modelcontextprotocol/inspector tessera mcp`. Once it does, wiring it into Tia takes a few
  lines.

## On the Tia side, once this exists

- Add tessera to Tia's tool servers (one entry in `src/brain.ts`'s MCP map).
- `video_open` goes through `tessera_open` instead of launching the app.
- Teach Tia (in `src/persona.ts`) when to use subtitles and frames.
