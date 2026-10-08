mod auth;
mod browser;
mod catalog;
mod compositor;
mod instance;
mod overlay;
mod platform;
mod poster;
mod thumbs;
mod png_util;
mod screen;
mod tile;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use glutin::display::GetGlDisplay;
use glutin::prelude::*;
use glutin_winit::DisplayBuilder;
use raw_window_handle::HasRawWindowHandle;
use tessera_core::{Admission, DecodeBudget, PlaybackQueue, QueueOrder};
use winit::event::{ElementState, Event, MouseButton, MouseScrollDelta, StartCause, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::WindowBuilder;

use compositor::set_gl_loader;
use overlay::{OverlayAction, OverlayState};
use screen::{Screen, ScreenRole};
use tile::{Tile, TileOptions};

enum ScreenSel {
    /// Every monitor, borderless fullscreen (`--max`).
    All,
    /// These monitors, borderless fullscreen (`--screens 0,2`).
    Indices(Vec<usize>),
    /// One ordinary window at this size. The default — extra windows are
    /// opened at runtime from the status bar's "+" button, and each one
    /// toggles its own fullscreen with W.
    Windowed(u32, u32),
}

/// The window/taskbar icon, baked into the binary so a window looks right
/// even when the app is run straight out of target/release with nothing
/// installed. The desktop entry uses the installed hicolor icons instead.
const WINDOW_ICON_PNG: &[u8] = include_bytes!("../../../assets/window-icon-64.png");

/// Decodes the embedded icon. A failure here is cosmetic — the window just
/// gets the WM's default — so it never stops startup.
fn window_icon() -> Option<winit::window::Icon> {
    let img = image::load_from_memory(WINDOW_ICON_PNG).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    winit::window::Icon::from_rgba(img.into_raw(), w, h).ok()
}

/// Size of the initial window, and of windows opened at runtime, when no
/// `--windowed WxH` is given. Clamped to the target monitor.
const DEFAULT_WINDOW: (u32, u32) = (1600, 900);

/// Tiles a plain (non---max) window opens with. `--max` fills to --tiles
/// instead; `--populate` overrides both. Matches the Quest app's dock
/// default (DockGridManager.INITIAL_TILES).
const DEFAULT_WINDOW_TILES: usize = 4;

/// Tiles a window opened at runtime starts with. Deliberately not
/// DEFAULT_WINDOW_TILES: at startup nothing is playing yet so filling is
/// free, but adding a window to a live wall would evict other screens'
/// streams (OLDEST_STARTED) to admit them. Grow it with + instead.
const NEW_WINDOW_TILES: usize = 1;

/// Pointer travel (px) before a held left press becomes a tile drag —
/// small enough to feel immediate, big enough that plain clicks stay clicks.
const DRAG_SLOP_PX: f32 = 8.0;

/// Minimum spacing between applied button-wheel steps. winit/X11 emits a
/// MouseWheel for BOTH the press and the release of a scroll button, so
/// without this a single notch would step the volume twice.
const WHEEL_NOTCH_INTERVAL: Duration = Duration::from_millis(50);

/// Smooth-scroll travel (logical px) that counts as one wheel notch.
/// Trackpads and the Magic Mouse (macOS, Wayland) report a gesture as a
/// stream of small pixel deltas — dozens per swipe — and stepping the volume
/// on each one slammed it from 0 to the cap. X11 never sends these.
const WHEEL_PIXELS_PER_NOTCH: f64 = 40.0;

struct Args {
    files: Vec<PathBuf>,
    tiles: usize,
    populate: Option<usize>,
    screens: ScreenSel,
    hwdec: String,
    audio: bool,
    audio_device: Option<String>,
    ui_scale: Option<f32>,
    cache_mb: Option<u32>,
    max_streams: Option<usize>,
    ceiling: f64,
    advance: bool,
    status_interval: u64,
    overlay: bool,
    plex: bool,
    jellyfin: bool,
    plex_library: Option<String>,
    jellyfin_library: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        files: Vec::new(),
        tiles: 12,
        populate: None,
        screens: ScreenSel::Windowed(DEFAULT_WINDOW.0, DEFAULT_WINDOW.1),
        hwdec: platform::DEFAULT_HWDEC.into(),
        audio: false,
        audio_device: None,
        ui_scale: None,
        cache_mb: None,
        max_streams: None,
        ceiling: tessera_core::budget::DEFAULT_CEILING_EQUIVALENTS,
        advance: false,
        status_interval: 5,
        overlay: false,
        plex: false,
        jellyfin: false,
        plex_library: None,
        jellyfin_library: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{arg} requires a value"));
        match arg.as_str() {
            "--tiles" => args.tiles = value()?.parse().map_err(|_| "bad --tiles")?,
            "--populate" => args.populate = Some(value()?.parse().map_err(|_| "bad --populate")?),
            "--max" => args.screens = ScreenSel::All,
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
            "--audio" => args.audio = true,
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
    Ok(args)
}

fn print_usage() {
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
  --windowed WxH     Size of the initial window (default 1600x900). Extra
                     windows: the '+' button in the status bar; W toggles
                     any one window between windowed and fullscreen
  --advance          Advance to the next queue item on end (default: loop)
  --audio            Enable audio (default muted)
  --audio-device DEV mpv audio device, e.g. pulse/alsa_output.pci-...hdmi-stereo
                     (default: system default; check `pactl list sinks short`)
  --ui-scale F       Overlay UI scale, 0.75-3.0 (default 1.0; also in settings)
  --cache-mb N       Demuxer cache per stream in MiB (default 512; also in settings)
  --hwdec MODE       {hwdec_help}
  --max-streams N    Budget: max concurrent streams (default 128; also in settings)
  --ceiling N        Budget: 1080p30-equivalent decode ceiling (default 96)
  --status-interval S  Seconds between status prints (default 5)

SECOND LAUNCH:
  Running tessera again does not start a second app: it hands its files to
  the instance already running, brings that window to the front and exits.
  One process owns one queue and one decode budget, so a second would
  double-count the GPU and both would overwrite each other's settings.
  The instance is keyed on the config directory, so a different HOME is a
  different instance. Set TESSERA_ALLOW_MULTI=1 to force a second process
  against the same config (development only — it reinstates both hazards).
  --overlay          Start with the overlay UI pinned on (Tab toggles pinning;
                     otherwise the UI shows on input and hides after 5s idle)
  --plex             Add the Plex server from ~/.config/opentessera/sources.conf
                     (or TESSERA_PLEX_URL / TESSERA_PLEX_TOKEN env vars)
  --jellyfin         Add the Jellyfin server the same way
                     (TESSERA_JELLYFIN_URL / TESSERA_JELLYFIN_KEY)
  --plex-library N         Only list this Plex library (by title)
  --jellyfin-library N     Only list this Jellyfin library (by name)

CONTROLS:
  Esc                Quit
  Tab                Pin/unpin the overlay UI (auto-hides after 5s idle)
  Space              Pause/resume all tiles on the focused screen
  Left click         Pause/resume the clicked tile
  Drag               Rearrange: drop on another tile to swap, or onto another
                     screen to move the tile there (Esc cancels)
  Right click        Mute/unmute the clicked tile (needs --audio)
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

/// Debug aid: print a backtrace on fatal signals (no gdb on this box).
unsafe extern "C" fn fatal_signal(sig: i32, info: *mut libc::siginfo_t, _: *mut libc::c_void) {
    let addr = unsafe { (*info).si_addr() } as usize;
    eprintln!(
        "\nFATAL signal {sig} at address 0x{addr:x}\nbacktrace:\n{}",
        std::backtrace::Backtrace::force_capture()
    );
    std::process::exit(128 + sig);
}

fn install_fatal_handler() {
    unsafe {
        let mut act: libc::sigaction = std::mem::zeroed();
        act.sa_sigaction = fatal_signal as usize;
        act.sa_flags = libc::SA_SIGINFO | libc::SA_NODEFER;
        libc::sigemptyset(&mut act.sa_mask);
        libc::sigaction(libc::SIGSEGV, &act, std::ptr::null_mut());
        libc::sigaction(libc::SIGABRT, &act, std::ptr::null_mut());
        libc::sigaction(libc::SIGBUS, &act, std::ptr::null_mut());
    }
}

fn main() {
    install_fatal_handler();
    platform::raise_fd_limit();
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n");
            print_usage();
            std::process::exit(2);
        }
    };

    // Single instance, before anything expensive. A second launch is a
    // request aimed at the running app, not a second app: it hands over its
    // paths and exits. See `instance.rs` for why this is ownership rather
    // than a nicety — one queue and one decode budget per *process*.
    let mut opens = match instance::acquire(&config_dir(), &args.files) {
        instance::Acquired::HandedOff => {
            // Say so rather than dropping them silently: a window that
            // already exists cannot retroactively adopt --windowed or
            // --tiles, and a user who passed them deserves to know they
            // did nothing.
            let flags: Vec<String> = std::env::args()
                .skip(1)
                .filter(|a| a.starts_with('-'))
                .collect();
            if !flags.is_empty() {
                eprintln!(
                    "note: {} ignored — flags apply only to the launch that starts the app",
                    flags.join(" ")
                );
            }
            return;
        }
        instance::Acquired::Primary(opens) => opens,
    };

    // Settings first: sources configured in the gear panel (persisted in
    // settings.conf) feed the wall alongside any CLI paths/flags.
    let mut settings = load_settings();
    if args.audio {
        settings.audio = true;
    }
    if let Some(c) = args.cache_mb {
        settings.cache_mb = c.clamp(64, 8192);
    }
    if let Some(m) = args.max_streams {
        settings.max_streams = (m as u32).clamp(1, 128);
    }

    // Media sources.
    //
    // Positional paths mean "play this". Opening a video from the file
    // manager or the dock has to play THAT video, not append it to a saved
    // library of hundreds and start somewhere else — so paths on the
    // command line replace the saved sources rather than joining them.
    // A bare launch still uses the saved sources, and --plex/--jellyfin
    // still merge in, because those are an explicit ask for a server.
    let mut sources: Vec<tessera_core::sources::Source> = if args.files.is_empty() {
        settings.sources.clone()
    } else {
        Vec::new()
    };
    let mut push_unique = |src: tessera_core::sources::Source| {
        if !sources.contains(&src) {
            sources.push(src);
        }
    };
    for f in &args.files {
        push_unique(tessera_core::sources::Source::Folder { path: f.display().to_string() });
    }
    if args.plex || args.jellyfin {
        let cfg = tessera_core::sources::SourceConfig::load();
        if args.plex {
            match cfg.plex {
                Some(srv) => push_unique(tessera_core::sources::Source::Plex {
                    url: srv.url,
                    token: srv.token,
                    library: args.plex_library.clone(),
                }),
                None => eprintln!("warning: --plex but no server configured (sources.conf / env)"),
            }
        }
        if args.jellyfin {
            match cfg.jellyfin {
                Some(srv) => push_unique(tessera_core::sources::Source::Jellyfin {
                    url: srv.url,
                    api_key: srv.token,
                    library: args.jellyfin_library.clone(),
                }),
                None => eprintln!("warning: --jellyfin but no server configured (sources.conf / env)"),
            }
        }
    }
    // Enumerated only when the command line named paths ("play this").
    // A bare launch takes its queue from the saved file, or starts empty —
    // so it no longer waits on an HTTP walk of every configured server, and
    // the library browser lists on demand when you actually open it.
    let mut items = Vec::new();
    if !args.files.is_empty() {
        for src in &sources {
            match src.list() {
                Ok(list) => {
                    println!("source {}: {} item(s)", src.label(), list.len());
                    items.extend(list);
                }
                Err(e) => eprintln!("warning: source {} failed: {e}", src.label()),
            }
        }
    }
    // Only an error when the user actually asked for something and it
    // yielded nothing; an empty queue is a normal state now.
    if items.is_empty() && !args.files.is_empty() {
        eprintln!("error: no video files found in the path(s) given");
        std::process::exit(2);
    }
    println!(
        "tessera: {} item(s) from {} source(s), tiles<={}, hwdec={}, audio={}, advance={}",
        items.len(),
        sources.len(),
        args.tiles,
        args.hwdec,
        settings.audio,
        args.advance
    );
    if let Some(p) = args.populate {
        println!("tessera: populating {} tile(s) per screen", p.min(args.tiles));
    }

    let mpv_lib = match tessera_mpv::MpvLib::load() {
        Ok(l) => {
            let v = l.client_api_version();
            // MPV_MAKE_VERSION packs major in the high 16 bits and minor
            // in the low 16 — splitting the minor into two bytes printed
            // API 2.2 (0x20002) as "2.0.2".
            println!("libmpv client API 0x{:X} ({}.{})", v, v >> 16, v & 0xFFFF);
            l
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    let event_loop = EventLoop::new().unwrap();
    // Between creating the event loop and running it: the file that
    // launched us from Finder is delivered as soon as it runs.
    platform::install_open_handler();
    let monitors: Vec<_> = event_loop.available_monitors().collect();
    for (i, m) in monitors.iter().enumerate() {
        let size = m.size();
        let pos = m.position();
        println!("monitor {i}: {}x{} +{}+{}", size.width, size.height, pos.x, pos.y);
    }
    if monitors.is_empty() {
        eprintln!("error: no monitors found");
        std::process::exit(1);
    }

    let monitor_indices: Vec<usize> = match &args.screens {
        ScreenSel::All => (0..monitors.len()).collect(),
        ScreenSel::Indices(list) => {
            if list.is_empty() {
                eprintln!("error: --screens needs at least one monitor index");
                std::process::exit(2);
            }
            for i in list {
                if *i >= monitors.len() {
                    eprintln!("error: monitor {i} not found (have {})", monitors.len());
                    std::process::exit(2);
                }
            }
            list.clone()
        }
        ScreenSel::Windowed(_, _) => vec![0],
    };

    let (first_window, gl_config) = {
        let wb = window_builder(&args, &monitors, monitor_indices[0]);
        let template = glutin::config::ConfigTemplateBuilder::new()
            .with_alpha_size(8)
            .with_stencil_size(0);
        match DisplayBuilder::new()
            .with_window_builder(Some(wb))
            .build(&event_loop, template, |mut configs| configs.next().unwrap())
        {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("error: display/window init failed: {e}");
                std::process::exit(1);
            }
        }
    };

    let display = gl_config.display();
    let loader = compositor::display_loader(&display);
    set_gl_loader(loader);

    let mut windows: Vec<(Arc<winit::window::Window>, usize)> = Vec::new();
    if let Some(w) = first_window {
        windows.push((Arc::new(w), monitor_indices[0]));
    }

    for &idx in monitor_indices.iter().skip(1) {
        let wb = window_builder(&args, &monitors, idx);
        match glutin_winit::finalize_window(&event_loop, wb, &gl_config) {
            Ok(w) => windows.push((Arc::new(w), idx)),
            Err(e) => eprintln!("warning: window for monitor {idx} failed: {e}"),
        }
    }

    let mut tile_opts = TileOptions {
        hwdec: args.hwdec.clone(),
        audio: settings.audio,
        audio_device: args.audio_device.clone(),
        cache_mb: settings.cache_mb,
        loop_file: !args.advance,
        log_verbose: std::env::var("TESSERA_MPV_LOG").map(|v| v == "v").unwrap_or(false),
    };

    // One queue, shared by every window: a tile is a slot fed from it
    // (same model as the Quest app's DockGridManager). Windows created at
    // runtime just pull from it — there is no per-screen slice to assign.
    let mut queue = PlaybackQueue::new(QueueOrder::Name);
    if !args.files.is_empty() {
        queue.add_items(items);
    } else if settings.retain_queue {
        match load_queue() {
            Some((saved, cursor)) => {
                println!("queue: restored {} item(s) from last run", saved.len());
                queue.restore(saved, cursor);
            }
            None => println!("queue: nothing saved yet — fill it from the library browser (L)"),
        }
    } else {
        println!("queue: retention is off — starting empty (library browser: L)");
    }
    // Flushed on a timer and at shutdown rather than on every mutation: the
    // cursor moves on every auto-advance, and rewriting a few hundred items
    // that often is pointless.
    let mut queue_saved_at = Instant::now();
    // Listings for the library browser, fetched on demand off-thread. The
    // startup enumeration above is separate — the wall starts playing
    // without waiting on any of this.
    let mut catalog = catalog::SourceCatalog::new();
    // Server sign-in (Jellyfin discovery/login, plex.tv link) — all of it
    // blocking, so it lives on its own worker.
    let mut auth = auth::Auth::new();
    let plex_client_id = tessera_core::signin::plex_client_id(&config_dir());

    let mut screens: Vec<Screen> = Vec::new();
    let mut budget = DecodeBudget::new(args.ceiling, settings.max_streams as usize);
    let mut next_tile_id = 0usize;

    for (window, monitor_index) in windows {
        let Some(mut screen) = create_screen(&event_loop, &display, &gl_config, window, monitor_index)
        else {
            continue;
        };
        screen.intended_monitor = Some(monitors[monitor_index].clone());
        screen.should_fullscreen = !matches!(args.screens, ScreenSel::Windowed(_, _));
        if screen.should_fullscreen {
            platform::finish_fullscreen_window(&screen.window, &monitors[monitor_index]);
        }
        // Each window carries its own scale from here on; the remembered
        // per-monitor value (or the fallback) is only the starting point.
        screen.ui_scale = seed_scale(&settings, monitors.get(monitor_index), args.ui_scale);
        screens.push(screen);
    }

    if screens.is_empty() {
        eprintln!("error: no usable screens");
        std::process::exit(1);
    }

    let mut mods = winit::keyboard::ModifiersState::empty();
    // Which window the mouse cursor is currently over (W applies there).
    // Held as a WindowId, not an index: screen indices shift when a window
    // is closed.
    let mut cursor_over: Option<winit::window::WindowId> = None;
    // Left mouse button currently held (drag-threshold tracking).
    let mut left_held = false;
    // Files dropped on a window this frame, per window. winit delivers one
    // DroppedFile event per file with no pointer position, so a multi-file
    // drop arrives as a burst and is acted on once, together.
    let mut dropped: Vec<(winit::window::WindowId, Vec<PathBuf>)> = Vec::new();

    // When the last button-wheel step was applied (see WHEEL_NOTCH_INTERVAL).
    let mut last_wheel_at: Option<Instant> = None;
    // Deferred WM-settle check after a window/fullscreen toggle:
    // (deadline, retries left). Retries continue while displaced walls
    // keep being found. Armed at startup too: pop-shell grabs freshly
    // mapped windows (tiling them onto the focused monitor, fullscreen
    // request ignored) until they receive focus — the settle re-asserts
    // fullscreen on the intended monitors, which also grabs focus.
    let mut wm_settle_at: Option<(Instant, u32)> =
        platform::WM_SETTLE.then(|| (Instant::now() + Duration::from_millis(1500), 5));
    let mut overlay = OverlayState { pinned: args.overlay, ..Default::default() };
    let mut last_input = Instant::now();
    // Unconsumed smooth-scroll travel (see WHEEL_PIXELS_PER_NOTCH).
    let mut wheel_accum = 0.0f64;
    // A ctrl-click being treated as a right-click (macOS), so its release
    // is too even if ctrl comes up first.
    let mut secondary_left = false;
    // Set once the wall has been torn down, by whichever path got there
    // first — Esc, closing the last window, or the OS quitting us.
    let mut shut_down = false;
    // Files opened from the OS while running, being listed off-thread.
    // (Drag-and-drop has its own path: see `dropped` and `open_handed_off`.)
    let (opened_tx, opened_rx) = std::sync::mpsc::channel::<Vec<tessera_core::MediaItem>>();

    // Startup population. --max is the video-wall mode: fill every
    // screen to the --tiles cap. A plain window starts at
    // DEFAULT_WINDOW_TILES instead — a wall of 12 streams is not what
    // you want from a bare `tessera`. --populate overrides either.
    let default_populate = match args.screens {
        ScreenSel::Windowed(_, _) => DEFAULT_WINDOW_TILES.min(args.tiles),
        _ => args.tiles,
    };
    let populate_limit = args.populate.map_or(default_populate, |p| p.min(args.tiles));
    if !platform::DEFER_STARTUP_TILES {
        populate_startup(&mut screens, &mut budget, &mut queue, &mpv_lib, &tile_opts, &mut next_tile_id, populate_limit);
    }

    let start = Instant::now();
    let mut last_status = Instant::now();
    let mut last_drops: std::collections::HashMap<usize, i64> = std::collections::HashMap::new();
    let vsync = std::env::var("TESSERA_VSYNC").map(|v| v != "0").unwrap_or(false);
    let frame_interval = if vsync { Duration::from_millis(15) } else { Duration::from_millis(16) };
    // When the next frame is due, counted from the start of the last one.
    // It used to be `now + frame_interval` re-armed on every callback — and
    // the loop's last callback each iteration comes *after* the frame's
    // work, so the real period was that work plus 16ms: ~45Hz on an M1 Max
    // at four tiles, with 60fps video visibly dropping frames to match.
    let mut next_frame_at = Instant::now();

    event_loop
        .run(move |event, target| {
            target.set_control_flow(ControlFlow::WaitUntil(next_frame_at));
            let is_init = matches!(event, Event::NewEvents(StartCause::Init));
            match event {
                Event::NewEvents(StartCause::Init)
                | Event::NewEvents(StartCause::ResumeTimeReached { .. })
                | Event::NewEvents(StartCause::WaitCancelled { .. }) => {
                    // Input still renders straight away (WaitCancelled);
                    // this only sets when the timer next fires. A frame
                    // that overran its interval is followed immediately.
                    next_frame_at = Instant::now() + frame_interval;
                    target.set_control_flow(ControlFlow::WaitUntil(next_frame_at));
                    if is_init && platform::DEFER_STARTUP_TILES {
                        // A Finder open that launched us has landed by now
                        // (see DEFER_STARTUP_TILES). It means what paths on
                        // the command line mean: play this, not the saved
                        // queue — so it replaces sources and queue alike.
                        let launched_with = platform::take_opened_paths();
                        if !launched_with.is_empty() {
                            let (opened_sources, items) = list_paths(&launched_with);
                            if items.is_empty() {
                                notify(&mut overlay, "no video files found in what was opened".into());
                            } else {
                                sources = opened_sources;
                                queue.clear();
                                queue.add_items(items);
                                println!("opened: {} item(s) replace the queue", queue.len());
                            }
                        }
                        populate_startup(&mut screens, &mut budget, &mut queue, &mpv_lib, &tile_opts, &mut next_tile_id, populate_limit);
                    }
                    // Files opened while running: listed off the render
                    // thread (a folder is a directory walk), played when
                    // the listing lands.
                    let opened = platform::take_opened_paths();
                    if !opened.is_empty() {
                        let tx = opened_tx.clone();
                        std::thread::spawn(move || {
                            let _ = tx.send(list_paths(&opened).1);
                        });
                    }
                    while let Ok(items) = opened_rx.try_recv() {
                        let mut wall = Wall {
                            screens: &mut screens,
                            budget: &mut budget,
                            queue: &mut queue,
                            sources: &sources,
                            overlay: &mut overlay,
                            mpv_lib: &mpv_lib,
                            tile_opts: &tile_opts,
                            next_tile_id: &mut next_tile_id,
                            max_tiles: args.tiles,
                            wm_settle_at: &mut wm_settle_at,
                        };
                        play_opened(&mut wall, items);
                    }
                    sync_fullscreen_chrome(&screens);
                    // Overlay auto-hide: visible while pinned or while user
                    // input arrived within the last 5 seconds.
                    overlay.visible = overlay.pinned || last_input.elapsed() < Duration::from_secs(5);
                    // Issue any seek the user has finished spelling out.
                    flush_seek(&mut screens, &mut overlay, false);
                    // Deferred re-anchoring of displaced fullscreen walls.
                    if let Some((at, retries)) = wm_settle_at {
                        if Instant::now() >= at {
                            if fix_displaced_walls(&mut screens) && retries > 1 {
                                wm_settle_at = Some((Instant::now() + Duration::from_millis(1000), retries - 1));
                            } else {
                                wm_settle_at = None;
                            }
                        }
                    }
                    let settings_before = settings.clone();
                    // Deferred: a new window has to be created after the
                    // per-screen loop, which is iterating `screens` by index.
                    let mut new_windows: Vec<(ScreenRole, browser::Tab)> = Vec::new();
                    // Files handed over by a later launch (see instance.rs).
                    for paths in opens.poll() {
                        if open_handed_off(&mut screens, &mut queue, &mut overlay, &paths, OpenTarget::Anywhere) {
                            new_windows.push((ScreenRole::Wall, browser::Tab::Library));
                        }
                    }
                    // Files dragged onto a window. winit reports one event
                    // per file with no position, so they are collected as
                    // they arrive and acted on together here — a multi-file
                    // drop is one gesture and should behave like one.
                    for (window_id, paths) in std::mem::take(&mut dropped) {
                        if open_handed_off(
                            &mut screens,
                            &mut queue,
                            &mut overlay,
                            &paths,
                            OpenTarget::Window(window_id),
                        ) {
                            new_windows.push((ScreenRole::Wall, browser::Tab::Library));
                        }
                    }
                    // Snapshot every wall tile for the queue tab: a Screen
                    // can't see its siblings, so this has to be gathered
                    // before the per-screen loop borrows them one at a time.
                    //
                    // Only when something will read it. Each tile here costs
                    // three mpv property round trips — `pause` and `mute` are
                    // string properties, so mpv formats a C string, we copy
                    // it and hand it back to be freed — plus a title clone.
                    // With no browser window open that was a few thousand
                    // calls a second producing a Vec nobody looked at.
                    let browser_open = screens.iter().any(|s| s.role == ScreenRole::Browser);
                    let wall_tiles: Vec<browser::WallTile> = if !browser_open {
                        Vec::new()
                    } else {
                        screens
                            .iter()
                            .enumerate()
                            .filter(|(_, s)| s.role == ScreenRole::Wall)
                            .flat_map(|(wi, s)| {
                                let monitor = s.monitor_index;
                                s.tiles.iter().map(move |t| browser::WallTile {
                                    window: wi,
                                    monitor,
                                    tile_id: t.id,
                                    title: t.item.title.clone(),
                                    paused: t.is_paused(),
                                    muted: t.is_muted(),
                                    audio_enabled: t.audio_enabled(),
                                    volume: t.volume(),
                                    time_pos: t.last_time_pos,
                                    duration: t.duration,
                                })
                            })
                            .collect()
                    };
                    for screen_idx in 0..screens.len() {
                        let mut services = screen::Services {
                            catalog: &mut catalog,
                            wall_tiles: &wall_tiles,
                            auth: &mut auth,
                            plex_client_id: &plex_client_id,
                        };
                        screens[screen_idx].tick(&mut budget, &mut queue, &mut services, args.advance, &mut overlay, &mut settings);
                        // UI actions from this screen's overlay: execute after
                        // the tick (pause/resume needs cross-screen budget
                        // access, which the drawing code can't have).
                        for action in std::mem::take(&mut overlay.actions) {
                            match action {
                                OverlayAction::NewWindow => {
                                    new_windows.push((ScreenRole::Wall, browser::Tab::Library));
                                    continue;
                                }
                                OverlayAction::NewLibraryWindow { tab } => {
                                    // Focus the browser window if there is
                                    // one, rather than opening a second.
                                    // The L/Q/S/C keys route through here
                                    // too, so keys and buttons cannot drift
                                    // apart on what "open the browser" means.
                                    match screens.iter_mut().find(|s| s.role == ScreenRole::Browser) {
                                        Some(b) => {
                                            b.browser.tab = tab;
                                            b.window.focus_window();
                                            println!("browser window focused ({tab:?} tab)");
                                        }
                                        None => new_windows.push((ScreenRole::Browser, tab)),
                                    }
                                    continue;
                                }
                                _ => {}
                            }
                            let mut wall = Wall {
                                screens: &mut screens,
                                budget: &mut budget,
                                queue: &mut queue,
                                sources: &sources,
                                overlay: &mut overlay,
                                mpv_lib: &mpv_lib,
                                tile_opts: &tile_opts,
                                next_tile_id: &mut next_tile_id,
                                max_tiles: args.tiles,
                                wm_settle_at: &mut wm_settle_at,
                            };
                            handle_overlay_action(&mut wall, action, screen_idx);
                        }
                    }
                    for (role, tab) in std::mem::take(&mut new_windows) {
                        // Place it on the monitor the cursor is on, else the
                        // one the last window is on.
                        let near = cursor_over
                            .and_then(|w| screens.iter().find(|s| s.window.id() == w))
                            .or_else(|| screens.last())
                            .and_then(|s| monitor_under_window(&s.window))
                            .map(|(_, m)| m);
                        let size = match args.screens {
                            ScreenSel::Windowed(w, h) => (w, h),
                            _ => DEFAULT_WINDOW,
                        };
                        let Some(window) = open_window(target, &gl_config, size, near.as_ref())
                        else {
                            notify(&mut overlay, "could not open a new window".into());
                            break;
                        };
                        let monitor_index =
                            monitor_under_window(&window).map(|(i, _)| i).unwrap_or(0);
                        let Some(mut screen) =
                            create_screen(target, &display, &gl_config, window, monitor_index)
                        else {
                            notify(&mut overlay, "could not open a new window (GL init)".into());
                            break;
                        };
                        screen.intended_monitor = monitor_under_window(&screen.window).map(|(_, m)| m);
                        // A new window starts as a window; W makes it a wall.
                        screen.should_fullscreen = false;
                        screen.ui_scale = seed_scale(
                            &settings,
                            screen.intended_monitor.as_ref(),
                            args.ui_scale,
                        );
                        screen.role = role;
                        screen.browser.tab = tab;
                        screen.window.set_title(match role {
                            ScreenRole::Wall => "Tessera",
                            ScreenRole::Browser => "Tessera — browser",
                        });
                        screens.push(screen);
                        let idx = screens.len() - 1;
                        println!(
                            "opened {} window {idx} (monitor {monitor_index}), {} total",
                            match role { ScreenRole::Wall => "wall", ScreenRole::Browser => "browser" },
                            screens.len()
                        );
                        // A browser window has no tiles.
                        let tiles = if role == ScreenRole::Browser { 0 } else { NEW_WINDOW_TILES };
                        for _ in 0..tiles {
                            let mut wall = Wall {
                                screens: &mut screens,
                                budget: &mut budget,
                                queue: &mut queue,
                                sources: &sources,
                                overlay: &mut overlay,
                                mpv_lib: &mpv_lib,
                                tile_opts: &tile_opts,
                                next_tile_id: &mut next_tile_id,
                                max_tiles: args.tiles,
                                wm_settle_at: &mut wm_settle_at,
                            };
                            add_tile_action(&mut wall, idx);
                        }
                    }
                    // Persist the queue on a slow timer. Retention off means
                    // never writing it, so turning the setting off and quitting
                    // does not silently leave last week's queue on disk.
                    if settings.retain_queue
                        && queue_saved_at.elapsed() >= Duration::from_secs(20)
                    {
                        save_queue(&queue);
                        queue_saved_at = Instant::now();
                    }
                    // Apply settings edited in the gear panel (and persist).
                    // ui_scale only moves when the scale slider is released,
                    // so a drag produces exactly one apply/save, not one per
                    // frame.
                    if settings != settings_before {
                        if settings.ui_scale != settings_before.ui_scale {
                            println!("settings: fallback UI scale {}x", settings.ui_scale);
                        }
                        if settings.monitor_scale != settings_before.monitor_scale {
                            for (name, v) in settings.monitor_scale.iter() {
                                if settings_before.monitor_scale.get(name) != Some(v) {
                                    println!("settings: UI scale {v}x on {name}");
                                }
                            }
                        }
                        if settings.retain_queue != settings_before.retain_queue {
                            if settings.retain_queue {
                                save_queue(&queue);
                                println!("settings: queue retention on");
                            } else {
                                // Off means off: leaving the file behind
                                // would resurrect this queue if it were
                                // switched back on months later.
                                let _ = std::fs::remove_file(queue_path());
                                println!("settings: queue retention off (saved queue discarded)");
                            }
                        }
                        if settings.audio != settings_before.audio {
                            for screen in screens.iter_mut() {
                                for tile in screen.tiles.iter_mut() {
                                    tile.set_audio_enabled(settings.audio);
                                }
                            }
                            tile_opts.audio = settings.audio;
                            println!("settings: audio {}", if settings.audio { "enabled" } else { "disabled" });
                        }
                        if settings.cache_mb != settings_before.cache_mb {
                            for screen in screens.iter_mut() {
                                for tile in screen.tiles.iter_mut() {
                                    tile.set_cache_mb(settings.cache_mb);
                                }
                            }
                            tile_opts.cache_mb = settings.cache_mb;
                            println!("settings: cache {} MiB per stream", settings.cache_mb);
                        }
                        if settings.max_streams != settings_before.max_streams {
                            let evicted = budget.set_max_streams(settings.max_streams as usize);
                            println!(
                                "settings: max streams {} (was {}){}",
                                settings.max_streams,
                                settings_before.max_streams,
                                if evicted.is_empty() {
                                    String::new()
                                } else {
                                    format!(" — evicted {} oldest stream(s)", evicted.len())
                                }
                            );
                            for id in evicted {
                                pause_and_release(&mut screens, &mut budget, id, "evicted: max streams lowered");
                            }
                        }
                        save_settings(&settings);
                    }
                    // Deferred click resolution. Right presses execute now
                    // (mute toggle); left presses that no UI layer claimed
                    // stay parked in pending_click until release — they
                    // become plain clicks (pause/resume) or tile drags once
                    // the pointer crosses DRAG_SLOP_PX.
                    if let Some(pc) = overlay.pending_click {
                        if pc.right {
                            overlay.pending_click = None;
                            overlay.solo_id = None;
                            if let Some(idx) = screens.iter().position(|s| s.window.id() == pc.window_id) {
                                screens[idx].click_mute(pc.x, pc.y);
                            }
                        }
                    }
                    if last_status.elapsed() >= Duration::from_secs(args.status_interval) {
                        let dt = last_status.elapsed().as_secs_f64();
                        last_status = Instant::now();
                        let elapsed = start.elapsed().as_secs();
                        println!("--- t+{elapsed}s --- fds {}", platform::fd_count());

                        // Adaptive decode-budget feedback: sample per-tile
                        // decoder drop rates; sustained drops shrink the
                        // effective ceiling and evict the worst offender,
                        // clean windows recover it.
                        let mut hot = false;
                        for screen in screens.iter() {
                            for tile in screen.tiles.iter() {
                                if tile.is_paused() {
                                    continue;
                                }
                                let drops = tile.decoder_drops();
                                let last = last_drops.get(&tile.id).copied().unwrap_or(drops);
                                last_drops.insert(tile.id, drops);
                                let rate = (drops - last).max(0) as f64 / dt;
                                if budget.report_drop_rate(tile.id, rate) {
                                    hot = true;
                                }
                            }
                        }
                        if hot {
                            if let Some(victim) = budget.enforce() {
                                pause_and_release(&mut screens, &mut budget, victim, "evicted: sustained decoder drops");
                            }
                        } else if budget.active_count() > 0 {
                            budget.recover();
                        }

                        if budget.adaptive() < 1.0 {
                            println!(
                                "budget: adaptive ceiling {:.0}% of static, {:.1}/{:.0} streams in use, {} evictions total",
                                budget.adaptive() * 100.0,
                                budget.used_equivalents(),
                                budget.ceiling_equivalents(),
                                budget.total_evictions()
                            );
                        }

                        for screen in screens.iter_mut() {
                            screen.status(dt);
                        }
                        if let Some(dir) = dump_dir() {
                            // Include the screen index: several windows can
                            // share a monitor now, and monitor_index alone
                            // made them overwrite each other's dumps.
                            for (i, screen) in screens.iter_mut().enumerate() {
                                let tag = format!("w{i}-t{elapsed}s");
                                screen.request_screenshot(dir.clone(), &tag);
                            }
                        }
                    }
                }
                Event::WindowEvent { window_id, event } => {
                    let idx = match screens.iter().position(|s| s.window.id() == window_id) {
                        Some(i) => i,
                        None => return,
                    };
                    // Tab toggles the overlay; checked before egui sees the
                    // event so it can't be swallowed for focus navigation.
                    if let WindowEvent::KeyboardInput { event: ke, .. } = &event {
                        if ke.state == ElementState::Pressed
                            && !screens[idx].egui.egui_ctx.wants_keyboard_input()
                            && ke.logical_key == Key::Named(NamedKey::Tab)
                        {
                            overlay.pinned = !overlay.pinned;
                            println!("overlay: {}", if overlay.pinned { "pinned on" } else { "auto-hide" });
                            return;
                        }
                    }
                    // Forward everything to egui so its input state stays
                    // fresh. Event-time consumption is NOT used to gate raw
                    // input: egui's pointer position lags one frame, so
                    // clicks are deferred and hit-tested during the next
                    // frame's egui pass (see OverlayState::pending_click).
                    let window = screens[idx].window.clone();
                    let _ = screens[idx].egui.on_window_event(&window, &event);
                    // OS shortcuts (macOS Cmd chords) outrank the wall's
                    // hotkeys: Cmd+W is a close request like the close
                    // button's, and no chord is ever read as its bare
                    // letter. None on Linux, where nothing changes.
                    let chord = match &event {
                        WindowEvent::KeyboardInput { event: ke, .. } => platform::key_chord(ke, mods),
                        _ => None,
                    };
                    let event = match chord {
                        None => event,
                        Some(platform::Chord::CloseWindow) => WindowEvent::CloseRequested,
                        Some(platform::Chord::ToggleFullscreen) => {
                            if toggle_window_mode(&mut screens, idx) {
                                wm_settle_at = Some((Instant::now() + Duration::from_millis(600), 4));
                            }
                            return;
                        }
                        Some(platform::Chord::Swallow) => return,
                    };
                    match event {
                        WindowEvent::CloseRequested => {
                            // Closing the last window quits; closing one of
                            // several just takes that window down.
                            if screens.len() <= 1 {
                                if settings.retain_queue {
                                    save_queue(&queue);
                                }
                                shutdown(&mut screens);
                                shut_down = true;
                                target.exit();
                            } else {
                                if cursor_over == Some(window_id) {
                                    cursor_over = None;
                                }
                                close_screen(&mut screens, &mut budget, &mut overlay, idx);
                            }
                            // NB: `idx` is stale past this point — the
                            // screens Vec shifted. Nothing below the match
                            // uses it, so there is nothing to guard.
                        }
                        WindowEvent::Resized(size) => {
                            screens[idx].resize(size.width, size.height);
                        }
                        WindowEvent::ModifiersChanged(m) => {
                            mods = m.state();
                        }
                        WindowEvent::CursorMoved { position, .. } => {
                            let (x, y) = (position.x as f32, position.y as f32);
                            screens[idx].cursor = Some((x, y));
                            cursor_over = Some(window_id);
                            last_input = Instant::now();
                            if let Some(drag) = overlay.tile_drag {
                                // Track the pointer across screens: pos follows
                                // whichever window reports motion, in that
                                // window's coordinate space. With an X button
                                // grab that's still the source window — with
                                // coordinates running past its bounds, which is
                                // what the cross-screen drop detection reads.
                                overlay.tile_drag = Some(overlay::TileDrag {
                                    pos: (x, y),
                                    pos_window: window_id,
                                    ..drag
                                });
                            } else if left_held {
                                // A held left press past the drag threshold
                                // becomes a tile drag and cancels the pending
                                // plain click. The press must have started on
                                // a tile, not on an overlay widget.
                                if let Some(pc) = overlay.pending_click.filter(|pc| !pc.right) {
                                    let (dx, dy) = (x - pc.x, y - pc.y);
                                    if dx * dx + dy * dy >= DRAG_SLOP_PX * DRAG_SLOP_PX {
                                        let sidx = screens
                                            .iter()
                                            .position(|s| s.window.id() == pc.window_id)
                                            .filter(|&sidx| {
                                                screens[sidx].zoomed.is_none()
                                                    && !pos_on_ui(&screens[sidx], pc.x, pc.y)
                                            });
                                        if let Some(sidx) = sidx {
                                            if let Some(from) = screens[sidx].tile_at(pc.x, pc.y) {
                                                overlay.tile_drag = Some(overlay::TileDrag {
                                                    window_id: pc.window_id,
                                                    pos_window: pc.window_id,
                                                    from_tile: from,
                                                    pos: (x, y),
                                                });
                                                overlay.pending_click = None;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        WindowEvent::CursorLeft { .. } => {
                            if cursor_over == Some(window_id) {
                                cursor_over = None;
                            }
                        }
                        WindowEvent::HoveredFile(_) => {
                            // Say the window will take it, before the drop.
                            overlay.drop_target = Some(window_id);
                            last_input = Instant::now();
                        }
                        WindowEvent::HoveredFileCancelled => {
                            if overlay.drop_target == Some(window_id) {
                                overlay.drop_target = None;
                            }
                        }
                        WindowEvent::DroppedFile(path) => {
                            overlay.drop_target = None;
                            last_input = Instant::now();
                            match dropped.iter_mut().find(|(w, _)| *w == window_id) {
                                Some((_, paths)) => paths.push(path),
                                None => dropped.push((window_id, vec![path])),
                            }
                        }
                        WindowEvent::MouseInput { state, button, .. } => {
                            // Ctrl-click is a right-click on macOS (never
                            // on Linux). Its release goes with it even if
                            // ctrl comes up first: a left release would
                            // consume the parked right press before the
                            // frame got to it.
                            let button = match (state, button) {
                                (ElementState::Pressed, MouseButton::Left)
                                    if platform::is_secondary_click(button, mods) =>
                                {
                                    secondary_left = true;
                                    MouseButton::Right
                                }
                                (ElementState::Released, MouseButton::Left) if secondary_left => {
                                    secondary_left = false;
                                    MouseButton::Right
                                }
                                _ => button,
                            };
                            match (state, button) {
                            (ElementState::Pressed, MouseButton::Left) => {
                                left_held = true;
                                last_input = Instant::now();
                                if let Some((x, y)) = screens[idx].cursor {
                                    overlay.pending_click = Some(overlay::PendingClick {
                                        window_id,
                                        x,
                                        y,
                                        right: false,
                                    });
                                }
                            }
                            (ElementState::Pressed, MouseButton::Right) => {
                                last_input = Instant::now();
                                if let Some((x, y)) = screens[idx].cursor {
                                    overlay.pending_click = Some(overlay::PendingClick {
                                        window_id,
                                        x,
                                        y,
                                        right: true,
                                    });
                                }
                            }
                            (ElementState::Released, MouseButton::Left) => {
                                left_held = false;
                                last_input = Instant::now();
                                if let Some(drag) = overlay.tile_drag.take() {
                                    // Same screen: drop-to-swap. Over another
                                    // screen's window: move the tile there.
                                    finish_tile_drag(
                                        &mut screens,
                                        &mut budget,
                                        &mpv_lib,
                                        &tile_opts,
                                        &mut overlay,
                                        drag,
                                        args.tiles,
                                    );
                                } else if let Some(pc) =
                                    overlay.pending_click.take().filter(|pc| !pc.right)
                                {
                                    // Plain click: never crossed the drag
                                    // threshold. The extra UI hit-test covers
                                    // a press+release racing past the frame
                                    // verdict in a single event batch.
                                    if let Some(sidx) =
                                        screens.iter().position(|s| s.window.id() == pc.window_id)
                                    {
                                        if !pos_on_ui(&screens[sidx], pc.x, pc.y) {
                                            raw_tile_click(&mut screens, &mut budget, pc);
                                        }
                                    }
                                }
                            }
                            _ => {}
                            }
                        }
                        WindowEvent::MouseWheel { delta, .. } => {
                            last_input = Instant::now();
                            // Scroll over a tile = that tile's volume ±5.
                            // If the pointer is over an overlay widget (e.g.
                            // the queue strip), egui owns the wheel instead.
                            if let Some((x, y)) = screens[idx].cursor {
                                if !pos_on_ui(&screens[idx], x, y) {
                                    let step = match delta {
                                        MouseScrollDelta::LineDelta(_, l) if l > 0.0 => 5,
                                        MouseScrollDelta::LineDelta(_, l) if l < 0.0 => -5,
                                        MouseScrollDelta::PixelDelta(p) => {
                                            let dy = p.y / screens[idx].window.scale_factor();
                                            5 * pixel_wheel_notches(&mut wheel_accum, dy) as i64
                                        }
                                        _ => 0,
                                    };
                                    if step != 0 {
                                        // winit/X11 reports a MouseWheel for both the press
                                        // and the release of a scroll button — throttle the
                                        // duplicate half of each notch (smooth-scroll
                                        // deltas are notched by accumulation instead).
                                        let from_button = matches!(delta, MouseScrollDelta::LineDelta(..));
                                        let throttled = from_button
                                            && matches!(last_wheel_at, Some(at) if at.elapsed() < WHEEL_NOTCH_INTERVAL);
                                        if !throttled {
                                            if from_button {
                                                last_wheel_at = Some(Instant::now());
                                            }
                                            let hit = screens[idx].tile_at(x, y);
                                            let monitor_index = screens[idx].monitor_index;
                                            if let Some(tile) =
                                                screens[idx].tiles.iter_mut().find(|t| Some(t.id) == hit)
                                            {
                                                if tile.audio_enabled() {
                                                    tile.set_volume(tile.volume() + step);
                                                    println!("screen {monitor_index} tile {}: vol {}%", tile.id, tile.volume());
                                                } else {
                                                    overlay.notice = Some((
                                                        "audio is off — turn it on in the config tab (C)".into(),
                                                        Instant::now(),
                                                    ));
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        WindowEvent::KeyboardInput { event: key_event, .. } => {
                            last_input = Instant::now();
                            // While egui holds keyboard focus (typing in a
                            // text field, e.g. the settings source form),
                            // keys belong to the UI — the wall's hotkeys
                            // must not fire mid-typing.
                            // Auto-repeat is wanted only for the continuous
                            // keys (seek, volume). Repeating a toggle storms
                            // the wall — and a held key is not the only way to
                            // get repeats: anything that moves focus mid-press
                            // (L/Q focusing the browser) can swallow the key
                            // release, after which X repeats it indefinitely.
                            let repeatable = matches!(
                                key_event.logical_key,
                                Key::Named(NamedKey::ArrowLeft)
                                    | Key::Named(NamedKey::ArrowRight)
                                    | Key::Named(NamedKey::ArrowUp)
                                    | Key::Named(NamedKey::ArrowDown)
                            );
                            if key_event.state == ElementState::Released
                                && matches!(
                                    key_event.logical_key,
                                    Key::Named(NamedKey::ArrowLeft) | Key::Named(NamedKey::ArrowRight)
                                )
                            {
                                // Don't wait out SEEK_SETTLE once the key is
                                // already up — that delay would be felt.
                                flush_seek(&mut screens, &mut overlay, true);
                            }
                            if key_event.state == ElementState::Pressed
                                && (!key_event.repeat || repeatable)
                                && !screens[idx].egui.egui_ctx.wants_keyboard_input()
                            {
                                match key_event.logical_key {
                                    Key::Named(NamedKey::Escape) => {
                                        if overlay.tile_drag.is_some() || overlay.pending_click.is_some() {
                                            // A tile drag (or its press) in progress: cancel
                                            // it before touching zoom/quit.
                                            overlay.tile_drag = None;
                                            overlay.pending_click = None;
                                        } else if screens[idx].zoomed.is_some() {
                                            // Leave fullscreen first; Esc only quits
                                            // when nothing is zoomed.
                                            toggle_fullscreen(&mut screens, idx, None);
                                        } else {
                                            if settings.retain_queue {
                                                save_queue(&queue);
                                            }
                                            shutdown(&mut screens);
                                            shut_down = true;
                                            target.exit();
                                        }
                                    }
                                    Key::Named(NamedKey::Space) => {
                                        let ids: Vec<usize> = screens[idx].tiles.iter().map(|t| t.id).collect();
                                        let any_playing = screens[idx].tiles.iter().any(|t| !t.is_paused());
                                        if any_playing {
                                            for id in ids {
                                                pause_and_release(&mut screens, &mut budget, id, "user pause");
                                            }
                                        } else {
                                            for id in ids {
                                                resume_tile(&mut screens, &mut budget, id);
                                            }
                                        }
                                    }
                                    Key::Named(NamedKey::ArrowUp) => {
                                        screens[idx].adjust_volume(5);
                                    }
                                    Key::Named(NamedKey::ArrowDown) => {
                                        screens[idx].adjust_volume(-5);
                                    }
                                    Key::Named(NamedKey::ArrowLeft) => {
                                        queue_seek(&mut screens, idx, &mut overlay, -10.0);
                                    }
                                    Key::Named(NamedKey::ArrowRight) => {
                                        queue_seek(&mut screens, idx, &mut overlay, 10.0);
                                    }
                                    Key::Character(ref c) if c.eq_ignore_ascii_case("w") => {
                                        // The window under the cursor, else the focused one.
                                        let target = cursor_over
                                            .and_then(|w| screens.iter().position(|s| s.window.id() == w))
                                            .unwrap_or(idx);
                                        if toggle_window_mode(&mut screens, target) {
                                            wm_settle_at = Some((Instant::now() + Duration::from_millis(600), 4));
                                        }
                                    }
                                    Key::Character(ref c) if !mods.shift_key() && c.eq_ignore_ascii_case("f") => {
                                        if screens[idx].zoomed.is_some() {
                                            toggle_fullscreen(&mut screens, idx, None);
                                        } else if let Some((x, y)) = screens[idx].cursor {
                                            if let Some(t) = screens[idx].tile_at(x, y) {
                                                toggle_fullscreen(&mut screens, idx, Some(t));
                                            } else {
                                                println!("F: no tile under the cursor");
                                            }
                                        }
                                    }
                                    Key::Character(ref c) if c == "?" || c == "/" => {
                                        // On the focused screen only.
                                        let win = screens[idx].window.id();
                                        overlay.help_open =
                                            if overlay.help_open == Some(win) { None } else { Some(win) };
                                    }
                                    Key::Character(ref c) if browser_tab_key(c).is_some() => {
                                        // L/Q/S/C all reach the same browser
                                        // window; they differ only in which
                                        // tab it lands on. Everything global
                                        // lives there, so there is exactly
                                        // one copy of each of these controls
                                        // no matter how many screens are up.
                                        // Focus-or-create is handled where
                                        // the action is drained, since making
                                        // a window needs the event loop.
                                        let tab = browser_tab_key(c).expect("guarded above");
                                        overlay.actions.push(OverlayAction::NewLibraryWindow { tab });
                                    }
                                    Key::Character(ref c) if c == "-" || c == "_" => {
                                        // Remove the tile under the cursor on
                                        // the focused screen.
                                        if let Some((x, y)) = screens[idx].cursor {
                                            if let Some(tile_id) = screens[idx].tile_at(x, y) {
                                                remove_tile_action(&mut screens, &mut budget, &mut overlay, tile_id);
                                            }
                                        }
                                    }
                                    Key::Character(ref c) if c == "+" || c == "=" => {
                                        // Add a tile to the focused screen.
                                        let mut wall = Wall {
                                screens: &mut screens,
                                budget: &mut budget,
                                queue: &mut queue,
                                sources: &sources,
                                overlay: &mut overlay,
                                mpv_lib: &mpv_lib,
                                tile_opts: &tile_opts,
                                next_tile_id: &mut next_tile_id,
                                max_tiles: args.tiles,
                                wm_settle_at: &mut wm_settle_at,
                            };
                                        add_tile_action(&mut wall, idx);
                                    }
                                    Key::Character(ref c) if c.eq_ignore_ascii_case("n") => {
                                        let ids: Vec<usize> = screens[idx].tiles.iter().map(|t| t.id).collect();
                                        for id in ids {
                                            tile_next(&mut screens, &mut queue, id, true);
                                        }
                                    }
                                    Key::Character(ref c) if c == "p" => {
                                        // Pin/unpin the tile under the cursor
                                        // on the focused screen: it takes most
                                        // of the screen, the rest share a
                                        // bottom strip.
                                        if let Some((x, y)) = screens[idx].cursor {
                                            if let Some(tile_id) = screens[idx].tile_at(x, y) {
                                                toggle_pin(&mut screens, idx, tile_id);
                                            }
                                        }
                                    }
                                    Key::Character(ref c) if c == "P" => {
                                        let ids: Vec<usize> = screens[idx].tiles.iter().map(|t| t.id).collect();
                                        for id in ids {
                                            tile_next(&mut screens, &mut queue, id, false);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Event::LoopExiting if !shut_down => {
                    // Quit by the OS rather than by us — macOS's Cmd+Q
                    // menu item terminates the app from under the event
                    // loop. Same teardown as Esc, or the queue since the
                    // last timed save is lost.
                    if settings.retain_queue {
                        save_queue(&queue);
                    }
                    shutdown(&mut screens);
                    shut_down = true;
                }
                _ => {}
            }
        })
        .unwrap();

    println!("bye (uptime {:.1}s)", start.elapsed().as_secs_f64());
}

/// Fills each screen with its startup tiles from the queue, up to
/// `populate_limit` per screen.
#[allow(clippy::too_many_arguments)]
fn populate_startup(
    screens: &mut [Screen],
    budget: &mut DecodeBudget,
    queue: &mut PlaybackQueue,
    mpv_lib: &Arc<tessera_mpv::MpvLib>,
    tile_opts: &TileOptions,
    next_tile_id: &mut usize,
    populate_limit: usize,
) {
    for screen_idx in 0..screens.len() {
        let target_tiles = queue.len().clamp(1, populate_limit);
        let monitor_index = screens[screen_idx].monitor_index;
        let mut created = 0;
        while created < target_tiles {
            let item = match queue.current() {
                Some(m) => m.clone(),
                None => break,
            };
            match request_admission(screens, budget, *next_tile_id, &tessera_core::StreamSpec::placeholder()) {
                Admission::Admitted | Admission::AdmittedEvicting { .. } => {}
                Admission::RejectedBudget { would_use, ceiling } => {
                    println!(
                        "screen {monitor_index}: decode budget full, stopping at {created} tiles ({:.1}/{:.0} streams)",
                        would_use / tessera_core::budget::MACROBLOCKS_1080P30,
                        ceiling / tessera_core::budget::MACROBLOCKS_1080P30
                    );
                    break;
                }
                Admission::RejectedMaxStreams { max } => {
                    println!("screen {monitor_index}: max streams ({max}) reached, stopping at {created} tiles");
                    break;
                }
            }
            screens[screen_idx].context.make_current(&screens[screen_idx].surface).ok();
            match Tile::new(mpv_lib, *next_tile_id, &item, tile_opts, None) {
                Ok(tile) => {
                    screens[screen_idx].add_tile(tile);
                    *next_tile_id += 1;
                    created += 1;
                    queue.advance();
                }
                Err(e) => {
                    budget.release(*next_tile_id);
                    eprintln!("screen {monitor_index}: tile creation failed: {e}");
                    break;
                }
            }
        }
        if created == 0 && !queue.is_empty() {
            eprintln!("warning: screen {monitor_index} has items but no tiles");
        }
    }

    println!(
        "running: {} screen(s), {} tile(s), budget {:.0}%",
        screens.len(),
        screens.iter().map(|s| s.tiles.len()).sum::<usize>(),
        budget.utilization() * 100.0
    );
}

/// Lists paths handed to us to play — the command line, or a Finder open
/// — as the sources they become and the items they yield. A file is a
/// one-item folder source, exactly as on the command line.
fn list_paths(paths: &[PathBuf]) -> (Vec<tessera_core::sources::Source>, Vec<tessera_core::MediaItem>) {
    let mut sources = Vec::new();
    let mut items = Vec::new();
    for p in paths {
        let src = tessera_core::sources::Source::Folder { path: p.display().to_string() };
        match src.list() {
            Ok(list) => {
                println!("source {}: {} item(s)", src.label(), list.len());
                items.extend(list);
            }
            Err(e) => eprintln!("warning: source {} failed: {e}", src.label()),
        }
        if !sources.contains(&src) {
            sources.push(src);
        }
    }
    (sources, items)
}

/// Plays files the OS opened while the wall was running ("Open With" on a
/// running Tessera): the first plays now on the first wall window with a
/// tile — on a new tile, if no wall window has one — and all of them join
/// the end of the queue. Drag-and-drop goes through `open_handed_off`.
fn play_opened(wall: &mut Wall, items: Vec<tessera_core::MediaItem>) {
    let Some(first) = items.first().cloned() else {
        notify(wall.overlay, "nothing playable in what was opened".into());
        return;
    };
    let n = items.len();
    wall.queue.append_items(items);
    println!("opened: {n} item(s) added to the queue ({} total)", wall.queue.len());
    let with_tile = wall.screens.iter().position(|s| s.role == ScreenRole::Wall && !s.tiles.is_empty());
    let any_wall = wall.screens.iter().position(|s| s.role == ScreenRole::Wall);
    let Some(idx) = with_tile.or(any_wall) else {
        notify(wall.overlay, "no wall window to play on — open one with + first".into());
        return;
    };
    if wall.screens[idx].tiles.is_empty() {
        if !can_add_tile(wall, idx) {
            return;
        }
        spawn_tile(wall, idx, first);
    } else {
        handle_overlay_action(wall, OverlayAction::PlayNow { item: first }, idx);
    }
    wall.screens[idx].window.focus_window();
}

/// The smooth-scroll half of the wheel: accumulates pixel travel and hands
/// back whole notches, keeping the remainder for the next event. A reversal
/// starts from zero, so a gesture never has to unwind what the previous
/// one left over.
fn pixel_wheel_notches(accum: &mut f64, dy: f64) -> i32 {
    if dy == 0.0 {
        return 0;
    }
    if *accum != 0.0 && accum.signum() != dy.signum() {
        *accum = 0.0;
    }
    *accum += dy;
    let notches = (*accum / WHEEL_PIXELS_PER_NOTCH).trunc();
    *accum -= notches * WHEEL_PIXELS_PER_NOTCH;
    notches as i32
}

/// Brings the OS's app-wide fullscreen chrome (macOS menu bar and Dock) in
/// line with whether any window is currently a fullscreen wall.
fn sync_fullscreen_chrome(screens: &[Screen]) {
    platform::sync_fullscreen_chrome(screens.iter().any(|s| platform::is_fullscreen(&s.window)));
}


/// The persisted queue. Kept out of settings.conf: it is a few hundred
/// items of churn, and settings.conf is hand-editable.
fn queue_path() -> std::path::PathBuf {
    config_dir().join("queue.json")
}

/// Restores the saved queue, or None if there isn't one (or it is
/// unreadable — a corrupt queue file is not worth refusing to start over).
fn load_queue() -> Option<(Vec<tessera_core::MediaItem>, usize)> {
    let text = std::fs::read_to_string(queue_path()).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| eprintln!("warning: queue.json unreadable ({e}) — starting empty"))
        .ok()?;
    let items: Vec<tessera_core::MediaItem> =
        serde_json::from_value(v.get("items")?.clone()).ok()?;
    let cursor = v.get("cursor").and_then(serde_json::Value::as_u64).unwrap_or(0) as usize;
    (!items.is_empty()).then_some((items, cursor))
}

/// Writes the queue out. Silent on success; a failure here must never take
/// the wall down, so it only warns.
fn save_queue(queue: &PlaybackQueue) {
    let dir = config_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let (items, cursor) = queue.snapshot();
    let body = serde_json::json!({ "cursor": cursor, "items": items });
    match serde_json::to_string(&body) {
        Ok(text) => {
            // Private: every remote item's uri carries its server token.
            if let Err(e) = write_private(&queue_path(), &text) {
                eprintln!("warning: could not save the queue: {e}");
            }
        }
        Err(e) => eprintln!("warning: could not serialise the queue: {e}"),
    }
}

/// Writes a file only its owner can read.
///
/// Both files this is used for carry live server tokens: settings.conf in
/// its `sources = [...]` JSON, and queue.json inside every item's `uri` and
/// `thumb_url` (a Plex/Jellyfin direct-play URL *is* `...?api_key=...`).
/// The default umask leaves them 0644, which on any shared machine hands
/// every local account a working key. Proper secret storage is a bigger
/// job — docs/ROADMAP.md §9 — but the mode costs nothing.
///
/// `mode()` only applies when the file is created, so an existing file
/// written before this change is tightened explicitly.
fn write_private(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    file.write_all(text.as_bytes())
}

/// `~/.config/opentessera`, where settings, source credentials and the
/// plex.tv device id live.
fn config_dir() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".config/opentessera"))
        .unwrap_or_default()
}

fn load_settings() -> overlay::UiSettings {
    let mut s = overlay::UiSettings::default();
    let path = std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".config/opentessera/settings.conf"))
        .unwrap_or_default();
    if let Ok(text) = std::fs::read_to_string(&path) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                let (k, v) = (k.trim(), v.trim());
                match k {
                    "ui_scale" => {
                        if let Ok(f) = v.parse::<f32>() {
                            s.ui_scale = f.clamp(0.75, 3.0);
                        }
                    }
                    "cache_mb" => {
                        if let Ok(n) = v.parse::<u32>() {
                            s.cache_mb = n.clamp(64, 8192);
                        }
                    }
                    "max_streams" => {
                        if let Ok(n) = v.parse::<u32>() {
                            s.max_streams = n.clamp(1, 128);
                        }
                    }
                    "monitor_scale" => {
                        match serde_json::from_str::<std::collections::BTreeMap<String, f32>>(v) {
                            Ok(map) => {
                                s.monitor_scale =
                                    map.into_iter().map(|(k, v)| (k, v.clamp(0.75, 3.0))).collect()
                            }
                            Err(e) => {
                                eprintln!("warning: bad monitor_scale line in settings.conf: {e}")
                            }
                        }
                    }
                    "audio" => s.audio = v == "true" || v == "yes" || v == "on",
                    "retain_queue" => s.retain_queue = v == "true" || v == "yes" || v == "on",
                    "sources" => match serde_json::from_str::<Vec<tessera_core::sources::Source>>(v) {
                        Ok(list) => s.sources = list,
                        Err(e) => eprintln!("warning: bad sources line in settings.conf: {e}"),
                    },
                    _ => {}
                }
            }
        }
    }
    s
}

fn save_settings(s: &overlay::UiSettings) {
    let Some(home) = std::env::var_os("HOME") else { return };
    let dir = std::path::PathBuf::from(home).join(".config/opentessera");
    if std::fs::create_dir_all(&dir).is_err() {
        eprintln!("warning: cannot create {}", dir.display());
        return;
    }
    let sources_json = serde_json::to_string(&s.sources).unwrap_or_else(|_| "[]".into());
    let scales_json = serde_json::to_string(&s.monitor_scale).unwrap_or_else(|_| "{}".into());
    let text = format!(
        "# openTessera settings (also editable at runtime: S sources, C config)\n\
         # ui_scale is the fallback; monitor_scale remembers each display by name.\n\
         ui_scale = {}\nmonitor_scale = {}\ncache_mb = {}\nmax_streams = {}\naudio = {}\nretain_queue = {}\nsources = {}\n",
        s.ui_scale, scales_json, s.cache_mb, s.max_streams, s.audio, s.retain_queue, sources_json
    );
    // Private: the sources line holds Plex tokens and Jellyfin API keys.
    let path = dir.join("settings.conf");
    if let Err(e) = write_private(&path, &text) {
        eprintln!("warning: could not save settings: {e}");
        return;
    }
    println!("settings: saved to {}", path.display());
}

fn dump_dir() -> Option<std::path::PathBuf> {
    let dir = std::path::PathBuf::from(std::env::var("TESSERA_DUMP_DIR").ok()?);
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// True when an egui layer (any painted widget) owns the given window
/// position — the same verdict the deferred click machinery applies inside
/// the egui pass. Used outside the pass only for the rare press+release
/// that lands inside a single event batch, before the frame verdict ran.
fn pos_on_ui(screen: &Screen, x: f32, y: f32) -> bool {
    let ctx = &screen.egui.egui_ctx;
    let ppp = ctx.pixels_per_point();
    matches!(
        ctx.layer_id_at(egui::pos2(x / ppp, y / ppp)),
        Some(layer) if layer.order != egui::Order::Background
    )
}

/// Executes a raw (non-UI) left click on the video wall: pause/resume the
/// tile under the press position, budget-aware.
fn raw_tile_click(screens: &mut [Screen], budget: &mut DecodeBudget, pc: overlay::PendingClick) {
    let Some(idx) = screens.iter().position(|s| s.window.id() == pc.window_id) else {
        return;
    };
    if let Some(id) = screens[idx].tile_at(pc.x, pc.y) {
        let playing = screens
            .iter()
            .any(|s| s.tiles.iter().any(|t| t.id == id && !t.is_paused()));
        if playing {
            pause_and_release(screens, budget, id, "user pause");
        } else {
            resume_tile(screens, budget, id);
        }
    }
}

/// Pins/unpins a tile: the pinned tile takes most of the screen while the
/// others share a bottom strip (the P key and the transport pin button).
fn toggle_pin(screens: &mut [Screen], screen_idx: usize, tile_id: usize) {
    let monitor_index = screens[screen_idx].monitor_index;
    if screens[screen_idx].pinned == Some(tile_id) {
        screens[screen_idx].pinned = None;
        println!("screen {monitor_index}: unpinned tile {tile_id}");
    } else {
        screens[screen_idx].pinned = Some(tile_id);
        println!("screen {monitor_index}: pinned tile {tile_id}");
    }
}

/// Shows a transient toast (and logs) the given message.
/// The UI scale a window on `monitor` should start at. `--ui-scale` wins
/// for the whole run without being persisted — it used to be written into
/// the settings, so a single debugging launch overwrote whatever scale the
/// user had set. Otherwise: this monitor's remembered scale, else the
/// fallback.
fn seed_scale(
    settings: &overlay::UiSettings,
    monitor: Option<&winit::monitor::MonitorHandle>,
    cli: Option<f32>,
) -> f32 {
    let name = monitor.and_then(|m| m.name());
    scale_for(settings, name.as_deref(), cli)
}

/// The decision itself, split out from the `MonitorHandle` so it can be
/// tested without a display.
fn scale_for(settings: &overlay::UiSettings, monitor: Option<&str>, cli: Option<f32>) -> f32 {
    if let Some(s) = cli {
        return s.clamp(0.75, 3.0);
    }
    monitor
        .and_then(|n| settings.monitor_scale.get(n).copied())
        .unwrap_or(settings.ui_scale)
}

/// Which browser tab a keypress asks for, if any. L/Q/S/C are one family:
/// every app-wide control lives in the browser window, so these are the
/// four ways in rather than four separate features.
fn browser_tab_key(c: &str) -> Option<browser::Tab> {
    match c.to_ascii_lowercase().as_str() {
        "l" => Some(browser::Tab::Library),
        "q" => Some(browser::Tab::Queue),
        "s" => Some(browser::Tab::Sources),
        "c" => Some(browser::Tab::Config),
        _ => None,
    }
}

fn notify(overlay: &mut overlay::OverlayState, msg: String) {
    println!("{msg}");
    overlay.notice = Some((msg, Instant::now()));
}

/// Inner window rect in global screen coordinates (x0, y0, x1, y1).
fn window_rect(s: &Screen) -> Option<(i32, i32, i32, i32)> {
    let p = s.window.inner_position().ok()?;
    let sz = s.window.inner_size();
    Some((p.x, p.y, p.x + sz.width as i32, p.y + sz.height as i32))
}

/// Drop handler for a tile drag. Released over another screen's window:
/// move the tile there. Released over its own screen: swap the dragged
/// tile with the tile under the pointer. Dropping on empty space, outside
/// the grid, or back on the source cell is a no-op.
fn finish_tile_drag(
    screens: &mut [Screen],
    budget: &mut DecodeBudget,
    mpv_lib: &std::sync::Arc<tessera_mpv::MpvLib>,
    tile_opts: &TileOptions,
    overlay: &mut overlay::OverlayState,
    drag: overlay::TileDrag,
    max_tiles: usize,
) {
    let Some(src) = screens.iter().position(|s| s.window.id() == drag.window_id) else {
        return;
    };
    // Global pointer position: drag.pos lives in pos_window's coordinate
    // space (physical px, possibly past that window's bounds while a
    // button grab routes motion to the drag source).
    let global = screens
        .iter()
        .position(|s| s.window.id() == drag.pos_window)
        .and_then(|i| screens[i].window.inner_position().ok())
        .map(|p| (p.x as f32 + drag.pos.0, p.y as f32 + drag.pos.1));
    let target = global.and_then(|(gx, gy)| {
        screens.iter().position(|s| {
            window_rect(s)
                .is_some_and(|(x0, y0, x1, y1)| gx >= x0 as f32 && gx < x1 as f32 && gy >= y0 as f32 && gy < y1 as f32)
        })
    });
    if let Some(dst) = target.filter(|&dst| dst != src) {
        move_tile_cross_screen(screens, budget, mpv_lib, tile_opts, overlay, src, dst, drag.from_tile, max_tiles);
        return;
    }
    // Same-screen drop: pos must be in the source window's coordinates
    // (a foreign pos_window means the pointer is over another window).
    if drag.pos_window != drag.window_id {
        return;
    }
    let Some(to) = screens[src].tile_at(drag.pos.0, drag.pos.1) else {
        return;
    };
    if to == drag.from_tile {
        return;
    }
    let monitor_index = screens[src].monitor_index;
    // Dropping a strip tile onto the pinned cell features the dragged tile
    // instead of swapping Vec positions (the big cell follows the pin id).
    if screens[src].pinned == Some(to) {
        toggle_pin(screens, src, drag.from_tile);
        return;
    }
    if screens[src].swap_tiles(drag.from_tile, to) {
        println!("screen {monitor_index}: swapped tiles {} and {}", drag.from_tile, to);
    }
}

/// Cross-screen drag drop (Phase 2 of #1): move a tile to another screen.
/// A tile's mpv render context is bound to its screen's GL context, so the
/// instance cannot survive the move — the tile is snapshotted (item,
/// position, paused, volume, mute, track picks), destroyed on the source
/// screen and rebuilt on the target, which appends it and re-flows its
/// grid. Only the playing item moves; the per-screen queues stay put.
#[allow(clippy::too_many_arguments)]
fn move_tile_cross_screen(
    screens: &mut [Screen],
    budget: &mut DecodeBudget,
    mpv_lib: &std::sync::Arc<tessera_mpv::MpvLib>,
    tile_opts: &TileOptions,
    overlay: &mut overlay::OverlayState,
    src: usize,
    dst: usize,
    tile_id: usize,
    max_tiles: usize,
) {
    let src_monitor = screens[src].monitor_index;
    let dst_monitor = screens[dst].monitor_index;
    if screens[dst].zoomed.is_some() {
        notify(overlay, "not moving: target screen is zoomed (F)".into());
        return;
    }
    if screens[dst].tiles.len() >= max_tiles {
        notify(overlay, format!("not moving: target screen is at the {max_tiles}-tile limit"));
        return;
    }
    let (item, mut resume) = {
        let Some(tile) = screens[src].tiles.iter_mut().find(|t| t.id == tile_id) else {
            return;
        };
        (tile.item.clone(), tile.snapshot_resume())
    };
    // The pin doesn't travel with a cross-screen move (the target screen
    // didn't ask for it); clear it so the layout doesn't reference a
    // missing tile.
    if screens[src].pinned == Some(tile_id) {
        screens[src].pinned = None;
    }
    if !resume.paused {
        // The rebuilt instance needs the slot the old one holds.
        budget.release(tile_id);
        let rejected = match request_admission(screens, budget, tile_id, &tessera_core::StreamSpec::placeholder()) {
            Admission::Admitted | Admission::AdmittedEvicting { .. } => None,
            Admission::RejectedBudget { would_use, ceiling } => {
                Some(format!(
                    "decode budget full ({:.1}/{:.0} streams)",
                    would_use / tessera_core::budget::MACROBLOCKS_1080P30,
                    ceiling / tessera_core::budget::MACROBLOCKS_1080P30
                ))
            }
            Admission::RejectedMaxStreams { max } => Some(format!("max concurrent streams ({max})")),
        };
        if let Some(reason) = rejected {
            // Move anyway, but paused: the source instance already gave up
            // its budget slot.
            pause_and_release(screens, budget, tile_id, &reason);
            resume.paused = true;
        }
    }
    if !screens[src].remove_tile(tile_id) {
        return;
    }
    // Tile creation happens on the target screen's GL context.
    screens[dst].context.make_current(&screens[dst].surface).ok();
    match Tile::new(mpv_lib, tile_id, &item, tile_opts, Some(&resume)) {
        Ok(tile) => {
            screens[dst].add_tile(tile);
            println!(
                "screen {src_monitor}: moved tile {tile_id} \"{}\" to screen {dst_monitor} (resume at {:.1}s{})",
                item.title,
                resume.start,
                if resume.paused { ", paused" } else { "" }
            );
            println!(
                "screen {dst_monitor}: tile {tile_id} added ({} tiles)",
                screens[dst].tiles.len()
            );
        }
        Err(e) => {
            budget.release(tile_id);
            notify(overlay, format!("not moving: tile creation failed: {e}"));
        }
    }
}

/// Pauses a tile (wherever it lives) and releases its budget slot. The
/// invariant: budget.active == the set of playing tiles.
fn pause_and_release(screens: &mut [Screen], budget: &mut DecodeBudget, id: usize, reason: &str) {
    budget.release(id);
    for screen in screens.iter_mut() {
        if screen.pause_tile(id) {
            println!("screen {} tile {id}: paused ({reason})", screen.monitor_index);
            return;
        }
    }
}

/// Requests admission, evicting (pausing) older streams as needed until the
/// request fits or only the requesting stream would remain. Mirrors the Quest
/// retry loop: the oldest stream is paused to maintain the stream count.
fn request_admission(
    screens: &mut [Screen],
    budget: &mut DecodeBudget,
    id: usize,
    spec: &tessera_core::StreamSpec,
) -> Admission {
    loop {
        match budget.request(id, spec) {
            Admission::Admitted => return Admission::Admitted,
            Admission::AdmittedEvicting { evicted } => {
                pause_and_release(screens, budget, evicted, "evicted: oldest stream");
                return Admission::AdmittedEvicting { evicted };
            }
            rejected @ (Admission::RejectedBudget { .. } | Admission::RejectedMaxStreams { .. }) => {
                // The single-stream rule guarantees admission once everything
                // else is evicted; stop short of evicting the last stream
                // only when the request itself is the lone candidate.
                if budget.active_count() > 1 {
                    if let Some(victim) = budget.evict_oldest() {
                        pause_and_release(screens, budget, victim, "evicted: oldest stream");
                        continue;
                    }
                }
                return rejected;
            }
        }
    }
}

/// Resumes a paused tile, securing budget first (evicting the oldest playing
/// stream if necessary). Prints the outcome.
fn resume_tile(screens: &mut [Screen], budget: &mut DecodeBudget, id: usize) {
    let spec = budget.spec(id).cloned().unwrap_or_else(tessera_core::StreamSpec::placeholder);
    match request_admission(screens, budget, id, &spec) {
        Admission::Admitted | Admission::AdmittedEvicting { .. } => {
            for screen in screens.iter_mut() {
                if screen.resume_tile(id) {
                    println!("screen {} tile {id}: playing", screen.monitor_index);
                    return;
                }
            }
        }
        Admission::RejectedBudget { would_use, ceiling } => {
            println!(
                "tile {id}: staying paused — budget {:.1}/{:.0} streams",
                would_use / tessera_core::budget::MACROBLOCKS_1080P30,
                ceiling / tessera_core::budget::MACROBLOCKS_1080P30
            );
        }
        Admission::RejectedMaxStreams { max } => {
            println!("tile {id}: staying paused — max streams ({max})");
        }
    }
}

/// Everything a wall action mutates, in one place.
///
/// Nearly every action needs most of it: pause/resume goes through the
/// global [`DecodeBudget`], adding a tile pulls from the shared
/// [`PlaybackQueue`] and needs the mpv library plus the tile options, and
/// anything that fails reports through the [`OverlayState`]. Passing them
/// individually had `handle_overlay_action` at eleven parameters.
///
/// Borrowed, not owned — build one for as long as the actions run, then let
/// it go so the frame's other code can use the same state.
struct Wall<'a> {
    screens: &'a mut Vec<Screen>,
    budget: &'a mut DecodeBudget,
    /// The one queue every window's tiles are fed from.
    queue: &'a mut PlaybackQueue,
    /// The configured sources, so a delete can be routed to the server the
    /// item actually came from.
    sources: &'a [tessera_core::sources::Source],
    overlay: &'a mut OverlayState,
    mpv_lib: &'a std::sync::Arc<tessera_mpv::MpvLib>,
    tile_opts: &'a TileOptions,
    next_tile_id: &'a mut usize,
    /// The --tiles cap, per window.
    max_tiles: usize,
    /// Deferred WM re-anchor check; armed by anything that moves a window.
    wm_settle_at: &'a mut Option<(Instant, u32)>,
}

/// Adds a tile to a screen from its queue — the transport "+" button and
/// the "+" hotkey. Enforces the zoomed and --tiles limits.
fn add_tile_action(wall: &mut Wall, screen_idx: usize) {
    if !can_add_tile(wall, screen_idx) {
        return;
    }
    // Peek at what the next advance would yield rather than taking it: a
    // rejected add used to consume a queue position anyway, so pressing "+"
    // against a full budget silently skipped an item. The cursor moves only
    // once the tile actually exists, below.
    let item = match wall.queue.upcoming(1).first().map(|m| (*m).clone()) {
        Some(m) => m,
        None => {
            notify(wall.overlay, "not adding: queue is empty".into());
            return;
        }
    };
    spawn_tile(wall, screen_idx, item);
}

/// Whether this screen can take another tile, explaining (as a toast) why
/// not: browser windows have none, a zoomed wall is showing one, and
/// --tiles caps the rest.
fn can_add_tile(wall: &mut Wall, screen_idx: usize) -> bool {
    if wall.screens[screen_idx].role == ScreenRole::Browser {
        notify(wall.overlay, "this is the browser window — open a wall window for tiles".into());
        return false;
    }
    if wall.screens[screen_idx].zoomed.is_some() {
        notify(wall.overlay, "exit fullscreen (F) before adding tiles".into());
        return false;
    }
    if wall.screens[screen_idx].tiles.len() >= wall.max_tiles {
        notify(wall.overlay, format!("not adding: already at the {}-tile limit", wall.max_tiles));
        return false;
    }
    true
}

/// Starts a tile playing `item` on a screen, through the decode budget.
fn spawn_tile(wall: &mut Wall, screen_idx: usize, item: tessera_core::MediaItem) {
    let monitor_index = wall.screens[screen_idx].monitor_index;
    let id = *wall.next_tile_id;
    match request_admission(wall.screens, wall.budget, id, &tessera_core::StreamSpec::placeholder()) {
        Admission::Admitted | Admission::AdmittedEvicting { .. } => {
            wall.screens[screen_idx].context.make_current(&wall.screens[screen_idx].surface).ok();
            match Tile::new(wall.mpv_lib, id, &item, wall.tile_opts, None) {
                Ok(tile) => {
                    wall.screens[screen_idx].add_tile(tile);
                    *wall.next_tile_id += 1;
                    // Only now: the item is really playing somewhere.
                    wall.queue.advance();
                    println!(
                        "screen {monitor_index}: added tile {id} \"{}\" ({} tiles)",
                        item.title,
                        wall.screens[screen_idx].tiles.len()
                    );
                }
                Err(e) => {
                    wall.budget.release(id);
                    notify(wall.overlay, format!("not adding: tile creation failed: {e}"));
                }
            }
        }
        Admission::RejectedBudget { would_use, ceiling } => {
            notify(
                wall.overlay,
                format!(
                    "not adding: decode budget full ({:.1}/{:.0} streams) — raise it in config (C)",
                    would_use / tessera_core::budget::MACROBLOCKS_1080P30,
                    ceiling / tessera_core::budget::MACROBLOCKS_1080P30
                ),
            );
        }
        Admission::RejectedMaxStreams { max } => {
            notify(wall.overlay, format!("not adding: max concurrent streams ({max}) — raise it in config (C)"));
        }
    }
}

/// Removes a tile by id, wherever it lives — the transport "−" button and
/// the "−" hotkey. Frees its budget slot and exits zoom if it was zoomed.
fn remove_tile_action(
    screens: &mut [Screen],
    budget: &mut DecodeBudget,
    overlay: &mut overlay::OverlayState,
    tile_id: usize,
) {
    if overlay.track_panel.map(|(id, _)| id) == Some(tile_id) {
        overlay.track_panel = None;
    }
    if overlay.solo_id == Some(tile_id) {
        overlay.solo_id = None;
    }
    for si in 0..screens.len() {
        if screens[si].tiles.iter().any(|t| t.id == tile_id) {
            let monitor_index = screens[si].monitor_index;
            let was_zoomed = screens[si].zoomed == Some(tile_id);
            if screens[si].pinned == Some(tile_id) {
                screens[si].pinned = None;
            }
            budget.release(tile_id);
            if screens[si].remove_tile(tile_id) {
                println!(
                    "screen {monitor_index}: removed tile {tile_id} ({} left)",
                    screens[si].tiles.len()
                );
            }
            if was_zoomed {
                toggle_fullscreen(screens, si, None);
            }
            break;
        }
    }
}

/// Executes one queued overlay interaction. `screen_idx` is the screen whose
/// overlay produced the action.
fn handle_overlay_action(wall: &mut Wall, action: OverlayAction, screen_idx: usize) {
    match action {
        OverlayAction::TogglePause { tile_id } => {
            let playing = wall.screens
                .iter()
                .any(|s| s.tiles.iter().any(|t| t.id == tile_id && !t.is_paused()));
            if playing {
                pause_and_release(wall.screens, wall.budget, tile_id, "user pause");
            } else {
                resume_tile(wall.screens, wall.budget, tile_id);
            }
        }
        OverlayAction::MuteButton { tile_id } => {
            wall.overlay.solo_id = None;
            for screen in wall.screens.iter_mut() {
                if let Some(tile) = screen.tiles.iter_mut().find(|t| t.id == tile_id) {
                    if tile.audio_enabled() {
                        let muted = tile.toggle_mute();
                        println!(
                            "screen {} tile {tile_id}: {}",
                            screen.monitor_index,
                            if muted { "muted".to_string() } else { format!("audio on (vol {}%)", tile.volume()) }
                        );
                    } else {
                        notify(wall.overlay, "audio is off — turn it on in the config tab (C)".into());
                    }
                    return;
                }
            }
        }
        OverlayAction::Solo { tile_id } => {
            // Audio focus: unmute this tile, mute every other stream on
            // every screen.
            let mut target_ok = false;
            let mut muted_others = 0usize;
            for screen in wall.screens.iter_mut() {
                for tile in screen.tiles.iter_mut() {
                    if !tile.audio_enabled() {
                        continue;
                    }
                    if tile.id == tile_id {
                        target_ok = true;
                        tile.set_muted(false);
                        println!(
                            "screen {} tile {tile_id}: solo — audio on (vol {}%)",
                            screen.monitor_index,
                            tile.volume()
                        );
                    } else if !tile.is_muted() {
                        tile.set_muted(true);
                        muted_others += 1;
                    }
                }
            }
            if !target_ok {
                notify(wall.overlay, "audio is off — turn it on in the config tab (C)".into());
            } else {
                if muted_others > 0 {
                    println!("solo: muted {muted_others} other stream(s)");
                }
                wall.overlay.solo_id = Some(tile_id);
            }
        }
        OverlayAction::GoLive { tile_id } => {
            for screen in wall.screens.iter_mut() {
                if let Some(tile) = screen.tiles.iter_mut().find(|t| t.id == tile_id) {
                    tile.go_live();
                    println!("screen {} tile {tile_id}: rejoined the live stream", screen.monitor_index);
                    return;
                }
            }
        }
        OverlayAction::TogglePin { tile_id } => {
            if let Some(si) = wall.screens.iter().position(|s| s.tiles.iter().any(|t| t.id == tile_id)) {
                toggle_pin(wall.screens, si, tile_id);
            }
        }
        OverlayAction::SetVolume { tile_id, volume } => {
            for screen in wall.screens.iter_mut() {
                if let Some(tile) = screen.tiles.iter_mut().find(|t| t.id == tile_id) {
                    tile.set_volume(volume);
                    println!("screen {} tile {tile_id}: vol {}%", screen.monitor_index, tile.volume());
                    return;
                }
            }
        }
        OverlayAction::ToggleTrackPanel { tile_id, kind } => {
            wall.overlay.track_panel = if wall.overlay.track_panel == Some((tile_id, kind)) {
                None
            } else {
                Some((tile_id, kind))
            };
            // Re-read the track cache so the row reflects the current
            // selection (covers files loaded since the last refresh).
            for screen in wall.screens.iter_mut() {
                if screen.tiles.iter().any(|t| t.id == tile_id) {
                    let monitor_index = screen.monitor_index;
                    if let Some(tile) = screen.tiles.iter_mut().find(|t| t.id == tile_id) {
                        tile.refresh_tracks();
                        println!(
                            "screen {monitor_index} tile {tile_id}: {} audio, {} subtitle track(s)",
                            tile.audio_tracks.len(),
                            tile.subtitle_tracks.len()
                        );
                    }
                    break;
                }
            }
        }
        OverlayAction::SetAudioTrack { tile_id, track } => {
            for screen in wall.screens.iter_mut() {
                if let Some(tile) = screen.tiles.iter_mut().find(|t| t.id == tile_id) {
                    tile.set_audio_track(track);
                    let label = track
                        .map(|id| {
                            tile.audio_tracks
                                .iter()
                                .find(|t| t.id == id)
                                .map(|t| t.describe())
                                .unwrap_or_else(|| format!("#{id}"))
                        })
                        .unwrap_or_else(|| "off".into());
                    println!("screen {} tile {tile_id}: audio track -> {label}", screen.monitor_index);
                    return;
                }
            }
        }
        OverlayAction::SetSubtitleTrack { tile_id, track } => {
            for screen in wall.screens.iter_mut() {
                if let Some(tile) = screen.tiles.iter_mut().find(|t| t.id == tile_id) {
                    tile.set_subtitle_track(track);
                    let label = track
                        .map(|id| {
                            tile.subtitle_tracks
                                .iter()
                                .find(|t| t.id == id)
                                .map(|t| t.describe())
                                .unwrap_or_else(|| format!("#{id}"))
                        })
                        .unwrap_or_else(|| "off".into());
                    println!("screen {} tile {tile_id}: subtitle track -> {label}", screen.monitor_index);
                    return;
                }
            }
        }
        OverlayAction::SeekTo { tile_id, to } => {
            for screen in wall.screens.iter_mut() {
                if let Some(tile) = screen.tiles.iter_mut().find(|t| t.id == tile_id) {
                    tile.seek(to, true);
                    println!(
                        "screen {} tile {tile_id}: seek to {} ({})",
                        screen.monitor_index,
                        fmt_time(to),
                        tile.item.title
                    );
                    return;
                }
            }
        }
        OverlayAction::Skip { tile_id, secs } => {
            for screen in wall.screens.iter_mut() {
                if let Some(tile) = screen.tiles.iter_mut().find(|t| t.id == tile_id) {
                    // NB: time-pos is read before the seek — mpv applies the
                    // command asynchronously, so a post-command read can be
                    // stale.
                    let from = tile.time_pos();
                    tile.seek(secs, false);
                    println!(
                        "screen {} tile {tile_id}: seek {}{secs:.0}s from {} ({})",
                        screen.monitor_index,
                        if secs >= 0.0 { "+" } else { "" },
                        fmt_time(from),
                        tile.item.title
                    );
                    return;
                }
            }
        }
        OverlayAction::Next { tile_id } => {
            tile_next(wall.screens, wall.queue, tile_id, true);
        }
        OverlayAction::Prev { tile_id } => {
            tile_next(wall.screens, wall.queue, tile_id, false);
        }
        OverlayAction::PlayOnTile { tile_id, index } => {
            for si in 0..wall.screens.len() {
                if !wall.screens[si].tiles.iter().any(|t| t.id == tile_id) {
                    continue;
                }
                let monitor_index = wall.screens[si].monitor_index;
                let item = wall.queue.items().get(index).cloned();
                if let Some(item) = item {
                    wall.queue.remove_pending(index);
                    if let Some(tile) = wall.screens[si].tiles.iter_mut().find(|t| t.id == tile_id) {
                        tile.item = item.clone();
                        if let Err(e) = tile.load(&item) {
                            eprintln!("screen {monitor_index} tile {tile_id}: load failed: {e}");
                        } else {
                            println!("screen {monitor_index} tile {tile_id}: now playing \"{}\"", item.title);
                        }
                    }
                }
                break;
            }
        }
        OverlayAction::ToggleHelp => {
            // The panel opens on the screen whose button was clicked; a
            // click on another screen's button moves it there.
            let win = wall.screens[screen_idx].window.id();
            wall.overlay.help_open = if wall.overlay.help_open == Some(win) { None } else { Some(win) };
        }
        OverlayAction::QueueAdd { items } => {
            let n = items.len();
            wall.queue.add_items(items);
            println!("library: added {n} item(s) to the queue ({} total)", wall.queue.len());
        }
        OverlayAction::QueueClear => {
            let n = wall.queue.len();
            wall.queue.clear();
            println!("queue: cleared ({n} item(s))");
            notify(wall.overlay, format!("queue cleared ({n} item(s))"));
        }
        OverlayAction::QueueAppend { item } => {
            println!("library: queued \"{}\" ({} total)", item.title, wall.queue.len() + 1);
            wall.queue.append_items(vec![item]);
        }
        OverlayAction::PlayNow { item } => {
            // The first wall window's first tile — a library window has no
            // tiles of its own to play on.
            let target = wall
                .screens
                .iter()
                .find(|s| s.role == ScreenRole::Wall && !s.tiles.is_empty())
                .map(|s| (s.monitor_index, s.tiles[0].id));
            match target {
                Some((monitor_index, tile_id)) => {
                    for screen in wall.screens.iter_mut() {
                        if let Some(tile) = screen.tiles.iter_mut().find(|t| t.id == tile_id) {
                            tile.item = item.clone();
                            if let Err(e) = tile.load(&item) {
                                eprintln!("screen {monitor_index} tile {tile_id}: load failed: {e}");
                            } else {
                                println!(
                                    "library: screen {monitor_index} tile {tile_id}: now playing \"{}\"",
                                    item.title
                                );
                            }
                            break;
                        }
                    }
                }
                None => notify(wall.overlay, "no wall tile to play on — open a window first".into()),
            }
        }
        // Intercepted by the event loop before they get here: opening a
        // window needs the EventLoopWindowTarget and the shared GL config,
        // and must happen outside the loop iterating `screens` by index.
        OverlayAction::NewWindow | OverlayAction::NewLibraryWindow { .. } => {}
        OverlayAction::ToggleSettings => {
            let win = wall.screens[screen_idx].window.id();
            wall.overlay.settings_open = if wall.overlay.settings_open == Some(win) { None } else { Some(win) };
            println!(
                "settings panel {} (screen {})",
                if wall.overlay.settings_open.is_some() { "open" } else { "closed" },
                wall.screens[screen_idx].monitor_index
            );
        }
        OverlayAction::ToggleWindowMode => {
            if toggle_window_mode(wall.screens, screen_idx) {
                *wall.wm_settle_at = Some((Instant::now() + Duration::from_millis(600), 4));
            }
        }
        OverlayAction::AddPanel => {
            add_tile_action(wall, screen_idx);
        }
        OverlayAction::RemovePanel { tile_id } => {
            remove_tile_action(wall.screens, wall.budget, wall.overlay, tile_id);
        }
        OverlayAction::FullscreenToggle { tile_id } => {
            if wall.screens[screen_idx].zoomed.is_some() {
                toggle_fullscreen(wall.screens, screen_idx, None);
            } else {
                toggle_fullscreen(wall.screens, screen_idx, Some(tile_id));
            }
        }
        OverlayAction::DeleteCurrent { tile_id } => {
            delete_current(wall.screens, wall.queue, wall.sources, screen_idx, tile_id, wall.overlay);
        }
        OverlayAction::QueueRemove { index } => {
            let monitor_index = wall.screens[screen_idx].monitor_index;
            let q = &mut *wall.queue;
            if let Some(m) = q.items().get(index).cloned() {
                q.remove_pending(index);
                println!("screen {monitor_index}: queue: removed \"{}\" ({} left)", m.title, q.len());
            }
        }
        OverlayAction::QueueReorder { from, to } => {
            let monitor_index = wall.screens[screen_idx].monitor_index;
            wall.queue.move_pending(from, to);
            println!("screen {monitor_index}: queue: reordered");
        }
        OverlayAction::QueueShuffle => {
            let monitor_index = wall.screens[screen_idx].monitor_index;
            wall.queue.set_order(QueueOrder::Shuffle);
            println!("screen {monitor_index}: queue: shuffled");
        }
        OverlayAction::QueueSortName => {
            let monitor_index = wall.screens[screen_idx].monitor_index;
            wall.queue.set_order(QueueOrder::Name);
            println!("screen {monitor_index}: queue: sorted by name");
        }
        OverlayAction::PauseAll => {
            let ids: Vec<usize> = wall.screens[screen_idx].tiles.iter().map(|t| t.id).collect();
            for id in ids {
                pause_and_release(wall.screens, wall.budget, id, "user pause");
            }
        }
        OverlayAction::ResumeAll => {
            let ids: Vec<usize> = wall.screens[screen_idx].tiles.iter().map(|t| t.id).collect();
            for id in ids {
                resume_tile(wall.screens, wall.budget, id);
            }
        }
    }
}

/// Where a batch of files should land.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpenTarget {
    /// No window in particular — a hand-off from another launch. Plays on
    /// the first wall window that has a tile.
    Anywhere,
    /// Dropped onto this window: play there, on the tile under the pointer
    /// if the drop landed on one. A browser window queues instead, since it
    /// has no tiles to play on.
    Window(winit::window::WindowId),
}

/// Acts on files handed over by a later launch.
///
/// "Play that file" rather than "quietly append it" is the same rule the
/// command line already follows — see the positional-paths note in `main`.
/// Double-clicking a video in the file manager has to result in that video
/// playing, whether or not an instance was already up.
///
/// Returns true when there was nowhere to play it and the caller should
/// open a wall window; the file is left at the head of the pending queue,
/// so the tile that window opens with is the one just asked for.
fn open_handed_off(
    screens: &mut [Screen],
    queue: &mut PlaybackQueue,
    overlay: &mut OverlayState,
    paths: &[PathBuf],
    target: OpenTarget,
) -> bool {
    // Even a bare re-launch means "come to the front" — that is what a
    // second click on the dock icon is asking for. A drop already happened
    // in the window the user was looking at, so it needs no raising.
    if target == OpenTarget::Anywhere {
        let front = screens
            .iter()
            .position(|s| s.role == ScreenRole::Wall)
            .or(if screens.is_empty() { None } else { Some(0) });
        if let Some(i) = front {
            screens[i].window.focus_window();
        }
    }
    if paths.is_empty() {
        return false;
    }
    let items = tessera_core::media::collect_items(paths);
    if items.is_empty() {
        notify(overlay, "nothing playable in what was dropped".into());
        return false;
    }
    let count = items.len();

    // Dropping on the browser window means "add these", not "play these" —
    // it has no tiles, and the queue tab is the thing you were looking at.
    if let OpenTarget::Window(id) = target {
        if screens.iter().any(|s| s.window.id() == id && s.role == ScreenRole::Browser) {
            queue.append_items(items);
            notify(overlay, format!("added {count} item(s) to the queue"));
            return false;
        }
    }

    // Where it will play is decided BEFORE the queue is touched, because
    // that is what decides whether the file belongs in the queue at all.
    // The tile to take over: the one under the pointer when the drop landed
    // on a specific window, else that window's first, else any wall tile.
    let spot = match target {
        OpenTarget::Window(id) => screens
            .iter()
            .position(|s| s.window.id() == id && s.role == ScreenRole::Wall && !s.tiles.is_empty())
            .map(|si| {
                let under = screens[si].cursor.and_then(|(x, y)| screens[si].tile_at(x, y));
                (si, under.unwrap_or(screens[si].tiles[0].id))
            }),
        OpenTarget::Anywhere => None,
    }
    .or_else(|| {
        screens
            .iter()
            .position(|s| s.role == ScreenRole::Wall && !s.tiles.is_empty())
            .map(|si| (si, screens[si].tiles[0].id))
    });

    let Some((si, tile_id)) = spot else {
        // Nowhere to play it, so the queue is the only place it can go. The
        // first one is moved to the head of the pending tail so the window
        // about to be opened comes up on the file that was asked for,
        // rather than wherever the cursor happened to be.
        let first_id = items[0].id.clone();
        queue.append_items(items);
        if let Some(idx) = queue.items().iter().position(|m| m.id == first_id) {
            queue.move_pending_to_front(idx);
        }
        println!("opened: {count} file(s) — no wall tile yet, opening a window");
        return true;
    };

    // Playing it and queueing it are alternatives, never both. A file on a
    // tile that is *also* sitting at the head of the queue starts again the
    // moment you press next, which reads as the wall being stuck on it.
    // This is the split `OverlayAction::PlayNow` already draws: the tile
    // owns the item it is playing, and the queue holds what is still to
    // come. Only the extras from a multi-file drop are queued.
    let mut items = items;
    let first = items.remove(0);
    let queued = items.len();
    queue.append_items(items);

    let monitor_index = screens[si].monitor_index;
    if let Some(tile) = screens[si].tiles.iter_mut().find(|t| t.id == tile_id) {
        tile.item = first.clone();
        if let Err(e) = tile.load(&first) {
            eprintln!("screen {monitor_index} tile {tile_id}: load failed: {e}");
        } else {
            println!(
                "opened: screen {monitor_index} tile {tile_id}: now playing \"{}\"{}",
                first.title,
                if queued > 0 { format!(" (+{queued} queued)") } else { String::new() }
            );
        }
    }
    screens[si].window.focus_window();
    false
}

/// Closes one window: frees its tiles' budget slots and GL resources, drops
/// the [`Screen`] (and with it the GL context, surface and egui context),
/// and terminates its mpv instances in parallel — serial teardown is what
/// made quitting slow. Overlay state pointing at the closed window or its
/// tiles is cleared, since none of it can be resolved any more.
fn close_screen(
    screens: &mut Vec<Screen>,
    budget: &mut DecodeBudget,
    overlay: &mut OverlayState,
    idx: usize,
) {
    let mut screen = screens.remove(idx);
    let window_id = screen.window.id();
    let monitor_index = screen.monitor_index;
    let tiles = screen.destroy();
    for tile in &tiles {
        budget.release(tile.id);
    }
    let gone: Vec<usize> = tiles.iter().map(|t| t.id).collect();
    if overlay.pending_click.map(|p| p.window_id) == Some(window_id) {
        overlay.pending_click = None;
    }
    if overlay.tile_drag.map(|d| d.window_id) == Some(window_id) {
        overlay.tile_drag = None;
    }
    if overlay.help_open == Some(window_id) {
        overlay.help_open = None;
    }
    if overlay.drop_target == Some(window_id) {
        overlay.drop_target = None;
    }
    if overlay.settings_open == Some(window_id) {
        overlay.settings_open = None;
    }
    if overlay.solo_id.is_some_and(|id| gone.contains(&id)) {
        overlay.solo_id = None;
    }
    if overlay.confirm_delete.is_some_and(|(id, _)| gone.contains(&id)) {
        overlay.confirm_delete = None;
    }
    if overlay.track_panel.is_some_and(|(id, _)| gone.contains(&id)) {
        overlay.track_panel = None;
    }
    println!(
        "closed window (monitor {monitor_index}, {} tile(s)); {} window(s) left",
        gone.len(),
        screens.len()
    );
    // Render contexts go first, here, while `screen.destroy()`'s
    // make_current is still in effect (see Tile::split_for_teardown); only
    // then may the GL context itself be dropped.
    let cores: Vec<tessera_mpv::Mpv> =
        tiles.into_iter().map(|t| t.split_for_teardown()).collect();
    drop(screen);
    terminate_parallel(cores);
}

/// Terminates mpv cores in parallel — serial teardown is what made quitting
/// slow. Safe on any thread: the GL-bound render contexts are already gone
/// by this point (see [`Tile::split_for_teardown`]).
fn terminate_parallel(cores: Vec<tessera_mpv::Mpv>) {
    std::thread::scope(|scope| {
        for core in cores {
            scope.spawn(move || drop(core));
        }
    });
}

/// Tears the wall down: GL resources are freed per-screen on their own GL
/// contexts (fast), then every tile's mpv instance is terminated in
/// parallel — serial teardown was what made quitting slow.
fn shutdown(screens: &mut [Screen]) {
    let start = Instant::now();
    let mut cores = Vec::new();
    for screen in screens.iter_mut() {
        // Per screen, and without interleaving: destroy() makes THIS
        // screen's GL context current, and the render contexts it just
        // returned can only be freed while that is true.
        let tiles = screen.destroy();
        cores.extend(tiles.into_iter().map(|t| t.split_for_teardown()));
    }
    terminate_parallel(cores);
    println!("shutdown in {:.2}s", start.elapsed().as_secs_f64());
}

/// Toggles the window itself between windowed and borderless-fullscreen on
/// its current monitor — the whole wall layout, no tile zoom. The focused
/// window only; other screens are untouched.
/// Toggles one screen's window between a fullscreen wall and a normal
/// decorated window. Expanding only calls set_fullscreen (never touches
/// decorations — changing them mid-transition makes pop-shell re-tile the
/// window back over the fullscreen switch); window-ifying turns decorations
/// on and restores the saved geometry (or a sensible default).
///
/// Returns whether the WM needs watching afterwards (see
/// [`fix_displaced_walls`]) — only ever on a platform that has one to fight.
fn toggle_window_mode(screens: &mut [Screen], idx: usize) -> bool {
    let screen = &mut screens[idx];
    let window = screen.window.clone();
    let monitor_index = screen.monitor_index;
    let mut schedule_fix = false;
    if platform::is_fullscreen(&window) {
        schedule_fix = true;
        // Fullscreen wall -> normal window.
        screen.zoomed = None;
        screen.should_fullscreen = false;
        platform::exit_fullscreen(&window);
        window.set_decorations(true);
        let restore = screen
            .windowed_restore
            .take()
            .or(screen.zoom_restore.take());
        match restore {
            Some((pos, size)) => {
                window.set_outer_position(pos);
                let _ = window.request_inner_size(size);
                screen.resize(size.width, size.height);
            }
            None => {
                if let Some(m) = window.current_monitor() {
                    let ms = m.size();
                    let (w, h) = (ms.width.min(1600), ms.height.min(900));
                    let _ = window.request_inner_size(winit::dpi::PhysicalSize::new(w, h));
                    let mp = m.position();
                    let x = mp.x + (ms.width.saturating_sub(w)) as i32 / 2;
                    let y = mp.y + (ms.height.saturating_sub(h)) as i32 / 2;
                    window.set_outer_position(winit::dpi::PhysicalPosition::new(x, y));
                    screen.resize(w, h);
                }
            }
        }
        println!("screen {monitor_index}: window mode");
    } else {
        // Normal window -> fullscreen wall. Aim at the intended monitor:
        // winit moves the window there and grabs focus, which pop-shell
        // needs before it honors fullscreen for a window it has tiled.
        screen.should_fullscreen = true;
        if screen.windowed_restore.is_none() {
            if let Ok(pos) = window.outer_position() {
                let size = window.inner_size();
                screen.windowed_restore = Some((pos, size));
            }
        }
        // Aim at the monitor the window is on NOW, not the one it was
        // created on: dragging a window to another screen and hitting W is
        // the whole point of per-window fullscreen.
        let here = monitor_under_window(&window);
        if let Some((i, m)) = &here {
            screen.monitor_index = *i;
            screen.intended_monitor = Some(m.clone());
        }
        let monitor = here
            .map(|(_, m)| m)
            .or_else(|| screen.intended_monitor.clone())
            .or_else(|| window.current_monitor());
        platform::enter_fullscreen(&window, monitor);
        println!("screen {}: fullscreen wall", screen.monitor_index);
    }
    schedule_fix && platform::WM_SETTLE
}

/// The monitor a window is actually on, found by containment of the
/// window's centre in each monitor's rect, with the index into
/// `available_monitors()` (the same order `monitors` is built from in
/// main). winit's `current_monitor()` is unreliable here — see
/// [`fix_displaced_walls`].
fn monitor_under_window(
    window: &winit::window::Window,
) -> Option<(usize, winit::monitor::MonitorHandle)> {
    let c = window_centre(window)?;
    window
        .available_monitors()
        .enumerate()
        .find(|(_, m)| monitor_contains(m, c))
}

/// Centre of a window in desktop coordinates, or None if the WM won't
/// report its position yet (freshly mapped windows).
fn window_centre(window: &winit::window::Window) -> Option<(i32, i32)> {
    let pos = window.outer_position().ok()?;
    let size = window.inner_size();
    Some((pos.x + size.width as i32 / 2, pos.y + size.height as i32 / 2))
}

/// Whether a point lies in a monitor's rect. Half-open on the far edges so
/// adjacent monitors can't both claim a point on their shared border.
fn monitor_contains(monitor: &winit::monitor::MonitorHandle, p: (i32, i32)) -> bool {
    let (mp, ms) = (monitor.position(), monitor.size());
    rect_contains((mp.x, mp.y, ms.width, ms.height), p)
}

/// The containment test itself, free of winit types so it can be tested.
fn rect_contains(rect: (i32, i32, u32, u32), p: (i32, i32)) -> bool {
    let (x, y, w, h) = rect;
    p.0 >= x && p.0 < x + w as i32 && p.1 >= y && p.1 < y + h as i32
}

/// Snaps the walls back onto their intended monitors. Two displacements
/// are fixed, both from pop-shell: (a) a tiling WM grabs freshly mapped
/// windows, ignoring their fullscreen request and tiling them onto the
/// focused monitor until they get focus; (b) it relocates other fullscreen
/// windows (piling them onto the primary) when one window leaves
/// fullscreen. Either way the cure is the same single call:
/// set_fullscreen(Borderless(Some(monitor))) — winit moves the window to
/// the monitor origin AND grabs input focus, which pop-shell needs before
/// it honors fullscreen. Runs on a short timer at startup and after mode
/// toggles, retrying while displaced windows keep being found.
/// (Detection uses the real window rect — winit's current_monitor() is
/// unreliable here.)
fn fix_displaced_walls(screens: &mut [Screen]) -> bool {
    let mut fixed_any = false;
    for screen in screens.iter_mut() {
        if !screen.should_fullscreen {
            continue;
        }
        let Some(m) = screen.intended_monitor.clone() else { continue };
        let Some(centre) = window_centre(&screen.window) else { continue };
        let inside = monitor_contains(&m, centre);
        // Non-fullscreen (tiled by the WM) or fullscreen on the wrong
        // monitor: re-assert fullscreen aimed at the intended monitor.
        if platform::is_fullscreen(&screen.window) && inside {
            continue;
        }
        let window = screen.window.clone();
        platform::enter_fullscreen(&window, Some(m));
        println!("screen {}: re-anchored to its monitor", screen.monitor_index);
        fixed_any = true;
    }
    fixed_any
}

/// Toggles a screen's zoomed/fullscreen state. `tile_id` is the tile to
/// zoom onto (None just exits zoom). In windowed mode the window itself
/// also goes borderless-fullscreen on its current monitor — other monitors
/// and windows are unaffected. In multi-screen mode the windows are already
/// fullscreen, so only the tile zoom applies.
fn toggle_fullscreen(screens: &mut [Screen], idx: usize, tile_id: Option<usize>) {
    let screen = &mut screens[idx];
    if screen.zoomed.is_some() {
        screen.zoomed = None;
        // Only restore the window if the ZOOM took it fullscreen (W's
        // fullscreen is undone by W, not by un-zooming).
        if let Some((pos, size)) = screen.zoom_restore.take() {
            platform::exit_fullscreen(&screen.window);
            screen.window.set_outer_position(pos);
            let _ = screen.window.request_inner_size(size);
            screen.resize(size.width, size.height);
        }
    } else if let Some(id) = tile_id {
        screen.zoomed = Some(id);
        if !platform::is_fullscreen(&screen.window) {
            // Windowed window: also take over the whole monitor.
            if let Ok(pos) = screen.window.outer_position() {
                let size = screen.window.inner_size();
                screen.zoom_restore = Some((pos, size));
            }
            let monitor = screen.window.current_monitor();
            platform::enter_fullscreen(&screen.window, monitor);
        }
    }
}

/// Deletes the tile's current video at its source (local file, or the
/// server's delete API), drops it from the queue, and advances the tile to
/// the next item.
fn delete_current(
    screens: &mut [Screen],
    queue: &mut PlaybackQueue,
    sources: &[tessera_core::sources::Source],
    screen_idx: usize,
    tile_id: usize,
    overlay: &mut OverlayState,
) {
    let Some(tile) = screens[screen_idx].tiles.iter().find(|t| t.id == tile_id) else {
        return;
    };
    let item = tile.item.clone();
    let monitor_index = screens[screen_idx].monitor_index;
    match tessera_core::sources::delete_item(&item, sources) {
        Ok(()) => println!(
            "screen {monitor_index} tile {tile_id}: deleted \"{}\" ({})",
            item.title,
            item.kind.label()
        ),
        Err(e) => {
            println!("delete failed: {e}");
            overlay.notice = Some((format!("delete failed: {e}"), Instant::now()));
            return;
        }
    }
    queue.remove_item(&item.id);
    tile_next(screens, queue, tile_id, true);
}

/// Loads the next (or previous) queue item onto the tile, advancing the
/// screen's shared queue. Returns false if the tile was not found.
fn tile_next(screens: &mut [Screen], queue: &mut PlaybackQueue, tile_id: usize, forward: bool) -> bool {
    for screen_idx in 0..screens.len() {
        if !screens[screen_idx].tiles.iter().any(|t| t.id == tile_id) {
            continue;
        }
        let item = {
            let queue = &mut *queue;
            if forward { queue.advance() } else { queue.go_back() }.cloned()
        };
        if let Some(item) = item {
            let monitor_index = screens[screen_idx].monitor_index;
            if let Some(tile) = screens[screen_idx].tiles.iter_mut().find(|t| t.id == tile_id) {
                tile.item = item.clone();
                if let Err(e) = tile.load(&item) {
                    eprintln!("screen {monitor_index} tile {tile_id}: load failed: {e}");
                } else {
                    println!(
                        "screen {monitor_index} tile {tile_id}: {} \"{}\"",
                        if forward { "next" } else { "prev" },
                        item.title
                    );
                }
            }
        }
        return true;
    }
    false
}

/// Seeks every tile of a screen by a relative offset.
/// Seeks the tile under the cursor. The arrows used to move every tile on
/// the screen at once, which is almost never what you mean on a wall — the
/// controls act on the tile you are pointing at, and so should the keys.
/// With the cursor over no tile, nothing moves.
/// Quiet time after the last arrow keypress before the accumulated seek is
/// issued. Below X's auto-repeat interval (~40ms), so a held key keeps
/// accumulating and a released one flushes on the next frame.
const SEEK_SETTLE: Duration = Duration::from_millis(200);
/// Longest a held arrow key goes without the video actually moving. The
/// transport shows the target position the whole time, so this only governs
/// how often mpv is asked to do the expensive part.
const SEEK_MAX_HOLD: Duration = Duration::from_millis(600);

/// Adds to the seek the user is spelling out on the tile under the cursor,
/// rather than seeking now. See [`overlay::PendingSeek`] for why.
fn queue_seek(
    screens: &mut [Screen],
    screen_idx: usize,
    overlay: &mut overlay::OverlayState,
    secs: f64,
) {
    let screen = &screens[screen_idx];
    let monitor_index = screen.monitor_index;
    let Some((x, y)) = screen.cursor else { return };
    let Some(id) = screen.tile_at(x, y) else {
        println!("screen {monitor_index}: seek ignored — cursor is not over a tile");
        return;
    };
    // The cursor moved to another tile mid-hold: whatever is pending belongs
    // to the tile it was aimed at, so it goes out before the new one starts.
    if overlay.pending_seek.is_some_and(|ps| ps.tile_id != id) {
        flush_seek(screens, overlay, true);
    }
    let now = Instant::now();
    match overlay.pending_seek {
        Some(ref mut ps) => {
            ps.delta += secs;
            ps.last_input = now;
        }
        None => {
            overlay.pending_seek = Some(overlay::PendingSeek {
                tile_id: id,
                delta: secs,
                last_input: now,
                last_flush: now,
            })
        }
    }
}

/// Deltas smaller than this are not worth a demuxer flush.
const SEEK_EPSILON: f64 = 0.05;

/// What to do with an accumulating seek this frame. Split out from
/// [`flush_seek`] so the timing rules can be tested without a display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SeekStep {
    /// Keep accumulating; nothing to issue yet.
    Wait,
    /// Issue the seek; the accumulator is finished.
    SeekAndEnd,
    /// Issue the seek but keep accumulating — the key is still down and a
    /// long hold should not go silent.
    SeekAndHold,
    /// Nothing accumulated and the key is up: drop the accumulator.
    End,
    /// Nothing accumulated but the key is still down. Happens when the
    /// long-hold timer comes round before another key repeat has landed;
    /// seeking by zero is a real flush for no movement.
    Hold,
}

fn seek_step(delta: f64, since_input: Duration, since_flush: Duration, force: bool) -> SeekStep {
    let settled = since_input >= SEEK_SETTLE;
    let held_long = since_flush >= SEEK_MAX_HOLD;
    if !(force || settled || held_long) {
        return SeekStep::Wait;
    }
    let done = force || settled;
    match (delta.abs() < SEEK_EPSILON, done) {
        (true, true) => SeekStep::End,
        (true, false) => SeekStep::Hold,
        (false, true) => SeekStep::SeekAndEnd,
        (false, false) => SeekStep::SeekAndHold,
    }
}

/// Issues the accumulated seek when the key has settled, when a long hold
/// has gone too long without the video moving, or when `force` (key
/// released). Called once a frame.
fn flush_seek(screens: &mut [Screen], overlay: &mut overlay::OverlayState, force: bool) {
    let Some(ps) = overlay.pending_seek else { return };
    let now = Instant::now();
    let step = seek_step(
        ps.delta,
        now.duration_since(ps.last_input),
        now.duration_since(ps.last_flush),
        force,
    );
    if step == SeekStep::Wait {
        return;
    }
    if matches!(step, SeekStep::SeekAndEnd | SeekStep::SeekAndHold) {
        for screen in screens.iter_mut() {
            let monitor_index = screen.monitor_index;
            if let Some(tile) = screen.tiles.iter_mut().find(|t| t.id == ps.tile_id) {
                let from = tile.time_pos();
                tile.seek(ps.delta, false);
                // Show the destination straight away. mpv overwrites this
                // with the real position within a frame or two, and without
                // it the readout snaps back to `from` until the seek lands.
                if from >= 0.0 {
                    tile.last_time_pos = (from + ps.delta).max(0.0);
                }
                println!(
                    "screen {monitor_index} tile {}: seek {}{:.0}s from {}",
                    ps.tile_id,
                    if ps.delta >= 0.0 { "+" } else { "" },
                    ps.delta,
                    fmt_time(from)
                );
                break;
            }
        }
    }
    overlay.pending_seek = match step {
        SeekStep::SeekAndEnd | SeekStep::End => None,
        // Still held: start a fresh delta and keep the accumulator alive.
        _ => Some(overlay::PendingSeek { delta: 0.0, last_flush: now, ..ps }),
    };
}

fn fmt_time(t: f64) -> String {
    if t < 0.0 {
        return "--:--".into();
    }
    let total = t as i64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use winit::keyboard::Key;

    /// The user's actual layout: DP-2 5120x2160 at +2160+1027, DP-0
    /// 3440x1440 at +7280+1435, DP-4 2160x3840 at +0+0.
    const MONITORS: [(i32, i32, u32, u32); 3] =
        [(2160, 1027, 5120, 2160), (7280, 1435, 3440, 1440), (0, 0, 2160, 3840)];

    fn monitor_at(p: (i32, i32)) -> Option<usize> {
        MONITORS.iter().position(|r| super::rect_contains(*r, p))
    }

    #[test]
    fn window_centre_picks_the_monitor_it_sits_on() {
        // A window dragged onto DP-0 must resolve to monitor 1, not the
        // one it was created on — this is what W-to-fullscreen relies on.
        assert_eq!(monitor_at((8261, 2369)), Some(1));
        assert_eq!(monitor_at((4000, 2000)), Some(0));
        assert_eq!(monitor_at((1000, 1000)), Some(2));
    }

    #[test]
    fn monitor_rects_are_half_open_so_borders_are_unambiguous() {
        // DP-4 spans x 0..2160; DP-2 starts at x 2160. The shared column
        // belongs to exactly one of them.
        assert_eq!(monitor_at((2159, 1500)), Some(2));
        assert_eq!(monitor_at((2160, 1500)), Some(0));
        // DP-2's right edge is 2160+5120 = 7280, where DP-0 begins.
        assert_eq!(monitor_at((7279, 2000)), Some(0));
        assert_eq!(monitor_at((7280, 2000)), Some(1));
    }

    #[test]
    fn a_point_off_every_monitor_resolves_to_none() {
        assert_eq!(monitor_at((20000, 20000)), None);
        assert_eq!(monitor_at((-5, -5)), None);
    }

    #[test]
    fn character_key_guards_match() {
        type K = winit::keyboard::Key<winit::keyboard::SmolStr>;
        let cases: Vec<(K, bool, bool)> = vec![
            (Key::Character("n".into()), true, false),
            (Key::Character("N".into()), true, false),
            (Key::Character("p".into()), false, true),
            (Key::Character("P".into()), false, true),
            (Key::Character("x".into()), false, false),
        ];
        for (k, expect_n, expect_p) in cases {
            let m = match &k {
                Key::Character(c) if c.eq_ignore_ascii_case("n") => 'n',
                Key::Character(c) if c.eq_ignore_ascii_case("p") => 'p',
                _ => '-',
            };
            assert_eq!(m == 'n', expect_n, "n-guard for {k:?}");
            assert_eq!(m == 'p', expect_p, "p-guard for {k:?}");
        }
    }

    #[test]
    fn browser_keys_cover_every_tab_in_both_cases() {
        use super::browser_tab_key;
        use crate::browser::Tab;
        for (key, tab) in [
            ("l", Tab::Library),
            ("q", Tab::Queue),
            ("s", Tab::Sources),
            ("c", Tab::Config),
        ] {
            assert_eq!(browser_tab_key(key), Some(tab), "{key}");
            assert_eq!(
                browser_tab_key(&key.to_uppercase()),
                Some(tab),
                "shifted {key}"
            );
        }
        // Keys the wall owns must not be swallowed by the browser family.
        for other in ["w", "f", "n", "p", "?", "-", "+"] {
            assert_eq!(browser_tab_key(other), None, "{other}");
        }
    }

    #[test]
    fn scale_seeds_from_the_monitor_then_the_fallback() {
        use super::scale_for;
        let mut s = crate::overlay::UiSettings { ui_scale: 1.25, ..Default::default() };
        s.monitor_scale.insert("DP-2".into(), 2.0);

        assert_eq!(scale_for(&s, Some("DP-2"), None), 2.0, "remembered monitor");
        assert_eq!(scale_for(&s, Some("DP-0"), None), 1.25, "unknown monitor");
        assert_eq!(scale_for(&s, None, None), 1.25, "no monitor name at all");

        // --ui-scale is a run-only override and beats both, clamped.
        assert_eq!(scale_for(&s, Some("DP-2"), Some(1.5)), 1.5);
        assert_eq!(scale_for(&s, Some("DP-2"), Some(9.0)), 3.0, "clamped high");
        assert_eq!(scale_for(&s, Some("DP-2"), Some(0.1)), 0.75, "clamped low");
    }

    #[test]
    fn smooth_scroll_steps_once_per_notch_of_travel() {
        use super::{pixel_wheel_notches, WHEEL_PIXELS_PER_NOTCH as N};
        let mut acc = 0.0;
        // A swipe arrives as many small deltas; only whole notches step.
        let steps: i32 = (0..20).map(|_| pixel_wheel_notches(&mut acc, N / 8.0)).sum();
        assert_eq!(steps, 2, "20 x 1/8 notch = 2.5 notches, 2 applied");
        assert!((acc - N / 2.0).abs() < 1e-9, "the half notch is kept");

        // Reversing throws the leftover away rather than unwinding it.
        assert_eq!(pixel_wheel_notches(&mut acc, -N * 0.9), 0);
        assert_eq!(pixel_wheel_notches(&mut acc, -N * 0.2), -1);

        // A fast fling can be several notches in one event.
        let mut acc = 0.0;
        assert_eq!(pixel_wheel_notches(&mut acc, N * 3.5), 3);
        assert_eq!(pixel_wheel_notches(&mut acc, 0.0), 0, "zero is not a reversal");
        assert!((acc - N / 2.0).abs() < 1e-9);
    }

    #[test]
    fn held_arrow_coalesces_into_few_seeks() {
        use super::{seek_step, SeekStep, SEEK_MAX_HOLD, SEEK_SETTLE};
        use std::time::Duration;
        let none = Duration::ZERO;

        // Mid-repeat: input is still arriving and the hold is young.
        assert_eq!(seek_step(30.0, none, none, false), SeekStep::Wait);

        // Key released: go now rather than waiting out the settle window.
        assert_eq!(seek_step(30.0, none, none, true), SeekStep::SeekAndEnd);

        // Quiet for the settle window: the user let go (or tapped once).
        assert_eq!(seek_step(10.0, SEEK_SETTLE, none, false), SeekStep::SeekAndEnd);

        // Still held, but the video has not moved for too long.
        assert_eq!(seek_step(120.0, none, SEEK_MAX_HOLD, false), SeekStep::SeekAndHold);

        // A long-hold tick that arrives before any new repeat must not turn
        // into a zero-length seek — that is a demuxer flush for no movement.
        assert_eq!(seek_step(0.0, none, SEEK_MAX_HOLD, false), SeekStep::Hold);
        assert_eq!(seek_step(0.0, SEEK_SETTLE, none, false), SeekStep::End);
    }
}

/// Opens a plain (decorated, non-fullscreen) window at runtime, centred on
/// `on` when a monitor is given and clamped to it. The GL config is the one
/// every other window shares, so [`create_screen`] can build on it.
fn open_window<T>(
    target: &winit::event_loop::EventLoopWindowTarget<T>,
    gl_config: &glutin::config::Config,
    size: (u32, u32),
    on: Option<&winit::monitor::MonitorHandle>,
) -> Option<Arc<winit::window::Window>> {
    let (mut w, mut h) = size;
    let mut wb = app_window().with_title("Tessera");
    if let Some(m) = on {
        let (mp, ms) = (m.position(), m.size());
        w = w.min(ms.width);
        h = h.min(ms.height);
        wb = wb.with_position(winit::dpi::PhysicalPosition::new(
            mp.x + (ms.width.saturating_sub(w)) as i32 / 2,
            mp.y + (ms.height.saturating_sub(h)) as i32 / 2,
        ));
    }
    wb = wb.with_inner_size(winit::dpi::PhysicalSize::new(w, h));
    match glutin_winit::finalize_window(target, wb, gl_config) {
        Ok(win) => Some(Arc::new(win)),
        Err(e) => {
            eprintln!("warning: new window failed: {e}");
            None
        }
    }
}

/// Builds a [`Screen`] around an existing window: a GL context and surface
/// on the shared config, an egui context, and the compositor. Startup and
/// runtime "new window" both come through here so the two can't drift.
/// Returns None (having explained itself) if any GL step fails.
fn create_screen<T>(
    target: &winit::event_loop::EventLoopWindowTarget<T>,
    display: &glutin::display::Display,
    gl_config: &glutin::config::Config,
    window: Arc<winit::window::Window>,
    monitor_index: usize,
) -> Option<Screen> {
    let raw = window.raw_window_handle();
    let context_attrs = glutin::context::ContextAttributesBuilder::new().build(Some(raw));
    let context = match unsafe { display.create_context(gl_config, &context_attrs) } {
        Ok(c) => c,
        Err(e) => {
            eprintln!("warning: GL context for monitor {monitor_index} failed: {e}");
            return None;
        }
    };
    let size = window.inner_size();
    let one = std::num::NonZeroU32::new(1).unwrap();
    let (nw, nh) = (
        std::num::NonZeroU32::new(size.width).unwrap_or(one),
        std::num::NonZeroU32::new(size.height).unwrap_or(one),
    );
    let surface_attrs =
        glutin::surface::SurfaceAttributesBuilder::<glutin::surface::WindowSurface>::new()
            .build(window.raw_window_handle(), nw, nh);
    let surface = match unsafe { display.create_window_surface(gl_config, &surface_attrs) } {
        Ok(s) => s,
        Err(e) => {
            eprintln!("warning: GL surface for monitor {monitor_index} failed: {e}");
            return None;
        }
    };
    let context = match context.make_current(&surface) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("warning: make_current for monitor {monitor_index} failed: {e}");
            return None;
        }
    };
    let vsync = std::env::var("TESSERA_VSYNC").map(|v| v != "0").unwrap_or(false);
    surface
        .set_swap_interval(
            &context,
            if vsync {
                glutin::surface::SwapInterval::Wait(std::num::NonZeroU32::new(1).unwrap())
            } else {
                glutin::surface::SwapInterval::DontWait
            },
        )
        .ok();
    let gl = Arc::new(unsafe {
        glow::Context::from_loader_function(|s| {
            let c = std::ffi::CString::new(s).unwrap();
            display.get_proc_address(&c) as *const std::os::raw::c_void
        })
    });
    // EguiGlow compiles its shaders now, so the GL context must be current.
    let egui = egui_glow::EguiGlow::new(target, gl.clone(), None, None);
    Some(Screen::new(window, surface, context, gl, egui, monitor_index))
}

fn window_builder(args: &Args, monitors: &[winit::monitor::MonitorHandle], index: usize) -> WindowBuilder {
    let monitor = &monitors[index];
    match &args.screens {
        ScreenSel::Windowed(w, h) => app_window()
            .with_title("Tessera")
            .with_inner_size(winit::dpi::PhysicalSize::new(*w, *h)),
        _ => platform::fullscreen_window_builder(
            app_window().with_title(format!("Tessera — screen {index}")),
            monitor,
        ),
    }
}

/// A WindowBuilder carrying this app's identity: WM_CLASS (so the desktop
/// entry's StartupWMClass matches and the dock groups our windows) and the
/// embedded icon.
fn app_window() -> WindowBuilder {
    platform::window_builder(WindowBuilder::new()).with_window_icon(window_icon())
}
