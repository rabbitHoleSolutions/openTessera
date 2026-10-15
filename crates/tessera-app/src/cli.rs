//! Command-line arguments: what `tessera` accepts and the usage text.

use std::path::PathBuf;

use crate::{platform, DEFAULT_WINDOW};

pub enum ScreenSel {
    /// Every monitor, borderless fullscreen (`--max`).
    All,
    /// These monitors, borderless fullscreen (`--screens 0,2`).
    Indices(Vec<usize>),
    /// One ordinary window at this size. The default — extra windows are
    /// opened at runtime from the status bar's "+" button, and each one
    /// toggles its own fullscreen with W.
    Windowed(u32, u32),
}

pub struct Args {
    pub files: Vec<PathBuf>,
    pub tiles: usize,
    pub populate: Option<usize>,
    pub screens: ScreenSel,
    pub hwdec: String,
    /// --audio / --no-audio: overrides the saved setting when given.
    pub audio: Option<bool>,
    pub audio_device: Option<String>,
    pub ui_scale: Option<f32>,
    pub cache_mb: Option<u32>,
    pub max_streams: Option<usize>,
    pub ceiling: f64,
    pub advance: bool,
    /// --status-interval: seconds between status reports; 0 (the default)
    /// prints none.
    pub status_interval: u64,
    pub overlay: bool,
    pub plex: bool,
    pub jellyfin: bool,
    pub plex_library: Option<String>,
    pub jellyfin_library: Option<String>,
    /// --monitor: which monitor the (single) window starts on — a number,
    /// a connector name or a position, resolved once monitors are known.
    pub monitor: Option<String>,
    /// --fullscreen: start that window fullscreen.
    pub fullscreen: bool,
}

pub fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        files: Vec::new(),
        tiles: 12,
        populate: None,
        screens: ScreenSel::Windowed(DEFAULT_WINDOW.0, DEFAULT_WINDOW.1),
        hwdec: platform::DEFAULT_HWDEC.into(),
        audio: None,
        audio_device: None,
        ui_scale: None,
        cache_mb: None,
        max_streams: None,
        ceiling: tessera_core::budget::DEFAULT_CEILING_EQUIVALENTS,
        advance: false,
        status_interval: 0,
        overlay: false,
        plex: false,
        jellyfin: false,
        plex_library: None,
        jellyfin_library: None,
        monitor: None,
        fullscreen: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{arg} requires a value"));
        match arg.as_str() {
            "--tiles" => args.tiles = value()?.parse().map_err(|_| "bad --tiles")?,
            "--populate" => args.populate = Some(value()?.parse().map_err(|_| "bad --populate")?),
            "--max" => args.screens = ScreenSel::All,
            "--monitor" => args.monitor = Some(value()?),
            "--fullscreen" => args.fullscreen = true,
            "--screens" => match value()?.as_str() {
                "all" => args.screens = ScreenSel::All,
                n => args.screens = ScreenSel::Indices(
                    n.split(',')
                        .map(|p| p.trim().parse().map_err(|_| "bad --screens"))
                        .collect::<Result<Vec<_>, _>>()?,
                ),
            },
            "--windowed" => {
                let v = value()?;
                let (w, h) = v.split_once('x').ok_or("--windowed expects WxH")?;
                args.screens = ScreenSel::Windowed(w.parse().map_err(|_| "bad --windowed W")?, h.parse().map_err(|_| "bad --windowed H")?);
            }
            "--hwdec" => args.hwdec = value()?,
            "--max-streams" => args.max_streams = Some(value()?.parse().map_err(|_| "bad --max-streams")?),
            "--ceiling" => args.ceiling = value()?.parse().map_err(|_| "bad --ceiling")?,
            "--status-interval" => args.status_interval = value()?.parse().map_err(|_| "bad --status-interval")?,
            "--overlay" => args.overlay = true,
            "--plex" => args.plex = true,
            "--jellyfin" => args.jellyfin = true,
            "--plex-library" => args.plex_library = Some(value()?),
            "--jellyfin-library" => args.jellyfin_library = Some(value()?),
            "--audio" => args.audio = Some(true),
            "--no-audio" => args.audio = Some(false),
            "--audio-device" => args.audio_device = Some(value()?),
            "--ui-scale" => args.ui_scale = Some(value()?.parse().map_err(|_| "bad --ui-scale")?),
            "--cache-mb" => args.cache_mb = Some(value()?.parse().map_err(|_| "bad --cache-mb")?),
            "--advance" => args.advance = true,
            "--loop" => args.advance = false,
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            // Launch Services' process serial number, which older macOS
            // passes to apps opened from Finder. Not ours to parse.
            s if s.starts_with("-psn_") => {}
            s if s.starts_with('-') => return Err(format!("unknown flag {s}")),
            s => args.files.push(PathBuf::from(s)),
        }
    }
    // NB: no files/flags is NOT an error here — sources saved in the
    // settings panel (settings.conf) can feed the wall with no CLI args.
    // The empty-items check after source gathering gives the real verdict.
    if args.tiles == 0 {
        return Err("--tiles must be >= 1".into());
    }
    if args.populate == Some(0) {
        return Err("--populate must be >= 1".into());
    }
    // --monitor and --fullscreen describe one window; --screens and --max
    // already pick their own monitors.
    let windowed = matches!(args.screens, ScreenSel::Windowed(_, _));
    if args.monitor.is_some() && !windowed {
        return Err("--monitor picks the one window's monitor; --screens/--max already choose theirs".into());
    }
    if args.fullscreen && !windowed {
        return Err("--fullscreen is for the one window; --screens/--max are already fullscreen".into());
    }
    Ok(args)
}

pub fn print_usage() {
    println!(
        "tessera — desktop multi-stream video wall

USAGE:
  tessera [OPTIONS] <FILE|DIR>...

OPTIONS:
  --tiles N          Max tiles per screen (default 12)
  --populate N       Tiles to open per screen at startup (default: same as
                     --tiles; e.g. --tiles 10 --populate 8 loads 8, max 10)
  --max              Every monitor, borderless fullscreen, filled to --tiles
                     (the video-wall mode; without it you get one window)
  --screens SPEC     Monitors to use fullscreen: 'all' or comma-separated
                     indices, e.g. '1' or '0,2'
  --monitor M        Start the window on monitor M: a number (as printed at
                     startup, monitor N: ...; 0 by default), a connector
                     name (DP-4) or a position (left, middle, right)
  --fullscreen       Start the window fullscreen on that monitor (as W does)
  --windowed WxH     Size of the initial window (default 1600x900). Extra
                     windows: the '+' button in the status bar; W toggles
                     any one window between windowed and fullscreen
  --advance          Advance to the next queue item on end (default: loop)
  --no-audio         Disable audio entirely (default: on — the first tile is
                     heard, tiles added beside it start muted). --audio turns
                     it back on if the config tab saved it off
  --audio-device DEV mpv audio device, e.g. pulse/alsa_output.pci-...hdmi-stereo
                     (default: system default; check `pactl list sinks short`)
  --ui-scale F       Overlay UI scale, 0.75-3.0 (default 1.0; also in settings)
  --cache-mb N       Demuxer cache per stream in MiB (default 512; also in settings)
  --hwdec MODE       {hwdec_help}
  --max-streams N    Budget: max concurrent streams (default 128; also in settings)
  --ceiling N        Budget: 1080p30-equivalent decode ceiling (default 96)
  --status-interval S  Print per-tile playback stats every S seconds
                     (default: off; 5 when TESSERA_DUMP_DIR is set)
  --overlay          Start with the overlay UI pinned on (Tab toggles pinning;
                     otherwise the UI shows on input and hides after 5s idle)
  --plex             Add the Plex server from ~/.config/opentessera/sources.conf
                     (or TESSERA_PLEX_URL / TESSERA_PLEX_TOKEN env vars)
  --jellyfin         Add the Jellyfin server the same way
                     (TESSERA_JELLYFIN_URL / TESSERA_JELLYFIN_KEY)
  --plex-library N         Only list this Plex library (by title)
  --jellyfin-library N     Only list this Jellyfin library (by name)

SECOND LAUNCH:
  Running tessera again does not start a second app: it hands its files to
  the instance already running, brings that window to the front and exits.
  One process owns one queue and one decode budget, so a second would
  double-count the GPU and both would overwrite each other's settings.
  The instance is keyed on the config directory, so a different HOME is a
  different instance. Set TESSERA_ALLOW_MULTI=1 to force a second process
  against the same config (development only — it reinstates both hazards).

CONTROLS:
  Esc                Quit
  Tab                Pin/unpin the overlay UI (auto-hides after 5s idle)
  Space              Pause/resume all tiles on the focused screen
  Left click         Pause/resume the clicked tile
  Drag               Rearrange: drop on another tile to swap, or onto another
                     screen to move the tile there (Esc cancels)
  Right click        Mute/unmute the clicked tile
  Up/Down arrows     Volume +/-5 on unmuted tiles of the focused screen
  Wheel              Volume +/-5 on the tile under the cursor
  Left/Right arrows  Seek -/+10s on the tile under the cursor
  N / Shift+P        Next / previous queue item on each tile of the focused screen
  P                  Pin the tile under the cursor: it takes most of the screen,
                     the others share a bottom strip (P again unpins)
  - / +              Remove the tile under the cursor / add a tile (focused screen)
  F                  Fullscreen the tile under the cursor on its screen (F/Esc exits)
  W                  Toggle THIS screen (cursor's window): fullscreen wall <-> window
  S                  Toggle the settings panel (gear) on the focused screen
  L / Q              Browser window on the library / queue tab (opens it if
                     there isn't one yet)
  Esc                Exit fullscreen if zoomed; otherwise quit",
        hwdec_help = platform::HWDEC_HELP
    );
}
