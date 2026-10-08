//! Naming a monitor the way a person or a client would.
//!
//! winit numbers monitors in enumeration order, which says nothing about
//! where they sit. A request can instead name the connector (`DP-4`, as the
//! OS reports it) or a position (`left`, `middle`, `right`, `top`,
//! `bottom`), so no client has to keep its own number-to-place table. The
//! numbers still work.

/// What is known about one monitor: its connector name, and its rect in
/// desktop coordinates.
#[derive(Clone, Debug, PartialEq)]
pub struct MonitorInfo {
    pub name: Option<String>,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl MonitorInfo {
    fn centre(&self) -> (i64, i64) {
        (self.x as i64 + self.w as i64 / 2, self.y as i64 + self.h as i64 / 2)
    }
}

/// Resolves `spec` — a number, a connector name, or a position word — to an
/// index into `monitors`. Errors list what would have worked.
pub fn resolve_monitor(spec: &str, monitors: &[MonitorInfo]) -> Result<usize, String> {
    let s = spec.trim();
    let lower = s.to_ascii_lowercase();
    if let Ok(n) = s.parse::<usize>() {
        return if n < monitors.len() { Ok(n) } else { Err(unknown(spec, monitors)) };
    }
    if let Some(i) = monitors.iter().position(|m| m.name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(s))) {
        return Ok(i);
    }
    const WORDS: &[&str] = &["left", "leftmost", "right", "rightmost", "middle", "centre", "center", "top", "bottom"];
    if !WORDS.contains(&lower.as_str()) {
        return Err(unknown(spec, monitors));
    }
    // A position that doesn't single one out (side-by-side monitors have
    // no "top") is an error, not a guess.
    pick_position(&lower, monitors)
        .ok_or_else(|| format!("\"{spec}\" doesn't pick out one monitor here — {}", describe_all(monitors)))
}

/// The monitor a position word names, if exactly one fits it.
fn pick_position(word: &str, monitors: &[MonitorInfo]) -> Option<usize> {
    if monitors.len() < 2 {
        return None;
    }
    let by_x = order(monitors, |m| m.centre().0);
    let by_y = order(monitors, |m| m.centre().1);
    let pick = match word {
        "left" | "leftmost" => by_x.first().copied(),
        "right" | "rightmost" => by_x.last().copied(),
        // Only meaningful with an odd count: with two there is no middle.
        "middle" | "centre" | "center" => (by_x.len() % 2 == 1).then(|| by_x[by_x.len() / 2]),
        "top" => by_y.first().copied(),
        "bottom" => by_y.last().copied(),
        _ => None,
    }?;
    distinct(monitors, word, pick).then_some(pick)
}

/// The position words that name monitor `i` unambiguously, e.g. ["left"].
pub fn position_words(monitors: &[MonitorInfo], i: usize) -> Vec<&'static str> {
    ["left", "middle", "right", "top", "bottom"]
        .into_iter()
        .filter(|w| pick_position(w, monitors) == Some(i))
        .collect()
}

/// Indices sorted by a coordinate.
fn order(monitors: &[MonitorInfo], key: impl Fn(&MonitorInfo) -> i64) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..monitors.len()).collect();
    idx.sort_by_key(|&i| key(&monitors[i]));
    idx
}

/// Whether `i` is clearly on that side: wholly past every other monitor's
/// centre. Sorting alone would call the taller of two side-by-side screens
/// "top"; this makes "top" mean stacked above, and a word that fits no
/// monitor an error rather than a guess.
fn distinct(monitors: &[MonitorInfo], word: &str, i: usize) -> bool {
    let c = &monitors[i];
    let (x0, y0) = (c.x as i64, c.y as i64);
    let (x1, y1) = (x0 + c.w as i64, y0 + c.h as i64);
    let (cx, _) = c.centre();
    monitors.iter().enumerate().filter(|(j, _)| *j != i).all(|(_, m)| {
        let (ox, oy) = m.centre();
        match word {
            "left" | "leftmost" => x1 <= ox,
            "right" | "rightmost" => x0 >= ox,
            "top" => y1 <= oy,
            "bottom" => y0 >= oy,
            // Middle: picked by order already; just not level with another.
            _ => ox != cx,
        }
    })
}

fn unknown(spec: &str, monitors: &[MonitorInfo]) -> String {
    format!("there is no monitor \"{spec}\" — {}", describe_all(monitors))
}

/// "monitors: 0 DP-2 (middle), 1 DP-0 (right), 2 DP-4 (left)".
pub fn describe_all(monitors: &[MonitorInfo]) -> String {
    let parts: Vec<String> = monitors
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let mut s = i.to_string();
            if let Some(n) = &m.name {
                s.push_str(&format!(" {n}"));
            }
            let words = position_words(monitors, i);
            if !words.is_empty() {
                s.push_str(&format!(" ({})", words.join(", ")));
            }
            s
        })
        .collect();
    format!("monitors: {}", parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(name: &str, x: i32, y: i32, w: u32, h: u32) -> MonitorInfo {
        MonitorInfo { name: Some(name.into()), x, y, w, h }
    }

    /// The machine this was written on: a portrait screen on the left, a
    /// 5K2K in the middle, an ultrawide on the right — enumerated in an
    /// order that matches none of that.
    fn desk() -> Vec<MonitorInfo> {
        vec![
            m("DP-2", 2160, 1027, 5120, 2160),
            m("DP-0", 7280, 1435, 3440, 1440),
            m("DP-4", 0, 0, 2160, 3840),
        ]
    }

    #[test]
    fn numbers_names_and_positions() {
        let d = desk();
        assert_eq!(resolve_monitor("1", &d), Ok(1));
        assert_eq!(resolve_monitor("DP-4", &d), Ok(2));
        assert_eq!(resolve_monitor("dp-0", &d), Ok(1));
        assert_eq!(resolve_monitor("left", &d), Ok(2));
        assert_eq!(resolve_monitor("Middle", &d), Ok(0));
        assert_eq!(resolve_monitor("center", &d), Ok(0));
        assert_eq!(resolve_monitor("right", &d), Ok(1));
        // Side by side, nothing is above anything: no top or bottom.
        assert!(resolve_monitor("top", &d).is_err());
        assert!(resolve_monitor("bottom", &d).is_err());
        // Stacked: a TV above a desk monitor.
        let stacked = vec![m("HDMI-0", 0, 0, 3840, 2160), m("DP-1", 960, 2160, 1920, 1080)];
        assert_eq!(resolve_monitor("top", &stacked), Ok(0));
        assert_eq!(resolve_monitor("bottom", &stacked), Ok(1));
    }

    #[test]
    fn what_doesnt_resolve_says_what_would() {
        let d = desk();
        let e = resolve_monitor("7", &d).unwrap_err();
        assert!(e.contains("0 DP-2 (middle), 1 DP-0 (right), 2 DP-4 (left)"), "{e}");
        assert!(resolve_monitor("HDMI-1", &d).is_err());
        assert!(resolve_monitor("the tv", &d).is_err());
        // Two side by side: no middle, and no top when level.
        let two = vec![m("A", 0, 0, 1920, 1080), m("B", 1920, 0, 1920, 1080)];
        assert_eq!(resolve_monitor("right", &two), Ok(1));
        assert!(resolve_monitor("middle", &two).is_err());
        assert!(resolve_monitor("top", &two).is_err());
        assert_eq!(position_words(&two, 0), ["left"]);
        // One monitor: positions add nothing.
        assert!(position_words(&two[..1], 0).is_empty());
    }
}
