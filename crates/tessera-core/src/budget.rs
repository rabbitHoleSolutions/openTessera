/// Decode admission controller, ported from the Quest app's DecodeBudgetController.
///
/// Gates: weighted macroblocks/sec throughput ceiling (static, hardware-derived)
/// multiplied by an adaptive factor driven by real frame-drop telemetry, and a
/// max concurrent streams setting. A single stream is always admissible alone.
///
/// Eviction policy: OLDEST_STARTED — the playing stream with the earliest
/// admission time is paused to make room, mirroring the Quest dock behavior.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
    Vp9,
    Av1,
    Other,
}

impl Codec {
    pub fn from_name(name: &str) -> Codec {
        let n = name.to_ascii_lowercase();
        if n.contains("h264") || n.contains("264") || n.contains("avc") {
            Codec::H264
        } else if n.contains("hevc") || n.contains("h265") || n.contains("265") {
            Codec::Hevc
        } else if n.contains("vp9") {
            Codec::Vp9
        } else if n.contains("av1") || n.contains("av01") {
            Codec::Av1
        } else {
            Codec::Other
        }
    }

    /// Relative decode cost per macroblock vs the HEVC baseline, from the
    /// Quest app's measurements (H.264 fixed-function costs more than HEVC/AV1
    /// on modern hardware decoders).
    pub fn weight(&self) -> f64 {
        match self {
            Codec::H264 => 1.35,
            Codec::Hevc => 1.0,
            Codec::Vp9 => 1.15,
            Codec::Av1 => 1.0,
            Codec::Other => 1.5,
        }
    }
}

#[derive(Clone, Debug)]
pub struct StreamSpec {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub codec: Codec,
}

impl StreamSpec {
    pub fn macroblocks_per_sec(&self) -> f64 {
        let mb_w = (self.width as f64 / 16.0).ceil();
        let mb_h = (self.height as f64 / 16.0).ceil();
        mb_w * mb_h * self.fps.max(1.0)
    }

    pub fn weight(&self) -> f64 {
        self.macroblocks_per_sec() * self.codec.weight()
    }

    pub fn placeholder() -> StreamSpec {
        StreamSpec { width: 1920, height: 1080, fps: 30.0, codec: Codec::H264 }
    }
}

/// Macroblocks/sec of one 1080p30 stream: ceil(1920/16) * ceil(1080/16) * 30
/// = 120 * 68 * 30 = 244800.
///
/// This is the unit the whole budget is denominated in: *macroblocks* per
/// second, weighted by codec. It was once abbreviated "mbps", which the
/// status bar duly printed — and "23500800 mbps" reads as twenty-three
/// terabits of video. Nothing here is a bitrate; the accessors say `load`
/// and the UI divides through to 1080p30-equivalents.
pub const MACROBLOCKS_1080P30: f64 = 244_800.0;

/// Default ceiling: 96 x 1080p30-equivalents (24 x 4K30-equivalents), measured
/// against NVDEC on an RTX 5080 with enormous headroom remaining.
pub const DEFAULT_CEILING_EQUIVALENTS: f64 = 96.0;

pub const DEFAULT_MAX_STREAMS: usize = 128;

/// Sustained decoder drop rate (drops/sec) that triggers adaptive shrink.
pub const DROP_RATE_THRESHOLD: f64 = 10.0;

/// Consecutive sampling windows of sustained drops before shrinking.
pub const DROP_STREAK_LIMIT: u32 = 2;

/// Adaptive multiplier floor: never shrink below 30% of the static ceiling.
pub const ADAPTIVE_FLOOR: f64 = 0.30;

#[derive(Clone, Debug, PartialEq)]
pub enum Admission {
    Admitted,
    /// Admitted after evicting another stream; the evicted id is included.
    AdmittedEvicting { evicted: usize },
    RejectedBudget { would_use: f64, ceiling: f64 },
    RejectedMaxStreams { max: usize },
}

struct ActiveStream {
    id: usize,
    spec: StreamSpec,
    started_at: std::time::Instant,
    drop_streak: u32,
}

pub struct DecodeBudget {
    ceiling_load: f64,
    /// Multiplier in [ADAPTIVE_FLOOR, 1.0] driven by drop feedback.
    adaptive: f64,
    max_streams: usize,
    active: Vec<ActiveStream>,
    total_evictions: u64,
}

impl DecodeBudget {
    pub fn new(ceiling_equivalents: f64, max_streams: usize) -> DecodeBudget {
        DecodeBudget {
            ceiling_load: ceiling_equivalents * MACROBLOCKS_1080P30,
            adaptive: 1.0,
            max_streams,
            active: Vec::new(),
            total_evictions: 0,
        }
    }

    pub fn ceiling_load(&self) -> f64 {
        self.ceiling_load
    }

    /// The effective ceiling after adaptive feedback.
    pub fn effective_ceiling_load(&self) -> f64 {
        self.ceiling_load * self.adaptive
    }

    pub fn adaptive(&self) -> f64 {
        self.adaptive
    }

    pub fn max_streams(&self) -> usize {
        self.max_streams
    }

    /// Changes the max-streams cap at runtime. When the new cap is below
    /// the active count, the oldest-started streams are evicted (returned
    /// so the caller can pause them) until the count fits.
    pub fn set_max_streams(&mut self, n: usize) -> Vec<usize> {
        self.max_streams = n;
        let mut evicted = Vec::new();
        while self.active.len() > self.max_streams {
            match self.evict_oldest() {
                Some(id) => evicted.push(id),
                None => break,
            }
        }
        evicted
    }

    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    pub fn active_ids(&self) -> Vec<usize> {
        self.active.iter().map(|a| a.id).collect()
    }

    pub fn used_load(&self) -> f64 {
        self.active.iter().map(|a| a.spec.weight()).sum()
    }

    pub fn utilization(&self) -> f64 {
        self.used_load() / self.effective_ceiling_load()
    }

    /// Current load expressed in 1080p30 streams — the unit to show a
    /// person. Raw load is weighted macroblocks/sec, which is a number in
    /// the millions and means nothing on a status bar.
    pub fn used_equivalents(&self) -> f64 {
        self.used_load() / MACROBLOCKS_1080P30
    }

    /// The effective ceiling in the same unit.
    pub fn ceiling_equivalents(&self) -> f64 {
        self.effective_ceiling_load() / MACROBLOCKS_1080P30
    }

    pub fn total_evictions(&self) -> u64 {
        self.total_evictions
    }

    fn fits(&self, spec: &StreamSpec) -> bool {
        // A single stream is always admissible alone — a wall that can't
        // play one stream is worse than one that drops all but one.
        self.active.is_empty() || self.used_load() + spec.weight() <= self.effective_ceiling_load()
    }

    fn admit(&mut self, stream_id: usize, spec: &StreamSpec) {
        self.active.push(ActiveStream {
            id: stream_id,
            spec: spec.clone(),
            started_at: std::time::Instant::now(),
            drop_streak: 0,
        });
    }

    /// Requests admission for a stream. On budget rejection, evicts the
    /// oldest-started active stream and retries — but only if the eviction
    /// would actually make room. Max-streams rejection also evicts, so
    /// "oldest stream is paused to maintain the user's desired stream count"
    /// holds globally.
    pub fn request(&mut self, stream_id: usize, spec: &StreamSpec) -> Admission {
        if self.active.iter().any(|a| a.id == stream_id) {
            return Admission::Admitted;
        }
        if self.active.len() < self.max_streams && self.fits(spec) {
            self.admit(stream_id, spec);
            return Admission::Admitted;
        }
        if let Some(oldest) = self.active.iter().min_by_key(|a| a.started_at) {
            let after_evict = self.used_load() - oldest.spec.weight();
            let slot_frees = self.active.len() - 1 < self.max_streams;
            // If the last stream would be evicted, the single-stream rule
            // admits anything; otherwise check the arithmetic.
            let fits_after = self.active.len() == 1
                || after_evict + spec.weight() <= self.effective_ceiling_load();
            if slot_frees && fits_after {
                let evicted = self.evict_oldest().expect("oldest exists");
                self.admit(stream_id, spec);
                return Admission::AdmittedEvicting { evicted };
            }
        }
        if self.active.len() >= self.max_streams {
            return Admission::RejectedMaxStreams { max: self.max_streams };
        }
        Admission::RejectedBudget {
            would_use: self.used_load() + spec.weight(),
            ceiling: self.effective_ceiling_load(),
        }
    }

    /// Removes and returns the oldest-started active stream id, if any.
    /// The caller is responsible for actually pausing the victim player.
    pub fn evict_oldest(&mut self) -> Option<usize> {
        let oldest = self
            .active
            .iter()
            .enumerate()
            .min_by_key(|(_, a)| a.started_at)
            .map(|(i, _)| i)?;
        let victim = self.active.remove(oldest);
        self.total_evictions += 1;
        Some(victim.id)
    }

    /// Replaces the spec for a known stream (e.g. real probe data replacing the
    /// admission placeholder). Reports the new utilization for logging.
    pub fn update(&mut self, stream_id: usize, spec: StreamSpec) -> f64 {
        if let Some(entry) = self.active.iter_mut().find(|a| a.id == stream_id) {
            entry.spec = spec;
        }
        self.utilization()
    }

    pub fn release(&mut self, stream_id: usize) {
        self.active.retain(|a| a.id != stream_id);
    }

    pub fn spec(&self, stream_id: usize) -> Option<&StreamSpec> {
        self.active.iter().find(|a| a.id == stream_id).map(|a| &a.spec)
    }

    /// Feeds a per-stream decoder drop rate (drops/sec over the last window)
    /// into the adaptive loop. Returns true while any stream is in a
    /// sustained-drop condition.
    pub fn report_drop_rate(&mut self, stream_id: usize, drops_per_sec: f64) -> bool {
        let mut hot = false;
        for a in self.active.iter_mut() {
            if a.id == stream_id {
                if drops_per_sec >= DROP_RATE_THRESHOLD {
                    a.drop_streak += 1;
                    hot = true;
                } else {
                    a.drop_streak = 0;
                }
            } else if a.drop_streak > 0 {
                hot = true;
            }
        }
        hot
    }

    /// Applies the adaptive shrink when streams have sustained drops. Returns
    /// the worst offender (max streak) to proactively evict, if any qualified.
    pub fn enforce(&mut self) -> Option<usize> {
        let victim = self
            .active
            .iter()
            .filter(|a| a.drop_streak >= DROP_STREAK_LIMIT)
            .max_by_key(|a| a.drop_streak)
            .map(|a| a.id);
        if victim.is_some() {
            self.adaptive = (self.adaptive * 0.85).max(ADAPTIVE_FLOOR);
            for a in self.active.iter_mut() {
                a.drop_streak = 0;
            }
        }
        victim
    }

    /// Call when a sampling window was clean (no sustained drops): recovers
    /// the adaptive multiplier toward 1.0.
    pub fn recover(&mut self) {
        self.adaptive = (self.adaptive * 1.10).min(1.0);
        for a in self.active.iter_mut() {
            a.drop_streak = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(w: u32, h: u32, fps: f64, codec: Codec) -> StreamSpec {
        StreamSpec { width: w, height: h, fps, codec }
    }

    #[test]
    fn weights() {
        let hd = s(1920, 1080, 30.0, Codec::Hevc);
        assert!((hd.weight() - MACROBLOCKS_1080P30).abs() < 1.0);
        let uhd_h264 = s(3840, 2160, 30.0, Codec::H264);
        assert!(uhd_h264.weight() > 4.0 * MACROBLOCKS_1080P30);
    }

    #[test]
    fn admit_until_ceiling() {
        let mut b = DecodeBudget::new(4.0, 100);
        for i in 0..4 {
            assert_eq!(b.request(i, &s(1920, 1080, 30.0, Codec::Hevc)), Admission::Admitted);
        }
        match b.request(99, &s(1920, 1080, 30.0, Codec::Hevc)) {
            // ceiling 4.0: evicting one 1x stream frees exactly one slot, so
            // the request succeeds by evicting the oldest.
            Admission::AdmittedEvicting { evicted } => assert_eq!(evicted, 0),
            Admission::RejectedBudget { would_use, ceiling } => {
                assert!(would_use > ceiling);
            }
            other => panic!("expected rejection-or-eviction, got {other:?}"),
        }
    }

    #[test]
    fn single_stream_always_admissible() {
        let mut b = DecodeBudget::new(0.5, 16);
        // Even a stream bigger than the whole ceiling plays alone.
        assert_eq!(b.request(1, &s(3840, 2160, 60.0, Codec::H264)), Admission::Admitted);
        // A second stream of any size forces eviction of the first.
        match b.request(2, &s(640, 360, 30.0, Codec::Hevc)) {
            Admission::AdmittedEvicting { evicted } => assert_eq!(evicted, 1),
            other => panic!("expected eviction, got {other:?}"),
        }
        assert_eq!(b.active_ids(), vec![2]);
    }

    #[test]
    fn eviction_picks_oldest_started() {
        let mut b = DecodeBudget::new(2.0, 100);
        b.request(1, &s(1920, 1080, 30.0, Codec::Hevc));
        b.request(2, &s(1920, 1080, 30.0, Codec::Hevc));
        // id 3 admitted, evicting 1 (oldest)
        match b.request(3, &s(1920, 1080, 30.0, Codec::Hevc)) {
            Admission::AdmittedEvicting { evicted } => assert_eq!(evicted, 1),
            other => panic!("expected eviction, got {other:?}"),
        }
        // re-admit 1: its started_at refreshes, so 2 is now the oldest
        b.release(3);
        assert_eq!(b.request(1, &s(1920, 1080, 30.0, Codec::Hevc)), Admission::Admitted);
        match b.request(3, &s(1920, 1080, 30.0, Codec::Hevc)) {
            Admission::AdmittedEvicting { evicted } => assert_eq!(evicted, 2),
            other => panic!("expected eviction of 2, got {other:?}"),
        }
        assert_eq!(b.total_evictions(), 2);
    }

    #[test]
    fn max_streams_gate() {
        let mut b = DecodeBudget::new(100.0, 2);
        assert_eq!(b.request(1, &s(1920, 1080, 30.0, Codec::Hevc)), Admission::Admitted);
        assert_eq!(b.request(2, &s(1920, 1080, 30.0, Codec::Hevc)), Admission::Admitted);
        // Max-streams also evicts: maintaining the desired stream count means
        // the oldest stream yields its slot.
        match b.request(3, &s(640, 360, 30.0, Codec::Hevc)) {
            Admission::AdmittedEvicting { evicted } => assert_eq!(evicted, 1),
            other => panic!("expected eviction, got {other:?}"),
        }
        b.release(2);
        b.release(3);
        assert_eq!(b.request(4, &s(640, 360, 30.0, Codec::Hevc)), Admission::Admitted);
    }

    #[test]
    fn set_max_streams_evicts_down_to_cap() {
        let mut b = DecodeBudget::new(100.0, 8);
        for id in 1..=4 {
            b.request(id, &s(640, 360, 30.0, Codec::Hevc));
        }
        assert_eq!(b.active_count(), 4);
        // Lowering below the active count evicts the oldest-started.
        assert_eq!(b.set_max_streams(2), vec![1, 2]);
        assert_eq!(b.active_ids(), vec![3, 4]);
        assert_eq!(b.max_streams(), 2);
        // The new cap admits again — at the cap, the oldest yields its slot.
        match b.request(5, &s(640, 360, 30.0, Codec::Hevc)) {
            Admission::AdmittedEvicting { evicted } => assert_eq!(evicted, 3),
            other => panic!("expected eviction, got {other:?}"),
        }
        // Raising never evicts; new room is available immediately.
        assert!(b.set_max_streams(16).is_empty());
        assert_eq!(b.request(6, &s(640, 360, 30.0, Codec::Hevc)), Admission::Admitted);
        assert_eq!(b.active_ids(), vec![4, 5, 6]);
    }

    #[test]
    fn update_replaces_spec() {
        let mut b = DecodeBudget::new(96.0, 16);
        b.request(7, &StreamSpec::placeholder());
        let util = b.update(7, s(3840, 2160, 60.0, Codec::H264));
        assert!(util > 0.1);
        assert_eq!(b.spec(7).unwrap().width, 3840);
    }

    #[test]
    fn adaptive_shrink_and_recover() {
        let mut b = DecodeBudget::new(100.0, 16);
        b.request(1, &s(1920, 1080, 30.0, Codec::Hevc));
        b.request(2, &s(1920, 1080, 30.0, Codec::Hevc));
        assert_eq!(b.adaptive(), 1.0);

        // One hot window is not enough.
        assert!(b.report_drop_rate(2, 15.0));
        assert_eq!(b.enforce(), None);
        assert_eq!(b.adaptive(), 1.0);

        // Second consecutive window triggers shrink + victim (caller pauses
        // and releases the victim's slot).
        b.report_drop_rate(2, 15.0);
        assert_eq!(b.enforce(), Some(2));
        assert!((b.adaptive() - 0.85).abs() < 1e-9);
        b.release(2);
        assert_eq!(b.active_ids(), vec![1]);
        // Effective ceiling shrank accordingly.
        assert!((b.effective_ceiling_load() - b.ceiling_load() * 0.85).abs() < 1.0);

        // Clean windows recover.
        b.recover();
        assert!((b.adaptive() - 0.935).abs() < 1e-9);
        b.recover();
        b.recover();
        assert!((b.adaptive() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn drop_streak_resets_when_clean() {
        let mut b = DecodeBudget::new(100.0, 16);
        b.request(1, &s(1920, 1080, 30.0, Codec::Hevc));
        b.report_drop_rate(1, 20.0);
        b.report_drop_rate(1, 0.0); // clean window resets the streak
        b.report_drop_rate(1, 20.0);
        assert_eq!(b.enforce(), None);
        assert_eq!(b.adaptive(), 1.0);
    }

    #[test]
    fn adaptive_floor() {
        let mut b = DecodeBudget::new(100.0, 16);
        b.request(1, &s(640, 360, 30.0, Codec::Hevc));
        for _ in 0..40 {
            b.report_drop_rate(1, 50.0);
            b.report_drop_rate(1, 50.0);
            b.enforce();
            b.admit(1, &StreamSpec::placeholder()); // keep one active
        }
        assert!((b.adaptive() - ADAPTIVE_FLOOR).abs() < 1e-9);
    }

    #[test]
    fn codec_parsing() {
        assert_eq!(Codec::from_name("h264"), Codec::H264);
        assert_eq!(Codec::from_name("avc1"), Codec::H264);
        assert_eq!(Codec::from_name("hevc"), Codec::Hevc);
        assert_eq!(Codec::from_name("H.265"), Codec::Hevc);
        assert_eq!(Codec::from_name("av01"), Codec::Av1);
        assert_eq!(Codec::from_name("mpeg4"), Codec::Other);
    }
}
