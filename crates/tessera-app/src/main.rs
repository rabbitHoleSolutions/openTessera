mod auth;
mod browser;
mod catalog;
mod cli;
mod compositor;
mod hud;
mod instance;
mod mcp;
mod overlay;
mod persist;
mod platform;
mod poster;
mod thumbs;
mod png_util;
mod rpc;
mod screen;
mod seek;
mod tile;
mod wall;
mod windowing;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use glutin::display::GetGlDisplay;
use glutin::prelude::*;
use glutin_winit::DisplayBuilder;
use tessera_core::{Admission, DecodeBudget, PlaybackQueue, QueueOrder};
use winit::event::{ElementState, Event, MouseButton, MouseScrollDelta, StartCause, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoopBuilder};
use winit::keyboard::{Key, NamedKey};

use cli::{parse_args, print_usage, ScreenSel};
use compositor::set_gl_loader;
use overlay::{OverlayAction, OverlayState};
use persist::{config_dir, dump_dir, load_queue, load_settings, queue_path, save_queue, save_settings};
use screen::{Screen, ScreenRole};
use seek::{flush_seek, queue_seek};
use tile::{Tile, TileOptions};
use wall::{
    add_tile_action, close_screen, finish_tile_drag, handle_overlay_action, list_off_thread, open_listed,
    pause_and_release, raw_tile_click, remove_tile_action, request_admission, resume_tile, shutdown, tile_next,
    toggle_pin, Listed, OpenTarget, Wall,
};
use windowing::{
    create_screen, fix_displaced_walls, monitor_infos, monitor_under_window, open_window, raise_front,
    sync_fullscreen_chrome, toggle_fullscreen, toggle_window_mode, window_builder, Placement,
};

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

/// How often decoder drop counts are sampled into the adaptive budget. The
/// budget's streak limits are counted in these windows, so changing it
/// changes how long drops must last before a tile is evicted.
const DROP_SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

/// Prints a backtrace on a fatal signal, so a crash inside mpv or the GL
/// driver leaves something to put in a bug report.
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
        act.sa_sigaction = fatal_signal as *const () as usize;
        act.sa_flags = libc::SA_SIGINFO | libc::SA_NODEFER;
        libc::sigemptyset(&mut act.sa_mask);
        libc::sigaction(libc::SIGSEGV, &act, std::ptr::null_mut());
        libc::sigaction(libc::SIGABRT, &act, std::ptr::null_mut());
        libc::sigaction(libc::SIGBUS, &act, std::ptr::null_mut());
    }
}

fn main() {
    // `tessera mcp`: an MCP server on stdio for the running app — no window,
    // no GL, no instance of its own (see mcp.rs).
    if std::env::args().nth(1).as_deref() == Some("mcp") {
        std::process::exit(mcp::run(std::env::args().skip(2).collect()));
    }
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
    if let Some(audio) = args.audio {
        settings.audio = audio;
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
    // A URL plays, but is not a folder to keep as a source.
    let mut url_items = Vec::new();
    for f in &args.files {
        match f.to_str().and_then(tessera_core::MediaItem::from_url) {
            Some(item) => url_items.push(item),
            None => push_unique(tessera_core::sources::Source::Folder { path: f.display().to_string() }),
        }
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
        items.extend(url_items);
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

    // User events carry synthetic input from the dev API (rpc::Synth).
    let event_loop = EventLoopBuilder::<rpc::Injected>::with_user_event().build().unwrap();
    if let Ok(mut slot) = opens.injector.lock() {
        *slot = Some(event_loop.create_proxy());
    }
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

    // --monitor resolves against the real monitors: a number, a connector
    // name (DP-4) or a position (left/middle/right). --fullscreen then means
    // what `--screens N` always did.
    let mut args = args;
    let chosen_monitor = match &args.monitor {
        Some(spec) => match tessera_core::monitor::resolve_monitor(spec, &monitor_infos(monitors.iter().cloned())) {
            Ok(i) => Some(i),
            Err(e) => {
                eprintln!("error: --monitor: {e}");
                std::process::exit(2);
            }
        },
        None => None,
    };
    if args.fullscreen {
        args.screens = ScreenSel::Indices(vec![chosen_monitor.unwrap_or(0)]);
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
        ScreenSel::Windowed(_, _) => vec![chosen_monitor.unwrap_or(0)],
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
    // The first tile is the app's, not the user's, to make. A launch with
    // nothing queued opens an empty wall, and making a queue in the
    // browser shouldn't then need a "+" before anything plays — so until
    // the first tile exists, whatever lands in the queue starts one.
    // Cleared for good once a tile has existed: closing them all later is
    // a choice, and the wall must not refill behind the user's back.
    let mut awaiting_first_tile = true;
    // Files that arrived while running — handed off, opened by the OS or
    // dropped — being listed off-thread (see `list_off_thread`).
    let (listed_tx, listed_rx) = std::sync::mpsc::channel::<Listed>();

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
    // The status report is a diagnostic, off unless asked for; a dump dir
    // implies one, since the dumps ride on it.
    let status_interval = match args.status_interval {
        0 if dump_dir().is_some() => 5,
        n => n,
    };
    let mut last_status = Instant::now();
    let mut last_drop_sample = Instant::now();
    let mut last_adaptive = 1.0;
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
            // Synthetic input becomes the event a real device would send,
            // so everything below treats it identically.
            let event = match event {
                Event::UserEvent(injected) => rpc::synthesize(injected),
                other => other,
            };
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
                    sync_fullscreen_chrome(&screens);
                    // Overlay auto-hide: solid while pinned or while user
                    // input is recent, then fading out. Each frame renders
                    // anyway (the wall is paced, not event-driven), so the
                    // fade needs no wakeups of its own.
                    overlay.opacity = if overlay.pinned {
                        1.0
                    } else {
                        overlay::chrome_opacity(
                            last_input.elapsed().as_secs_f32(),
                            settings.hide_after_secs,
                            settings.fade_secs,
                        )
                    };
                    overlay.visible = overlay.opacity > 0.0;
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
                    let mut new_windows: Vec<(ScreenRole, browser::Tab, Placement)> = Vec::new();
                    // Files that arrived from outside: handed over by a
                    // later launch (see instance.rs), opened by the OS
                    // while running, or dragged onto a window. All three
                    // are listed off the render thread — any of them can be
                    // a folder, which is a directory walk — and acted on
                    // below once the listing lands.
                    for paths in opens.poll() {
                        raise_front(&screens);
                        if !paths.is_empty() {
                            list_off_thread(&listed_tx, OpenTarget::Anywhere, paths);
                        }
                    }
                    let opened = platform::take_opened_paths();
                    if !opened.is_empty() {
                        raise_front(&screens);
                        list_off_thread(&listed_tx, OpenTarget::Anywhere, opened);
                    }
                    // winit reports one DroppedFile per file with no
                    // position, so a drop is collected as it arrives and
                    // listed here as one batch — a multi-file drop is one
                    // gesture and should behave like one. The tile under
                    // the pointer is read now, not when the listing lands,
                    // by which time the pointer has moved on.
                    for (window, paths) in std::mem::take(&mut dropped) {
                        let tile = screens
                            .iter()
                            .find(|s| s.window.id() == window)
                            .and_then(|s| s.cursor.and_then(|(x, y)| s.tile_at(x, y)));
                        list_off_thread(&listed_tx, OpenTarget::Window { window, tile }, paths);
                    }
                    while let Ok((target, items)) = listed_rx.try_recv() {
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
                        if open_listed(&mut wall, target, items) {
                            new_windows.push((ScreenRole::Wall, browser::Tab::Library, Placement::default()));
                        }
                    }
                    // API calls (rpc.rs) — `tessera mcp`, and the dev tools.
                    let mut fx = rpc::Effects::default();
                    for call in opens.poll_calls() {
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
                        rpc::handle(call, &mut wall, &mut settings, cursor_over, &mut fx);
                    }
                    if let Some(place) = fx.new_wall_window {
                        new_windows.push((ScreenRole::Wall, browser::Tab::Library, place));
                    }
                    if fx.quit {
                        // LoopExiting runs the same teardown as Esc.
                        target.exit();
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
                                    new_windows.push((ScreenRole::Wall, browser::Tab::Library, Placement::default()));
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
                                        None => new_windows.push((ScreenRole::Browser, tab, Placement::default())),
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
                    if awaiting_first_tile {
                        if screens.iter().any(|s| !s.tiles.is_empty()) {
                            awaiting_first_tile = false;
                        } else if !queue.is_empty() {
                            if let Some(si) = screens.iter().position(|s| s.role == ScreenRole::Wall) {
                                awaiting_first_tile = false;
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
                                add_tile_action(&mut wall, si);
                            }
                        }
                    }
                    for (role, tab, place) in std::mem::take(&mut new_windows) {
                        // On the monitor asked for, else the one the cursor
                        // is on, else the one the last window is on.
                        let wanted = place.monitor.and_then(|i| target.available_monitors().nth(i).map(|m| (i, m)));
                        let near = wanted.as_ref().map(|(_, m)| m.clone()).or_else(|| {
                            cursor_over
                                .and_then(|w| screens.iter().find(|s| s.window.id() == w))
                                .or_else(|| screens.last())
                                .and_then(|s| monitor_under_window(&s.window))
                                .map(|(_, m)| m)
                        });
                        let size = match args.screens {
                            ScreenSel::Windowed(w, h) => (w, h),
                            _ => DEFAULT_WINDOW,
                        };
                        let Some(window) = open_window(target, &gl_config, size, near.as_ref())
                        else {
                            notify(&mut overlay, "could not open a new window".into());
                            break;
                        };
                        let monitor_index = wanted
                            .as_ref()
                            .map(|(i, _)| *i)
                            .or_else(|| monitor_under_window(&window).map(|(i, _)| i))
                            .unwrap_or(0);
                        let Some(mut screen) =
                            create_screen(target, &display, &gl_config, window, monitor_index)
                        else {
                            notify(&mut overlay, "could not open a new window (GL init)".into());
                            break;
                        };
                        screen.intended_monitor = wanted
                            .as_ref()
                            .map(|(_, m)| m.clone())
                            .or_else(|| monitor_under_window(&screen.window).map(|(_, m)| m));
                        // A new window starts as a window (W makes it a
                        // wall) unless it was asked for fullscreen. The WM
                        // may grab a fresh window onto another monitor
                        // before honouring that; the settle timer puts it
                        // back on the intended one, as at startup.
                        screen.should_fullscreen = place.fullscreen;
                        if place.fullscreen {
                            platform::enter_fullscreen(&screen.window, screen.intended_monitor.clone());
                            if platform::WM_SETTLE {
                                wm_settle_at = Some((Instant::now() + Duration::from_millis(800), 5));
                            }
                        }
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
                    // Adaptive decode-budget feedback runs on its own fixed
                    // cadence: it once shared the status report's timer, so
                    // the report could not be turned off without turning
                    // the budget's drop detection off with it.
                    if last_drop_sample.elapsed() >= DROP_SAMPLE_INTERVAL {
                        let dt = last_drop_sample.elapsed().as_secs_f64();
                        last_drop_sample = Instant::now();
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

                        // Only when it moves: a wall held under the ceiling
                        // for an hour would otherwise say so every 5s.
                        if budget.adaptive() != last_adaptive {
                            last_adaptive = budget.adaptive();
                            println!(
                                "budget: adaptive ceiling {:.0}% of static, {:.1}/{:.0} streams in use, {} evictions total",
                                budget.adaptive() * 100.0,
                                budget.used_equivalents(),
                                budget.ceiling_equivalents(),
                                budget.total_evictions()
                            );
                        }
                    }
                    if status_interval > 0 && last_status.elapsed() >= Duration::from_secs(status_interval) {
                        let dt = last_status.elapsed().as_secs_f64();
                        last_status = Instant::now();
                        let elapsed = start.elapsed().as_secs();
                        println!("--- t+{elapsed}s --- fds {}", platform::fd_count());
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
            let item = match queue.upcoming(1).first() {
                Some(m) => (*m).clone(),
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
            let muted = screens.iter().any(|s| !s.tiles.is_empty());
            match Tile::new(mpv_lib, *next_tile_id, &item, tile_opts, None, muted) {
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

#[cfg(test)]
mod tests {
    use winit::keyboard::Key;

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
}
