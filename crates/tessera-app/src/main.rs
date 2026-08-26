mod compositor;
mod overlay;
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
use tessera_core::{Admission, DecodeBudget, MediaItem, PlaybackQueue, QueueOrder};
use winit::event::{ElementState, Event, MouseButton, StartCause, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, WindowBuilder};

use compositor::set_gl_loader;
use overlay::{OverlayAction, OverlayState};
use screen::Screen;
use tile::{Tile, TileOptions};

enum ScreenSel {
    All,
    Index(usize),
    Windowed(u32, u32),
}

/// Pointer travel (px) before a held left press becomes a tile drag —
/// small enough to feel immediate, big enough that plain clicks stay clicks.
const DRAG_SLOP_PX: f32 = 8.0;

struct Args {
    files: Vec<PathBuf>,
    tiles: usize,
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
        screens: ScreenSel::All,
        hwdec: "nvdec-copy".into(),
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
            "--screens" => match value()?.as_str() {
                "all" => args.screens = ScreenSel::All,
                n => args.screens = ScreenSel::Index(n.parse().map_err(|_| "bad --screens")?),
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
            s if s.starts_with('-') => return Err(format!("unknown flag {s}")),
            s => args.files.push(PathBuf::from(s)),
        }
    }
    if args.files.is_empty() && !args.plex && !args.jellyfin {
        return Err("no files or directories given (or use --plex / --jellyfin)".into());
    }
    if args.tiles == 0 {
        return Err("--tiles must be >= 1".into());
    }
    Ok(args)
}

fn print_usage() {
    println!(
        "opentessera — desktop multi-stream video wall

USAGE:
  opentessera [OPTIONS] <FILE|DIR>...

OPTIONS:
  --tiles N          Max tiles per screen (default 12)
  --screens all|N    All monitors fullscreen, or monitor index N (default all)
  --windowed WxH     Single windowed window instead of fullscreen
  --advance          Advance to the next queue item on end (default: loop)
  --audio            Enable audio (default muted)
  --audio-device DEV mpv audio device, e.g. pulse/alsa_output.pci-...hdmi-stereo
                     (default: system default; check `pactl list sinks short`)
  --ui-scale F       Overlay UI scale, 0.75-3.0 (default 1.0; also in settings)
  --cache-mb N       Demuxer cache per stream in MiB (default 512; also in settings)
  --hwdec MODE       mpv hardware decode mode (default nvdec-copy — hardware
                     decode without the CUDA/GL zero-copy interop, which
                     segfaults in mpv 0.34 when many decoders re-initialize;
                     use auto-safe for zero-copy at your own risk)
  --max-streams N    Budget: max concurrent streams (default 128; also in settings)
  --ceiling N        Budget: 1080p30-equivalent decode ceiling (default 96)
  --status-interval S  Seconds between status prints (default 5)
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
  Drag               Rearrange: drag a tile onto another to swap them (Esc cancels)
  Right click        Mute/unmute the clicked tile (needs --audio)
  Up/Down arrows     Volume +/-5 on unmuted tiles of the focused screen
  Left/Right arrows  Seek -/+10s on all tiles of the focused screen
  N / P              Next / previous queue item on each tile of the focused screen
  F                  Fullscreen the tile under the cursor on its screen (F/Esc exits)
  W                  Toggle THIS screen (cursor's window): fullscreen wall <-> window
  Esc                Exit fullscreen if zoomed; otherwise quit"
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

/// Raises the soft fd limit to the hard limit. Every tile's mpv holds
/// dozens of fds (stream socket, demuxer cache, event pipes, and — with
/// audio — a pipewire context), so a full wall easily exhausts the GNOME
/// session's default 1024 soft limit. That doesn't just fail loads: GL
/// fence creation needs an fd too, and exhausting them wedged the NVIDIA
/// DRM driver hard enough to take the whole desktop down (2026-08-26).
fn raise_fd_limit() {
    unsafe {
        let mut rl: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) != 0 || rl.rlim_cur >= rl.rlim_max {
            return;
        }
        let (old, target) = (rl.rlim_cur, rl.rlim_max);
        rl.rlim_cur = target;
        if libc::setrlimit(libc::RLIMIT_NOFILE, &rl) == 0 {
            let show = |v: libc::rlim_t| {
                if v == libc::RLIM_INFINITY { "unlimited".to_string() } else { v.to_string() }
            };
            println!("fd limit: raised soft limit {} -> {}", show(old), show(target));
        }
    }
}

/// Current fd usage (open descriptors in /proc/self/fd) for the status
/// line — fd exhaustion is invisible otherwise.
fn fd_count() -> usize {
    std::fs::read_dir("/proc/self/fd").map(|d| d.count()).unwrap_or(0)
}

fn main() {
    install_fatal_handler();
    raise_fd_limit();
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n");
            print_usage();
            std::process::exit(2);
        }
    };

    let mut items = tessera_core::media::collect_items(&args.files);
    if args.plex || args.jellyfin {
        let cfg = tessera_core::sources::SourceConfig::load();
        if args.plex {
            match cfg.plex {
                Some(srv) => match (tessera_core::sources::PlexSource { base: srv.url.clone(), token: srv.token.clone() }).list(args.plex_library.as_deref()) {
                    Ok(list) => {
                        println!("plex: {} item(s) from {}", list.len(), srv.url);
                        items.extend(list);
                    }
                    Err(e) => eprintln!("warning: plex source failed: {e}"),
                },
                None => eprintln!("warning: --plex but no server configured (sources.conf / env)"),
            }
        }
        if args.jellyfin {
            match cfg.jellyfin {
                Some(srv) => match (tessera_core::sources::JellyfinSource { base: srv.url.clone(), api_key: srv.token.clone() }).list(args.jellyfin_library.as_deref()) {
                    Ok(list) => {
                        println!("jellyfin: {} item(s) from {}", list.len(), srv.url);
                        items.extend(list);
                    }
                    Err(e) => eprintln!("warning: jellyfin source failed: {e}"),
                },
                None => eprintln!("warning: --jellyfin but no server configured (sources.conf / env)"),
            }
        }
    }
    if items.is_empty() {
        eprintln!("error: no video items found (no readable local paths, or all remote sources failed)");
        std::process::exit(2);
    }
    println!(
        "openTessera: {} item(s), tiles<={}, hwdec={}, audio={}, advance={}",
        items.len(),
        args.tiles,
        args.hwdec,
        args.audio,
        args.advance
    );

    let mpv_lib = match tessera_mpv::MpvLib::load() {
        Ok(l) => {
            let v = l.client_api_version();
            println!(
                "libmpv client API 0x{:X} ({}.{}.{})",
                v,
                v >> 16,
                (v >> 8) & 0xFF,
                v & 0xFF
            );
            l
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    let event_loop = EventLoop::new().unwrap();
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
        ScreenSel::Index(i) => {
            if *i >= monitors.len() {
                eprintln!("error: monitor {i} not found (have {})", monitors.len());
                std::process::exit(2);
            }
            vec![*i]
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

    let mut settings = load_settings();
    if args.audio {
        settings.audio = true;
    }
    if let Some(s) = args.ui_scale {
        settings.ui_scale = s.clamp(0.75, 3.0);
    }
    if let Some(c) = args.cache_mb {
        settings.cache_mb = c.clamp(64, 8192);
    }
    if let Some(m) = args.max_streams {
        settings.max_streams = (m as u32).clamp(1, 128);
    }
    let mut tile_opts = TileOptions {
        hwdec: args.hwdec.clone(),
        audio: settings.audio,
        audio_device: args.audio_device.clone(),
        cache_mb: settings.cache_mb,
        loop_file: !args.advance,
        log_verbose: std::env::var("TESSERA_MPV_LOG").map(|v| v == "v").unwrap_or(false),
    };

    let n_screens = windows.len();
    let mut screens: Vec<Screen> = Vec::new();
    let mut budget = DecodeBudget::new(args.ceiling, settings.max_streams as usize);
    let mut next_tile_id = 0usize;

    for (window, monitor_index) in windows {
        let raw = window.raw_window_handle();
        let context_attrs = glutin::context::ContextAttributesBuilder::new().build(Some(raw));
        let context = match unsafe { display.create_context(&gl_config, &context_attrs) } {
            Ok(c) => c,
            Err(e) => {
                eprintln!("warning: GL context for monitor {monitor_index} failed: {e}");
                continue;
            }
        };
        let size = window.inner_size();
        let (nw, nh) = (
            std::num::NonZeroU32::new(size.width).unwrap_or(std::num::NonZeroU32::new(1).unwrap()),
            std::num::NonZeroU32::new(size.height).unwrap_or(std::num::NonZeroU32::new(1).unwrap()),
        );
        let surface_attrs = glutin::surface::SurfaceAttributesBuilder::<glutin::surface::WindowSurface>::new()
            .build(window.raw_window_handle(), nw, nh);
        let surface = match unsafe { display.create_window_surface(&gl_config, &surface_attrs) } {
            Ok(s) => s,
            Err(e) => {
                eprintln!("warning: GL surface for monitor {monitor_index} failed: {e}");
                continue;
            }
        };
        let context = match context.make_current(&surface) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("warning: make_current for monitor {monitor_index} failed: {e}");
                continue;
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

        let screen_idx = screens.len();
        let mut queue = PlaybackQueue::new(QueueOrder::Name);
        let assigned: Vec<MediaItem> = items.iter().enumerate().filter(|(i, _)| i % n_screens == screen_idx).map(|(_, m)| m.clone()).collect();
        if assigned.is_empty() {
            eprintln!("warning: no items for monitor {monitor_index}");
        }
        queue.add_items(assigned);

        let gl = Arc::new(unsafe {
            glow::Context::from_loader_function(|s| {
                let c = std::ffi::CString::new(s).unwrap();
                display.get_proc_address(&c) as *const std::os::raw::c_void
            })
        });
        // EguiGlow compiles its shaders now, so the GL context must be current.
        let egui = egui_glow::EguiGlow::new(&event_loop, gl.clone(), None, None);
        let screen = Screen::new(window.clone(), surface, context, gl, egui, monitor_index, queue);
        let mut screen = screen;
        screen.intended_monitor = Some(monitors[monitor_index].clone());
        screens.push(screen);
    }

    if screens.is_empty() {
        eprintln!("error: no usable screens");
        std::process::exit(1);
    }

    let mut cursor_pos: Vec<Option<(f32, f32)>> = vec![None; screens.len()];
    let mut mods = winit::keyboard::ModifiersState::empty();
    // Which screen the mouse cursor is currently over (W applies there).
    let mut cursor_over: Option<usize> = None;
    // Left mouse button currently held (drag-threshold tracking).
    let mut left_held = false;
    // Deferred WM-settle check after a window/fullscreen toggle:
    // (deadline, retries left). Retries continue while displaced walls
    // keep being found.
    let mut wm_settle_at: Option<(Instant, u32)> = None;
    let mut overlay = OverlayState { pinned: args.overlay, ..Default::default() };
    let mut last_input = Instant::now();

    for screen_idx in 0..screens.len() {
        let target_tiles = screens[screen_idx].queue.len().clamp(1, args.tiles);
        let monitor_index = screens[screen_idx].monitor_index;
        let mut created = 0;
        while created < target_tiles {
            let item = match screens[screen_idx].queue.current() {
                Some(m) => m.clone(),
                None => break,
            };
            match request_admission(&mut screens, &mut budget, next_tile_id, &tessera_core::StreamSpec::placeholder()) {
                Admission::Admitted | Admission::AdmittedEvicting { .. } => {}
                Admission::RejectedBudget { would_use, ceiling } => {
                    println!(
                        "screen {monitor_index}: decode budget full, stopping at {created} tiles ({would_use:.0}/{} mbps)",
                        ceiling
                    );
                    break;
                }
                Admission::RejectedMaxStreams { max } => {
                    println!("screen {monitor_index}: max streams ({max}) reached, stopping at {created} tiles");
                    break;
                }
            }
            screens[screen_idx].context.make_current(&screens[screen_idx].surface).ok();
            match Tile::new(&mpv_lib, next_tile_id, &item, &tile_opts) {
                Ok(tile) => {
                    screens[screen_idx].add_tile(tile);
                    next_tile_id += 1;
                    created += 1;
                    screens[screen_idx].queue.advance();
                }
                Err(e) => {
                    budget.release(next_tile_id);
                    eprintln!("screen {monitor_index}: tile creation failed: {e}");
                    break;
                }
            }
        }
        if created == 0 && !screens[screen_idx].queue.is_empty() {
            eprintln!("warning: screen {monitor_index} has items but no tiles");
        }
    }

    println!(
        "running: {} screen(s), {} tile(s), budget {:.0}%",
        screens.len(),
        screens.iter().map(|s| s.tiles.len()).sum::<usize>(),
        budget.utilization() * 100.0
    );

    let start = Instant::now();
    let mut last_status = Instant::now();
    let mut last_drops: std::collections::HashMap<usize, i64> = std::collections::HashMap::new();
    let vsync = std::env::var("TESSERA_VSYNC").map(|v| v != "0").unwrap_or(false);
    let frame_interval = if vsync { Duration::from_millis(15) } else { Duration::from_millis(16) };

    event_loop
        .run(move |event, target| {
            target.set_control_flow(ControlFlow::WaitUntil(Instant::now() + frame_interval));
            match event {
                Event::NewEvents(StartCause::Init)
                | Event::NewEvents(StartCause::ResumeTimeReached { .. })
                | Event::NewEvents(StartCause::WaitCancelled { .. }) => {
                    // Overlay auto-hide: visible while pinned or while user
                    // input arrived within the last 5 seconds.
                    overlay.visible = overlay.pinned || last_input.elapsed() < Duration::from_secs(5);
                    // Deferred re-anchoring of displaced fullscreen walls.
                    if let Some((at, retries)) = wm_settle_at {
                        if Instant::now() >= at {
                            if fix_displaced_walls(&mut screens) && retries > 1 {
                                wm_settle_at = Some((Instant::now() + Duration::from_millis(600), retries - 1));
                            } else {
                                wm_settle_at = None;
                            }
                        }
                    }
                    let settings_before = settings;
                    for screen_idx in 0..screens.len() {
                        screens[screen_idx].tick(&mut budget, args.advance, &mut overlay, cursor_pos[screen_idx], &mut settings);
                        // UI actions from this screen's overlay: execute after
                        // the tick (pause/resume needs cross-screen budget
                        // access, which the drawing code can't have).
                        for action in std::mem::take(&mut overlay.actions) {
                            handle_overlay_action(
                                &mut screens,
                                &mut budget,
                                action,
                                screen_idx,
                                &mpv_lib,
                                &tile_opts,
                                &mut next_tile_id,
                                args.tiles,
                                &mut overlay,
                                &mut wm_settle_at,
                            );
                        }
                    }
                    // Apply settings edited in the gear panel (and persist).
                    // (Mid-drag scale churn is skipped; the final value
                    // applies on release.)
                    if settings != settings_before && !settings.scale_dragging {
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
                            if let Some(idx) = screens.iter().position(|s| s.window.id() == pc.window_id) {
                                screens[idx].click_mute(pc.x, pc.y);
                            }
                        }
                    }
                    if last_status.elapsed() >= Duration::from_secs(args.status_interval) {
                        let dt = last_status.elapsed().as_secs_f64();
                        last_status = Instant::now();
                        let elapsed = start.elapsed().as_secs();
                        println!("--- t+{elapsed}s --- fds {}", fd_count());

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
                                "budget: adaptive ceiling {:.0}% of static, {:.0}/{:.0} mbps in use, {} evictions total",
                                budget.adaptive() * 100.0,
                                budget.used_mbps(),
                                budget.effective_ceiling_mbps(),
                                budget.total_evictions()
                            );
                        }

                        for screen in screens.iter_mut() {
                            screen.status(dt);
                        }
                        if let Some(dir) = dump_dir() {
                            let tag = format!("t{elapsed}s");
                            for screen in screens.iter_mut() {
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
                        if ke.state == ElementState::Pressed {
                            last_input = Instant::now();
                            if ke.logical_key == Key::Named(NamedKey::Tab) {
                                overlay.pinned = !overlay.pinned;
                                println!("overlay: {}", if overlay.pinned { "pinned on" } else { "auto-hide" });
                                return;
                            }
                        }
                    }
                    // Forward everything to egui so its input state stays
                    // fresh. Event-time consumption is NOT used to gate raw
                    // input: egui's pointer position lags one frame, so
                    // clicks are deferred and hit-tested during the next
                    // frame's egui pass (see OverlayState::pending_click).
                    let window = screens[idx].window.clone();
                    let _ = screens[idx].egui.on_window_event(&window, &event);
                    match event {
                        WindowEvent::CloseRequested => {
                            shutdown(&mut screens);
                            target.exit();
                        }
                        WindowEvent::Resized(size) => {
                            screens[idx].resize(size.width, size.height);
                        }
                        WindowEvent::ModifiersChanged(m) => {
                            mods = m.state();
                        }
                        WindowEvent::CursorMoved { position, .. } => {
                            let (x, y) = (position.x as f32, position.y as f32);
                            cursor_pos[idx] = Some((x, y));
                            cursor_over = Some(idx);
                            last_input = Instant::now();
                            if let Some(drag) = overlay.tile_drag {
                                if drag.window_id == window_id {
                                    overlay.tile_drag = Some(overlay::TileDrag { pos: (x, y), ..drag });
                                } else {
                                    // Cross-screen drops are Phase 2: cancel.
                                    overlay.tile_drag = None;
                                }
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
                            if cursor_over == Some(idx) {
                                cursor_over = None;
                            }
                        }
                        WindowEvent::MouseInput { state, button, .. } => match (state, button) {
                            (ElementState::Pressed, MouseButton::Left) => {
                                left_held = true;
                                last_input = Instant::now();
                                if let Some((x, y)) = cursor_pos[idx] {
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
                                if let Some((x, y)) = cursor_pos[idx] {
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
                                    // Released on its own screen: drop (swap).
                                    // Anywhere else: just cancel.
                                    if drag.window_id == window_id {
                                        finish_tile_drag(&mut screens, drag);
                                    }
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
                        },
                        WindowEvent::KeyboardInput { event: key_event, .. } => {
                            last_input = Instant::now();
                            if key_event.state == ElementState::Pressed {
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
                                            shutdown(&mut screens);
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
                                        seek_screen(&mut screens[idx], -10.0);
                                    }
                                    Key::Named(NamedKey::ArrowRight) => {
                                        seek_screen(&mut screens[idx], 10.0);
                                    }
                                    Key::Character(ref c) if c.eq_ignore_ascii_case("w") => {
                                        // The window under the cursor, else the focused one.
                                        let target = cursor_over.unwrap_or(idx);
                                        if toggle_window_mode(&mut screens, target) {
                                            wm_settle_at = Some((Instant::now() + Duration::from_millis(600), 4));
                                        }
                                    }
                                    Key::Character(ref c) if !mods.shift_key() && c.eq_ignore_ascii_case("f") => {
                                        if screens[idx].zoomed.is_some() {
                                            toggle_fullscreen(&mut screens, idx, None);
                                        } else if let Some((x, y)) = cursor_pos[idx] {
                                            if let Some(t) = screens[idx].tile_at(x, y) {
                                                toggle_fullscreen(&mut screens, idx, Some(t));
                                            } else {
                                                println!("F: no tile under the cursor");
                                            }
                                        }
                                    }
                                    Key::Character(ref c) if c == "?" || c == "/" => {
                                        overlay.help_open = !overlay.help_open;
                                    }
                                    Key::Character(ref c) if c.eq_ignore_ascii_case("n") => {
                                        let ids: Vec<usize> = screens[idx].tiles.iter().map(|t| t.id).collect();
                                        for id in ids {
                                            tile_next(&mut screens, id, true);
                                        }
                                    }
                                    Key::Character(ref c) if c.eq_ignore_ascii_case("p") => {
                                        let ids: Vec<usize> = screens[idx].tiles.iter().map(|t| t.id).collect();
                                        for id in ids {
                                            tile_next(&mut screens, id, false);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        })
        .unwrap();

    println!("bye (uptime {:.1}s)", start.elapsed().as_secs_f64());
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
                    "audio" => s.audio = v == "true" || v == "yes" || v == "on",
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
        return;
    }
    let text = format!(
        "# openTessera settings (also editable at runtime via the gear panel)\nui_scale = {}\ncache_mb = {}\nmax_streams = {}\naudio = {}\n",
        s.ui_scale, s.cache_mb, s.max_streams, s.audio
    );
    let _ = std::fs::write(dir.join("settings.conf"), text);
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

/// Drop handler for a same-screen tile drag: swap the dragged tile with
/// the tile under the pointer. Dropping on empty space, outside the grid,
/// or back on the source cell is a no-op.
fn finish_tile_drag(screens: &mut [Screen], drag: overlay::TileDrag) {
    let Some(idx) = screens.iter().position(|s| s.window.id() == drag.window_id) else {
        return;
    };
    let Some(to) = screens[idx].tile_at(drag.pos.0, drag.pos.1) else {
        return;
    };
    if to == drag.from_tile {
        return;
    }
    let monitor_index = screens[idx].monitor_index;
    if screens[idx].swap_tiles(drag.from_tile, to) {
        println!("screen {monitor_index}: swapped tiles {} and {}", drag.from_tile, to);
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
            println!("tile {id}: staying paused — budget {would_use:.0}/{} mbps", ceiling);
        }
        Admission::RejectedMaxStreams { max } => {
            println!("tile {id}: staying paused — max streams ({max})");
        }
    }
}

/// Executes one queued overlay interaction. `screen_idx` is the screen whose
/// overlay produced the action.
#[allow(clippy::too_many_arguments)]
fn handle_overlay_action(
    screens: &mut [Screen],
    budget: &mut DecodeBudget,
    action: OverlayAction,
    screen_idx: usize,
    mpv_lib: &std::sync::Arc<tessera_mpv::MpvLib>,
    tile_opts: &TileOptions,
    next_tile_id: &mut usize,
    max_tiles: usize,
    overlay: &mut OverlayState,
    wm_settle_at: &mut Option<(Instant, u32)>,
) {
    let notify = |overlay: &mut OverlayState, msg: String| {
        println!("{msg}");
        overlay.notice = Some((msg, Instant::now()));
    };
    match action {
        OverlayAction::TogglePause { tile_id } => {
            let playing = screens
                .iter()
                .any(|s| s.tiles.iter().any(|t| t.id == tile_id && !t.is_paused()));
            if playing {
                pause_and_release(screens, budget, tile_id, "user pause");
            } else {
                resume_tile(screens, budget, tile_id);
            }
        }
        OverlayAction::MuteButton { tile_id } => {
            for screen in screens.iter_mut() {
                if let Some(tile) = screen.tiles.iter_mut().find(|t| t.id == tile_id) {
                    if tile.audio_enabled() {
                        let muted = tile.toggle_mute();
                        println!(
                            "screen {} tile {tile_id}: {}",
                            screen.monitor_index,
                            if muted { "muted".to_string() } else { format!("audio on (vol {}%)", tile.volume()) }
                        );
                    } else {
                        notify(overlay, "audio is off — restart with --audio to enable sound".into());
                    }
                    return;
                }
            }
        }
        OverlayAction::SeekTo { tile_id, to } => {
            for screen in screens.iter_mut() {
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
            for screen in screens.iter_mut() {
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
            tile_next(screens, tile_id, true);
        }
        OverlayAction::Prev { tile_id } => {
            tile_next(screens, tile_id, false);
        }
        OverlayAction::PlayOnTile { tile_id, index } => {
            for si in 0..screens.len() {
                if !screens[si].tiles.iter().any(|t| t.id == tile_id) {
                    continue;
                }
                let monitor_index = screens[si].monitor_index;
                let item = screens[si].queue.items().get(index).cloned();
                if let Some(item) = item {
                    screens[si].queue.remove_pending(index);
                    if let Some(tile) = screens[si].tiles.iter_mut().find(|t| t.id == tile_id) {
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
            overlay.help_open = !overlay.help_open;
        }
        OverlayAction::ToggleSettings => {
            overlay.settings_open = !overlay.settings_open;
        }
        OverlayAction::ToggleWindowMode => {
            if toggle_window_mode(screens, screen_idx) {
                *wm_settle_at = Some((Instant::now() + Duration::from_millis(600), 4));
            }
        }
        OverlayAction::AddPanel => {
            let monitor_index = screens[screen_idx].monitor_index;
            if screens[screen_idx].zoomed.is_some() {
                notify(overlay, "exit fullscreen (F) before adding tiles".into());
                return;
            }
            if screens[screen_idx].tiles.len() >= max_tiles {
                notify(overlay, format!("not adding: already at the --tiles {max_tiles} limit"));
                return;
            }
            let item = match screens[screen_idx].queue.advance().cloned() {
                Some(m) => m,
                None => {
                    notify(overlay, "not adding: queue is empty".into());
                    return;
                }
            };
            let id = *next_tile_id;
            match request_admission(screens, budget, id, &tessera_core::StreamSpec::placeholder()) {
                Admission::Admitted | Admission::AdmittedEvicting { .. } => {
                    screens[screen_idx].context.make_current(&screens[screen_idx].surface).ok();
                    match Tile::new(mpv_lib, id, &item, tile_opts) {
                        Ok(tile) => {
                            screens[screen_idx].add_tile(tile);
                            *next_tile_id += 1;
                            println!(
                                "screen {monitor_index}: added tile {id} \"{}\" ({} tiles)",
                                item.title,
                                screens[screen_idx].tiles.len()
                            );
                        }
                        Err(e) => {
                            budget.release(id);
                            notify(overlay, format!("not adding: tile creation failed: {e}"));
                        }
                    }
                }
                Admission::RejectedBudget { would_use, ceiling } => {
                    notify(overlay, format!("not adding: decode budget full ({would_use:.0}/{ceiling} mbps)"));
                }
                Admission::RejectedMaxStreams { max } => {
                    notify(overlay, format!("not adding: max concurrent streams ({max})"));
                }
            }
        }
        OverlayAction::RemovePanel { tile_id } => {
            for si in 0..screens.len() {
                if screens[si].tiles.iter().any(|t| t.id == tile_id) {
                    let monitor_index = screens[si].monitor_index;
                    let was_zoomed = screens[si].zoomed == Some(tile_id);
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
        OverlayAction::FullscreenToggle { tile_id } => {
            if screens[screen_idx].zoomed.is_some() {
                toggle_fullscreen(screens, screen_idx, None);
            } else {
                toggle_fullscreen(screens, screen_idx, Some(tile_id));
            }
        }
        OverlayAction::DeleteCurrent { tile_id } => {
            delete_current(screens, screen_idx, tile_id, overlay);
        }
        OverlayAction::QueueRemove { index } => {
            let monitor_index = screens[screen_idx].monitor_index;
            let q = &mut screens[screen_idx].queue;
            if let Some(m) = q.items().get(index).cloned() {
                q.remove_pending(index);
                println!("screen {monitor_index}: queue: removed \"{}\" ({} left)", m.title, q.len());
            }
        }
        OverlayAction::QueueReorder { from, to } => {
            let monitor_index = screens[screen_idx].monitor_index;
            screens[screen_idx].queue.move_pending(from, to);
            println!("screen {monitor_index}: queue: reordered");
        }
        OverlayAction::QueueShuffle => {
            let monitor_index = screens[screen_idx].monitor_index;
            screens[screen_idx].queue.set_order(QueueOrder::Shuffle);
            println!("screen {monitor_index}: queue: shuffled");
        }
        OverlayAction::QueueSortName => {
            let monitor_index = screens[screen_idx].monitor_index;
            screens[screen_idx].queue.set_order(QueueOrder::Name);
            println!("screen {monitor_index}: queue: sorted by name");
        }
        OverlayAction::PauseAll => {
            let ids: Vec<usize> = screens[screen_idx].tiles.iter().map(|t| t.id).collect();
            for id in ids {
                pause_and_release(screens, budget, id, "user pause");
            }
        }
        OverlayAction::ResumeAll => {
            let ids: Vec<usize> = screens[screen_idx].tiles.iter().map(|t| t.id).collect();
            for id in ids {
                resume_tile(screens, budget, id);
            }
        }
    }
}

/// Tears the wall down: GL resources are freed per-screen on their own GL
/// contexts (fast), then every tile's mpv instance is terminated in
/// parallel — serial teardown was what made quitting slow.
fn shutdown(screens: &mut [Screen]) {
    let start = Instant::now();
    let mut tiles = Vec::new();
    for screen in screens.iter_mut() {
        tiles.extend(screen.destroy());
    }
    std::thread::scope(|scope| {
        for tile in tiles {
            scope.spawn(move || drop(tile));
        }
    });
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
fn toggle_window_mode(screens: &mut [Screen], idx: usize) -> bool {
    let screen = &mut screens[idx];
    let window = screen.window.clone();
    let monitor_index = screen.monitor_index;
    let mut schedule_fix = false;
    if window.fullscreen().is_some() {
        schedule_fix = true;
        // Fullscreen wall -> normal window.
        screen.zoomed = None;
        window.set_fullscreen(None);
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
        // Normal window -> fullscreen wall.
        if screen.windowed_restore.is_none() {
            if let Ok(pos) = window.outer_position() {
                let size = window.inner_size();
                screen.windowed_restore = Some((pos, size));
            }
        }
        let monitor = window.current_monitor();
        window.set_fullscreen(Some(winit::window::Fullscreen::Borderless(monitor)));
        println!("screen {monitor_index}: fullscreen wall");
    }
    schedule_fix
}

/// Snaps fullscreen walls back onto their intended monitors. The tiling
/// WM relocates other fullscreen windows (piling them onto the primary
/// monitor) when one window leaves fullscreen; the relocation lands
/// asynchronously, so this runs on a short timer after a mode toggle and
/// retries while it keeps finding displaced walls. Fullscreen can't be
/// aimed at a monitor on X11: un-fullscreen, move, fullscreen again.
/// (Detection uses the real window rect — winit's current_monitor() is
/// unreliable here.)
fn fix_displaced_walls(screens: &mut [Screen]) -> bool {
    let mut fixed_any = false;
    for screen in screens.iter_mut() {
        if screen.window.fullscreen().is_none() {
            continue;
        }
        let Some(m) = screen.intended_monitor.clone() else { continue };
        let (mp, ms) = (m.position(), m.size());
        let Ok(wp) = screen.window.outer_position() else { continue };
        let ws = screen.window.inner_size();
        let center = (wp.x + ws.width as i32 / 2, wp.y + ws.height as i32 / 2);
        let inside = center.0 >= mp.x
            && center.0 < mp.x + ms.width as i32
            && center.1 >= mp.y
            && center.1 < mp.y + ms.height as i32;
        if !inside {
            let window = screen.window.clone();
            window.set_fullscreen(None);
            window.set_outer_position(mp);
            window.set_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
            println!("screen {}: re-anchored to its monitor", screen.monitor_index);
            fixed_any = true;
        }
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
            screen.window.set_fullscreen(None);
            screen.window.set_outer_position(pos);
            let _ = screen.window.request_inner_size(size);
            screen.resize(size.width, size.height);
        }
    } else if let Some(id) = tile_id {
        screen.zoomed = Some(id);
        if screen.window.fullscreen().is_none() {
            // Windowed window: also take over the whole monitor.
            if let Ok(pos) = screen.window.outer_position() {
                let size = screen.window.inner_size();
                screen.zoom_restore = Some((pos, size));
            }
            let monitor = screen.window.current_monitor();
            screen.window.set_fullscreen(Some(winit::window::Fullscreen::Borderless(monitor)));
        }
    }
}

/// Deletes the tile's current video at its source (local file, or the
/// server's delete API), drops it from the queue, and advances the tile to
/// the next item.
fn delete_current(screens: &mut [Screen], screen_idx: usize, tile_id: usize, overlay: &mut OverlayState) {
    let Some(tile) = screens[screen_idx].tiles.iter().find(|t| t.id == tile_id) else {
        return;
    };
    let item = tile.item.clone();
    let monitor_index = screens[screen_idx].monitor_index;
    match tessera_core::sources::delete_item(&item) {
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
    screens[screen_idx].queue.remove_item(&item.id);
    tile_next(screens, tile_id, true);
}

/// Loads the next (or previous) queue item onto the tile, advancing the
/// screen's shared queue. Returns false if the tile was not found.
fn tile_next(screens: &mut [Screen], tile_id: usize, forward: bool) -> bool {
    for screen_idx in 0..screens.len() {
        if !screens[screen_idx].tiles.iter().any(|t| t.id == tile_id) {
            continue;
        }
        let item = {
            let queue = &mut screens[screen_idx].queue;
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
fn seek_screen(screen: &mut Screen, secs: f64) {
    let monitor_index = screen.monitor_index;
    for tile in screen.tiles.iter_mut() {
        tile.seek(secs, false);
    }
    println!("screen {monitor_index}: seek {}{secs:.0}s on {} tile(s)", if secs >= 0.0 { "+" } else { "" }, screen.tiles.len());
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
}

fn window_builder(args: &Args, monitors: &[winit::monitor::MonitorHandle], index: usize) -> WindowBuilder {
    let monitor = &monitors[index];
    match &args.screens {
        ScreenSel::Windowed(w, h) => WindowBuilder::new()
            .with_title("openTessera")
            .with_inner_size(winit::dpi::PhysicalSize::new(*w, *h)),
        _ => WindowBuilder::new()
            .with_title(format!("openTessera — screen {index}"))
            .with_decorations(false)
            .with_fullscreen(Some(Fullscreen::Borderless(Some(monitor.clone())))),
    }
}
