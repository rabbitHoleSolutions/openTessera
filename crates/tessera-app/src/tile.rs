use glow::HasContext;
use tessera_core::MediaItem;
use tessera_mpv::{Mpv, MpvEvent, RenderContext};

pub struct TileOptions {
    pub hwdec: String,
    pub audio: bool,
    pub audio_device: Option<String>,
    pub cache_mb: u32,
    pub loop_file: bool,
    pub log_verbose: bool,
}

pub enum TileEvent {
    EndOfFile,
    Loaded { width: u32, height: u32, fps: f64, codec: String },
}

/// One entry of mpv's track-list (audio or subtitle stream).
#[derive(Clone, Debug)]
pub struct TrackInfo {
    pub id: i64,
    pub lang: Option<String>,
    pub title: Option<String>,
    pub codec: Option<String>,
    pub default: bool,
    pub selected: bool,
}

impl TrackInfo {
    /// Short chip label: language, else title, else "?".
    pub fn short_label(&self) -> String {
        let base = self
            .lang
            .as_deref()
            .or(self.title.as_deref())
            .unwrap_or("?");
        let mut s: String = base.chars().take(10).collect();
        if self.default {
            s.push('*');
        }
        s
    }

    /// Full description for tooltips.
    pub fn describe(&self) -> String {
        let mut parts = vec![format!("#{}", self.id)];
        if let Some(l) = &self.lang {
            parts.push(l.clone());
        }
        if let Some(c) = &self.codec {
            parts.push(c.clone());
        }
        if let Some(t) = &self.title {
            parts.push(t.clone());
        }
        if self.default {
            parts.push("default".into());
        }
        parts.join(" · ")
    }
}

// The mpv client API is fully thread-safe; GL resources are destroyed via
// destroy_gl before tiles are handed to another thread (parallel shutdown).
unsafe impl Send for Tile {}

pub struct Tile {
    pub render: RenderContext,
    pub mpv: Mpv,
    pub item: MediaItem,
    pub id: usize,
    audio_enabled: bool,
    texture: Option<<glow::Context as HasContext>::Texture>,
    fbo: Option<<glow::Context as HasContext>::Framebuffer>,
    tex_w: i32,
    tex_h: i32,
    pub frames: u64,
    pub last_time_pos: f64,
    pub duration: f64,
    pub min_rate: f64,
    pub loaded: bool,
    /// Track caches (mpv track-list) refreshed on file load and whenever the
    /// track panel opens — the overlay snapshot reads them every frame.
    pub audio_tracks: Vec<TrackInfo>,
    pub subtitle_tracks: Vec<TrackInfo>,
    /// The resume load set mpv's `start` option; cleared on FileLoaded so
    /// the NEXT file (queue advance) starts from its own beginning.
    pending_start_reset: bool,
}

/// Playback state that survives a screen move. The mpv instance (and its
/// render context) cannot cross GL contexts, so a moved tile is rebuilt
/// from this snapshot: same item, resumed at `start`, same volume/mute/
/// pause and track picks.
pub struct ResumeState {
    pub start: f64,
    pub paused: bool,
    pub volume: i64,
    pub muted: bool,
    pub aid: Option<i64>,
    pub sid: Option<i64>,
}

impl Tile {
    pub fn new(
        lib: &std::sync::Arc<tessera_mpv::MpvLib>,
        id: usize,
        item: &MediaItem,
        opts: &TileOptions,
        resume: Option<&ResumeState>,
    ) -> Result<Tile, tessera_mpv::Error> {
        let mut options: Vec<(&str, &str)> = vec![
            ("vo", "libmpv"),
            ("hwdec", opts.hwdec.as_str()),
            ("idle", "yes"),
            ("osc", "no"),
            ("terminal", "no"),
            ("keep-open", "no"),
        ];
        if opts.audio {
            // Don't pin `ao`: let mpv auto-negotiate (pulse via PipeWire on
            // Linux, coreaudio on macOS). Every tile starts muted — a video
            // wall with N simultaneous streams is noise; right-click unmutes
            // an individual tile, mirroring the Quest app's per-tile mute.
            options.push(("mute", "yes"));
            if let Some(dev) = &opts.audio_device {
                options.push(("audio-device", dev));
            }
        } else {
            options.push(("audio", "no"));
        }
        if opts.loop_file {
            options.push(("loop-file", "inf"));
        }
        options.push(("cache", "yes"));
        let cache = format!("{}MiB", opts.cache_mb);
        options.push(("demuxer-max-bytes", &cache));
        options.push(("msg-level", if opts.log_verbose { "all=v" } else { "all=error" }));

        let mpv = Mpv::new(lib, &options)?;
        let render = RenderContext::new_gl(&mpv, crate::compositor::mpv_get_proc_address, std::ptr::null_mut())?;
        // Restore-before-load: volume/mute/pause/aid/sid are ordinary
        // runtime properties, and `start` positions the next load. All
        // set before load_file so the resumed file comes up in the right
        // state with no visible settling.
        let mut pending_start_reset = false;
        if let Some(r) = resume {
            let _ = mpv.command(&["set", "volume", &r.volume.clamp(0, 130).to_string()]);
            let _ = mpv.command(&["set", "mute", if r.muted { "yes" } else { "no" }]);
            let _ = mpv.command(&["set", "start", &format!("{:.3}", r.start.max(0.0))]);
            if r.paused {
                let _ = mpv.command(&["set", "pause", "yes"]);
            }
            let aid = r.aid.map(|i| i.to_string()).unwrap_or_else(|| "no".into());
            let _ = mpv.command(&["set", "aid", &aid]);
            let sid = r.sid.map(|i| i.to_string()).unwrap_or_else(|| "no".into());
            let _ = mpv.command(&["set", "sid", &sid]);
            pending_start_reset = true;
        }
        mpv.load_file(&item.uri, "replace")?;

        Ok(Tile {
            render,
            mpv,
            item: item.clone(),
            id,
            audio_enabled: opts.audio,
            texture: None,
            fbo: None,
            tex_w: 0,
            tex_h: 0,
            frames: 0,
            last_time_pos: -1.0,
            duration: -1.0,
            min_rate: f64::MAX,
            loaded: false,
            audio_tracks: Vec::new(),
            subtitle_tracks: Vec::new(),
            pending_start_reset,
        })
    }

    /// Captures the state a screen move preserves. Refreshes the track
    /// caches first so the selected audio/subtitle picks are current.
    pub fn snapshot_resume(&mut self) -> ResumeState {
        self.refresh_tracks();
        ResumeState {
            start: self.time_pos().max(0.0),
            paused: self.is_paused(),
            volume: self.volume(),
            muted: self.is_muted(),
            aid: self.audio_tracks.iter().find(|t| t.selected).map(|t| t.id),
            sid: self.subtitle_tracks.iter().find(|t| t.selected).map(|t| t.id),
        }
    }

    pub fn load(&self, item: &MediaItem) -> Result<(), tessera_mpv::Error> {
        // Async: opening a file can block on I/O (network sources), which
        // would stall the whole render loop.
        self.mpv.load_file_async(&item.uri, "replace")
    }

    pub fn set_paused(&self, paused: bool) {
        if let Err(e) = self.mpv.command(&["set", "pause", if paused { "yes" } else { "no" }]) {
            eprintln!("tile {}: set pause failed: {e}", self.id);
        }
    }

    /// Seeks the stream. `absolute` targets a position from the start;
    /// otherwise `seconds` is relative to the current position.
    pub fn seek(&self, seconds: f64, absolute: bool) {
        // Async: a seek flushes the demuxer (and re-requests over HTTP for
        // remote sources) — synchronous seeks stall every tile's playback.
        let v = format!("{seconds:.3}");
        let r = if absolute {
            self.mpv.command_async(&["seek", &v, "absolute"])
        } else {
            self.mpv.command_async(&["seek", &v])
        };
        if let Err(e) = r {
            eprintln!("tile {}: seek failed: {e}", self.id);
        }
    }

    /// Current playback position (seconds), or -1 if unknown.
    pub fn time_pos(&self) -> f64 {
        self.mpv.get_f64("time-pos").unwrap_or(-1.0)
    }

    pub fn is_paused(&self) -> bool {
        self.mpv.get_string("pause").map(|v| v == "yes").unwrap_or(false)
    }

    /// Cumulative decoder-side frame drops (monotonic counter).
    pub fn decoder_drops(&self) -> i64 {
        self.mpv.get_i64("decoder-frame-drop-count").unwrap_or(0)
    }

    /// Toggles this tile's audio. Returns the new muted state.
    /// Only meaningful when the app was started with --audio; otherwise the
    /// whole audio pipeline is disabled per-tile.
    pub fn toggle_mute(&self) -> bool {
        let muted = self.is_muted();
        if let Err(e) = self.mpv.command(&["set", "mute", if muted { "no" } else { "yes" }]) {
            eprintln!("tile {}: set mute failed: {e}", self.id);
        }
        !muted
    }

    pub fn is_muted(&self) -> bool {
        self.mpv.get_string("mute").map(|v| v == "yes").unwrap_or(true)
    }

    pub fn set_volume(&self, percent: i64) {
        let v = percent.clamp(0, 130).to_string();
        if let Err(e) = self.mpv.command(&["set", "volume", &v]) {
            eprintln!("tile {}: set volume failed: {e}", self.id);
        }
    }

    pub fn volume(&self) -> i64 {
        self.mpv.get_i64("volume").unwrap_or(100)
    }

    /// Re-reads mpv's track-list into the audio/subtitle caches. track-list
    /// is a node property; requested as a string mpv returns it as JSON.
    pub fn refresh_tracks(&mut self) {
        self.audio_tracks.clear();
        self.subtitle_tracks.clear();
        let Some(json) = self.mpv.get_string("track-list") else {
            return;
        };
        let Ok(list) = serde_json::from_str::<Vec<serde_json::Value>>(&json) else {
            eprintln!("tile {}: track-list parse failed", self.id);
            return;
        };
        for t in list {
            let kind = t.get("type").and_then(|v| v.as_str());
            let Some(id) = t.get("id").and_then(|v| v.as_i64()) else {
                continue;
            };
            let info = TrackInfo {
                id,
                lang: t.get("lang").and_then(|v| v.as_str()).map(str::to_owned),
                title: t.get("title").and_then(|v| v.as_str()).map(str::to_owned),
                codec: t.get("codec").and_then(|v| v.as_str()).map(str::to_owned),
                default: t.get("default").and_then(|v| v.as_bool()).unwrap_or(false),
                selected: t.get("selected").and_then(|v| v.as_bool()).unwrap_or(false),
            };
            match kind {
                Some("audio") => self.audio_tracks.push(info),
                Some("sub") => self.subtitle_tracks.push(info),
                _ => {}
            }
        }
    }

    /// Selects an audio track (None = off, mpv "aid=no").
    pub fn set_audio_track(&mut self, track: Option<i64>) {
        let v = track.map(|i| i.to_string()).unwrap_or_else(|| "no".into());
        if let Err(e) = self.mpv.command(&["set", "aid", &v]) {
            eprintln!("tile {}: set aid failed: {e}", self.id);
        }
        self.refresh_tracks();
    }

    /// Selects a subtitle track (None = off, mpv "sid=no"). Subtitles render
    /// through the existing render-API path (baked into the video frame).
    pub fn set_subtitle_track(&mut self, track: Option<i64>) {
        let v = track.map(|i| i.to_string()).unwrap_or_else(|| "no".into());
        if let Err(e) = self.mpv.command(&["set", "sid", &v]) {
            eprintln!("tile {}: set sid failed: {e}", self.id);
        }
        self.refresh_tracks();
    }

    pub fn audio_enabled(&self) -> bool {
        self.audio_enabled
    }

    /// Enables/disables the audio pipeline at runtime (settings panel).
    /// Tiles keep their muted state either way.
    pub fn set_audio_enabled(&mut self, enabled: bool) {
        self.audio_enabled = enabled;
        let v = if enabled { "yes" } else { "no" };
        if let Err(e) = self.mpv.command(&["set", "audio", v]) {
            eprintln!("tile {}: set audio failed: {e}", self.id);
        }
        if enabled {
            // audio=no skipped track selection; reselect.
            let _ = self.mpv.command(&["set", "aid", "auto"]);
        }
    }

    /// Adjusts the demuxer cache at runtime (best effort — mpv applies it
    /// to ongoing demuxing where it can, and fully on the next load).
    pub fn set_cache_mb(&mut self, cache_mb: u32) {
        let v = format!("{cache_mb}MiB");
        if let Err(e) = self.mpv.command(&["set", "demuxer-max-bytes", &v]) {
            eprintln!("tile {}: set cache size failed: {e}", self.id);
        }
    }

    /// Polls mpv events and renders any new frame into this tile's FBO texture.
    /// Must be called with the tile's screen GL context current.
    pub fn tick(&mut self, gl: &glow::Context) -> Vec<TileEvent> {
        let profile = std::env::var("TESSERA_PROFILE").map(|v| v != "0").unwrap_or(false);
        let t0 = std::time::Instant::now();
        let mut events = Vec::new();
        while let Some(ev) = self.mpv.poll_event() {
            match ev {
                MpvEvent::FileLoaded => {
                    if self.pending_start_reset {
                        let _ = self.mpv.command(&["set", "start", "none"]);
                        self.pending_start_reset = false;
                    }
                    let w = self.mpv.get_i64("width").unwrap_or(0) as u32;
                    let h = self.mpv.get_i64("height").unwrap_or(0) as u32;
                    let fps = self
                        .mpv
                        .get_f64("container-fps")
                        .or_else(|| self.mpv.get_f64("estimated-vf-fps"))
                        .unwrap_or(30.0);
                    let codec = self.mpv.get_string("video-codec").unwrap_or_default();
                    let pixfmt = self
                        .mpv
                        .get_string("video-params/pixelformat")
                        .or_else(|| self.mpv.get_string("video-out-params/pixelformat"))
                        .unwrap_or_else(|| "?".into());
                    let hwdec = self.mpv.get_string("hwdec-current").unwrap_or_else(|| "no".into());
                    let ao = self
                        .mpv
                        .get_string("audio-device")
                        .or_else(|| self.mpv.get_string("ao"))
                        .unwrap_or_else(|| "-".into());
                    println!(
                        "tile {}: {}x{} @{:.3} {} pix={} hwdec={} ao={}",
                        self.id, w, h, fps, codec, pixfmt, hwdec, ao
                    );
                    self.loaded = true;
                    self.duration = self.mpv.get_f64("duration").unwrap_or(-1.0);
                    self.refresh_tracks();
                    events.push(TileEvent::Loaded { width: w, height: h, fps, codec });
                }
                MpvEvent::EndFile { reason: tessera_mpv::EndFileReason::Eof, .. } => {
                    events.push(TileEvent::EndOfFile);
                }
                MpvEvent::EndFile { reason, error } => {
                    if error != 0 {
                        eprintln!("tile {}: stream error ({reason:?}, code {error})", self.id);
                    }
                }
                MpvEvent::Shutdown => {
                    eprintln!("tile {}: mpv shutdown", self.id);
                }
                _ => {}
            }
        }

        let t1 = std::time::Instant::now();
        let flags = self.render.update();
        let t2 = std::time::Instant::now();
        if flags & tessera_mpv::MPV_RENDER_UPDATE_FRAME != 0 {
            self.ensure_texture(gl);
            if let Some(fbo) = self.fbo {
                // Per render_gl.h, MPV_RENDER_PARAM_OPENGL_FBO=0 means the
                // default framebuffer — mpv binds the target itself, so we
                // must pass our FBO's actual GL name, not merely bind it.
                let fbo_id = fbo.0.get() as i32;
                if let Err(e) = self.render.render(fbo_id, self.tex_w, self.tex_h, true) {
                    eprintln!("tile {}: render failed: {e}", self.id);
                }
                unsafe {
                    // mpv leaves assorted GL state behind (notably scissor,
                    // which would silently clip later clears/draws).
                    gl.disable(glow::SCISSOR_TEST);
                    gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                }
                self.frames += 1;
            }
        }
        let t3 = std::time::Instant::now();

        if profile {
            let mut slow = t2.duration_since(t1);
            let mut label = "render.update";
            if t3.duration_since(t2) > slow {
                slow = t3.duration_since(t2);
                label = "render";
            }
            if slow.as_secs_f64() > 0.008 {
                eprintln!("[prof] tile {} {} blocked {:.1}ms (poll {:.1}ms)", self.id, label, slow.as_secs_f64()*1000.0, t1.duration_since(t0).as_secs_f64()*1000.0);
            }
        }

        let tp = self.mpv.get_f64("time-pos").unwrap_or(-1.0);
        if tp >= 0.0 && self.last_time_pos >= 0.0 {
            let delta = tp - self.last_time_pos;
            if delta >= 0.0 && delta < 10.0 {
                self.min_rate = self.min_rate.min(delta);
            }
        }
        let t4 = std::time::Instant::now();
        if profile && t4.duration_since(t3).as_secs_f64() > 0.008 {
            eprintln!("[prof] tile {} property read blocked {:.1}ms", self.id, t4.duration_since(t3).as_secs_f64()*1000.0);
        }
        if tp >= 0.0 {
            self.last_time_pos = tp;
        }

        events
    }

    fn ensure_texture(&mut self, gl: &glow::Context) {
        let w = self.mpv.get_i64("width").unwrap_or(0) as i32;
        let h = self.mpv.get_i64("height").unwrap_or(0) as i32;
        if w <= 0 || h <= 0 {
            return;
        }
        if self.texture.is_some() && w == self.tex_w && h == self.tex_h {
            return;
        }
        unsafe {
            if let Some(t) = self.texture.take() {
                gl.delete_texture(t);
            }
            if let Some(f) = self.fbo.take() {
                gl.delete_framebuffer(f);
            }
            let tex = gl.create_texture().expect("tile texture");
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGB8 as i32, w, h, 0, glow::RGB, glow::UNSIGNED_BYTE, None);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);

            let fbo = gl.create_framebuffer().expect("tile fbo");
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(tex), 0);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);

            self.texture = Some(tex);
            self.fbo = Some(fbo);
            self.tex_w = w;
            self.tex_h = h;
        }
    }

    pub fn texture(&self) -> Option<<glow::Context as HasContext>::Texture> {
        self.texture
    }

    pub fn video_aspect(&self) -> f32 {
        if self.tex_w > 0 && self.tex_h > 0 {
            self.tex_w as f32 / self.tex_h as f32
        } else {
            16.0 / 9.0
        }
    }

    pub fn status(&self) -> String {
        let hwdec = self.mpv.get_string("hwdec-current").unwrap_or_else(|| "n/a".into());
        let drops = self.mpv.get_i64("decoder-frame-drop-count").unwrap_or(-1);
        let paused = self.is_paused();
        let audio = if self.audio_enabled() {
            if self.is_muted() { "muted".to_string() } else { format!("vol {}%", self.volume()) }
        } else {
            "audio-off".to_string()
        };
        format!(
            "{} [{}x{}] {} drops={} {} {}",
            self.item.title,
            self.tex_w,
            self.tex_h,
            hwdec,
            drops,
            if paused { "paused" } else { "playing" },
            audio
        )
    }

    /// Frees GL resources. Must be called with the tile's screen GL context current.
    pub fn destroy_gl(&mut self, gl: &glow::Context) {
        unsafe {
            if let Some(t) = self.texture.take() {
                gl.delete_texture(t);
            }
            if let Some(f) = self.fbo.take() {
                gl.delete_framebuffer(f);
            }
        }
    }

    /// Dumps this tile's current FBO content to a PNG. Diagnostic aid.
    pub fn dump_fbo(&self, gl: &glow::Context, path: &std::path::Path) -> std::io::Result<()> {
        let Some(fbo) = self.fbo else { return Ok(()) };
        let w = self.tex_w;
        let h = self.tex_h;
        let mut buf = vec![0u8; (w as usize) * (h as usize) * 3];
        unsafe {
            gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.read_pixels(0, 0, w, h, glow::RGB, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(&mut buf));
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }
        crate::png_util::write_png_rgb(path, w, h, &buf)
    }
}
