//! Which part of the picture a tile shows: a crop, then a zoom into it.
//!
//! The use that drove this is the letterbox. A 2.39:1 film mastered into a
//! 16:9 file carries black bars in the picture itself; fitted into a 21:9
//! monitor it gets pillarboxed as well, and shows black on all four sides.
//! Cropping the bars out makes the *picture's* aspect 2.39:1, which the
//! compositor then fits — so the film fills the screen. Zoom is the
//! blunter tool: a magnification of whatever the crop left, centred unless
//! panned (`pan_x`/`pan_y` place the zoomed window anywhere inside the crop,
//! which is how a client walks the picture, e.g. a voice assistant's Esper mode).
//!
//! All fractions are of the picture, measured from each edge, with "top"
//! meaning the top of the image as seen (the compositor deals with the
//! texture being stored upside down).

/// Crop fractions per edge, a zoom factor (1 = none), and where the zoomed
/// window sits inside the crop (`pan_x`/`pan_y`: its centre as fractions of
/// the crop, 0.5 = centred; kept inside the crop however far it's asked to go).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    pub left: f32,
    pub right: f32,
    pub top: f32,
    pub bottom: f32,
    pub zoom: f32,
    pub pan_x: f32,
    pub pan_y: f32,
}

/// Never crop more than this from one edge, and never zoom past this: a
/// mis-detected crop or a runaway zoom should still leave a picture.
const MAX_EDGE: f32 = 0.45;
pub const MAX_ZOOM: f32 = 16.0;

impl Default for View {
    fn default() -> View {
        View::FULL
    }
}

impl View {
    pub const FULL: View = View { left: 0.0, right: 0.0, top: 0.0, bottom: 0.0, zoom: 1.0, pan_x: 0.5, pan_y: 0.5 };

    pub fn is_full(&self) -> bool {
        *self == View::FULL
    }

    pub fn is_cropped(&self) -> bool {
        self.left > 0.0 || self.right > 0.0 || self.top > 0.0 || self.bottom > 0.0
    }

    /// This view with the crop replaced and the zoom kept.
    pub fn with_crop(self, c: Crop) -> View {
        View { left: c.left, right: c.right, top: c.top, bottom: c.bottom, ..self }
    }

    /// The region shown, as `(x0, y0, x1, y1)` fractions of the picture
    /// with y running down: the crop, then shrunk by the zoom about the pan
    /// point (the crop's centre unless panned), kept inside the crop.
    pub fn source_rect(&self) -> (f32, f32, f32, f32) {
        let (x0, x1) = (self.left, 1.0 - self.right);
        let (y0, y1) = (self.top, 1.0 - self.bottom);
        let z = self.zoom.clamp(1.0, MAX_ZOOM);
        let (hw, hh) = ((x1 - x0) / (2.0 * z), (y1 - y0) / (2.0 * z));
        let pan = |p: f32| if p.is_finite() { p.clamp(0.0, 1.0) } else { 0.5 };
        // Not f32::clamp, which panics when min > max. At zoom 1 the two
        // bounds are the same number computed two ways, and for an uneven
        // crop rounding can put them an ulp the wrong way round: Fellowship
        // of the Ring's auto-crop (top 13.09%, bottom 13.27%) took the whole
        // app down a few seconds after it was applied.
        let keep = |c: f32, lo: f32, hi: f32| if lo < hi { c.clamp(lo, hi) } else { (lo + hi) / 2.0 };
        let cx = keep(x0 + pan(self.pan_x) * (x1 - x0), x0 + hw, x1 - hw);
        let cy = keep(y0 + pan(self.pan_y) * (y1 - y0), y0 + hh, y1 - hh);
        (cx - hw, cy - hh, cx + hw, cy + hh)
    }

    /// Part way from `a` to `b` (t in 0..=1): crop and pan linearly, zoom
    /// geometrically, so a 1x→16x move feels even rather than rushing at
    /// the end.
    pub fn between(a: &View, b: &View, t: f32) -> View {
        let t = t.clamp(0.0, 1.0);
        let l = |x: f32, y: f32| x + (y - x) * t;
        let za = a.zoom.clamp(1.0, MAX_ZOOM);
        let zb = b.zoom.clamp(1.0, MAX_ZOOM);
        View {
            left: l(a.left, b.left),
            right: l(a.right, b.right),
            top: l(a.top, b.top),
            bottom: l(a.bottom, b.bottom),
            zoom: za * (zb / za).powf(t),
            pan_x: l(a.pan_x, b.pan_x),
            pan_y: l(a.pan_y, b.pan_y),
        }
    }

    /// The aspect of what is shown, given the whole picture's. Zoom keeps
    /// it; a crop changes it — which is the point of cropping a letterbox.
    pub fn aspect(&self, picture_aspect: f32) -> f32 {
        let (x0, y0, x1, y1) = self.source_rect();
        picture_aspect * (x1 - x0) / (y1 - y0)
    }

    /// "top 12%, bottom 12%", "zoom 1.5x", or "full picture".
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        for (name, v) in [("top", self.top), ("bottom", self.bottom), ("left", self.left), ("right", self.right)] {
            if v > 0.0 {
                parts.push(format!("{name} {:.0}%", v * 100.0));
            }
        }
        let mut s = if parts.is_empty() { String::new() } else { format!("cropped {}", parts.join(", ")) };
        if self.zoom > 1.0 {
            if !s.is_empty() {
                s.push_str(", ");
            }
            s.push_str(&format!("zoom {:.2}x", self.zoom));
            if (self.pan_x - 0.5).abs() > 0.005 || (self.pan_y - 0.5).abs() > 0.005 {
                s.push_str(&format!(" at {:.0}%, {:.0}%", self.pan_x * 100.0, self.pan_y * 100.0));
            }
        }
        if s.is_empty() {
            "full picture".into()
        } else {
            s
        }
    }
}

/// Fractions cut from each edge.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Crop {
    pub left: f32,
    pub right: f32,
    pub top: f32,
    pub bottom: f32,
}

impl Crop {
    pub const NONE: Crop = Crop { left: 0.0, right: 0.0, top: 0.0, bottom: 0.0 };

    fn clamped(self) -> Crop {
        let c = |v: f32| if v.is_finite() { v.clamp(0.0, MAX_EDGE) } else { 0.0 };
        Crop { left: c(self.left), right: c(self.right), top: c(self.top), bottom: c(self.bottom) }
    }

    /// The centred crop that gives a picture of `picture_aspect` the
    /// `target` aspect: top and bottom for a wider target, sides for a
    /// narrower one.
    pub fn to_aspect(picture_aspect: f32, target: f32) -> Crop {
        if !(picture_aspect > 0.0 && target > 0.0) {
            return Crop::NONE;
        }
        if target > picture_aspect {
            let cut = (1.0 - picture_aspect / target) / 2.0;
            Crop { top: cut, bottom: cut, ..Crop::NONE }.clamped()
        } else {
            let cut = (1.0 - target / picture_aspect) / 2.0;
            Crop { left: cut, right: cut, ..Crop::NONE }.clamped()
        }
    }

    /// From a detected content box (`x, y, w, h` inside a `frame_w` ×
    /// `frame_h` frame), as ffmpeg's cropdetect reports it.
    pub fn from_box(frame_w: u32, frame_h: u32, x: u32, y: u32, w: u32, h: u32) -> Crop {
        if frame_w == 0 || frame_h == 0 {
            return Crop::NONE;
        }
        let (fw, fh) = (frame_w as f32, frame_h as f32);
        Crop {
            left: x as f32 / fw,
            right: (fw - (x + w) as f32).max(0.0) / fw,
            top: y as f32 / fh,
            bottom: (fh - (y + h) as f32).max(0.0) / fh,
        }
        .clamped()
    }

    /// Whether this is a crop worth applying: anything under 1% an edge is
    /// encoder noise along the border, not a letterbox.
    pub fn is_significant(&self) -> bool {
        [self.left, self.right, self.top, self.bottom].iter().any(|v| *v >= 0.01)
    }
}

/// Reads a crop request. `cell_aspect` is the aspect of the tile's cell,
/// for `fill`. Accepts:
/// - `off` / `none` / `reset` — no crop
/// - an aspect ratio: `2.39:1`, `2.39`, `21:9`, `16:9` — centred crop to it
/// - `fill` — crop to the cell's aspect, so the picture fills the tile
/// - `top,bottom[,left,right]` — per edge, as fractions (0.12) or percents (12)
///
/// (`auto` is not here: detection needs the video, so the MCP shim does it
/// and sends the result as explicit edges.)
pub fn parse_crop(value: &str, picture_aspect: f32, cell_aspect: f32) -> Result<Crop, String> {
    let v = value.trim().to_ascii_lowercase();
    match v.as_str() {
        "off" | "none" | "reset" | "0" => return Ok(Crop::NONE),
        "fill" => return Ok(Crop::to_aspect(picture_aspect, cell_aspect)),
        _ => {}
    }
    if v.contains(',') {
        let nums: Vec<f32> = v
            .split(',')
            .map(|p| p.trim().trim_end_matches('%').parse::<f32>())
            .collect::<Result<_, _>>()
            .map_err(|_| format!("can't read \"{value}\" as edges — use top,bottom or top,bottom,left,right"))?;
        let frac = |n: f32| if n > 1.0 { n / 100.0 } else { n };
        let get = |i: usize| nums.get(i).copied().map(frac).unwrap_or(0.0);
        if nums.len() != 2 && nums.len() != 4 {
            return Err("give 2 or 4 edges: top,bottom or top,bottom,left,right".into());
        }
        return Ok(Crop { top: get(0), bottom: get(1), left: get(2), right: get(3) }.clamped());
    }
    if let Some(aspect) = parse_aspect(&v) {
        return Ok(Crop::to_aspect(picture_aspect, aspect));
    }
    Err(format!(
        "can't read \"{value}\" as a crop — use auto, off, fill, an aspect (2.39:1, 21:9) or edges (12%,12%)"
    ))
}

/// `2.39:1`, `21:9`, `2.39` (a plausible screen aspect, 1–4).
fn parse_aspect(v: &str) -> Option<f32> {
    let a = match v.split_once(':') {
        Some((w, h)) => w.trim().parse::<f32>().ok()? / h.trim().parse::<f32>().ok()?,
        None => v.trim().parse::<f32>().ok()?,
    };
    (a.is_finite() && (1.0..=4.0).contains(&a)).then_some(a)
}

/// Reads a zoom request against the current factor: `1.5`, `150%`, `+0.25`,
/// `-0.25`, `in` / `out` (±0.25), `off` / `reset` (1).
pub fn parse_zoom(value: &str, current: f32) -> Result<f32, String> {
    let v = value.trim().to_ascii_lowercase();
    let z = match v.as_str() {
        "off" | "reset" | "none" => 1.0,
        "in" => current + 0.25,
        "out" => current - 0.25,
        _ => {
            let bad = || format!("can't read \"{value}\" as a zoom — use 1.5, 150%, +0.25, -0.25, in, out or off");
            let (rel, body) = match v.chars().next() {
                Some('+') => (Some(1.0), &v[1..]),
                Some('-') => (Some(-1.0), &v[1..]),
                _ => (None, v.as_str()),
            };
            let pct = body.ends_with('%');
            let mut n: f32 = body.trim_end_matches(['%', 'x']).trim().parse().map_err(|_| bad())?;
            if pct {
                n /= 100.0;
            }
            if !n.is_finite() {
                return Err(bad());
            }
            match rel {
                Some(k) => current + k * n,
                None => n,
            }
        }
    };
    Ok(z.clamp(1.0, MAX_ZOOM))
}

/// The last `crop=W:H:X:Y` in ffmpeg cropdetect output, as (w, h, x, y).
pub fn parse_cropdetect(log: &str) -> Option<(u32, u32, u32, u32)> {
    let tail = &log[log.rfind("crop=")? + 5..];
    let mut it = tail.split(|c: char| c == ':' || c.is_whitespace()).map(|s| s.parse::<u32>().ok());
    Some((it.next()??, it.next()??, it.next()??, it.next()??))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn letterbox_crop_restores_the_film_aspect() {
        // 2.39:1 inside a 16:9 frame: 12.8% bars top and bottom.
        let c = Crop::to_aspect(16.0 / 9.0, 2.39);
        assert!(close(c.top, 0.1281) && close(c.bottom, 0.1281), "{c:?}");
        assert_eq!((c.left, c.right), (0.0, 0.0));
        let v = View::FULL.with_crop(c);
        assert!(close(v.aspect(16.0 / 9.0), 2.39));
        // A narrower target crops the sides instead.
        let c = Crop::to_aspect(16.0 / 9.0, 4.0 / 3.0);
        assert!(close(c.left, 0.125) && c.top == 0.0, "{c:?}");
    }

    #[test]
    fn zoom_keeps_aspect_and_centre() {
        let v = View { zoom: 2.0, ..View::FULL };
        assert_eq!(v.source_rect(), (0.25, 0.25, 0.75, 0.75));
        assert!(close(v.aspect(1.5), 1.5));
        // Zoom applies inside the crop.
        let v = View { top: 0.1, bottom: 0.1, zoom: 2.0, ..View::FULL };
        let (x0, y0, x1, y1) = v.source_rect();
        assert!(close(x0, 0.25) && close(x1, 0.75) && close(y0, 0.3) && close(y1, 0.7));
    }

    #[test]
    fn pan_reaches_the_corners_and_stays_inside_the_crop() {
        // 4x into the top-left corner: the window is pushed against the edges.
        let v = View { zoom: 4.0, pan_x: 0.0, pan_y: 0.0, ..View::FULL };
        let (x0, y0, x1, y1) = v.source_rect();
        assert!(close(x0, 0.0) && close(y0, 0.0) && close(x1, 0.25) && close(y1, 0.25));
        // Panned inside a letterbox crop it never shows the bars.
        let v = View { top: 0.13, bottom: 0.13, zoom: 8.0, pan_x: 0.9, pan_y: 1.0, ..View::FULL };
        let (_, _, x1, y1) = v.source_rect();
        assert!(close(y1, 0.87) && x1 <= 1.0);
        // Centred pan is the old behaviour.
        let v = View { zoom: 2.0, pan_x: 0.5, pan_y: 0.5, ..View::FULL };
        assert_eq!(v.source_rect(), (0.25, 0.25, 0.75, 0.75));
        // Garbage pan falls back to the centre.
        let v = View { zoom: 2.0, pan_x: f32::NAN, ..View::FULL };
        assert!(close(v.source_rect().0, 0.25));
    }

    #[test]
    fn an_uneven_crop_never_panics() {
        // At zoom 1 the window's bounds meet, and rounding used to cross
        // them for some uneven crops — f32::clamp's panic, in the render
        // loop. Sweep uneven crops, and the animation into each.
        for i in 0..600 {
            let top = 0.10 + i as f32 * 0.0001;
            let bottom = 0.1327;
            let v = View { top, bottom, ..View::FULL };
            for t in [0.0, 0.25, 0.5, 0.75, 1.0] {
                let (_, y0, _, y1) = View::between(&View::FULL, &v, t).source_rect();
                assert!(y0 < y1 && y0 >= 0.0 && y1 <= 1.0, "top {top}, t {t}: {y0}..{y1}");
            }
            assert!(close(v.source_rect().1, top) && close(v.source_rect().3, 1.0 - bottom));
        }
    }

    #[test]
    fn between_eases_zoom_geometrically() {
        let a = View::FULL;
        let b = View { zoom: 16.0, pan_x: 0.8, ..View::FULL };
        let mid = View::between(&a, &b, 0.5);
        assert!(close(mid.zoom, 4.0), "{mid:?}"); // sqrt(16)
        assert!(close(mid.pan_x, 0.65));
        assert_eq!(View::between(&a, &b, 1.0), b);
        assert_eq!(View::between(&a, &b, 0.0).zoom, 1.0);
        assert!(View { zoom: 3.0, pan_x: 0.2, ..View::FULL }.describe().contains("at 20%"));
    }

    #[test]
    fn crop_requests() {
        let p = 16.0 / 9.0;
        assert_eq!(parse_crop("off", p, 2.0), Ok(Crop::NONE));
        assert!(close(parse_crop("2.39:1", p, 2.0).unwrap().top, 0.1281));
        assert!(close(parse_crop("2.39", p, 2.0).unwrap().top, 0.1281));
        assert!(close(parse_crop("21:9", p, 2.0).unwrap().top, (1.0 - p / (21.0 / 9.0)) / 2.0));
        // fill: a 16:9 picture in a 21:9 cell loses top and bottom.
        let fill = parse_crop("fill", p, 3440.0 / 1440.0).unwrap();
        assert!(fill.top > 0.1 && fill.left == 0.0);
        // Edges as percents or fractions; 2 or 4 of them.
        assert_eq!(parse_crop("12%,12%", p, 2.0).unwrap(), Crop { top: 0.12, bottom: 0.12, ..Crop::NONE });
        assert_eq!(parse_crop("0.1,0.2,0.05,0.05", p, 2.0).unwrap(), Crop { top: 0.1, bottom: 0.2, left: 0.05, right: 0.05 });
        // Clamped: no edge past 45%.
        assert_eq!(parse_crop("90,0", p, 2.0).unwrap().top, 0.45);
        for bad in ["sideways", "1,2,3", "0.5", "9:1", "a,b"] {
            assert!(parse_crop(bad, p, 2.0).is_err(), "{bad}");
        }
    }

    #[test]
    fn zoom_requests() {
        assert_eq!(parse_zoom("1.5", 1.0), Ok(1.5));
        assert_eq!(parse_zoom("150%", 1.0), Ok(1.5));
        assert_eq!(parse_zoom("2x", 1.0), Ok(2.0));
        assert_eq!(parse_zoom("+0.25", 1.5), Ok(1.75));
        assert_eq!(parse_zoom("in", 1.0), Ok(1.25));
        assert_eq!(parse_zoom("out", 1.0), Ok(1.0)); // never below 1
        assert_eq!(parse_zoom("off", 3.0), Ok(1.0));
        assert_eq!(parse_zoom("99", 1.0), Ok(MAX_ZOOM));
        assert!(parse_zoom("lots", 1.0).is_err());
    }

    #[test]
    fn cropdetect_output() {
        let log = "[Parsed_cropdetect_0 @ 0x5] x1:0 x2:1919 y1:138 y2:941 w:1920 h:800 x:0 y:140 pts:1 t:0.04 crop=1920:800:0:140\n\
                   [Parsed_cropdetect_0 @ 0x5] x1:0 x2:1919 y1:138 y2:941 w:1920 h:800 x:0 y:140 pts:2 t:0.08 crop=1920:800:0:140\n";
        assert_eq!(parse_cropdetect(log), Some((1920, 800, 0, 140)));
        assert_eq!(parse_cropdetect("nothing here"), None);
        let c = Crop::from_box(1920, 1080, 0, 140, 1920, 800);
        assert!(close(c.top, 140.0 / 1080.0) && close(c.bottom, 140.0 / 1080.0) && c.left == 0.0);
        assert!(c.is_significant());
        assert!(!Crop::from_box(1920, 1080, 2, 2, 1916, 1076).is_significant());
    }

    #[test]
    fn describes_itself() {
        assert_eq!(View::FULL.describe(), "full picture");
        let v = View { top: 0.128, bottom: 0.128, zoom: 1.0, ..View::FULL };
        assert_eq!(v.describe(), "cropped top 13%, bottom 13%");
        assert_eq!(View { zoom: 1.5, ..View::FULL }.describe(), "zoom 1.50x");
    }
}
