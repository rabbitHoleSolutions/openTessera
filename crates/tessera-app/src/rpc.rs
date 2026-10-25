//! The running app's request/response API, served over the instance socket.
//!
//! `tessera mcp` (see `mcp.rs`) is its client: an MCP server on stdio that
//! forwards each tool call here as one JSON line. The split follows the
//! render-thread rule. A call is parsed on the socket's connection thread;
//! anything that touches tiles, the queue or GL becomes a [`Call`] for the
//! event loop, which answers on its next frame (so ~16ms); anything that can
//! block — listing a folder, encoding a JPEG, pacing synthetic input — stays
//! on the connection thread, before or after that hand-off.
//!
//! Every reply is `{"ok": true, ...}` or `{"ok": false, "error": "..."}`, and
//! errors say what to do next: the reader is a language model choosing its
//! next tool call.

use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tessera_core::timecode::{fmt_hms, resolve_seek};
use tessera_core::MediaItem;

use crate::overlay::{OverlayAction, UiSettings};
use crate::screen::{Capture, CaptureWhat, RawImage, ScreenRole};
use crate::wall::{handle_overlay_action, open_listed, pause_and_release, remove_tile_action, resume_tile, tile_next, OpenTarget, Wall};
use crate::windowing::{monitor_under_window, toggle_window_mode, unfloat_for_fullscreen, Placement};

/// How long a connection waits for the event loop. A frame is ~16ms; this
/// is for a loop stalled behind something slow, which should surface as an
/// error rather than a hang.
const LOOP_TIMEOUT: Duration = Duration::from_secs(5);

/// A request for the event loop.
pub struct Call {
    pub method: String,
    pub params: Value,
    /// Media the connection thread already listed (open, queue add).
    pub items: Vec<MediaItem>,
    pub reply: mpsc::Sender<Reply>,
}

pub enum Reply {
    Json(Value),
    /// A frame grab queued on a screen; it arrives at that screen's next
    /// tick, and the connection thread encodes it.
    Capture { image: mpsc::Receiver<Option<RawImage>>, meta: Value },
}

/// What a call needs from the event loop beyond the [`Wall`].
#[derive(Default)]
pub struct Effects {
    /// Open a wall window, placed as given.
    pub new_wall_window: Option<crate::windowing::Placement>,
    pub quit: bool,
}

/// Synthetic pointer input for development (`tessera_dev_input`). Turned
/// into ordinary winit events at the top of the event loop, so it takes the
/// same path as a real mouse: egui, the click router, tile drags.
pub enum Synth {
    Move(f64, f64),
    Button { pressed: bool, right: bool },
    Wheel(f32),
}

pub struct Injected {
    /// `u64::from(WindowId)`.
    pub window: u64,
    pub synth: Synth,
}

pub type Injector = Arc<Mutex<Option<winit::event_loop::EventLoopProxy<Injected>>>>;

/// Synthetic input is only honoured by an instance launched for it: the
/// dev launcher sets this. A user's own instance ignores it, whatever
/// connects to its socket.
const DEV_INPUT_ENV: &str = "TESSERA_DEV_INPUT";

/// Turns synthetic input into the event a real device would have sent.
pub fn synthesize(i: Injected) -> winit::event::Event<Injected> {
    use winit::event::{DeviceId, ElementState, MouseButton, MouseScrollDelta, TouchPhase, WindowEvent};
    // SAFETY: the dummy id is only ever compared, never handed to winit —
    // these events go straight to our own handler and egui's.
    let device_id = unsafe { DeviceId::dummy() };
    let event = match i.synth {
        Synth::Move(x, y) => WindowEvent::CursorMoved { device_id, position: winit::dpi::PhysicalPosition::new(x, y) },
        Synth::Button { pressed, right } => WindowEvent::MouseInput {
            device_id,
            state: if pressed { ElementState::Pressed } else { ElementState::Released },
            button: if right { MouseButton::Right } else { MouseButton::Left },
        },
        Synth::Wheel(dy) => WindowEvent::MouseWheel {
            device_id,
            delta: MouseScrollDelta::LineDelta(0.0, dy),
            phase: TouchPhase::Moved,
        },
    };
    winit::event::Event::WindowEvent { window_id: winit::window::WindowId::from(i.window), event }
}

// ---------------------------------------------------------------------------
// Connection thread
// ---------------------------------------------------------------------------

/// Serves one request on a connection thread. Never panics on bad input.
pub fn serve(req: &Value, calls: &mpsc::Sender<Call>, injector: &Injector) -> Value {
    match serve_inner(req, calls, injector) {
        Ok(v) => v,
        Err(e) => json!({ "ok": false, "error": e }),
    }
}

fn serve_inner(req: &Value, calls: &mpsc::Sender<Call>, injector: &Injector) -> Result<Value, String> {
    let method = req.get("method").and_then(Value::as_str).ok_or("missing \"method\"")?;
    let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
    match method {
        "open" => {
            let items = list_requested(&params, calls)?;
            ask_json(calls, method, params, items)
        }
        "queue" if params.get("action").and_then(Value::as_str) == Some("add") => {
            let items = list_requested(&params, calls)?;
            ask_json(calls, method, params, items)
        }
        "sources" if params.get("action").and_then(Value::as_str) == Some("add") => {
            let path = params.get("path").and_then(Value::as_str).ok_or("add needs \"path\": the folder to add")?;
            let (dir, found) = check_source_folder(path)?;
            let mut params = params.clone();
            params["path"] = json!(dir.to_string_lossy());
            params["videos"] = json!(found);
            ask_json(calls, method, params, Vec::new())
        }
        "frame" => {
            let long_side = params.get("long_side").and_then(Value::as_u64).unwrap_or(768).clamp(64, 4096) as u32;
            let (img, mut meta) = ask_capture(calls, method, params)?;
            meta["image"] = encode(&img, Encoding::Jpeg { long_side })?;
            Ok(meta)
        }
        "colors" => {
            // Raw and tiny: a client asks many times a second
            use base64::Engine;
            let (img, mut meta) = ask_capture(calls, method, params)?;
            let stride = img.w as usize * 3;
            let upright: Vec<u8> = img.rgb.chunks_exact(stride).rev().flatten().copied().collect();
            meta["rgb"] = json!(base64::engine::general_purpose::STANDARD.encode(&upright));
            Ok(meta)
        }
        "screenshot" => {
            let scale = params.get("scale").and_then(Value::as_f64).unwrap_or(1.0).clamp(0.1, 1.0);
            let (img, mut meta) = ask_capture(calls, method, params)?;
            meta["image"] = encode(&img, Encoding::Png { scale })?;
            Ok(meta)
        }
        "input" => dev_input(&params, calls, injector),
        _ => ask_json(calls, method, params, Vec::new()),
    }
}

fn ask(calls: &mpsc::Sender<Call>, method: &str, params: Value, items: Vec<MediaItem>) -> Result<Reply, String> {
    let (tx, rx) = mpsc::channel();
    calls
        .send(Call { method: method.to_string(), params, items, reply: tx })
        .map_err(|_| "tessera is shutting down".to_string())?;
    rx.recv_timeout(LOOP_TIMEOUT).map_err(|_| "tessera didn't answer in time — it may be busy; try again".to_string())
}

fn ask_json(calls: &mpsc::Sender<Call>, method: &str, params: Value, items: Vec<MediaItem>) -> Result<Value, String> {
    match ask(calls, method, params, items)? {
        Reply::Json(v) => Ok(v),
        Reply::Capture { .. } => Err("internal: unexpected capture".into()),
    }
}

fn ask_capture(calls: &mpsc::Sender<Call>, method: &str, params: Value) -> Result<(RawImage, Value), String> {
    match ask(calls, method, params, Vec::new())? {
        Reply::Json(v) => match v.get("error").and_then(Value::as_str) {
            Some(e) => Err(e.to_string()),
            None => Err("internal: expected a capture".into()),
        },
        Reply::Capture { image, meta } => match image.recv_timeout(LOOP_TIMEOUT) {
            Ok(Some(img)) => Ok((img, meta)),
            Ok(None) => Err("no frame yet — the video hasn't rendered one; try again in a moment".into()),
            Err(_) => Err("tessera didn't render a frame in time — is the window minimised?".into()),
        },
    }
}

/// Lists `params.paths` (files, folders, URLs) after checking each against
/// the allowed roots. Runs here because a folder is a directory walk.
fn list_requested(params: &Value, calls: &mpsc::Sender<Call>) -> Result<Vec<MediaItem>, String> {
    let paths: Vec<&str> = params
        .get("paths")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if paths.is_empty() {
        return Err("give \"paths\": files, folders or URLs to open".into());
    }
    let roots: Vec<PathBuf> = ask_json(calls, "roots", json!({}), Vec::new())?
        .get("roots")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(PathBuf::from).collect())
        .unwrap_or_default();
    let mut items = Vec::new();
    for p in paths {
        if p.contains("://") {
            items.push(MediaItem::from_url(p).ok_or_else(|| {
                format!("can't open {p}: only http(s) and streaming URLs (rtsp, rtmp, srt, udp) are supported")
            })?);
            continue;
        }
        let path = check_path(p, &roots)?;
        if path.is_file() && !tessera_core::media::is_video_file(&path) {
            return Err(format!("{p} isn't a video file tessera recognises"));
        }
        let found = tessera_core::media::collect_items(&[path]);
        if found.is_empty() {
            return Err(format!("nothing playable in {p} — it has no video files tessera recognises"));
        }
        items.extend(found);
    }
    Ok(items)
}

/// `~/x` as a path under `$HOME`; anything else as given.
pub fn expand_home(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map(|h| PathBuf::from(h).join(rest)).unwrap_or_else(|| PathBuf::from(p)),
        None => PathBuf::from(p),
    }
}

/// Resolves a requested local path and checks it lies under one of `roots`.
/// Shared with the MCP shim, which checks before starting the app with it.
pub fn check_path(p: &str, roots: &[PathBuf]) -> Result<PathBuf, String> {
    let expanded = expand_home(p);
    // Canonical, so `..` and symlinks can't walk out of a root.
    let path = std::fs::canonicalize(&expanded).map_err(|_| format!("{p} doesn't exist"))?;
    if roots.iter().any(|r| path.starts_with(r)) {
        return Ok(path);
    }
    let shown: Vec<String> = roots.iter().map(|r| r.display().to_string()).collect();
    Err(format!(
        "{p} is outside the media folders ({}). Ask the user to add its folder as a source in tessera (the browser's sources tab, S)",
        if shown.is_empty() { "none configured".to_string() } else { shown.join(", ") }
    ))
}

/// Checks a folder offered as a new source: real, a directory, not the
/// root or a system tree, and with videos in it somewhere. Returns the
/// canonical path and how many videos it holds. Runs on the connection
/// thread — the count is a full walk, which on a NAS can take a while.
///
/// The checks matter beyond tidiness: a folder source is also a root that
/// `tessera_open` may play from, so this is what keeps an assistant from
/// widening that to the whole disk.
fn check_source_folder(p: &str) -> Result<(PathBuf, usize), String> {
    let expanded = expand_home(p);
    let dir = std::fs::canonicalize(&expanded).map_err(|_| format!("{p} doesn't exist"))?;
    if !dir.is_dir() {
        return Err(format!("{p} is a file — a source is a folder"));
    }
    const SYSTEM: &[&str] = &["/proc", "/sys", "/dev", "/etc", "/boot", "/usr", "/bin", "/sbin", "/lib", "/lib64", "/var", "/run", "/root", "/snap", "/tmp"];
    if dir == Path::new("/") || SYSTEM.iter().any(|s| dir.starts_with(s)) {
        return Err(format!("{} is a system folder — pick the folder the videos are in", dir.display()));
    }
    let found = tessera_core::media::collect_items(std::slice::from_ref(&dir)).len();
    if found == 0 {
        return Err(format!("no videos found in {} (subfolders included)", dir.display()));
    }
    Ok((dir, found))
}

/// Where opening is allowed: every folder source, plus the user's videos
/// folder. Canonicalised, to compare with canonical paths.
pub fn allowed_roots(sources: &[tessera_core::sources::Source]) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = sources
        .iter()
        .filter_map(|s| match s {
            tessera_core::sources::Source::Folder { path } => Some(PathBuf::from(path)),
            _ => None,
        })
        .collect();
    if let Some(home) = std::env::var_os("HOME") {
        let videos = std::env::var_os("XDG_VIDEOS_DIR").map(PathBuf::from).unwrap_or_else(|| Path::new(&home).join("Videos"));
        roots.push(videos);
    }
    let mut out: Vec<PathBuf> = roots.into_iter().filter_map(|r| std::fs::canonicalize(r).ok()).collect();
    out.dedup();
    out
}

enum Encoding {
    Jpeg { long_side: u32 },
    Png { scale: f64 },
}

/// Flips the GL readback upright, scales it, and returns
/// `{mime, data (base64), width, height}`.
fn encode(img: &RawImage, how: Encoding) -> Result<Value, String> {
    use base64::Engine;
    let stride = img.w as usize * 3;
    let mut upright = Vec::with_capacity(img.rgb.len());
    for row in img.rgb.chunks_exact(stride).rev() {
        upright.extend_from_slice(row);
    }
    let rgb = image::RgbImage::from_raw(img.w, img.h, upright).ok_or("internal: bad frame buffer")?;
    let factor = match how {
        Encoding::Jpeg { long_side } => (long_side as f64 / img.w.max(img.h) as f64).min(1.0),
        Encoding::Png { scale } => scale,
    };
    let rgb = if factor < 1.0 {
        let (w, h) = (((img.w as f64 * factor).round() as u32).max(1), ((img.h as f64 * factor).round() as u32).max(1));
        image::imageops::resize(&rgb, w, h, image::imageops::FilterType::Triangle)
    } else {
        rgb
    };
    let mut bytes = Vec::new();
    let mime = match how {
        Encoding::Jpeg { .. } => {
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 82)
                .encode_image(&rgb)
                .map_err(|e| e.to_string())?;
            "image/jpeg"
        }
        Encoding::Png { .. } => {
            image::DynamicImage::ImageRgb8(rgb.clone())
                .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageOutputFormat::Png)
                .map_err(|e| e.to_string())?;
            "image/png"
        }
    };
    Ok(json!({
        "mime": mime,
        "data": base64::engine::general_purpose::STANDARD.encode(&bytes),
        "width": rgb.width(),
        "height": rgb.height(),
    }))
}

/// Plays a list of pointer steps into a window, paced so each lands in its
/// own frame — egui reads the pointer a frame late, and the app resolves
/// clicks a frame late on purpose (see CLAUDE.md), so a press and release
/// in one frame would be a different gesture from a real click.
fn dev_input(params: &Value, calls: &mpsc::Sender<Call>, injector: &Injector) -> Result<Value, String> {
    if std::env::var(DEV_INPUT_ENV).as_deref() != Ok("1") {
        return Err("synthetic input is off in this instance — only one started by tessera_dev_launch accepts it".into());
    }
    let window = ask_json(calls, "window_id", params.clone(), Vec::new())?;
    let window = window.get("id").and_then(Value::as_u64).ok_or_else(|| {
        window.get("error").and_then(Value::as_str).unwrap_or("no such window").to_string()
    })?;
    let proxy = injector.lock().ok().and_then(|g| g.clone()).ok_or("the event loop isn't running yet")?;
    let send = |synth: Synth| proxy.send_event(Injected { window, synth }).map_err(|_| "tessera is shutting down".to_string());
    let pause = |ms: u64| std::thread::sleep(Duration::from_millis(ms));
    let xy = |v: &Value| -> Result<(f64, f64), String> {
        match v.as_array().map(|a| a.as_slice()) {
            Some([x, y]) => Ok((x.as_f64().ok_or("bad x")?, y.as_f64().ok_or("bad y")?)),
            _ => Err(format!("expected [x, y], got {v}")),
        }
    };
    let steps = params.get("steps").and_then(Value::as_array).ok_or("give \"steps\": a list of input steps")?;
    // A real click only lands after real motion into the target: approach
    // from a few pixels away first.
    let approach = |x: f64, y: f64| -> Result<(), String> {
        send(Synth::Move(x + 4.0, y + 4.0))?;
        pause(50);
        send(Synth::Move(x, y))?;
        pause(50);
        Ok(())
    };
    for step in steps {
        let obj = step.as_object().ok_or("each step is an object, e.g. {\"click\": [100, 40]}")?;
        let (kind, arg) = obj.iter().next().ok_or("empty step")?;
        match kind.as_str() {
            "move" => {
                let (x, y) = xy(arg)?;
                send(Synth::Move(x, y))?;
                pause(50);
            }
            "click" | "right_click" => {
                let (x, y) = xy(arg)?;
                let right = kind == "right_click";
                approach(x, y)?;
                send(Synth::Button { pressed: true, right })?;
                pause(80);
                send(Synth::Button { pressed: false, right })?;
                pause(80);
            }
            "down" => {
                let (x, y) = xy(arg)?;
                approach(x, y)?;
                send(Synth::Button { pressed: true, right: false })?;
                pause(80);
            }
            "up" => {
                if let Ok((x, y)) = xy(arg) {
                    send(Synth::Move(x, y))?;
                    pause(50);
                }
                send(Synth::Button { pressed: false, right: false })?;
                pause(80);
            }
            "drag" => {
                let pair = arg.as_array().filter(|a| a.len() == 2).ok_or("drag takes [[x1, y1], [x2, y2]]")?;
                let ((x1, y1), (x2, y2)) = (xy(&pair[0])?, xy(&pair[1])?);
                approach(x1, y1)?;
                send(Synth::Button { pressed: true, right: false })?;
                pause(80);
                for i in 1..=10 {
                    let t = i as f64 / 10.0;
                    send(Synth::Move(x1 + (x2 - x1) * t, y1 + (y2 - y1) * t))?;
                    pause(30);
                }
                pause(60);
                send(Synth::Button { pressed: false, right: false })?;
                pause(80);
            }
            "scroll" => {
                let dy = arg.as_f64().ok_or("scroll takes a number of notches (+ is up)")?;
                send(Synth::Wheel(dy as f32))?;
                pause(50);
            }
            "wait" => pause(arg.as_u64().ok_or("wait takes milliseconds")?.min(10_000)),
            other => return Err(format!("unknown step {other:?} — use move, click, right_click, down, up, drag, scroll, wait")),
        }
    }
    // Let the last step's frame land before the caller looks.
    pause(100);
    Ok(json!({ "ok": true, "steps": steps.len() }))
}

// ---------------------------------------------------------------------------
// Event loop
// ---------------------------------------------------------------------------

/// Answers one call on the event loop thread.
pub fn handle(
    call: Call,
    wall: &mut Wall,
    settings: &mut UiSettings,
    cursor_over: Option<winit::window::WindowId>,
    fx: &mut Effects,
) {
    let Call { method, params, items, reply } = call;
    let out = match dispatch(&method, &params, items, wall, settings, cursor_over, fx) {
        Ok(r) => r,
        Err(e) => Reply::Json(json!({ "ok": false, "error": e })),
    };
    let _ = reply.send(out);
}

fn ok(mut v: Value) -> Reply {
    v["ok"] = json!(true);
    Reply::Json(v)
}

fn dispatch(
    method: &str,
    p: &Value,
    items: Vec<MediaItem>,
    wall: &mut Wall,
    settings: &mut UiSettings,
    cursor_over: Option<winit::window::WindowId>,
    fx: &mut Effects,
) -> Result<Reply, String> {
    let s = |k: &str| p.get(k).and_then(Value::as_str);
    match method {
        "roots" => {
            let roots: Vec<String> = allowed_roots(&settings.sources).iter().map(|r| r.display().to_string()).collect();
            Ok(ok(json!({ "roots": roots })))
        }
        "status" => Ok(ok(status(wall, settings, cursor_over))),
        "window" => {
            let action = s("action").unwrap_or("list");
            let monitor = monitor_arg(wall, p)?;
            match action {
                "list" => Ok(ok(windows_json(wall))),
                "new" => {
                    let fullscreen = p.get("fullscreen").and_then(Value::as_bool) == Some(true);
                    fx.new_wall_window = Some(Placement { monitor, fullscreen });
                    Ok(ok(json!({ "opening": true, "monitor": monitor, "fullscreen": fullscreen })))
                }
                "fullscreen" | "windowed" | "move" => {
                    if action == "move" && monitor.is_none() {
                        return Err("move needs \"monitor\" — see action=list for the monitors".into());
                    }
                    let wi = match p.get("window").and_then(Value::as_u64) {
                        Some(w) => window_index(wall, p).map_err(|_| format!("no window {w}"))?,
                        // The window holding the focused tile, else the
                        // first wall window.
                        None => focused_tile(wall, cursor_over)
                            .and_then(|id| locate(wall, id).map(|(si, _)| si))
                            .or_else(|| wall.screens.iter().position(|s| s.role == ScreenRole::Wall))
                            .ok_or("there is no wall window — open something with tessera_open first")?,
                    };
                    let fs = match action {
                        "fullscreen" => Some(true),
                        "windowed" => Some(false),
                        _ => None,
                    };
                    place_window(wall, wi, monitor, fs)?;
                    Ok(ok(json!({ "window": wi, "monitor": monitor.or(monitor_of(&wall.screens[wi])), "fullscreen": fs.unwrap_or_else(|| crate::platform::is_fullscreen(&wall.screens[wi].window)) })))
                }
                other => Err(format!("unknown window action {other:?} — use list, fullscreen, windowed, move or new")),
            }
        }
        "sources" => match s("action").unwrap_or("list") {
            "list" => {
                let list: Vec<Value> = settings
                    .sources
                    .iter()
                    .map(|src| {
                        let kind = match src {
                            tessera_core::sources::Source::Folder { .. } => "folder",
                            tessera_core::sources::Source::Plex { .. } => "plex",
                            tessera_core::sources::Source::Jellyfin { .. } => "jellyfin",
                            tessera_core::sources::Source::Cameras { .. } => "cameras",
                        };
                        // The label never carries a token (see Source::label).
                        json!({ "kind": kind, "label": src.label() })
                    })
                    .collect();
                Ok(ok(json!({ "sources": list })))
            }
            "add" => {
                let path = s("path").ok_or("add needs \"path\"")?.to_string();
                // Compare as real paths: "/x/video/" and "/x/video" are one
                // source.
                let same = settings.sources.iter().any(|src| match src {
                    tessera_core::sources::Source::Folder { path: have } => {
                        std::fs::canonicalize(have).map(|h| h == Path::new(&path)).unwrap_or(false)
                    }
                    _ => false,
                });
                if same {
                    return Err(format!("{path} is already a source"));
                }
                let src = tessera_core::sources::Source::Folder { path: path.clone() };
                println!("settings: added source \"{}\" (over the API)", src.label());
                let label = src.label();
                // Persisted by the frame's settings diff, like the sources tab.
                settings.sources.push(src);
                Ok(ok(json!({ "added": label, "videos": p.get("videos") })))
            }
            other => Err(format!("unknown sources action {other:?} — use list or add")),
        },
        "view" => {
            let (si, id) = resolve_tile(wall, p, cursor_over)?;
            view(wall, si, id, p)
        }
        "control" => {
            let (si, id) = resolve_tile(wall, p, cursor_over)?;
            let action = s("action").ok_or("give \"action\"")?;
            let value = p.get("value").map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            });
            control(wall, settings, si, id, action, value.as_deref())
        }
        "open" => {
            let first_uri = items.first().map(|m| m.uri.clone()).ok_or("nothing to open")?;
            let count = items.len();
            let fullscreen = p.get("fullscreen").and_then(Value::as_bool);
            let new_tile = p.get("new_tile").and_then(Value::as_bool) == Some(true);
            // A monitor asked for: a wall window already there, or a new
            // one opened there — with the file at the head of the queue, so
            // its first tile is this file.
            let on_monitor = match monitor_arg(wall, p)? {
                Some(n) => {
                    let here = |want_tiles: bool| {
                        wall.screens.iter().position(|s| {
                            s.role == ScreenRole::Wall && monitor_of(s) == Some(n) && (!want_tiles || !s.tiles.is_empty())
                        })
                    };
                    match here(true).or_else(|| here(false)) {
                        Some(wi) => Some(wi),
                        None => {
                            let first_idx = wall.queue.len();
                            wall.queue.append_items(items);
                            wall.queue.move_pending_to_front(first_idx);
                            fx.new_wall_window = Some(Placement { monitor: Some(n), fullscreen: fullscreen == Some(true) });
                            return Ok(ok(json!({ "opening": format!("a new window on monitor {n}"), "items": count })));
                        }
                    }
                }
                None => None,
            };
            let target = match (p.get("tile").and_then(Value::as_u64), on_monitor) {
                (Some(tid), _) => {
                    let (si, id) = resolve_tile(wall, &json!({ "tile": tid }), cursor_over)?;
                    OpenTarget::Window { window: wall.screens[si].window.id(), tile: Some(id) }
                }
                (None, Some(wi)) if new_tile => OpenTarget::NewTile(Some(wall.screens[wi].window.id())),
                (None, Some(wi)) => OpenTarget::Window { window: wall.screens[wi].window.id(), tile: None },
                (None, None) if new_tile => OpenTarget::NewTile(None),
                (None, None) => OpenTarget::Anywhere,
            };
            if open_listed(wall, target, items) {
                fx.new_wall_window = Some(Placement { monitor: None, fullscreen: fullscreen == Some(true) });
                return Ok(ok(json!({ "opening": "a new wall window", "items": count })));
            }
            // Fullscreen the window it landed in (W), if asked.
            if let Some(want) = fullscreen {
                let landed = wall.screens.iter().position(|s| s.tiles.iter().any(|t| t.item.uri == first_uri));
                if let Some(wi) = landed {
                    place_window(wall, wi, None, Some(want))?;
                }
            }
            let unmuted = p.get("unmuted").and_then(Value::as_bool) == Some(true);
            let found = wall
                .screens
                .iter()
                .enumerate()
                .flat_map(|(si, s)| s.tiles.iter().map(move |t| (si, t)))
                .find(|(_, t)| t.item.uri == first_uri)
                .map(|(si, t)| (si, t.id));
            match found {
                Some((si, id)) => {
                    if unmuted {
                        if let Some(t) = wall.screens[si].tiles.iter().find(|t| t.id == id) {
                            t.set_muted(false);
                        }
                    }
                    let tile = tile_json(wall, si, id, focused_tile(wall, cursor_over));
                    Ok(ok(json!({ "tile": tile, "items": count })))
                }
                None => Ok(ok(json!({
                    "queued": count,
                    "note": "couldn't start a tile (decode budget or tile limit) — it is first in the queue",
                }))),
            }
        }
        "queue" => match s("action").unwrap_or("list") {
            "list" => {
                let limit = p.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize;
                let pending: Vec<Value> = wall
                    .queue
                    .pending(limit)
                    .into_iter()
                    .map(|(i, m)| json!({ "index": i, "title": display_title(m) }))
                    .collect();
                let total_pending = wall.queue.pending(usize::MAX).len();
                Ok(ok(json!({
                    "pending": pending,
                    "pending_total": total_pending,
                    "last_started": wall.queue.current().map(display_title),
                })))
            }
            "add" => {
                let count = items.len();
                wall.queue.append_items(items);
                Ok(ok(json!({ "added": count, "pending_total": wall.queue.pending(usize::MAX).len() })))
            }
            "next" | "previous" => {
                let (_, id) = resolve_tile(wall, p, cursor_over)?;
                let forward = s("action") == Some("next");
                if !tile_next(wall.screens, wall.queue, id, forward) {
                    return Err("the queue is empty — add something with tessera_queue action=add".into());
                }
                let (si, id) = locate(wall, id).ok_or("internal: tile vanished")?;
                Ok(ok(json!({ "tile": tile_json(wall, si, id, focused_tile(wall, cursor_over)) })))
            }
            other => Err(format!("unknown queue action {other:?} — use list, add, next or previous")),
        },
        "close" => {
            let (_, id) = resolve_tile(wall, p, cursor_over)?;
            let title = locate(wall, id).map(|(si, id)| tile_title(wall, si, id)).unwrap_or_default();
            remove_tile_action(wall.screens, wall.budget, wall.overlay, id);
            let left = wall.screens.iter().map(|s| s.tiles.len()).sum::<usize>();
            Ok(ok(json!({ "closed": id, "title": title, "tiles_left": left })))
        }
        "subtitle_info" => {
            let (si, id) = resolve_tile(wall, p, cursor_over)?;
            let tile = wall.screens[si].tiles.iter_mut().find(|t| t.id == id).ok_or("internal: tile vanished")?;
            tile.refresh_tracks();
            let tracks: Vec<Value> = tile
                .subtitle_tracks
                .iter()
                .map(|t| {
                    json!({
                        "id": t.id, "lang": t.lang, "title": t.title, "codec": t.codec,
                        "selected": t.selected, "ff_index": t.ff_index, "external": t.external_filename,
                    })
                })
                .collect();
            Ok(ok(json!({
                "tile": id,
                "title": display_title(&tile.item),
                // The real URI, which can carry a server token: for the
                // shim's ffmpeg, never to be shown.
                "uri": tile.item.uri,
                "live": tile.item.is_live(),
                "position": tile.time_pos().max(0.0),
                "duration": tile.duration(),
                "sub_text": tile.sub_text(),
                "tracks": tracks,
            })))
        }
        "subtitle_select" => {
            let (si, id) = resolve_tile(wall, p, cursor_over)?;
            let track = p.get("track").and_then(Value::as_i64);
            handle_overlay_action(wall, OverlayAction::SetSubtitleTrack { tile_id: id, track }, si);
            Ok(ok(json!({ "tile": id, "track": track })))
        }
        "colors" => {
            let (si, id) = resolve_tile(wall, p, cursor_over)?;
            let dim = |k: &str, d: u64, max: u64| p.get(k).and_then(Value::as_u64).unwrap_or(d).clamp(1, max) as u32;
            let (w, h) = (dim("w", 16, 64), dim("h", 9, 36));
            let (tx, rx) = mpsc::channel();
            let meta = json!({
                "ok": true, "tile": id, "w": w, "h": h,
                "paused": wall.screens[si].tiles.iter().find(|t| t.id == id).map(|t| t.is_paused()),
            });
            wall.screens[si].captures.push(Capture { what: CaptureWhat::TileColors { id, w, h }, reply: tx });
            Ok(Reply::Capture { image: rx, meta })
        }
        "frame" => {
            let (si, id) = resolve_tile(wall, p, cursor_over)?;
            let (tx, rx) = mpsc::channel();
            let meta = json!({
                "ok": true, "tile": id, "title": tile_title(wall, si, id),
                "position": wall.screens[si].tiles.iter().find(|t| t.id == id).map(|t| fmt_hms(t.time_pos())),
            });
            wall.screens[si].captures.push(Capture { what: CaptureWhat::Tile(id), reply: tx });
            Ok(Reply::Capture { image: rx, meta })
        }
        // ---- development ----
        "screenshot" => {
            let wi = window_index(wall, p)?;
            let overlay = p.get("overlay").and_then(Value::as_bool).unwrap_or(true);
            let (tx, rx) = mpsc::channel();
            let size = wall.screens[wi].window.inner_size();
            let meta = json!({
                "ok": true, "window": wi, "role": role_name(wall.screens[wi].role),
                "window_width": size.width, "window_height": size.height,
            });
            wall.screens[wi].captures.push(Capture { what: CaptureWhat::Window { overlay }, reply: tx });
            Ok(Reply::Capture { image: rx, meta })
        }
        "window_id" => {
            let wi = window_index(wall, p)?;
            Ok(ok(json!({ "id": u64::from(wall.screens[wi].window.id()) })))
        }
        "state" => Ok(ok(dev_state(wall, settings, cursor_over))),
        "quit" => {
            fx.quit = true;
            Ok(ok(json!({ "quitting": true })))
        }
        other => Err(format!("unknown method {other:?}")),
    }
}

/// Where the zoomed window is (`x`, `y`: its centre, % of the cropped
/// picture), how far in (`zoom`), over `duration_ms` (animated) or at once,
/// and what is drawn over the picture (`grid`, `reticle` as [x0, y0, x1, y1] %
/// of the whole picture or null, `readout`). Moves start from where the view
/// is heading, so a stream of small moves glides.
fn view(wall: &mut Wall, si: usize, id: usize, p: &Value) -> Result<Reply, String> {
    let num = |k: &str| p.get(k).and_then(Value::as_f64).map(|v| v as f32).filter(|v| v.is_finite());
    let s = &mut wall.screens[si];
    let t = s.tiles.iter_mut().find(|t| t.id == id).ok_or("internal: tile vanished")?;
    let mut next = t.view_target();
    let mut moved = false;
    if let Some(x) = num("x") {
        next.pan_x = (x / 100.0).clamp(0.0, 1.0);
        moved = true;
    }
    if let Some(y) = num("y") {
        next.pan_y = (y / 100.0).clamp(0.0, 1.0);
        moved = true;
    }
    if let Some(z) = num("zoom") {
        next.zoom = z.clamp(1.0, tessera_core::view::MAX_ZOOM);
        moved = true;
    }
    if moved {
        let secs = num("duration_ms").unwrap_or(0.0).clamp(0.0, 10_000.0) / 1000.0;
        t.set_view_animated(next, secs);
    }
    let mut hud = t.hud().cloned().unwrap_or_default();
    let mut touched = false;
    if let Some(g) = p.get("grid").and_then(Value::as_bool) {
        hud.grid = g;
        touched = true;
    }
    match p.get("reticle") {
        Some(Value::Null) => {
            hud.reticle = None;
            touched = true;
        }
        Some(Value::Array(a)) => {
            let v: Vec<f32> = a.iter().filter_map(Value::as_f64).map(|v| (v as f32 / 100.0).clamp(0.0, 1.0)).collect();
            if v.len() != 4 {
                return Err("reticle needs [x0, y0, x1, y1] in % of the picture (or null to remove it)".into());
            }
            hud.reticle = Some((v[0].min(v[2]), v[1].min(v[3]), v[0].max(v[2]), v[1].max(v[3])));
            touched = true;
        }
        Some(_) => return Err("reticle needs [x0, y0, x1, y1] in % of the picture (or null to remove it)".into()),
        None => {}
    }
    if let Some(r) = p.get("readout").and_then(Value::as_str) {
        hud.readout = r.chars().take(80).collect();
        touched = true;
    }
    if touched {
        t.set_hud((!hud.is_empty()).then_some(hud));
    }
    let (si, id) = locate(wall, id).ok_or("internal: tile vanished")?;
    Ok(ok(json!({ "tile": tile_json(wall, si, id, Some(id)) })))
}

fn control(
    wall: &mut Wall,
    settings: &UiSettings,
    si: usize,
    id: usize,
    action: &str,
    value: Option<&str>,
) -> Result<Reply, String> {
    let tile = |wall: &Wall| wall.screens[si].tiles.iter().find(|t| t.id == id).map(|t| (t.is_paused(), t.item.is_live()));
    let (paused, live) = tile(wall).ok_or("internal: tile vanished")?;
    let mut seek_to = None;
    match action {
        "play" | "resume" => {
            if paused {
                resume_tile(wall.screens, wall.budget, id);
                if tile(wall).is_some_and(|(p, _)| p) {
                    return Err("couldn't resume: the decode budget is full — pause or close another tile first".into());
                }
            }
        }
        "pause" => {
            if !paused {
                pause_and_release(wall.screens, wall.budget, id, "paused by request");
            }
        }
        "toggle" => handle_overlay_action(wall, OverlayAction::TogglePause { tile_id: id }, si),
        "seek" => {
            if live {
                return Err("this is a live stream — there is nothing to seek in".into());
            }
            let value = value.ok_or("seek needs a \"value\": 90, 01:02:03, +30, -10, or a chapter name")?;
            let t = wall.screens[si].tiles.iter().find(|t| t.id == id).ok_or("internal: tile vanished")?;
            let target = resolve_seek(value, t.time_pos().max(0.0), t.duration(), &t.chapters())?;
            t.seek(target, true);
            seek_to = Some(target);
        }
        "volume" => {
            let value = value.ok_or("volume needs a \"value\": 0-130, or +10 / -10")?;
            let t = wall.screens[si].tiles.iter().find(|t| t.id == id).ok_or("internal: tile vanished")?;
            let v = value.trim();
            let new = match v.strip_prefix('+').or_else(|| v.starts_with('-').then_some(v)) {
                Some(delta) => t.volume() + delta.trim_end_matches('%').parse::<i64>().map_err(|_| format!("bad volume {v:?}"))?,
                None => v.trim_end_matches('%').parse::<i64>().map_err(|_| format!("bad volume {v:?} — use 0-130, +10 or -10"))?,
            };
            t.set_volume(new);
        }
        "mute" | "unmute" => {
            if !settings.audio {
                return Err("audio is turned off in tessera (config tab, or --no-audio) — the user has to turn it on".into());
            }
            wall.overlay.solo_id = None;
            if let Some(t) = wall.screens[si].tiles.iter().find(|t| t.id == id) {
                t.set_muted(action == "mute");
            }
        }
        "next" | "previous" => {
            if !tile_next(wall.screens, wall.queue, id, action == "next") {
                return Err("the queue is empty — add something with tessera_queue action=add".into());
            }
        }
        "crop" | "zoom" => {
            let value = value.ok_or_else(|| {
                if action == "crop" {
                    "crop needs a \"value\": auto, off, fill, an aspect (2.39:1), or edges (top,bottom in %)".to_string()
                } else {
                    "zoom needs a \"value\": 1.5, +0.25, -0.25, in, out or off".to_string()
                }
            })?;
            let s = &mut wall.screens[si];
            // The cell this tile is drawn in, for `fill`.
            let pos = s.tiles.iter().position(|t| t.id == id).ok_or("internal: tile vanished")?;
            let cell_aspect = s.effective_layout().get(pos).filter(|r| r.w > 0.0 && r.h > 0.0).map_or(16.0 / 9.0, |r| r.w / r.h);
            let t = &mut s.tiles[pos];
            let view = t.view();
            let next = if action == "crop" {
                view.with_crop(tessera_core::view::parse_crop(value, t.video_aspect(), cell_aspect)?)
            } else {
                tessera_core::view::View { zoom: tessera_core::view::parse_zoom(value, view.zoom)?, ..view }
            };
            t.set_view(next);
            println!("screen {} tile {id}: {}", s.monitor_index, next.describe());
        }
        "pin" => handle_overlay_action(wall, OverlayAction::TogglePin { tile_id: id }, si),
        "fullscreen" => handle_overlay_action(wall, OverlayAction::FullscreenToggle { tile_id: id }, si),
        other => {
            return Err(format!(
                "unknown action {other:?} — use play, pause, toggle, seek, volume, mute, unmute, crop, zoom, next, previous, pin or fullscreen"
            ))
        }
    }
    let (si, id) = locate(wall, id).ok_or("internal: tile vanished")?;
    let mut out = tile_json(wall, si, id, Some(id));
    // A seek is asynchronous; report where it is going, not where mpv
    // still is this instant.
    if let Some(t) = seek_to {
        out["position"] = json!(fmt_hms(t));
        out["position_seconds"] = json!(t.floor());
    }
    Ok(ok(json!({ "tile": out })))
}

// ---------------------------------------------------------------------------
// Tiles
// ---------------------------------------------------------------------------

fn locate(wall: &Wall, id: usize) -> Option<(usize, usize)> {
    wall.screens.iter().position(|s| s.tiles.iter().any(|t| t.id == id)).map(|si| (si, id))
}

/// The tile a request means when it names none: the only one; the one
/// filling a window (F); the only one you can hear; the one under the
/// pointer. Anything else is ambiguous, and saying so beats guessing.
fn focused_tile(wall: &Wall, cursor_over: Option<winit::window::WindowId>) -> Option<usize> {
    let all: Vec<(usize, &crate::tile::Tile)> = wall
        .screens
        .iter()
        .enumerate()
        .filter(|(_, s)| s.role == ScreenRole::Wall)
        .flat_map(|(si, s)| s.tiles.iter().map(move |t| (si, t)))
        .collect();
    if all.len() == 1 {
        return Some(all[0].1.id);
    }
    let zoomed: Vec<usize> = wall.screens.iter().filter_map(|s| s.zoomed).collect();
    if zoomed.len() == 1 {
        return Some(zoomed[0]);
    }
    let audible: Vec<usize> = all.iter().filter(|(_, t)| t.audio_enabled() && !t.is_muted()).map(|(_, t)| t.id).collect();
    if audible.len() == 1 {
        return Some(audible[0]);
    }
    let s = wall.screens.iter().find(|s| Some(s.window.id()) == cursor_over)?;
    s.cursor.and_then(|(x, y)| s.tile_at(x, y))
}

fn resolve_tile(wall: &Wall, p: &Value, cursor_over: Option<winit::window::WindowId>) -> Result<(usize, usize), String> {
    let list = || {
        wall.screens
            .iter()
            .enumerate()
            .flat_map(|(si, s)| s.tiles.iter().map(move |t| (si, t.id)))
            .map(|(si, id)| format!("{id} ({})", tile_title(wall, si, id)))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if !wall.screens.iter().any(|s| !s.tiles.is_empty()) {
        return Err("nothing is playing — open something with tessera_open first".into());
    }
    let id = match p.get("tile").filter(|v| !v.is_null()) {
        Some(v) => v.as_u64().ok_or("\"tile\" is a tile id number from tessera_status")? as usize,
        None => focused_tile(wall, cursor_over)
            .ok_or_else(|| format!("several tiles are playing — say which with \"tile\": {}", list()))?,
    };
    locate(wall, id).ok_or_else(|| format!("there is no tile {id} — tiles are: {}", list()))
}

fn display_title(m: &MediaItem) -> String {
    let parsed = tessera_core::title::parse_title(&m.title);
    if parsed.title.is_empty() {
        m.title.clone()
    } else {
        parsed.display()
    }
}

fn tile_title(wall: &Wall, si: usize, id: usize) -> String {
    wall.screens[si].tiles.iter().find(|t| t.id == id).map(|t| display_title(&t.item)).unwrap_or_default()
}

/// Strips anything after `?` — Plex and Jellyfin direct-play URLs carry
/// their access token there, and this goes to a language model.
fn redact(uri: &str) -> &str {
    uri.split('?').next().unwrap_or(uri)
}

fn tile_json(wall: &Wall, si: usize, id: usize, focused: Option<usize>) -> Value {
    let s = &wall.screens[si];
    let Some(t) = s.tiles.iter().find(|t| t.id == id) else { return Value::Null };
    let parsed = tessera_core::title::parse_title(&t.item.title);
    let pos = t.time_pos();
    let chapters = t.chapters();
    let chapter = t.chapter().and_then(|i| chapters.get(i).map(|c| (i, c)));
    json!({
        "id": t.id,
        "title": display_title(&t.item),
        "show_or_film": parsed.title,
        "season": parsed.season,
        "episode": parsed.episode,
        "year": parsed.year,
        "file": t.item.title,
        "path": redact(&t.item.uri),
        "source": t.item.kind.label(),
        "live": t.item.is_live(),
        "position": (pos >= 0.0).then(|| fmt_hms(pos)),
        "position_seconds": (pos >= 0.0).then(|| pos.floor()),
        "duration": t.duration().map(fmt_hms),
        "chapter": chapter.map(|(i, (_, name))| json!({ "number": i + 1, "of": chapters.len(), "title": name })),
        "paused": t.is_paused(),
        "muted": !t.audio_enabled() || t.is_muted(),
        "volume": t.volume(),
        "window": si,
        "monitor": monitor_of(s),
        "monitor_count": crate::windowing::available_monitors(&s.window).count(),
        "pinned": s.pinned == Some(t.id),
        "fullscreen": s.zoomed == Some(t.id),
        "focused": focused == Some(t.id),
        "view": (!t.view().is_full()).then(|| t.view().describe()),
    })
}

fn status(wall: &Wall, settings: &UiSettings, cursor_over: Option<winit::window::WindowId>) -> Value {
    let focused = focused_tile(wall, cursor_over);
    let tiles: Vec<Value> = wall
        .screens
        .iter()
        .enumerate()
        .flat_map(|(si, s)| s.tiles.iter().map(move |t| (si, t.id)))
        .map(|(si, id)| tile_json(wall, si, id, focused))
        .collect();
    let next: Vec<String> = wall.queue.pending(3).into_iter().map(|(_, m)| display_title(m)).collect();
    json!({
        "tiles": tiles,
        "focused": focused,
        "wall_windows": wall.screens.iter().filter(|s| s.role == ScreenRole::Wall).count(),
        "audio": settings.audio,
        "queue": { "pending": wall.queue.pending(usize::MAX).len(), "next": next },
    })
}

/// The monitor a window is on now — by where it actually is, since a
/// window can be dragged — falling back to the one it was put on.
fn monitor_of(s: &crate::screen::Screen) -> Option<usize> {
    monitor_under_window(&s.window).map(|(i, _)| i).or(Some(s.monitor_index))
}

/// The `monitor` argument, if given: a number, a connector name ("DP-4")
/// or a position ("left"), resolved against the monitors there are now.
fn monitor_arg(wall: &Wall, p: &Value) -> Result<Option<usize>, String> {
    let spec = match p.get("monitor") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
    };
    let infos = wall.screens.first().map(|s| crate::windowing::monitor_infos(crate::windowing::available_monitors(&s.window))).unwrap_or_default();
    tessera_core::monitor::resolve_monitor(&spec, &infos).map(Some)
}

/// Monitors (with which windows are on each) and windows.
fn windows_json(wall: &Wall) -> Value {
    let Some(first) = wall.screens.first() else { return json!({ "monitors": [], "windows": [] }) };
    let infos = crate::windowing::monitor_infos(crate::windowing::available_monitors(&first.window));
    let monitors: Vec<Value> = infos
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let on: Vec<usize> = wall.screens.iter().enumerate().filter(|(_, s)| monitor_of(s) == Some(i)).map(|(wi, _)| wi).collect();
            json!({
                "index": i, "name": m.name, "size": [m.w, m.h], "position": [m.x, m.y], "windows": on,
                // The words that name it, so a client can say "left".
                "positions": tessera_core::monitor::position_words(&infos, i),
            })
        })
        .collect();
    let windows: Vec<Value> = wall
        .screens
        .iter()
        .enumerate()
        .map(|(wi, s)| {
            let size = s.window.inner_size();
            json!({
                "index": wi,
                "role": role_name(s.role),
                "monitor": monitor_of(s),
                "fullscreen": crate::platform::is_fullscreen(&s.window),
                "size": [size.width, size.height],
                "tiles": s.tiles.iter().map(|t| json!({ "id": t.id, "title": display_title(&t.item) })).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({ "monitors": monitors, "windows": windows })
}

/// Puts window `wi` on `monitor` and/or into or out of fullscreen (the W
/// toggle). Fullscreen is borderless on the monitor; windowed is centred
/// on it at its current size.
fn place_window(wall: &mut Wall, wi: usize, monitor: Option<usize>, fullscreen: Option<bool>) -> Result<(), String> {
    let window = wall.screens[wi].window.clone();
    let target = match monitor {
        Some(n) => Some((n, crate::windowing::available_monitors(&window).nth(n).ok_or(format!("there is no monitor {n}"))?)),
        None => None,
    };
    let is_fs = crate::platform::is_fullscreen(&window);
    let want_fs = fullscreen.unwrap_or(is_fs);
    let mut settle = false;
    if want_fs {
        let s = &mut wall.screens[wi];
        if !is_fs && s.windowed_restore.is_none() {
            s.windowed_restore = Some((window.outer_position().ok(), window.inner_size()));
        }
        // Going fullscreen somewhere else: "windowed" afterwards should stay
        // there, not return to where it was before.
        if target.as_ref().is_some_and(|(n, _)| Some(*n) != monitor_of(s)) {
            s.windowed_restore = None;
        }
        // Aim at the monitor asked for, else the one it is on now (as W does).
        let m = match target {
            Some((n, m)) => Some((n, m)),
            None => monitor_under_window(&window),
        };
        if let Some((n, m)) = &m {
            s.monitor_index = *n;
            s.intended_monitor = Some(m.clone());
        }
        // A floating wall stops floating, as with W. If the WM let go of
        // it to do that, the fullscreen request waits for
        // `fix_displaced_walls` (`should_fullscreen` is set).
        let remapped = unfloat_for_fullscreen(s);
        s.should_fullscreen = true;
        if !(remapped && s.intended_monitor.is_some()) {
            crate::platform::enter_fullscreen(&window, m.map(|(_, m)| m).or_else(|| s.intended_monitor.clone()));
        }
        settle = true;
    } else {
        if is_fs {
            settle = toggle_window_mode(wall.screens, wi);
        }
        if let Some((n, m)) = target {
            let s = &mut wall.screens[wi];
            s.monitor_index = n;
            s.intended_monitor = Some(m.clone());
            s.windowed_restore = None;
            // A request, not a guarantee: a tiling WM (pop-shell) snaps a
            // tiled window back to its slot ~0.3s after the app moves it,
            // and restores that slot when fullscreen ends, so neither a
            // move nor a fullscreen hop sticks there. What does: placement
            // when a window is created, and fullscreen on a monitor. The
            // shim reports where the window really ended up.
            let (mp, ms) = (m.position(), m.size());
            let size = window.outer_size();
            let (w, h) = (size.width.min(ms.width), size.height.min(ms.height));
            window.set_outer_position(winit::dpi::PhysicalPosition::new(
                mp.x + (ms.width - w) as i32 / 2,
                mp.y + (ms.height - h) as i32 / 2,
            ));
        }
    }
    if settle && crate::platform::WM_SETTLE {
        *wall.wm_settle_at = Some((std::time::Instant::now() + Duration::from_millis(600), 4));
    }
    Ok(())
}

fn role_name(r: ScreenRole) -> &'static str {
    match r {
        ScreenRole::Wall => "wall",
        ScreenRole::Browser => "browser",
    }
}

fn window_index(wall: &Wall, p: &Value) -> Result<usize, String> {
    let wi = p.get("window").and_then(Value::as_u64).unwrap_or(0) as usize;
    if wi < wall.screens.len() {
        Ok(wi)
    } else {
        Err(format!("no window {wi} — there are {} (see tessera_dev_state)", wall.screens.len()))
    }
}

/// Everything a developer checking a change wants in one look, minus
/// anything secret (sources by label only).
fn dev_state(wall: &Wall, settings: &UiSettings, cursor_over: Option<winit::window::WindowId>) -> Value {
    let windows: Vec<Value> = wall
        .screens
        .iter()
        .enumerate()
        .map(|(wi, s)| {
            let size = s.window.inner_size();
            let rects = s.effective_layout();
            let tiles: Vec<Value> = s
                .tiles
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let r = rects.get(i);
                    json!({
                        "id": t.id, "title": display_title(&t.item),
                        "rect": r.map(|r| [r.x, r.y, r.w, r.h]),
                        "paused": t.is_paused(), "muted": t.is_muted(),
                    })
                })
                .collect();
            json!({
                "index": wi,
                "role": role_name(s.role),
                "size": [size.width, size.height],
                "monitor": monitor_of(s),
                "position": s.window.outer_position().ok().map(|p| [p.x, p.y]),
                "fullscreen": crate::platform::is_fullscreen(&s.window),
                "floating": s.floating,
                "ui_scale": s.ui_scale,
                "cursor": s.cursor.map(|(x, y)| [x, y]),
                "pointer_here": cursor_over == Some(s.window.id()),
                "zoomed": s.zoomed,
                "pinned": s.pinned,
                "tiles": tiles,
                "browser_tab": (s.role == ScreenRole::Browser).then(|| format!("{:?}", s.browser.tab)),
            })
        })
        .collect();
    json!({
        "windows": windows,
        "overlay": {
            "visible": wall.overlay.visible,
            "pinned": wall.overlay.pinned,
            "help_open": wall.overlay.help_open.is_some(),
            "settings_open": wall.overlay.settings_open.is_some(),
            "notice": wall.overlay.notice.as_ref().map(|(m, _)| m),
            "solo": wall.overlay.solo_id,
        },
        "budget": {
            "streams": wall.budget.active_count(),
            "used_1080p30": (wall.budget.used_equivalents() * 10.0).round() / 10.0,
            "ceiling_1080p30": wall.budget.ceiling_equivalents(),
            "evictions": wall.budget.total_evictions(),
        },
        "queue": {
            "len": wall.queue.len(),
            "current_index": wall.queue.current_index(),
            "pending": wall.queue.pending(usize::MAX).len(),
        },
        "settings": {
            "audio": settings.audio,
            "ui_scale": settings.ui_scale,
            "max_streams": settings.max_streams,
            "cache_mb": settings.cache_mb,
            "retain_queue": settings.retain_queue,
            "sources": settings.sources.iter().map(|s| s.label()).collect::<Vec<_>>(),
        },
    })
}
