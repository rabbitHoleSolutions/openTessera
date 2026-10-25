use glow::HasContext;
use tessera_core::tracks::{Choice, Kind};
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
    /// The stream could not be opened, or died mid-play.
    Failed { reason: String },
    /// A live stream that had dropped came back on its own.
    Reconnected,
    /// A file has opened and its track list is read: the moment to put
    /// back remembered audio and subtitle picks. Separate from `Loaded`, which also
    /// fires again when the first frame reports a different size.
    TracksKnown,
}

/// A live stream that dropped, and when to try it again.
struct Reconnect {
    attempts: u32,
    next_at: std::time::Instant,
}

/// Backoff between reconnect attempts, in seconds, holding at the last
/// value forever after.
///
/// Quick at first because most drops are a blip — a switch renegotiating,
/// a camera rebooting after a firmware update — and those come back within
/// seconds. Then it backs off, because a camera that has been unplugged
/// should not be a permanent connection attempt every second for the rest
/// of the week. It never gives up: a wall is a thing you leave running,
/// and a camera that returns on Monday should simply be there again.
const RECONNECT_BACKOFF_SECS: [u64; 6] = [1, 2, 5, 10, 20, 30];

/// How long a live stream may go without its clock advancing before it is
/// treated as dropped.
///
/// Necessary because **a dead stream frequently reports nothing at all**.
/// Killing the source mid-play produced no end-of-file, no error, no event
/// of any kind — mpv simply stopped, and the tile sat at 0.0fps
/// indefinitely. Reconnecting only on an error event therefore misses the
/// most ordinary failure there is: a camera that stops sending.
///
/// Detection is not instant, and cannot be: after the source goes away mpv
/// plays out whatever it has buffered — up to the ~10s live cache — so the
/// clock keeps advancing for a while before it stops. This is measured from
/// the moment it *stops*, so the total is roughly the buffer plus this.
const STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn reconnect_delay(attempts: u32) -> std::time::Duration {
    let i = (attempts.max(1) as usize - 1).min(RECONNECT_BACKOFF_SECS.len() - 1);
    std::time::Duration::from_secs(RECONNECT_BACKOFF_SECS[i])
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
    /// The stream's index in the container, as ffmpeg numbers it — what
    /// the MCP side needs to extract an embedded subtitle track.
    pub ff_index: Option<i64>,
    /// Set for a track mpv loaded from a separate file.
    pub external_filename: Option<String>,
}

impl TrackInfo {
    /// The fields a remembered pick is matched on.
    pub fn core(&self) -> tessera_core::tracks::Track {
        tessera_core::tracks::Track {
            id: self.id,
            lang: self.lang.clone(),
            title: self.title.clone(),
            codec: self.codec.clone(),
        }
    }

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

// NB: deliberately NOT `unsafe impl Send for Tile`. A Tile owns a
// RenderContext, and freeing one off its GL thread segfaults inside mpv —
// which is the whole reason `split_for_teardown` exists. Nothing sends a
// Tile across threads any more (parallel shutdown moves the bare `Mpv`,
// which is Send on its own), so leaving Tile !Send lets the compiler keep
// that invariant instead of a comment.

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
    pub loaded: bool,
    /// The size last sent in a `Loaded` event, so the first frame only
    /// re-reports a stream whose size it actually changed.
    reported_size: (u32, u32),
    /// The recorded-media cache size and loop setting this tile was built
    /// with. Kept so [`Tile::load`] can re-derive the right options for
    /// whatever it is about to play — see the comment there.
    cache_mb: u32,
    loop_file: bool,
    /// Track caches (mpv track-list) refreshed on file load and whenever the
    /// track panel opens — the overlay snapshot reads them every frame.
    pub audio_tracks: Vec<TrackInfo>,
    pub subtitle_tracks: Vec<TrackInfo>,
    /// Crop and zoom (see `tessera_core::view`), and the item they were set
    /// for. They belong to that picture: a letterbox crop carried onto the
    /// next queue item would cut into a film that has no bars.
    view: tessera_core::view::View,
    view_for: Option<String>,
    /// A move from one view to another under way: drawn part way each frame.
    view_anim: Option<ViewAnim>,
    /// Drawn over the picture (grid, reticle, readout), for the same item as
    /// the view; see `crate::hud`.
    hud: Option<crate::hud::Hud>,
    /// The resume load set mpv's `start` option; cleared on FileLoaded so
    /// the NEXT file (queue advance) starts from its own beginning.
    pending_start_reset: bool,
    /// Set while a live stream is down and being retried.
    reconnect: Option<Reconnect>,
    /// When this tile's clock last moved. A live stream whose clock stops
    /// has dropped, whether or not it said so.
    progress_at: std::time::Instant,
}

/// Playback state that survives a screen move. The mpv instance (and its
/// render context) cannot cross GL contexts, so a moved tile is rebuilt
/// from this snapshot: same item, resumed at `start`, same volume/mute/
/// pause and track picks.
/// An animated move between views (see `Tile::set_view_animated`).
struct ViewAnim {
    from: tessera_core::view::View,
    start: std::time::Instant,
    secs: f32,
}

pub struct ResumeState {
    pub start: f64,
    pub paused: bool,
    pub volume: i64,
    pub muted: bool,
    pub aid: Option<i64>,
    pub sid: Option<i64>,
    /// Crop/zoom, so a tile moved to another screen frames the same.
    pub view: tessera_core::view::View,
}

impl Tile {
    pub fn new(
        lib: &std::sync::Arc<tessera_mpv::MpvLib>,
        id: usize,
        item: &MediaItem,
        opts: &TileOptions,
        resume: Option<&ResumeState>,
        muted: bool,
    ) -> Result<Tile, tessera_mpv::Error> {
        // A live feed and a film want genuinely different players. mpv
        // options were one global set applied identically to every tile,
        // which for a camera is wrong in the one way that matters: a 512
        // MiB demuxer cache on a stream nobody can seek is not resilience,
        // it is latency. Everything below keys off the item, so the
        // difference lives in one place instead of at each call site.
        let live = item.is_live();
        let mut options: Vec<(&str, &str)> = vec![
            ("vo", "libmpv"),
            ("hwdec", opts.hwdec.as_str()),
            ("idle", "yes"),
            ("osc", "no"),
            ("terminal", "no"),
            ("keep-open", "no"),
        ];
        if live {
            // Ordered first so the explicit options below still win: mpv
            // applies a profile where it appears.
            options.push(("profile", "low-latency"));
            // UDP loses packets and cameras are not worth tearing frames
            // for; TCP is what every NVR client asks for.
            options.push(("rtsp-transport", "tcp"));
            // Don't sit forever on a camera that has gone off the network.
            options.push(("network-timeout", "10"));
        }
        // The audio options below are set whether or not audio is on now:
        // the config tab can turn it on under a running tile.
        //
        // What the sound server calls the stream. mpv's default is "mpv",
        // which tells a mixer — or an assistant ducking audio under its own
        // voice — nothing about where it came from.
        options.push(("audio-client-name", "Tessera"));
        // The stream's media.name comes from mpv's `title`, which defaults
        // to "<file> - mpv"; a now-playing display wants just the title.
        options.push(("title", "${media-title}"));
        if let Some(dev) = &opts.audio_device {
            options.push(("audio-device", dev));
        }
        // The caller decides `muted`: the first tile is heard — one video
        // should just play — and every tile added beside it starts muted,
        // since a wall of N simultaneous streams is noise; right-click
        // unmutes one, mirroring the Quest app's per-tile mute. A resume
        // overrides this with the tile's own state, below. Set even with
        // audio off, so turning it on doesn't sound the whole wall at once.
        options.push(("mute", if muted { "yes" } else { "no" }));
        // `ao` is never pinned: mpv auto-negotiates (pulse via PipeWire on
        // Linux, coreaudio on macOS).
        if !opts.audio {
            options.push(("audio", "no"));
        }
        // Looping a stream with no end is meaningless, and mpv would only
        // ever act on it after an EOF that never arrives.
        if opts.loop_file && !live {
            options.push(("loop-file", "inf"));
        }
        options.push(("cache", "yes"));
        // Live: a small cache, because every byte buffered is a second
        // further behind. Recorded: whatever the user configured, which is
        // there to ride out a slow network read of a file.
        let cache = if live { "8MiB".to_owned() } else { format!("{}MiB", opts.cache_mb) };
        options.push(("demuxer-max-bytes", &cache));
        if live {
            options.push(("demuxer-readahead-secs", "0"));
        }
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
            loaded: false,
            reported_size: (0, 0),
            cache_mb: opts.cache_mb,
            loop_file: opts.loop_file,
            audio_tracks: Vec::new(),
            subtitle_tracks: Vec::new(),
            view: resume.map_or(tessera_core::view::View::FULL, |r| r.view),
            view_for: resume.map(|_| item.id.clone()),
            view_anim: None,
            hud: None,
            pending_start_reset,
            reconnect: None,
            progress_at: std::time::Instant::now(),
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
            view: self.view(),
        }
    }

    pub fn load(&self, item: &MediaItem) -> Result<(), tessera_mpv::Error> {
        // This mpv instance was configured for whatever the tile held when
        // it was *created*, and one queue mixes films with cameras freely.
        // So the options that differ are re-applied here, in both
        // directions — otherwise a camera loaded into a tile that was
        // playing a film inherits a 512 MiB demuxer cache, which on a live
        // feed is not resilience but half a minute of latency, and a film
        // loaded after a camera inherits a cache far too small to ride out
        // a slow read.
        let live = item.is_live();
        let cache = if live { "8MiB".to_owned() } else { format!("{}MiB", self.cache_mb) };
        for (key, value) in [
            ("demuxer-max-bytes", cache.as_str()),
            ("demuxer-readahead-secs", if live { "0" } else { "1" }),
            ("cache-pause", if live { "no" } else { "yes" }),
            // Harmless for anything that is not RTSP, and UDP loses packets.
            ("rtsp-transport", "tcp"),
            ("network-timeout", if live { "10" } else { "0" }),
            // Looping a stream with no end would only ever act on an EOF
            // that never arrives.
            ("loop-file", if self.loop_file && !live { "inf" } else { "no" }),
            // A pick made on the last item (or restored by a screen move)
            // is an mpv option and would carry over to this one. Each item
            // starts from its own default; a remembered pick for *this*
            // one goes back on at TracksKnown. Audio stays "no" while the
            // config tab has it off.
            ("sid", "auto"),
            ("aid", if self.audio_enabled { "auto" } else { "no" }),
        ] {
            if let Err(e) = self.mpv.command(&["set", key, value]) {
                eprintln!("tile {}: set {key} failed: {e}", self.id);
            }
        }
        // Async: opening a file can block on I/O (network sources), which
        // would stall the whole render loop.
        self.mpv.load_file_async(&item.uri, "replace")
    }

    pub fn set_paused(&mut self, paused: bool) {
        // Resuming a live stream rejoins it at the live edge, because on a
        // camera "play" means "show me now", not "show me what I missed".
        //
        // The demuxer keeps reading while paused, so a paused camera falls
        // behind by however long it was paused — bounded by the live cache,
        // which measured 10.3s on a 4K feed (8 MiB at that bitrate). Ten
        // seconds is a lot on a security camera.
        let resuming_live = !paused && self.item.is_live() && self.is_paused();
        if let Err(e) = self.mpv.command(&["set", "pause", if paused { "yes" } else { "no" }]) {
            eprintln!("tile {}: set pause failed: {e}", self.id);
        }
        if !paused {
            // The clock has not moved for as long as this was paused, which
            // is not a stall. Start it over.
            self.progress_at = std::time::Instant::now();
        }
        if resuming_live {
            self.go_live();
        }
    }

    /// Fires a reconnect attempt if one is scheduled and due.
    ///
    /// Deliberately driven from the render tick rather than a timer thread:
    /// the retry is a `loadfile`, which is exactly what the tick already
    /// does, and a tile that is not being ticked is not on screen anyway.
    fn retry_if_due(&mut self) {
        let Some(r) = &self.reconnect else { return };
        if std::time::Instant::now() < r.next_at {
            return;
        }
        // Nothing to reconnect to while the user has it paused — and
        // resuming rejoins on its own.
        if self.is_paused() {
            return;
        }
        let attempts = r.attempts;
        self.reconnect = Some(Reconnect {
            attempts,
            // Re-armed before the attempt, so a load that never completes
            // (a camera answering but never sending a frame) still gets
            // retried rather than wedging here forever.
            next_at: std::time::Instant::now() + reconnect_delay(attempts + 1),
        });
        self.progress_at = std::time::Instant::now();
        self.go_live();
    }

    /// True while a live stream is down and being retried.
    pub fn reconnecting(&self) -> bool {
        self.reconnect.is_some()
    }

    /// Rejoins a live stream at its current edge; a no-op for recorded media.
    ///
    /// By reloading, not seeking. Seeking forward on a live stream *looks*
    /// like it should work — mpv accepts the command and returns success —
    /// and changes nothing at all; measured, the lag was identical before
    /// and after. A reload genuinely returns to the edge and costs 0.07s on
    /// a LAN camera, which is below noticing.
    pub fn go_live(&self) {
        if !self.item.is_live() {
            return;
        }
        if let Err(e) = self.mpv.load_file_async(&self.item.uri, "replace") {
            eprintln!("tile {}: could not rejoin the live stream: {e}", self.id);
        }
    }

    /// Seconds behind the live edge — what the demuxer holds ahead of the
    /// playback position. Zero for recorded media, which is not behind
    /// anything.
    pub fn behind_live(&self) -> f64 {
        if !self.item.is_live() {
            return 0.0;
        }
        self.mpv.get_f64("demuxer-cache-duration").unwrap_or(0.0)
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

    /// The crop/zoom in effect: the one set for the item now playing (part
    /// way through an animated move, if one is under way), else the full
    /// picture.
    pub fn view(&self) -> tessera_core::view::View {
        if self.view_for.as_deref() != Some(self.item.id.as_str()) {
            return tessera_core::view::View::FULL;
        }
        match &self.view_anim {
            Some(a) => {
                let t = (a.start.elapsed().as_secs_f32() / a.secs).min(1.0);
                // Smoothstep: eases in and out, like a camera move
                tessera_core::view::View::between(&a.from, &self.view, t * t * (3.0 - 2.0 * t))
            }
            None => self.view,
        }
    }

    /// Where the view is heading (the end of a move under way), so moves
    /// asked for in quick succession build on each other.
    pub fn view_target(&self) -> tessera_core::view::View {
        if self.view_for.as_deref() == Some(self.item.id.as_str()) {
            self.view
        } else {
            tessera_core::view::View::FULL
        }
    }

    /// Sets the crop/zoom for the item now playing.
    pub fn set_view(&mut self, view: tessera_core::view::View) {
        self.view = view;
        self.view_anim = None;
        self.view_for = Some(self.item.id.clone());
    }

    /// Moves to `view` over `secs`, from wherever the view is right now.
    pub fn set_view_animated(&mut self, view: tessera_core::view::View, secs: f32) {
        let from = self.view();
        self.view_for = Some(self.item.id.clone());
        self.view = view;
        self.view_anim = (secs > 0.0).then(|| ViewAnim { from, start: std::time::Instant::now(), secs });
    }

    /// The overlay drawn on the picture, for the item now playing.
    pub fn hud(&self) -> Option<&crate::hud::Hud> {
        (self.view_for.as_deref() == Some(self.item.id.as_str())).then_some(()).and(self.hud.as_ref())
    }

    pub fn set_hud(&mut self, hud: Option<crate::hud::Hud>) {
        if self.view_for.as_deref() != Some(self.item.id.as_str()) {
            // Tie it to this item, starting from the full picture
            self.view = tessera_core::view::View::FULL;
            self.view_anim = None;
            self.view_for = Some(self.item.id.clone());
        }
        self.hud = hud;
    }

    /// Length in seconds, if known (not for a live stream).
    pub fn duration(&self) -> Option<f64> {
        self.mpv.get_f64("duration").filter(|d| *d > 0.0)
    }

    /// The chapters as (start seconds, title), in order. Empty if none.
    pub fn chapters(&self) -> Vec<(f64, String)> {
        let n = self.mpv.get_i64("chapter-list/count").unwrap_or(0);
        (0..n)
            .map(|i| {
                let time = self.mpv.get_f64(&format!("chapter-list/{i}/time")).unwrap_or(0.0);
                let title = self
                    .mpv
                    .get_string(&format!("chapter-list/{i}/title"))
                    // Rippers often title chapters with their own start
                    // time ("00:12:03.000"), which says nothing a reader
                    // can use: treat those as unnamed.
                    .filter(|t| !t.is_empty() && tessera_core::timecode::parse_time(t).is_none())
                    .unwrap_or_else(|| format!("chapter {}", i + 1));
                (time, title)
            })
            .collect()
    }

    /// Index into `chapters()` of the current chapter.
    pub fn chapter(&self) -> Option<usize> {
        self.mpv.get_i64("chapter").filter(|c| *c >= 0).map(|c| c as usize)
    }

    /// The subtitle line on screen right now, if any.
    pub fn sub_text(&self) -> Option<String> {
        self.mpv.get_string("sub-text").filter(|t| !t.trim().is_empty())
    }

    pub fn is_paused(&self) -> bool {
        self.mpv.get_string("pause").map(|v| v == "yes").unwrap_or(false)
    }

    /// Cumulative decoder-side frame drops (monotonic counter).
    pub fn decoder_drops(&self) -> i64 {
        self.mpv.get_i64("decoder-frame-drop-count").unwrap_or(0)
    }

    /// Toggles this tile's audio. Returns the new muted state.
    /// Only meaningful while audio is enabled (the default; --no-audio or
    /// the config tab turn it off); otherwise the whole audio pipeline is
    /// disabled per-tile.
    /// Sets mute directly (solo applies this to every tile at once).
    pub fn set_muted(&self, muted: bool) {
        if let Err(e) = self.mpv.command(&["set", "mute", if muted { "yes" } else { "no" }]) {
            eprintln!("tile {}: set mute failed: {e}", self.id);
        }
    }

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
                ff_index: t.get("ff-index").and_then(|v| v.as_i64()),
                external_filename: t.get("external-filename").and_then(|v| v.as_str()).map(str::to_owned),
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

    /// Puts back a remembered track pick, if this file still has the track
    /// it names. Returns what changed, for the log.
    pub fn apply_choice(&mut self, kind: Kind, choice: &Choice) -> Option<String> {
        // With audio off in the config tab, `aid` is "no" on purpose; a
        // remembered language must not turn the sound back on.
        if kind == Kind::Audio && !self.audio_enabled {
            return None;
        }
        let list = match kind {
            Kind::Audio => &self.audio_tracks,
            Kind::Subtitle => &self.subtitle_tracks,
        };
        let tracks: Vec<_> = list.iter().map(TrackInfo::core).collect();
        let want = tessera_core::tracks::pick(choice, &tracks)?;
        if want == list.iter().find(|t| t.selected).map(|t| t.id) {
            return None;
        }
        match kind {
            Kind::Audio => self.set_audio_track(want),
            Kind::Subtitle => self.set_subtitle_track(want),
        }
        let list = match kind {
            Kind::Audio => &self.audio_tracks,
            Kind::Subtitle => &self.subtitle_tracks,
        };
        Some(match want {
            Some(id) => list.iter().find(|t| t.id == id).map_or_else(|| format!("#{id}"), TrackInfo::describe),
            None => "off".into(),
        })
    }

    /// What to remember about the track of `kind` now playing, or that
    /// none is.
    pub fn choice(&self, kind: Kind) -> Choice {
        let list = match kind {
            Kind::Audio => &self.audio_tracks,
            Kind::Subtitle => &self.subtitle_tracks,
        };
        match list.iter().find(|t| t.selected) {
            Some(t) => Choice::Track { id: t.id, lang: t.lang.clone(), title: t.title.clone(), codec: t.codec.clone() },
            None => Choice::Off,
        }
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
        // `audio` is mpv's alias for `aid`, which takes a track, `auto` or
        // `no` — not `yes`. Asking for `audio=yes` failed with a raw
        // MPV_ERROR_COMMAND on every tile.
        let aid = if enabled { "auto" } else { "no" };
        if let Err(e) = self.mpv.command(&["set", "aid", aid]) {
            eprintln!("tile {}: set aid {aid} failed: {e}", self.id);
        }
    }

    /// Adjusts the demuxer cache at runtime (best effort — mpv applies it
    /// to ongoing demuxing where it can, and fully on the next load).
    ///
    /// Live tiles are left alone: the global cache setting exists to ride
    /// out slow reads of a *file*, and applying it to a camera would hand
    /// back the latency the live profile just removed.
    pub fn set_cache_mb(&mut self, cache_mb: u32) {
        self.cache_mb = cache_mb;
        if self.item.is_live() {
            return;
        }
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
        self.retry_if_due();
        while let Some(ev) = self.mpv.poll_event() {
            match ev {
                MpvEvent::FileLoaded => {
                    // A load that follows a failure is a recovery, and is
                    // worth saying so — a tile that went black and came back
                    // silently leaves you wondering whether you imagined it.
                    if self.reconnect.take().is_some() {
                        events.push(TileEvent::Reconnected);
                    }
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
                    // A fresh load starts the stall clock over; otherwise a
                    // reconnect would be judged on the silence that
                    // preceded it.
                    self.progress_at = std::time::Instant::now();
                    self.duration = self.mpv.get_f64("duration").unwrap_or(-1.0);
                    self.refresh_tracks();
                    self.reported_size = (w, h);
                    events.push(TileEvent::Loaded { width: w, height: h, fps, codec });
                    events.push(TileEvent::TracksKnown);
                }
                MpvEvent::EndFile { reason: tessera_mpv::EndFileReason::Eof, .. } => {
                    events.push(TileEvent::EndOfFile);
                }
                MpvEvent::EndFile { reason, error } => {
                    if error != 0 {
                        eprintln!("tile {}: stream error ({reason:?}, code {error})", self.id);
                        // Drop the frame still on screen. It belongs to the
                        // item this tile played *before*, and leaving it up
                        // makes a failed load look like a freeze — the
                        // queue has moved on and the picture has not, which
                        // reads as the app being stuck rather than the
                        // stream being unopenable.
                        self.destroy_gl(gl);
                        self.tex_w = 0;
                        self.tex_h = 0;
                        self.loaded = false;
                        let why = format!(
                            "could not open the stream ({})",
                            tessera_mpv::error_name(error).to_lowercase().replace('_', " ")
                        );
                        if self.item.is_live() {
                            // A camera is not a file that failed to open —
                            // it is a thing that will probably be back. Keep
                            // the slot and keep trying.
                            let attempts =
                                self.reconnect.as_ref().map_or(0, |r| r.attempts) + 1;
                            let wait = reconnect_delay(attempts);
                            self.reconnect = Some(Reconnect {
                                attempts,
                                next_at: std::time::Instant::now() + wait,
                            });
                            println!(
                                "tile {}: \"{}\" dropped — retry {attempts} in {}s",
                                self.id,
                                self.item.title,
                                wait.as_secs()
                            );
                            // Only the first drop is announced; the rest are
                            // the retry loop doing its job, and a toast every
                            // thirty seconds for an unplugged camera is noise.
                            if attempts == 1 {
                                events.push(TileEvent::Failed {
                                    reason: format!("{why} — reconnecting"),
                                });
                            }
                        } else {
                            events.push(TileEvent::Failed { reason: why });
                        }
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
            // A live stream reports no dimensions at file-load: mpv has the
            // container open but has not seen a frame, and the low-latency
            // profile deliberately probes as little as possible. The first
            // real frame is where the size actually becomes known, so a
            // size change here re-reports the stream — otherwise the decode
            // budget keeps the 0x0 it was told, which weighs nothing, and a
            // wall of cameras admits itself for free.
            // A file already gave its size at load, so its first frame is
            // only news if the size came out different.
            let resized = self.ensure_texture(gl);
            let size = (self.tex_w as u32, self.tex_h as u32);
            if resized && self.tex_w > 0 && self.tex_h > 0 && size != self.reported_size {
                self.reported_size = size;
                let fps = self
                    .mpv
                    .get_f64("container-fps")
                    .or_else(|| self.mpv.get_f64("estimated-vf-fps"))
                    .unwrap_or(30.0);
                let codec = self.mpv.get_string("video-codec").unwrap_or_default();
                events.push(TileEvent::Loaded {
                    width: self.tex_w as u32,
                    height: self.tex_h as u32,
                    fps,
                    codec,
                });
            }
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
        // A live stream that has stopped advancing has dropped, even though
        // it never said so. Checked only for live tiles, and the paused
        // check — an mpv round trip — only once the threshold is already
        // exceeded, so the common path stays free.
        if self.item.is_live() {
            if tp > self.last_time_pos {
                self.progress_at = std::time::Instant::now();
            } else if self.reconnect.is_none()
                && self.progress_at.elapsed() > STALL_TIMEOUT
                && !self.is_paused()
            {
                let wait = reconnect_delay(1);
                self.reconnect = Some(Reconnect {
                    attempts: 1,
                    next_at: std::time::Instant::now() + wait,
                });
                println!(
                    "tile {}: \"{}\" stopped sending — retry 1 in {}s",
                    self.id,
                    self.item.title,
                    wait.as_secs()
                );
                events.push(TileEvent::Failed {
                    reason: "the feed stopped — reconnecting".to_owned(),
                });
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

    /// Ensures the FBO matches the stream's size. Returns true when the
    /// size changed, which the caller reports so the budget can correct
    /// itself.
    fn ensure_texture(&mut self, gl: &glow::Context) -> bool {
        let w = self.mpv.get_i64("width").unwrap_or(0) as i32;
        let h = self.mpv.get_i64("height").unwrap_or(0) as i32;
        if w <= 0 || h <= 0 {
            return false;
        }
        if self.texture.is_some() && w == self.tex_w && h == self.tex_h {
            return false;
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
        true
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

    /// What mpv is doing with this file right now, for the info panel:
    /// which decoder, what frames it is producing, how fast the bits are
    /// arriving. The one source of format details for a local file, which
    /// has no server to ask.
    pub fn playback_info(&self) -> tessera_core::info::Section {
        use tessera_core::info::human_bitrate;
        let s = |p: &str| self.mpv.get_string(p).filter(|v| !v.is_empty());
        let f = |p: &str| self.mpv.get_f64(p);
        let mut out = tessera_core::info::Section::new("playing");
        let hw = match s("hwdec-current").as_deref() {
            None | Some("no") => "software".to_owned(),
            Some(h) => h.to_owned(),
        };
        out.row("decoder", [Some(hw), s("video-format")].into_iter().flatten().collect::<Vec<_>>().join(" · "));
        let mut pic = Vec::new();
        if let (Some(w), Some(h)) = (self.mpv.get_i64("video-params/w"), self.mpv.get_i64("video-params/h")) {
            pic.push(format!("{w}×{h}"));
        }
        pic.extend(s("video-params/pixelformat"));
        // Primaries and transfer together are what says HDR: bt.2020 + pq.
        if let (Some(p), Some(g)) = (s("video-params/primaries"), s("video-params/gamma")) {
            pic.push(format!("{p} / {g}"));
        }
        out.row("frames", pic.join(" · "));
        if let Some(fps) = f("estimated-vf-fps").or_else(|| f("container-fps")).filter(|v| *v > 0.0) {
            out.row("frame rate", format!("{fps:.3} fps"));
        }
        let rates: Vec<String> = [("video", "video-bitrate"), ("audio", "audio-bitrate")]
            .iter()
            .filter_map(|(what, p)| f(p).filter(|v| *v > 0.0).map(|v| format!("{what} {}", human_bitrate(v))))
            .collect();
        out.row("bitrate", rates.join(" · "));
        if self.audio_enabled() {
            let ch = self.mpv.get_i64("audio-params/channel-count").map(|c| format!("{c} ch"));
            out.row("audio", [s("audio-codec-name"), ch].into_iter().flatten().collect::<Vec<_>>().join(" · "));
        }
        out.row("container", s("file-format").unwrap_or_default());
        if let Some(c) = f("demuxer-cache-duration") {
            out.row("buffered", format!("{c:.1} s ahead"));
        }
        out.row("dropped", format!("{} frames", self.decoder_drops()));
        out
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

    /// Splits the tile for teardown: frees the mpv **render** context on
    /// the CALLING thread and hands back the bare mpv core.
    ///
    /// `mpv_render_context_free` frees GL objects, so it must run with this
    /// tile's GL context current — which means the main thread, right after
    /// the owning screen's `make_current`. Doing it on a worker (no context
    /// current anywhere) segfaults inside mpv. Terminating the core, the
    /// genuinely slow half, touches no GL and can go on any thread.
    pub fn split_for_teardown(self) -> tessera_mpv::Mpv {
        // Tile has no Drop impl, so the fields can be moved out; `render`
        // is dropped here and now, `mpv` escapes.
        let Tile { render, mpv, .. } = self;
        drop(render);
        mpv
    }

    /// Dumps this tile's current FBO content to a PNG. Diagnostic aid.
    pub fn dump_fbo(&self, gl: &glow::Context, path: &std::path::Path) -> std::io::Result<()> {
        let Some(img) = self.read_fbo(gl) else { return Ok(()) };
        crate::png_util::write_png_rgb(path, img.w as i32, img.h as i32, &img.rgb)
    }

    /// The colours of what this tile shows (its crop and zoom applied), as a
    /// `w`×`h` grid of averages, bottom row first: for lights that follow
    /// the picture, many times a second. The GPU does the averaging: the
    /// frame's mipmaps are rebuilt (level 0 is mpv's, so the picture itself
    /// is untouched) and a level a few texels per cell is read back, a few
    /// kilobytes rather than a full-size frame. `read` is a spare
    /// framebuffer to attach that level to. None before the first frame.
    pub fn read_colors(&self, gl: &glow::Context, read: glow::Framebuffer, w: u32, h: u32) -> Option<crate::screen::RawImage> {
        let tex = self.texture?;
        if self.tex_w <= 0 || self.tex_h <= 0 || w == 0 || h == 0 {
            return None;
        }
        // The region shown, y down, as fractions; the texture is bottom-up
        let (x0, y0, x1, y1) = self.view().source_rect();
        let (rw, rh) = ((x1 - x0) * self.tex_w as f32, (y1 - y0) * self.tex_h as f32);
        // The smallest level that still has eight texels or more a cell each
        // way: coarser, and the texels at the crop's edge would carry the
        // letterbox's black into the picture's
        let mut level = 0;
        while rw / (1 << (level + 1)) as f32 >= (w * 8) as f32 && rh / (1 << (level + 1)) as f32 >= (h * 8) as f32 {
            level += 1;
        }
        let (lw, lh) = ((self.tex_w >> level).max(1), (self.tex_h >> level).max(1));
        // Rounded inwards, for the same reason
        let px0 = ((x0 * lw as f32).ceil() as i32).clamp(0, lw - 1);
        let px1 = ((x1 * lw as f32).floor() as i32).clamp(px0 + 1, lw);
        let py0 = (((1.0 - y1) * lh as f32).ceil() as i32).clamp(0, lh - 1);
        let py1 = (((1.0 - y0) * lh as f32).floor() as i32).clamp(py0 + 1, lh);
        let (sw, sh) = ((px1 - px0) as usize, (py1 - py0) as usize);
        let mut px = vec![0u8; sw * sh * 3];
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            if level > 0 {
                gl.generate_mipmap(glow::TEXTURE_2D);
            }
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(read));
            gl.framebuffer_texture_2d(glow::READ_FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(tex), level);
            gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
            gl.read_pixels(px0, py0, sw as i32, sh as i32, glow::RGB, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(&mut px));
            gl.framebuffer_texture_2d(glow::READ_FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, None, 0);
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
        }
        Some(crate::screen::RawImage { w, h, rgb: tessera_core::colors::box_average(&px, sw, sh, w as usize, h as usize) })
    }

    /// This tile's current frame, read back from its FBO (bottom row
    /// first). None before the first frame has been rendered.
    pub fn read_fbo(&self, gl: &glow::Context) -> Option<crate::screen::RawImage> {
        let fbo = self.fbo?;
        let (w, h) = (self.tex_w, self.tex_h);
        if w <= 0 || h <= 0 {
            return None;
        }
        let mut rgb = vec![0u8; (w as usize) * (h as usize) * 3];
        unsafe {
            gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.read_pixels(0, 0, w, h, glow::RGB, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(&mut rgb));
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }
        Some(crate::screen::RawImage { w: w as u32, h: h as u32, rgb })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_backs_off_then_holds_and_never_gives_up() {
        let s = |a: u32| reconnect_delay(a).as_secs();
        // Quick for the first few, because most drops are a blip.
        assert_eq!(s(1), 1);
        assert_eq!(s(2), 2);
        assert_eq!(s(3), 5);
        // Then it settles, rather than hammering an unplugged camera.
        assert_eq!(s(6), 30);
        // And holds there forever — a camera that comes back on Monday is
        // simply there again, with no attempt cap to have exhausted.
        assert_eq!(s(50), 30);
        assert_eq!(s(u32::MAX), 30);
        // Attempt 0 should not underflow into the last slot.
        assert_eq!(s(0), 1);
    }

    #[test]
    fn the_backoff_never_reaches_zero() {
        // A zero delay would turn a dead camera into a busy loop issuing
        // loadfile as fast as the render thread ticks.
        for a in 0..64 {
            assert!(reconnect_delay(a).as_secs() >= 1, "attempt {a}");
        }
    }
}
