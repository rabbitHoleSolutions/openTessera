/// Decode admission controller, ported from the Quest app's DecodeBudgetController.
///
/// Desktop gates for phase 1: weighted macroblocks/sec throughput ceiling and a
/// max concurrent streams setting. Eviction, per-codec session caps, and the
/// frame-drop adaptive feedback loop arrive with the full player integration.
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
pub const MBPS_1080P30: f64 = 244_800.0;

/// Default ceiling: 96 x 1080p30-equivalents (24 x 4K30-equivalents), measured
/// against NVDEC on an RTX 5080 with enormous headroom remaining.
pub const DEFAULT_CEILING_EQUIVALENTS: f64 = 96.0;

pub const DEFAULT_MAX_STREAMS: usize = 16;

#[derive(Clone, Debug, PartialEq)]
pub enum Admission {
    Admitted,
    RejectedBudget { would_use: f64, ceiling: f64 },
    RejectedMaxStreams { max: usize },
}

pub struct DecodeBudget {
    ceiling_mbps: f64,
    max_streams: usize,
    active: Vec<(usize, StreamSpec)>,
}

impl DecodeBudget {
    pub fn new(ceiling_equivalents: f64, max_streams: usize) -> DecodeBudget {
        DecodeBudget {
            ceiling_mbps: ceiling_equivalents * MBPS_1080P30,
            max_streams,
            active: Vec::new(),
        }
    }

    pub fn ceiling_mbps(&self) -> f64 {
        self.ceiling_mbps
    }

    pub fn max_streams(&self) -> usize {
        self.max_streams
    }

    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    pub fn used_mbps(&self) -> f64 {
        self.active.iter().map(|(_, s)| s.weight()).sum()
    }

    pub fn utilization(&self) -> f64 {
        self.used_mbps() / self.ceiling_mbps
    }

    pub fn request(&mut self, stream_id: usize, spec: &StreamSpec) -> Admission {
        if self.active.iter().any(|(id, _)| *id == stream_id) {
            return Admission::Admitted;
        }
        if self.active.len() >= self.max_streams {
            return Admission::RejectedMaxStreams { max: self.max_streams };
        }
        let would_use = self.used_mbps() + spec.weight();
        if would_use > self.ceiling_mbps {
            return Admission::RejectedBudget { would_use, ceiling: self.ceiling_mbps };
        }
        self.active.push((stream_id, spec.clone()));
        Admission::Admitted
    }

    /// Replaces the spec for a known stream (e.g. real probe data replacing the
    /// admission placeholder). Reports the new utilization for logging.
    pub fn update(&mut self, stream_id: usize, spec: StreamSpec) -> f64 {
        if let Some(entry) = self.active.iter_mut().find(|(id, _)| *id == stream_id) {
            entry.1 = spec;
        }
        self.utilization()
    }

    pub fn release(&mut self, stream_id: usize) {
        self.active.retain(|(id, _)| *id != stream_id);
    }

    pub fn spec(&self, stream_id: usize) -> Option<&StreamSpec> {
        self.active.iter().find(|(id, _)| *id == stream_id).map(|(_, s)| s)
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
        assert!((hd.weight() - MBPS_1080P30).abs() < 1.0);
        let uhd_h264 = s(3840, 2160, 30.0, Codec::H264);
        assert!(uhd_h264.weight() > 4.0 * MBPS_1080P30);
    }

    #[test]
    fn admit_until_ceiling() {
        let mut b = DecodeBudget::new(4.0, 100);
        for i in 0..4 {
            assert_eq!(b.request(i, &s(1920, 1080, 30.0, Codec::Hevc)), Admission::Admitted);
        }
        match b.request(99, &s(1920, 1080, 30.0, Codec::Hevc)) {
            Admission::RejectedBudget { would_use, ceiling } => {
                assert!(would_use > ceiling);
            }
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    #[test]
    fn max_streams_gate() {
        let mut b = DecodeBudget::new(100.0, 2);
        assert_eq!(b.request(1, &s(1920, 1080, 30.0, Codec::Hevc)), Admission::Admitted);
        assert_eq!(b.request(2, &s(1920, 1080, 30.0, Codec::Hevc)), Admission::Admitted);
        assert_eq!(b.request(3, &s(640, 360, 30.0, Codec::Hevc)), Admission::RejectedMaxStreams { max: 2 });
        b.release(1);
        assert_eq!(b.request(3, &s(640, 360, 30.0, Codec::Hevc)), Admission::Admitted);
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
    fn codec_parsing() {
        assert_eq!(Codec::from_name("h264"), Codec::H264);
        assert_eq!(Codec::from_name("avc1"), Codec::H264);
        assert_eq!(Codec::from_name("hevc"), Codec::Hevc);
        assert_eq!(Codec::from_name("H.265"), Codec::Hevc);
        assert_eq!(Codec::from_name("av01"), Codec::Av1);
        assert_eq!(Codec::from_name("mpeg4"), Codec::Other);
    }
}
