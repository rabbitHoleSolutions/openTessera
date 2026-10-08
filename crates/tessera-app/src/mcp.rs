//! `tessera mcp` — an MCP server on stdio for the running app.
//!
//! A thin shim: each tool call becomes one request on the single-instance
//! socket (`instance::call` → `rpc.rs`), so there is no port and no token,
//! and the client config is one line: `{ command: "tessera", args: ["mcp"] }`.
//! The shim itself never opens a window. What it does do is the slow work
//! the app shouldn't: running ffmpeg to index subtitles or grab a frame at
//! another time, and starting the app when `tessera_open` finds it closed.
//!
//! Replies are compact text (`[01:12:03] line`), not JSON: the first client
//! is a voice assistant that re-reads every token and speaks the answers.
//!
//! `--dev` adds tools for developing tessera itself, against a sandboxed
//! instance (its own HOME, so the user's settings are never touched): launch
//! and quit it, read its log, screenshot a window with the overlay, dump the
//! UI state, and click/drag/scroll by synthetic input — the xdotool dance in
//! CLAUDE.md, minus the flakiness.

use std::collections::HashMap;
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tessera_core::subs::{self, Cue};
use tessera_core::timecode::{fmt_hms, parse_time, TimeSpec};

use crate::instance::{self, CallError};

const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "tessera is the video player on this machine: a wall of tiles, each playing one video. \
Call tessera_status first when a request is ambiguous; it lists each tile's id and marks the focused one. \
Every other tool takes an optional `tile` and defaults to the focused tile, so \"pause it\" needs no lookup. \
Times are HH:MM:SS everywhere, and any timestamp a tool returns can be passed straight back to seek.";

pub fn run(args: Vec<String>) -> i32 {
    let mut dev = false;
    let mut sandbox_dir: Option<PathBuf> = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dev" => dev = true,
            "--home" => sandbox_dir = it.next().map(PathBuf::from),
            "-h" | "--help" => {
                println!(
                    "usage: tessera mcp [--dev [--home DIR]]\n\n\
                     An MCP server on stdio for the running tessera.\n\
                     --dev       add development tools, all aimed at a sandboxed instance\n\
                     --home DIR  where the sandbox lives (default $XDG_RUNTIME_DIR/tessera-dev)"
                );
                return 0;
            }
            other => {
                eprintln!("tessera mcp: unknown argument {other:?}");
                return 2;
            }
        }
    }
    let sandbox = dev.then(|| Sandbox::new(sandbox_dir));
    let config_dir = match &sandbox {
        Some(s) => s.home.join(".config/opentessera"),
        None => crate::config_dir(),
    };
    let mut server = Server { config_dir, sandbox, subs: HashMap::new() };

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => server.message(&msg),
            Err(e) => Some(json!({
                "jsonrpc": "2.0", "id": null,
                "error": { "code": -32700, "message": format!("parse error: {e}") },
            })),
        };
        if let Some(reply) = reply {
            let mut out = reply.to_string();
            out.push('\n');
            if stdout.write_all(out.as_bytes()).and_then(|_| stdout.flush()).is_err() {
                break;
            }
        }
    }
    0
}

/// One tool result's content.
enum Content {
    Text(String),
    Image { mime: String, data: String },
}

type ToolResult = Result<Vec<Content>, String>;

struct Server {
    /// Which instance: the user's, or the sandbox's.
    config_dir: PathBuf,
    sandbox: Option<Sandbox>,
    /// Subtitle tracks indexed (or being indexed) this session.
    subs: HashMap<String, SubState>,
}

enum SubState {
    Ready(Arc<Vec<Cue>>),
    Pending(mpsc::Receiver<Result<Vec<Cue>, String>>),
}

impl Server {
    /// Handles one JSON-RPC message; None for a notification.
    fn message(&mut self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
        // Notifications (initialized, cancelled) get no answer.
        let id = id?;
        let result = match method {
            "initialize" => {
                let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
                let version = if PROTOCOL_VERSIONS.contains(&asked) { asked } else { PROTOCOL_VERSIONS[0] };
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "tessera", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": INSTRUCTIONS,
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": self.tools() })),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                Ok(match self.call_tool(name, &args) {
                    Ok(content) => json!({ "content": content_json(content), "isError": false }),
                    Err(e) => json!({ "content": [{ "type": "text", "text": e }], "isError": true }),
                })
            }
            _ => Err(json!({ "code": -32601, "message": format!("method not found: {method}") })),
        };
        Some(match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e }),
        })
    }

    fn tools(&self) -> Vec<Value> {
        let mut tools = player_tools();
        if self.sandbox.is_some() {
            tools.extend(dev_tools());
        }
        tools
    }

    fn call_tool(&mut self, name: &str, a: &Value) -> ToolResult {
        match name {
            "tessera_status" => self.status(),
            "tessera_open" => self.open(a),
            "tessera_control" => {
                let mut params = a.clone();
                params["action"] = json!(str_arg(a, "action").ok_or("give \"action\"")?);
                let v = self.rpc("control", params, Duration::from_secs(8))?;
                Ok(text(tile_line(&v["tile"])))
            }
            "tessera_subtitles" => self.subtitles(a),
            "tessera_frame" => self.frame(a),
            "tessera_queue" => self.queue(a),
            "tessera_sources" => {
                let action = str_arg(a, "action").unwrap_or("list");
                let mut params = a.clone();
                params["action"] = json!(action);
                // An add walks the whole folder to count it; a NAS can be slow.
                let v = self.rpc("sources", params, Duration::from_secs(120))?;
                Ok(text(match action {
                    "add" => format!(
                        "added {} — {} video(s) found; they appear in the library, and tessera_open can now play from it",
                        v["added"].as_str().unwrap_or("?"),
                        v["videos"]
                    ),
                    _ => {
                        let list: Vec<String> = v["sources"]
                            .as_array()
                            .map(|a| a.iter().map(|s| format!("- {}", s["label"].as_str().unwrap_or("?"))).collect())
                            .unwrap_or_default();
                        if list.is_empty() {
                            "no sources configured".to_string()
                        } else {
                            format!("{} source(s):\n{}", list.len(), list.join("\n"))
                        }
                    }
                }))
            }
            "tessera_close" => {
                let v = self.rpc("close", a.clone(), Duration::from_secs(8))?;
                Ok(text(format!(
                    "closed tile {} ({}); {} tile(s) left",
                    v["closed"], v["title"].as_str().unwrap_or("?"), v["tiles_left"]
                )))
            }
            n if n.starts_with("tessera_dev_") && self.sandbox.is_some() => self.dev(n, a),
            other => Err(format!("unknown tool {other}")),
        }
    }

    /// One request to the app. Turns "not running" into what to do next.
    fn rpc(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        match instance::call(&self.config_dir, &json!({ "method": method, "params": params }), timeout) {
            Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => Ok(v),
            Ok(v) => Err(v.get("error").and_then(Value::as_str).unwrap_or("tessera refused the request").to_string()),
            Err(CallError::NotRunning) => Err(if self.sandbox.is_some() {
                "the sandbox instance isn't running — start it with tessera_dev_launch".into()
            } else {
                "tessera isn't running — open something with tessera_open to start it".into()
            }),
            Err(CallError::Failed(e)) => Err(e),
        }
    }

    fn status(&self) -> ToolResult {
        let v = self.rpc("status", json!({}), Duration::from_secs(5))?;
        let tiles = v["tiles"].as_array().cloned().unwrap_or_default();
        let mut out = Vec::new();
        if tiles.is_empty() {
            out.push("nothing is playing".to_string());
        }
        for t in &tiles {
            out.push(tile_line(t));
            if let Some(p) = t["path"].as_str() {
                out.push(format!("  file: {p}"));
            }
        }
        if tiles.len() > 1 && v["focused"].is_null() {
            out.push("no tile is focused — name one with `tile` in the other tools".into());
        }
        let pending = v["queue"]["pending"].as_u64().unwrap_or(0);
        let next: Vec<&str> = v["queue"]["next"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        out.push(if pending == 0 {
            "queue: empty".into()
        } else {
            format!("queue: {pending} pending — next: {}", next.join("; "))
        });
        if v["audio"] == json!(false) {
            out.push("audio is turned off in tessera's settings".into());
        }
        Ok(text(out.join("\n")))
    }

    fn open(&self, a: &Value) -> ToolResult {
        let paths: Vec<String> = a
            .get("paths")
            .and_then(Value::as_array)
            .map(|v| v.iter().filter_map(Value::as_str).map(str::to_owned).collect())
            .unwrap_or_default();
        if paths.is_empty() {
            return Err("give \"paths\": files, folders or URLs".into());
        }
        let mut params = a.clone();
        params["paths"] = json!(paths);
        match self.rpc("open", params.clone(), Duration::from_secs(20)) {
            Ok(v) => Ok(text(open_reply(&v))),
            Err(e) if e.starts_with("tessera isn't running") => self.launch_and_open(&paths),
            Err(e) => Err(e),
        }
    }

    /// Starts the app on `paths` — the same as opening them from a file
    /// manager — after checking them against the same roots the app would.
    fn launch_and_open(&self, paths: &[String]) -> ToolResult {
        let roots = crate::rpc::allowed_roots(&crate::load_settings().sources);
        let mut argv = Vec::new();
        for p in paths {
            if p.contains("://") {
                tessera_core::MediaItem::from_url(p)
                    .ok_or_else(|| format!("can't open {p}: only http(s) and streaming URLs are supported"))?;
                argv.push(p.clone());
            } else {
                argv.push(crate::rpc::check_path(p, &roots)?.to_string_lossy().into_owned());
            }
        }
        let log = cache_dir().join("launched.log");
        let _ = std::fs::create_dir_all(cache_dir());
        let out = std::fs::File::create(&log).map_err(|e| format!("can't write {}: {e}", log.display()))?;
        let err = out.try_clone().map_err(|e| e.to_string())?;
        let mut cmd = Command::new(own_exe());
        cmd.args(&argv).stdin(Stdio::null()).stdout(out).stderr(err);
        detach(&mut cmd);
        cmd.spawn().map_err(|e| format!("couldn't start tessera: {e}"))?;
        if !wait_until(Duration::from_secs(15), || self.rpc("status", json!({}), Duration::from_secs(1)).is_ok()) {
            return Err(format!("tessera didn't come up within 15s — is there a display? its log: {}", log.display()));
        }
        // Let the first tile load before describing it.
        wait_until(Duration::from_secs(5), || {
            self.rpc("status", json!({}), Duration::from_secs(1))
                .is_ok_and(|v| v["tiles"].as_array().is_some_and(|t| !t.is_empty()))
        });
        let mut out = vec![format!("started tessera with {} item(s)", argv.len())];
        if let Ok(Content::Text(s)) = self.status().map(|mut c| c.remove(0)) {
            out.push(s);
        }
        Ok(text(out.join("\n")))
    }

    fn queue(&self, a: &Value) -> ToolResult {
        let action = str_arg(a, "action").unwrap_or("list");
        let mut params = a.clone();
        params["action"] = json!(action);
        let v = self.rpc("queue", params, Duration::from_secs(20))?;
        Ok(text(match action {
            "list" => {
                let items: Vec<String> = v["pending"]
                    .as_array()
                    .map(|a| a.iter().enumerate().map(|(i, m)| format!("{}. {}", i + 1, m["title"].as_str().unwrap_or("?"))).collect())
                    .unwrap_or_default();
                let total = v["pending_total"].as_u64().unwrap_or(0);
                if total == 0 {
                    "the queue is empty".to_string()
                } else {
                    let more = if total as usize > items.len() { format!("\n…and {} more", total as usize - items.len()) } else { String::new() };
                    format!("{total} pending:\n{}{more}", items.join("\n"))
                }
            }
            "add" => format!("added {} — {} pending", v["added"], v["pending_total"]),
            _ => tile_line(&v["tile"]),
        }))
    }

    // ---- subtitles ----

    fn subtitles(&mut self, a: &Value) -> ToolResult {
        let action = str_arg(a, "action").unwrap_or("now");
        if action == "select" {
            let track = match a.get("track") {
                Some(Value::String(s)) if s.eq_ignore_ascii_case("off") => Value::Null,
                Some(Value::Number(n)) => json!(n),
                Some(Value::String(s)) => json!(s.parse::<i64>().map_err(|_| format!("bad track {s:?} — a track id, or \"off\""))?),
                _ => return Err("select needs \"track\": a track id from action=tracks, or \"off\"".into()),
            };
            let v = self.rpc("subtitle_select", json!({ "tile": a.get("tile"), "track": track }), Duration::from_secs(5))?;
            return Ok(text(match v["track"].as_i64() {
                Some(t) => format!("tile {}: subtitles on, track {t}", v["tile"]),
                None => format!("tile {}: subtitles off", v["tile"]),
            }));
        }
        let info = self.rpc("subtitle_info", json!({ "tile": a.get("tile") }), Duration::from_secs(5))?;
        let tracks = info["tracks"].as_array().cloned().unwrap_or_default();
        let title = info["title"].as_str().unwrap_or("?").to_string();
        let tile = info["tile"].clone();
        if action == "tracks" {
            if tracks.is_empty() {
                return Ok(text(format!("tile {tile} ({title}) has no subtitle tracks")));
            }
            let lines: Vec<String> = tracks
                .iter()
                .map(|t| {
                    let mut l = format!("{}: {}", t["id"], track_name(t));
                    if !is_text_codec(t["codec"].as_str()) {
                        l.push_str(" — image-based, can't be read as text");
                    }
                    if t["selected"] == json!(true) {
                        l.push_str(" (on screen)");
                    }
                    l
                })
                .collect();
            return Ok(text(format!("tile {tile} ({title}) subtitle tracks:\n{}", lines.join("\n"))));
        }
        let position = info["position"].as_f64().unwrap_or(0.0);
        let (track, cues) = self.cues(&info, a.get("track").and_then(Value::as_i64))?;
        let header = format!("tile {tile} ({title}), track {}", track);
        match action {
            "now" => {
                let back = a.get("back_seconds").and_then(Value::as_f64).unwrap_or(60.0).clamp(0.0, 1800.0);
                let ahead = a.get("ahead_seconds").and_then(Value::as_f64).unwrap_or(10.0).clamp(0.0, 600.0);
                let window = subs::around(&cues, position, back, ahead);
                let body = if window.is_empty() {
                    format!("no lines between {} and {}", fmt_hms(position - back), fmt_hms(position + ahead))
                } else {
                    // Mark where "now" falls, so said and about-to-be-said
                    // can't be confused.
                    let split = window.partition_point(|c| c.start <= position);
                    let mut parts = Vec::new();
                    if split > 0 {
                        parts.push(subs::format_cues(&window[..split]));
                    }
                    parts.push(format!("-- now {} --", fmt_hms(position)));
                    if split < window.len() {
                        parts.push(subs::format_cues(&window[split..]));
                    }
                    parts.join("\n")
                };
                Ok(text(format!("{header} — now at {}:\n{body}", fmt_hms(position))))
            }
            "search" => {
                let query = str_arg(a, "query").ok_or("search needs \"query\": words to find")?;
                let limit = a.get("limit").and_then(Value::as_u64).unwrap_or(10).clamp(1, 50) as usize;
                let hits = subs::search(&cues, query, limit);
                if hits.is_empty() {
                    return Ok(text(format!("{header}: no line matches \"{query}\"")));
                }
                Ok(text(format!(
                    "{header}: {} match(es) for \"{query}\" — seek with tessera_control action=seek value=HH:MM:SS\n{}",
                    hits.len(),
                    subs::format_cues(hits)
                )))
            }
            other => Err(format!("unknown action {other:?} — use now, search, tracks or select")),
        }
    }

    /// The cues for the asked-for track (else the one on screen, else the
    /// first readable one), indexing it with ffmpeg on first use.
    fn cues(&mut self, info: &Value, want: Option<i64>) -> Result<(String, Arc<Vec<Cue>>), String> {
        let tracks = info["tracks"].as_array().cloned().unwrap_or_default();
        if tracks.is_empty() {
            return Err(format!("{} has no subtitle tracks", info["title"].as_str().unwrap_or("this video")));
        }
        let track = match want {
            Some(id) => tracks.iter().find(|t| t["id"].as_i64() == Some(id)).ok_or(format!("no subtitle track {id} — see action=tracks"))?,
            None => tracks
                .iter()
                .find(|t| t["selected"] == json!(true) && is_text_codec(t["codec"].as_str()))
                .or_else(|| tracks.iter().find(|t| is_text_codec(t["codec"].as_str())))
                .ok_or("every subtitle track here is image-based (PGS/VobSub) — none can be read as text")?,
        };
        let name = format!("{} ({})", track["id"], track_name(track));
        if !is_text_codec(track["codec"].as_str()) {
            return Err(format!("track {name} is image-based — it can't be read as text"));
        }
        let uri = info["uri"].as_str().unwrap_or("");
        let (input, map) = match track["external"].as_str() {
            Some(file) => (file.to_string(), "0:s:0".to_string()),
            None => {
                if uri.contains("://") {
                    let now = info["sub_text"].as_str().map(|t| format!(" The line on screen now: \"{t}\"")).unwrap_or_default();
                    return Err(format!(
                        "subtitles inside a streamed video can't be indexed (that would mean downloading the whole file).{now}"
                    ));
                }
                let idx = track["ff_index"].as_i64().ok_or("this track has no stream index to extract")?;
                (uri.to_string(), format!("0:{idx}"))
            }
        };
        let key = format!("{input}#{map}");
        if let Some(SubState::Pending(rx)) = self.subs.get(&key) {
            match rx.recv_timeout(Duration::from_secs(8)) {
                Ok(Ok(cues)) => {
                    self.subs.insert(key.clone(), SubState::Ready(Arc::new(cues)));
                }
                Ok(Err(e)) => {
                    self.subs.remove(&key);
                    return Err(e);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err("still indexing these subtitles — try again in a few seconds".into())
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.subs.remove(&key);
                    return Err("indexing the subtitles failed".into());
                }
            }
        }
        if let Some(SubState::Ready(c)) = self.subs.get(&key) {
            return Ok((name, c.clone()));
        }
        // First use: extract on a worker so a big file can't hang the call
        // past a few seconds; a later call picks up the result.
        let (tx, rx) = mpsc::channel();
        let cache = cache_dir().join("subs").join(format!("{:016x}.srt", fnv(&key)));
        std::thread::spawn(move || {
            let _ = tx.send(extract_subs(&input, &map, &cache));
        });
        self.subs.insert(key.clone(), SubState::Pending(rx));
        self.cues(info, want)
    }

    // ---- frames ----

    fn frame(&self, a: &Value) -> ToolResult {
        let (caption, mime, data) = match str_arg(a, "at") {
            Some(at) => {
                let info = self.rpc("subtitle_info", json!({ "tile": a.get("tile") }), Duration::from_secs(5))?;
                if info["live"] == json!(true) {
                    return Err("this is a live stream — only the current frame exists; leave out \"at\"".into());
                }
                let t = match parse_time(at).ok_or(format!("can't read \"{at}\" as a time — use HH:MM:SS, seconds, or +30/-10"))? {
                    TimeSpec::Absolute(t) => t,
                    TimeSpec::Relative(d) => (info["position"].as_f64().unwrap_or(0.0) + d).max(0.0),
                };
                let jpeg = ffmpeg_frame(info["uri"].as_str().unwrap_or(""), t)?;
                use base64::Engine;
                (
                    format!("tile {} — {} at {}", info["tile"], info["title"].as_str().unwrap_or("?"), fmt_hms(t)),
                    "image/jpeg".to_string(),
                    base64::engine::general_purpose::STANDARD.encode(jpeg),
                )
            }
            None => {
                let v = self.rpc("frame", json!({ "tile": a.get("tile") }), Duration::from_secs(10))?;
                (
                    format!(
                        "tile {} — {} at {}",
                        v["tile"],
                        v["title"].as_str().unwrap_or("?"),
                        v["position"].as_str().unwrap_or("?")
                    ),
                    v["image"]["mime"].as_str().unwrap_or("image/jpeg").to_string(),
                    v["image"]["data"].as_str().unwrap_or("").to_string(),
                )
            }
        };
        let mut caption = caption;
        if a.get("save").and_then(Value::as_bool) == Some(true) {
            caption.push_str(&format!("\nsaved to {}", save_frame(&caption, &data)?.display()));
        }
        Ok(vec![Content::Text(caption), Content::Image { mime, data }])
    }

    // ---- development ----

    fn dev(&mut self, name: &str, a: &Value) -> ToolResult {
        let sb = self.sandbox.as_ref().ok_or("dev tools need --dev")?;
        match name {
            "tessera_dev_launch" => {
                if self.rpc("status", json!({}), Duration::from_secs(1)).is_ok() {
                    return Err("the sandbox instance is already running — tessera_dev_quit first".into());
                }
                let fresh = a.get("fresh").and_then(Value::as_bool).unwrap_or(false);
                if fresh {
                    let _ = std::fs::remove_dir_all(&sb.home);
                }
                let cfg = sb.home.join(".config/opentessera");
                std::fs::create_dir_all(&cfg).map_err(|e| e.to_string())?;
                if let Some(settings) = str_arg(a, "settings") {
                    std::fs::write(cfg.join("settings.conf"), settings).map_err(|e| e.to_string())?;
                }
                // The real videos folder, so the sandbox can open real
                // media without a source configured.
                let videos = sb.home.join("Videos");
                if let (false, Some(real)) = (videos.exists(), std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Videos"))) {
                    if real.is_dir() {
                        let _ = std::os::unix::fs::symlink(real, &videos);
                    }
                }
                let size = str_arg(a, "size").unwrap_or("1400x900");
                let mut argv: Vec<String> = vec!["--windowed".into(), size.into(), "--ui-scale".into(), "1".into(), "--overlay".into()];
                argv.extend(string_list(a, "args"));
                argv.extend(string_list(a, "files"));
                let log = std::fs::File::create(&sb.log).map_err(|e| format!("can't write {}: {e}", sb.log.display()))?;
                let err = log.try_clone().map_err(|e| e.to_string())?;
                let mut cmd = Command::new(own_exe());
                cmd.args(&argv)
                    .env("HOME", &sb.home)
                    .env("TESSERA_DEV_INPUT", "1")
                    .stdin(Stdio::null())
                    .stdout(log)
                    .stderr(err);
                detach(&mut cmd);
                let child = cmd.spawn().map_err(|e| format!("couldn't start tessera: {e}"))?;
                let up = wait_until(Duration::from_secs(20), || self.rpc("status", json!({}), Duration::from_secs(1)).is_ok());
                let tail = tail_file(&sb.log, 20, None);
                if !up {
                    return Err(format!("the sandbox didn't come up within 20s. log:\n{tail}"));
                }
                Ok(text(format!(
                    "launched pid {} (HOME={}), args: {}\nlog: {}\n{tail}",
                    child.id(),
                    sb.home.display(),
                    argv.join(" "),
                    sb.log.display()
                )))
            }
            "tessera_dev_quit" => {
                self.rpc("quit", json!({}), Duration::from_secs(3))?;
                let gone = wait_until(Duration::from_secs(10), || {
                    matches!(instance::call(&self.config_dir, &json!({ "method": "status" }), Duration::from_millis(500)), Err(CallError::NotRunning))
                });
                let sb = self.sandbox.as_ref().ok_or("dev tools need --dev")?;
                Ok(text(format!(
                    "{}\n{}",
                    if gone { "quit" } else { "asked it to quit, but it is still answering after 10s" },
                    tail_file(&sb.log, 15, None)
                )))
            }
            "tessera_dev_log" => {
                let lines = a.get("lines").and_then(Value::as_u64).unwrap_or(40).clamp(1, 2000) as usize;
                Ok(text(tail_file(&sb.log, lines, str_arg(a, "grep"))))
            }
            "tessera_dev_screenshot" => {
                let v = self.rpc("screenshot", a.clone(), Duration::from_secs(10))?;
                let caption = format!(
                    "window {} ({}), {}x{} — image is {}x{}; tessera_dev_input takes window coordinates (image × {:.3})",
                    v["window"], v["role"].as_str().unwrap_or("?"),
                    v["window_width"], v["window_height"], v["image"]["width"], v["image"]["height"],
                    v["window_width"].as_f64().unwrap_or(1.0) / v["image"]["width"].as_f64().unwrap_or(1.0),
                );
                Ok(vec![
                    Content::Text(caption),
                    Content::Image {
                        mime: v["image"]["mime"].as_str().unwrap_or("image/png").to_string(),
                        data: v["image"]["data"].as_str().unwrap_or("").to_string(),
                    },
                ])
            }
            "tessera_dev_state" => {
                let mut v = self.rpc("state", json!({}), Duration::from_secs(5))?;
                if let Some(o) = v.as_object_mut() {
                    o.remove("ok");
                }
                Ok(text(serde_json::to_string_pretty(&v).unwrap_or_default()))
            }
            "tessera_dev_input" => {
                let n = a.get("steps").and_then(Value::as_array).map_or(0, |s| s.len()) as u64;
                let v = self.rpc("input", a.clone(), Duration::from_secs(10 + n))?;
                Ok(text(format!("sent {} step(s) — take a tessera_dev_screenshot to see the result", v["steps"])))
            }
            other => Err(format!("unknown tool {other}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

fn text(s: impl Into<String>) -> Vec<Content> {
    vec![Content::Text(s.into())]
}

fn content_json(content: Vec<Content>) -> Value {
    Value::Array(
        content
            .into_iter()
            .map(|c| match c {
                Content::Text(t) => json!({ "type": "text", "text": t }),
                Content::Image { mime, data } => json!({ "type": "image", "mimeType": mime, "data": data }),
            })
            .collect(),
    )
}

fn str_arg<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(Value::as_str).filter(|s| !s.trim().is_empty())
}

fn string_list(a: &Value, k: &str) -> Vec<String> {
    a.get(k).and_then(Value::as_array).map(|v| v.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default()
}

/// One tile, as a sentence: "tile 0: House Of Lies S01E02 — playing at
/// 00:12:03 of 00:29:00, chapter 3 of 8 "Act Two", volume 100% (focused)".
fn tile_line(t: &Value) -> String {
    if t.is_null() {
        return "(no tile)".into();
    }
    let mut parts = Vec::new();
    let state = if t["paused"] == json!(true) { "paused" } else { "playing" };
    parts.push(match (t["position"].as_str(), t["duration"].as_str()) {
        _ if t["live"] == json!(true) => format!("{state} live"),
        (Some(p), Some(d)) => format!("{state} at {p} of {d}"),
        (Some(p), None) => format!("{state} at {p}"),
        _ => format!("{state}, loading"),
    });
    if let Some(c) = t["chapter"].as_object() {
        parts.push(format!("chapter {} of {} \"{}\"", c["number"], c["of"], c["title"].as_str().unwrap_or("")));
    }
    parts.push(if t["muted"] == json!(true) { "muted".to_string() } else { format!("volume {}%", t["volume"]) });
    let mut flags = Vec::new();
    for (k, label) in [("focused", "focused"), ("pinned", "pinned"), ("fullscreen", "fullscreen")] {
        if t[k] == json!(true) {
            flags.push(label);
        }
    }
    let flags = if flags.is_empty() { String::new() } else { format!(" ({})", flags.join(", ")) };
    format!("tile {}: {} — {}{flags}", t["id"], t["title"].as_str().unwrap_or("?"), parts.join(", "))
}

fn open_reply(v: &Value) -> String {
    if let Some(w) = v["opening"].as_str() {
        return format!("opening {} item(s) in {w}", v["items"]);
    }
    if !v["tile"].is_null() {
        let more = v["items"].as_u64().unwrap_or(1).saturating_sub(1);
        let queued = if more > 0 { format!("\n{more} more added to the queue") } else { String::new() };
        return format!("{}{queued}", tile_line(&v["tile"]));
    }
    format!("{} item(s) queued — {}", v["queued"], v["note"].as_str().unwrap_or(""))
}

fn track_name(t: &Value) -> String {
    let mut bits: Vec<String> = Vec::new();
    if let Some(l) = t["lang"].as_str() {
        bits.push(l.to_string());
    }
    if let Some(n) = t["title"].as_str() {
        bits.push(n.to_string());
    }
    if let Some(c) = t["codec"].as_str() {
        bits.push(format!("[{c}]"));
    }
    if t["external"].is_string() {
        bits.push("external file".into());
    }
    if bits.is_empty() {
        "unnamed".into()
    } else {
        bits.join(" ")
    }
}

/// Text subtitle codecs, as mpv names them; the rest are pictures.
fn is_text_codec(codec: Option<&str>) -> bool {
    matches!(
        codec,
        Some("subrip" | "srt" | "ass" | "ssa" | "webvtt" | "mov_text" | "text" | "microdvd" | "subviewer" | "eia_608")
    )
}

// ---------------------------------------------------------------------------
// ffmpeg
// ---------------------------------------------------------------------------

/// Runs a command with a deadline, returning stdout. A hung ffmpeg (a dead
/// network share) becomes an error instead of a hung tool call.
fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Result<Vec<u8>, String> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("couldn't run ffmpeg ({e}) — is it installed?"))?;
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let mut stderr = child.stderr.take().ok_or("no stderr")?;
    let out = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let err = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf);
        buf
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = out.join().unwrap_or_default();
                let err = err.join().unwrap_or_default();
                if status.success() && !out.is_empty() {
                    return Ok(out);
                }
                let why = err.lines().last().unwrap_or("no output").to_string();
                return Err(format!("ffmpeg failed: {why}"));
            }
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("ffmpeg took longer than {}s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(30)),
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// Converts one subtitle stream to SRT and parses it, caching the SRT on
/// disk so a restart of the shim doesn't re-read a 3 GB file.
fn extract_subs(input: &str, map: &str, cache: &Path) -> Result<Vec<Cue>, String> {
    if let Ok(srt) = std::fs::read_to_string(cache) {
        return Ok(subs::parse_srt(&srt));
    }
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-nostdin", "-v", "error", "-i", input, "-map", map, "-f", "srt", "pipe:1"]);
    let out = run_with_timeout(cmd, Duration::from_secs(180))?;
    let srt = String::from_utf8_lossy(&out).into_owned();
    if let Some(dir) = cache.parent() {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(cache, &srt);
    }
    Ok(subs::parse_srt(&srt))
}

/// One frame at `t` seconds, as a ~768px JPEG. Works on files and URLs —
/// `-ss` before `-i` seeks by index rather than decoding up to it.
fn ffmpeg_frame(uri: &str, t: f64) -> Result<Vec<u8>, String> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-nostdin", "-v", "error", "-ss", &format!("{t:.3}"), "-i", uri, "-frames:v", "1"])
        .args(["-vf", "scale=w=768:h=768:force_original_aspect_ratio=decrease"])
        .args(["-f", "image2pipe", "-vcodec", "mjpeg", "-q:v", "4", "pipe:1"]);
    run_with_timeout(cmd, Duration::from_secs(20))
}

fn save_frame(caption: &str, data_b64: &str) -> Result<PathBuf, String> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(data_b64).map_err(|e| e.to_string())?;
    let home = std::env::var_os("HOME").map(PathBuf::from).ok_or("no HOME")?;
    let dir = home.join("Pictures/Tessera");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let stem: String = caption
        .lines()
        .next()
        .unwrap_or("frame")
        .chars()
        .map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' { c } else { '-' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    // Never overwrite: a new name for each grab.
    for n in 0..1000 {
        let name = if n == 0 { format!("{stem}.jpg") } else { format!("{stem} ({n}).jpg") };
        let path = dir.join(name);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                f.write_all(&bytes).map_err(|e| e.to_string())?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.to_string()),
        }
    }
    Err("couldn't find a free file name".into())
}

// ---------------------------------------------------------------------------
// Processes and files
// ---------------------------------------------------------------------------

/// The sandbox `--dev` aims everything at.
struct Sandbox {
    home: PathBuf,
    log: PathBuf,
}

impl Sandbox {
    fn new(dir: Option<PathBuf>) -> Sandbox {
        let dir = dir.unwrap_or_else(|| {
            std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("tessera-dev")
        });
        Sandbox { home: dir.join("home"), log: dir.join("tessera.log") }
    }
}

/// This binary. After a rebuild the running shim's own inode is gone and
/// /proc reports "<path> (deleted)"; the path itself is the new build,
/// which is the one worth launching.
fn own_exe() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("tessera"));
    let s = exe.to_string_lossy();
    match s.strip_suffix(" (deleted)") {
        Some(p) => PathBuf::from(p),
        None => exe,
    }
}

/// Starts the child in its own session, so it outlives the shim and a
/// signal aimed at the shim's group doesn't take the player with it.
fn detach(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid is async-signal-safe, the only requirement in pre_exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
}

fn wait_until(limit: Duration, mut f: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if f() {
            return true;
        }
        if start.elapsed() > limit {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("opentessera")
}

fn tail_file(path: &Path, lines: usize, grep: Option<&str>) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return format!("(no log at {})", path.display());
    };
    let picked: Vec<&str> = text.lines().filter(|l| grep.is_none_or(|g| l.contains(g))).collect();
    picked[picked.len().saturating_sub(lines)..].join("\n")
}

fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

// ---------------------------------------------------------------------------
// Tool definitions
// ---------------------------------------------------------------------------

fn tool(name: &str, title: &str, description: &str, schema: Value, read_only: bool, idempotent: bool) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": schema,
        "annotations": {
            "title": title,
            "readOnlyHint": read_only,
            // Nothing here deletes or overwrites anything: the worst any
            // tool does is stop a video playing.
            "destructiveHint": false,
            "idempotentHint": idempotent,
            "openWorldHint": false,
        },
    })
}

fn tile_prop() -> Value {
    json!({ "type": "integer", "description": "Tile id from tessera_status. Leave out to use the focused tile." })
}

fn player_tools() -> Vec<Value> {
    vec![
        tool(
            "tessera_status",
            "What's playing",
            "What tessera is playing right now, per tile: title (show + season/episode, or film + year), position and \
             duration (HH:MM:SS), chapter, paused/muted/volume, the file, and which tile is focused — the one every other \
             tool acts on when you don't name a tile. Cheap enough to call before any ambiguous request. \
             Example line: `tile 0: House Of Lies S01E02 — playing at 00:12:03 of 00:29:00, volume 100% (focused)`.",
            json!({ "type": "object", "properties": {} }),
            true,
            true,
        ),
        tool(
            "tessera_open",
            "Open videos",
            "Play video files, folders or URLs. Starts tessera if it isn't running. The first item plays now — replacing \
             what the focused tile shows, or in `tile`, or in a new tile with new_tile=true — and the rest join the queue. \
             Local paths must be inside the user's media folders (~/Videos or a configured source). \
             Example: {\"paths\": [\"~/Videos/Heat (1995).mkv\"], \"unmuted\": true}.",
            json!({
                "type": "object",
                "properties": {
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Files, folders, or http(s)/rtsp URLs." },
                    "tile": tile_prop(),
                    "new_tile": { "type": "boolean", "description": "Play in a new tile beside the others instead of replacing one." },
                    "unmuted": { "type": "boolean", "description": "Make sure it plays with sound." },
                },
                "required": ["paths"],
            }),
            false,
            false,
        ),
        tool(
            "tessera_control",
            "Control playback",
            "Play, pause, seek, change volume, mute, skip to the next/previous queue item, pin, or fullscreen a tile. \
             Returns the tile's new state. `value` for seek: seconds (90), HH:MM:SS (01:02:03), relative (+30, -10), \
             `next chapter`/`previous chapter`, `chapter 3`, or part of a chapter title. `value` for volume: 0-130 (percent) \
             or relative (+10, -10). Example: {\"action\": \"seek\", \"value\": \"-30\"}.",
            json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["play", "pause", "toggle", "seek", "volume", "mute", "unmute", "next", "previous", "pin", "fullscreen"] },
                    "value": { "type": "string", "description": "Position for seek, percent for volume." },
                    "tile": tile_prop(),
                },
                "required": ["action"],
            }),
            false,
            false,
        ),
        tool(
            "tessera_subtitles",
            "Read subtitles",
            "Read what is being said, from the subtitle track. action=now: the lines from the last `back_seconds` (default 60) \
             to `ahead_seconds` (default 10) around the current position, with a `-- now --` line marking it — use it for \
             \"what did they just say\" or \"what's happening\". action=search: every line containing `query`, with timestamps you can seek to — \"skip to where they \
             say rosebud\" is one search plus one tessera_control seek. action=tracks lists tracks; action=select turns one on \
             (`track` id) or off (`track`: \"off\"). Lines come as `[HH:MM:SS] text`. The first read of a track takes a few \
             seconds while it is indexed. Example: {\"action\": \"search\", \"query\": \"rosebud\"}.",
            json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["now", "search", "tracks", "select"] },
                    "query": { "type": "string", "description": "Words to find (search). Case and punctuation are ignored." },
                    "track": { "type": ["integer", "string"], "description": "Track id from action=tracks; \"off\" with select. Default: the track on screen, else the first readable one." },
                    "back_seconds": { "type": "number", "description": "now: seconds of lines before the current position (default 60)." },
                    "ahead_seconds": { "type": "number", "description": "now: seconds after it (default 10)." },
                    "limit": { "type": "integer", "description": "search: most matches to return (default 10)." },
                    "tile": tile_prop(),
                },
                "required": ["action"],
            }),
            true,
            true,
        ),
        tool(
            "tessera_frame",
            "Capture a frame",
            "A still of a tile's video as an image (JPEG, ~768px), for questions like \"what's on screen\" or \"who is that\". \
             By default the frame showing now; `at` takes another time (HH:MM:SS, seconds, or +30/-10) without disturbing \
             playback. save=true also writes it to ~/Pictures/Tessera. Example: {\"at\": \"00:41:10\"}.",
            json!({
                "type": "object",
                "properties": {
                    "tile": tile_prop(),
                    "at": { "type": "string", "description": "Time to grab instead of the current frame." },
                    "save": { "type": "boolean", "description": "Also save the JPEG to ~/Pictures/Tessera (never overwrites)." },
                },
            }),
            false,
            true,
        ),
        tool(
            "tessera_queue",
            "Playback queue",
            "The queue of what plays next. action=list shows what is pending (`limit`, default 10); action=add appends files, \
             folders or URLs (`paths`, same rules as tessera_open); action=next / previous moves a tile on to the next or \
             previous queue item. Example: {\"action\": \"add\", \"paths\": [\"~/Videos/Season 2\"]}.",
            json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "add", "next", "previous"] },
                    "paths": { "type": "array", "items": { "type": "string" } },
                    "limit": { "type": "integer" },
                    "tile": tile_prop(),
                },
                "required": ["action"],
            }),
            false,
            false,
        ),
        tool(
            "tessera_sources",
            "Video sources",
            "Where tessera finds videos. action=list shows the configured sources (folders, Plex/Jellyfin servers, \
             cameras). action=add adds a folder as a source: its videos, subfolders included, appear in tessera's library, \
             and tessera_open can play files inside it. The folder must exist and contain videos. Plex, Jellyfin and \
             cameras need a sign-in, so they are added in tessera's own sources tab instead. \
             Example: {\"action\": \"add\", \"path\": \"/mnt/storage/video\"}.",
            json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "add"] },
                    "path": { "type": "string", "description": "add: the folder." },
                },
                "required": ["action"],
            }),
            false,
            false,
        ),
        tool(
            "tessera_close",
            "Close a tile",
            "Stop a tile and remove it from the wall (the file is untouched; it can be opened again). Example: {\"tile\": 2}.",
            json!({ "type": "object", "properties": { "tile": tile_prop() } }),
            false,
            false,
        ),
    ]
}

fn dev_tools() -> Vec<Value> {
    let window = json!({ "type": "integer", "description": "Window index from tessera_dev_state (default 0)." });
    vec![
        tool(
            "tessera_dev_launch",
            "Launch sandbox",
            "Start the sandboxed tessera (its own HOME; the user's settings are never touched), windowed at `size` with \
             --ui-scale 1 --overlay. `files` play at startup; `args` are extra CLI flags; `settings` is written as its \
             settings.conf first; fresh=true wipes the sandbox HOME. ~/Videos is linked in. Returns the log head. \
             Example: {\"files\": [\"~/Videos/a.mkv\"], \"args\": [\"--populate\", \"2\"]}.",
            json!({
                "type": "object",
                "properties": {
                    "files": { "type": "array", "items": { "type": "string" } },
                    "args": { "type": "array", "items": { "type": "string" } },
                    "settings": { "type": "string", "description": "settings.conf contents, e.g. 'retain_queue = false\\nsources = [{\"kind\":\"folder\",\"path\":\"/x\"}]'" },
                    "size": { "type": "string", "description": "WxH, default 1400x900." },
                    "fresh": { "type": "boolean" },
                },
            }),
            false,
            false,
        ),
        tool(
            "tessera_dev_quit",
            "Quit sandbox",
            "Quit the sandboxed instance the way Esc does (queue saved, mpv torn down) and return the log tail.",
            json!({ "type": "object", "properties": {} }),
            false,
            true,
        ),
        tool(
            "tessera_dev_log",
            "Sandbox log",
            "The sandbox's stdout/stderr: the last `lines` (default 40), optionally only lines containing `grep`.",
            json!({ "type": "object", "properties": { "lines": { "type": "integer" }, "grep": { "type": "string" } } }),
            true,
            true,
        ),
        tool(
            "tessera_dev_screenshot",
            "Screenshot window",
            "A PNG of one sandbox window as composited, overlay included unless overlay=false. Pixel coordinates in it, \
             divided by `scale`, are what tessera_dev_input takes. Example: {\"window\": 1, \"scale\": 0.5}.",
            json!({
                "type": "object",
                "properties": {
                    "window": window,
                    "overlay": { "type": "boolean", "description": "Include the egui overlay (default true)." },
                    "scale": { "type": "number", "description": "0.1-1, default 1." },
                },
            }),
            true,
            true,
        ),
        tool(
            "tessera_dev_state",
            "UI state",
            "Everything about the sandbox's UI as JSON: windows (role, size, tile ids and rects, pointer), overlay flags \
             (help/settings open, toast), budget, queue cursor, settings (sources by label only).",
            json!({ "type": "object", "properties": {} }),
            true,
            true,
        ),
        tool(
            "tessera_dev_input",
            "Pointer input",
            "Synthetic mouse input into a sandbox window, through the same path as a real mouse, paced a frame apart. \
             Steps: {\"move\": [x, y]}, {\"click\": [x, y]}, {\"right_click\": [x, y]}, {\"down\": [x, y]}, {\"up\": [x, y]}, \
             {\"drag\": [[x1, y1], [x2, y2]]}, {\"scroll\": notches (+ is up)}, {\"wait\": ms}. Coordinates are window \
             pixels. Keys aren't supported (use xdotool key). Example: {\"window\": 1, \"steps\": [{\"click\": [277, 69]}]}.",
            json!({
                "type": "object",
                "properties": { "window": window, "steps": { "type": "array", "items": { "type": "object" } } },
                "required": ["steps"],
            }),
            false,
            false,
        ),
    ]
}
