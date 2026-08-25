mod compositor;
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
use screen::Screen;
use tile::{Tile, TileOptions};

enum ScreenSel {
    All,
    Index(usize),
    Windowed(u32, u32),
}

struct Args {
    files: Vec<PathBuf>,
    tiles: usize,
    screens: ScreenSel,
    hwdec: String,
    audio: bool,
    advance: bool,
    max_streams: usize,
    ceiling: f64,
    status_interval: u64,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        files: Vec::new(),
        tiles: 4,
        screens: ScreenSel::All,
        hwdec: "auto-safe".into(),
        audio: false,
        advance: false,
        max_streams: tessera_core::budget::DEFAULT_MAX_STREAMS,
        ceiling: tessera_core::budget::DEFAULT_CEILING_EQUIVALENTS,
        status_interval: 5,
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
            "--max-streams" => args.max_streams = value()?.parse().map_err(|_| "bad --max-streams")?,
            "--ceiling" => args.ceiling = value()?.parse().map_err(|_| "bad --ceiling")?,
            "--status-interval" => args.status_interval = value()?.parse().map_err(|_| "bad --status-interval")?,
            "--audio" => args.audio = true,
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
    if args.files.is_empty() {
        return Err("no files or directories given".into());
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
  --tiles N          Max tiles per screen (default 4)
  --screens all|N    All monitors fullscreen, or monitor index N (default all)
  --windowed WxH     Single windowed window instead of fullscreen
  --advance          Advance to the next queue item on end (default: loop)
  --audio            Enable audio (default muted)
  --hwdec MODE       mpv hardware decode mode (default auto-safe)
  --max-streams N    Budget: max concurrent streams (default 16)
  --ceiling N        Budget: 1080p30-equivalent decode ceiling (default 96)
  --status-interval S  Seconds between status prints (default 5)

CONTROLS:
  Esc                Quit
  Space              Pause/resume all tiles on the focused screen
  Left click         Pause/resume the clicked tile
  Right click        Mute/unmute the clicked tile (needs --audio)
  Up/Down arrows     Volume +/-5 on unmuted tiles of the focused screen"
    );
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n");
            print_usage();
            std::process::exit(2);
        }
    };

    let items = tessera_core::media::collect_items(&args.files);
    if items.is_empty() {
        eprintln!("error: no video files found in the given paths");
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

    let tile_opts = TileOptions {
        hwdec: args.hwdec.clone(),
        audio: args.audio,
        loop_file: !args.advance,
        log_verbose: std::env::var("TESSERA_MPV_LOG").map(|v| v == "v").unwrap_or(false),
    };

    let n_screens = windows.len();
    let mut screens: Vec<Screen> = Vec::new();
    let mut budget = DecodeBudget::new(args.ceiling, args.max_streams);
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

        let gl = unsafe {
            glow::Context::from_loader_function(|s| {
                let c = std::ffi::CString::new(s).unwrap();
                display.get_proc_address(&c) as *const std::os::raw::c_void
            })
        };
        let screen = Screen::new(window.clone(), surface, context, gl, monitor_index, queue);
        screens.push(screen);
    }

    if screens.is_empty() {
        eprintln!("error: no usable screens");
        std::process::exit(1);
    }

    let mut cursor_pos: Vec<(f32, f32)> = vec![(0.0, 0.0); screens.len()];

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
                    for screen in screens.iter_mut() {
                        screen.tick(&mut budget, args.advance);
                    }
                    if last_status.elapsed() >= Duration::from_secs(args.status_interval) {
                        let dt = last_status.elapsed().as_secs_f64();
                        last_status = Instant::now();
                        let elapsed = start.elapsed().as_secs();
                        println!("--- t+{elapsed}s ---");

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
                    match event {
                        WindowEvent::CloseRequested => {
                            for screen in screens.iter_mut() {
                                screen.destroy();
                            }
                            target.exit();
                        }
                        WindowEvent::Resized(size) => {
                            screens[idx].resize(size.width, size.height);
                        }
                        WindowEvent::CursorMoved { position, .. } => {
                            cursor_pos[idx] = (position.x as f32, position.y as f32);
                        }
                        WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                            let (x, y) = cursor_pos[idx];
                            if let Some(id) = screens[idx].tile_at(x, y) {
                                let playing = screens
                                    .iter()
                                    .any(|s| s.tiles.iter().any(|t| t.id == id && !t.is_paused()));
                                if playing {
                                    pause_and_release(&mut screens, &mut budget, id, "user pause");
                                } else {
                                    resume_tile(&mut screens, &mut budget, id);
                                }
                            }
                        }
                        WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Right, .. } => {
                            let (x, y) = cursor_pos[idx];
                            screens[idx].click_mute(x, y);
                        }
                        WindowEvent::KeyboardInput { event: key_event, .. } => {
                            if key_event.state == ElementState::Pressed {
                                match key_event.logical_key {
                                    Key::Named(NamedKey::Escape) => {
                                        for screen in screens.iter_mut() {
                                            screen.destroy();
                                        }
                                        target.exit();
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

fn dump_dir() -> Option<std::path::PathBuf> {
    let dir = std::path::PathBuf::from(std::env::var("TESSERA_DUMP_DIR").ok()?);
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
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
