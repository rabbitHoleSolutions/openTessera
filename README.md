<p align="center"><img src="packaging/icons/tessera-128.png" alt="" width="96"></p>

# openTessera

A desktop video wall: many videos playing at once, tiled across one window or
every monitor you have. Local files, Plex, Jellyfin and live security cameras
all go into one shared queue, and every tile is a slot fed from it.

![Four films playing at once in one window, with the controls of the hovered tile showing](docs/screenshot.jpg)

<sub>The films are fakes: single AI-generated frames, made for testing video players.</sub>

openTessera is the desktop sibling of **Tessera**, a Meta Quest app built
around the same ideas. The two share a design, not code. It runs on Linux and
macOS, is written in Rust on top of libmpv, and is licensed GPL-3.0-or-later.

**Built with AI.** Most of the code and documentation were written by
Claude (Anthropic's model) through Claude Code, with a person directing the
design, testing on real hardware, and deciding what ships. Many commits
carry a `Co-Authored-By: Claude` line; one without it wasn't necessarily
written by hand, since the early history predates the habit.
[CLAUDE.md](CLAUDE.md) is the working notes it was given, and
[docs/ROADMAP.md](docs/ROADMAP.md) records why things were built the way
they are.

> **Status: a personal project, shared as-is.** It is used every day on one
> Linux machine and has had a first pass on one Mac. Anything outside that
> setup is untested — see [what has been tested](#what-has-been-tested). Bug
> reports from other setups are the most useful contribution there is.

## Features

- **Many streams, cheaply.** Every tile is its own libmpv player rendering
  straight into a GL texture, with hardware decoding (NVDEC, VA-API or
  VideoToolbox). On an RTX 5080, twelve 4K30 H.264 streams play in real time
  using about 2.4% of the CPU.
- **A decode budget.** New streams are admitted against a GPU decode ceiling,
  measured in 1080p30-equivalents and weighted by codec. When the wall is
  full, the oldest stream is paused to make room. The ceiling also shrinks
  on its own if decoders start dropping frames.
- **Windows and walls.** It starts as one ordinary window. You can open more
  at any time. `W` turns any window into a fullscreen wall on whatever
  monitor it is on, and `--max` fills every monitor at once. `T` floats
  a window above everything else without a title bar, picture-in-picture
  style.
- **Tiles you can move.** Drag a tile onto another to swap them, or onto
  another window to move it there. You can also pin a tile large, zoom one
  to fill its screen, or crop the black bars out of a letterboxed film.
  Pinning or zooming a tile gives it the sound and mutes the rest, unless
  everything was muted already (a setting in the config tab, on by default).
- **One queue for everything.** Build it from the library browser, reorder,
  shuffle or sort it, and choose whether it comes back after a restart.
  Hovering a tile shows the queue as a thumbnail strip.
- **Sources:** local folders, Plex, Jellyfin, live cameras (RTSP/RTSPS and
  similar), and http(s) URLs. Signing in to Plex or Jellyfin doesn't need a
  pasted token. Jellyfin servers and Ubiquiti cameras are found on the LAN.
- **Controls scale to their tile.** On a twelve-tile wall the per-tile
  controls shrink with the tile, so they don't bury the video. Each monitor
  remembers its own UI scale.
- **An assistant interface.** `tessera mcp` is an
  [MCP](https://modelcontextprotocol.io) server. An assistant can ask what's
  playing, control playback, search subtitles and look at a frame. See
  [docs/MCP.md](docs/MCP.md).

## What has been tested

There is one maintainer and one set of hardware, so "supported" here means
"used on that hardware". Everything else is a guess until someone reports
otherwise.

| Area | Tested on | Untested — reports welcome |
|---|---|---|
| **Linux** | Pop!_OS, GNOME with the pop-shell tiling extension, X11 session | Wayland sessions; other desktops and window managers (KDE, stock GNOME, i3, sway, …); other distributions |
| **GPU (Linux)** | NVIDIA RTX 5080, NVDEC | AMD and Intel (VA-API); other NVIDIA generations |
| **macOS** | M1 Max, macOS 26, Homebrew mpv 0.41 (first pass) | Intel Macs; older macOS; mixed Retina and non-Retina displays; much of the interactive UI (see [ROADMAP §16](docs/ROADMAP.md)) |
| **Windows** | — | Not supported, and no plans |
| **Plex / Jellyfin** | The maintainer's own servers | Other server versions and setups |
| **Cameras** | Ubiquiti UniFi Protect (G4 Dome, G6 Turret) over RTSPS from the controller, on Linux and macOS | Other RTSP cameras and NVRs; RTMP, SRT, HLS, MJPEG |

Some notes on those rows:

- **Window managers.** Window placement was tuned under pop-shell, which
  moves tiled windows back to their slot after the app moves them. Other
  window managers may place new windows or restore from fullscreen
  differently.
- **Cameras.** Cameras are plain stream URLs, so anything mpv can open
  should play. LAN discovery only finds Ubiquiti devices; for anything else,
  paste the stream URL. A UniFi Protect camera streams through its
  controller, not on its own; the add-source form explains this.
- **macOS.** The DMG on the [releases page](https://github.com/rabbitHoleSolutions/openTessera/releases)
  is not signed by Apple yet, so macOS asks you to approve it once (the
  release notes say how). It needs Apple Silicon and macOS 15 or newer.

Known rough edges are listed in [docs/KNOWN_ISSUES.md](docs/KNOWN_ISSUES.md).

## Requirements

- **Rust** 1.90 or newer, plus a C compiler (one dependency builds a
  little C). Not needed for the macOS DMG.
- **libmpv**, loaded at runtime, so no development headers are needed:
  - Debian/Ubuntu: `sudo apt install libmpv2` (older releases: `libmpv1`)
  - Fedora: `sudo dnf install mpv-libs`
  - Arch: `sudo pacman -S mpv`
  - macOS: `brew install mpv` (the DMG carries its own)

  Set `TESSERA_MPV_LIB=/path/to/libmpv.so` to use a particular copy.
- **Hardware decoding** works through mpv. On Linux the default is
  `--hwdec nvdec-copy,vaapi-copy`: NVDEC on NVIDIA, VA-API on AMD and
  Intel, both copying frames back rather than using GL zero-copy interop,
  because NVIDIA's CUDA/GL interop crashes when a decoder re-initializes
  mid-playback. (mpv older than 0.36 can't take a list and gets
  `nvdec-copy` alone; pass `--hwdec vaapi-copy` there on AMD or Intel.)
  On macOS the default is `videotoolbox`.

## Build and install

```sh
cargo build --release            # the binary is target/release/tessera
```

**Linux desktop:**

```sh
./packaging/install.sh           # --default also makes it the default video player
```

This installs for the current user only, without root. It puts `tessera` in
`~/.local/bin`, adds the icon and a desktop entry, and lists the app under
**Open With** for video files. `--uninstall` reverses it.

**macOS:** download the DMG from the
[releases page](https://github.com/rabbitHoleSolutions/openTessera/releases)
and drag Tessera into Applications. To build it yourself instead:

```sh
./packaging/macos/build-app.sh --install
```

This builds `Tessera.app` into `~/Applications`, using Homebrew's libmpv.
`--default` also offers to make it the default player for video types.
macOS asks you to confirm each one. `--dmg` builds the release DMG instead,
with libmpv and its libraries copied into the app.

Configuration lives in `~/.config/opentessera/` on both systems. The
installers never touch it.

## Usage

```sh
tessera ~/Videos/                    # play the first video, the rest queued (+ adds tiles)
tessera film.mkv                     # play this file
tessera --max --tiles 6 ~/Videos/    # every monitor fullscreen, six tiles each
tessera --monitor right --fullscreen film.mkv
tessera --plex --plex-library Movies
tessera rtsps://controller:7441/abcdef   # a camera
```

Files and folders on the command line replace the saved sources for that run,
so opening a video from the file manager plays that video. Running `tessera`
again while it is already open hands the files to the running instance
instead of starting a second one. `tessera --help` lists every option.

### Sources

Add sources in the browser window's **sources** tab (press `S`):

- **Folder:** a local or mounted directory, scanned recursively. To use a
  network share, mount it and add the mount point.
- **Jellyfin:** found on the LAN. You sign in with a username and password,
  which are exchanged once for a token and never stored.
- **Plex:** link through plex.tv by entering the code shown at
  plex.tv/link, then pick a server.
- **Cameras:** a list of stream URLs. The scan button finds Ubiquiti cameras
  on the LAN.

Remote videos stream as the original file over each server's direct-play
endpoint. Nothing is transcoded, and seeking works as it does for local files.

### Controls

| Key / mouse | Action |
|---|---|
| Click a tile | Pause / resume it |
| Right-click a tile | Mute / unmute it (ctrl-click on macOS) |
| Drag a tile | Swap with another tile, or move it to another window |
| Wheel | Volume of the tile under the cursor |
| `←` / `→` | Seek ∓10s on the tile under the cursor (hold to scan) |
| `↑` / `↓` | Volume of the unmuted tiles in the window |
| `Space` | Pause / resume every tile in the window |
| `N` / `Shift+P` | Next / previous queue item |
| `P` | Pin the tile under the cursor large (its sound plays, the rest mute) |
| `F` | Zoom the tile under the cursor to fill its screen (likewise) |
| `W` | Toggle this window between windowed and fullscreen |
| `T` | Float this window on top of everything, without a title bar (drag the status bar to move it, the edges to resize) |
| `-` / `+` | Remove the tile under the cursor / add a tile |
| `L` `Q` `S` `C` | Browser window: library, queue, sources, config |
| `Tab` | Keep the overlay visible (otherwise it fades out after 1s idle; the browser's config tab sets both times) |
| `?` | Help |
| `Esc` | Leave tile zoom; otherwise quit |

On macOS, `Cmd+W` closes a window, `Ctrl+Cmd+F` toggles fullscreen, and
`Cmd+Q` quits.

### Diagnostics

The useful switches when something misbehaves:

- `--status-interval 5` prints per-tile playback rate, frame rate, decoder
  and dropped frames every 5 seconds.
- `TESSERA_MPV_LOG=v` turns on mpv's own verbose log.
- `TESSERA_PROFILE=1` logs slow render and property calls.
- `TESSERA_VSYNC=1` turns vsync on. It is off by default because of a swap
  stall seen with occluded windows on X11.

## Assistants (MCP)

`tessera mcp` is an MCP server on stdio that talks to the running app. Any
MCP client connects with:

```json
{ "command": "tessera", "args": ["mcp"] }
```

There is no port and no token. It can only open media from configured folder
sources and `~/Videos`, and no tool deletes anything. `tessera mcp --dev`
adds tools for developing openTessera itself. Details are in
[docs/MCP.md](docs/MCP.md).

## Contributing

Bug reports and test reports from setups not in the table above are the most
welcome thing you can send. Code is welcome too. See
[CONTRIBUTING.md](CONTRIBUTING.md) for how to build, test and report.

The workspace is three crates:

| Crate | What it holds |
|---|---|
| `tessera-mpv` | libmpv render-API wrapper. The library is `dlopen`ed and its C ABI declared by hand. |
| `tessera-core` | Logic with no UI: the queue, the layout solver, the decode budget, sources, title and subtitle parsing. Most of the tests live here. |
| `tessera-app` | The `tessera` binary: windows, GL compositing, the egui overlay, the API and the MCP server. |

Feature history and plans are in [docs/ROADMAP.md](docs/ROADMAP.md).

## License

GPL-3.0-or-later; see [LICENSE](LICENSE). libmpv is GPLv2+ (or LGPLv2.1+,
depending on how it was built).
