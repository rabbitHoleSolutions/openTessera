//! Positions as people say them, and where a seek request lands.
//!
//! An assistant asks for "+30", "01:02:03", "-1:00" or "the chapter called
//! Act Two"; every timestamp handed back is `HH:MM:SS`, so what it reads out
//! can be fed straight back in.

/// `HH:MM:SS`, the one notation used for every timestamp handed out.
pub fn fmt_hms(secs: f64) -> String {
    let t = secs.max(0.0).floor() as u64;
    format!("{:02}:{:02}:{:02}", t / 3600, (t / 60) % 60, t % 60)
}

/// A position someone asked for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TimeSpec {
    Absolute(f64),
    /// From the current position; negative is back.
    Relative(f64),
}

/// Reads `90`, `90s`, `5m`, `1h`, `1:30`, `01:02:03`, and any of those with
/// a leading `+`/`-` for relative. Chapters are [`resolve_seek`]'s business.
pub fn parse_time(s: &str) -> Option<TimeSpec> {
    let s = s.trim();
    let (sign, body) = match s.chars().next()? {
        '+' => (Some(1.0), &s[1..]),
        '-' => (Some(-1.0), &s[1..]),
        _ => (None, s),
    };
    let body = body.trim();
    let secs = if body.contains(':') {
        let mut total = 0.0;
        for part in body.split(':') {
            total = total * 60.0 + part.parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0)?;
        }
        total
    } else {
        let (num, mult) = match body.chars().last()? {
            's' => (&body[..body.len() - 1], 1.0),
            'm' => (&body[..body.len() - 1], 60.0),
            'h' => (&body[..body.len() - 1], 3600.0),
            _ => (body, 1.0),
        };
        num.trim().parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0)? * mult
    };
    Some(match sign {
        Some(k) => TimeSpec::Relative(k * secs),
        None => TimeSpec::Absolute(secs),
    })
}

/// Where a seek request lands, in seconds. `value` is a time (see
/// [`parse_time`]), `chapter N`, `next chapter` / `previous chapter`, or
/// part of a chapter's title. The error says what would have worked.
pub fn resolve_seek(
    value: &str,
    position: f64,
    duration: Option<f64>,
    chapters: &[(f64, String)],
) -> Result<f64, String> {
    let target = match parse_time(value) {
        Some(TimeSpec::Absolute(t)) => t,
        Some(TimeSpec::Relative(d)) => position + d,
        None => chapter_target(value, position, chapters).ok_or_else(|| {
            let names = if chapters.is_empty() {
                "this video has no chapters".to_string()
            } else {
                format!(
                    "chapters: {}",
                    chapters.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>().join(", ")
                )
            };
            format!(
                "can't read \"{value}\" as a position — use seconds (90), HH:MM:SS (01:02:03), \
                 relative (+30, -10), or a chapter ({names})"
            )
        })?,
    };
    // Past the end would just end the file; stop a moment short instead.
    let max = duration.map_or(f64::MAX, |d| (d - 1.0).max(0.0));
    Ok(target.clamp(0.0, max))
}

fn chapter_target(value: &str, position: f64, chapters: &[(f64, String)]) -> Option<f64> {
    let v = value.trim().to_lowercase();
    // The chapter we're in: the last one starting at or before here. A
    // small slack, or "previous" right after a chapter seek would land on
    // the same chapter again.
    let current = chapters.iter().rposition(|(t, _)| *t <= position + 0.5);
    match v.as_str() {
        "next chapter" | "next" => {
            let i = current.map_or(0, |c| c + 1);
            return chapters.get(i).map(|c| c.0);
        }
        "previous chapter" | "prev chapter" | "previous" | "prev" => {
            // Like a remote: back to the start of this chapter unless
            // already near it, then the one before.
            let c = current?;
            let i = if position - chapters[c].0 > 3.0 { c } else { c.saturating_sub(1) };
            return chapters.get(i).map(|c| c.0);
        }
        _ => {}
    }
    if let Some(n) = v.strip_prefix("chapter").and_then(|n| n.trim().parse::<usize>().ok()) {
        return chapters.get(n.checked_sub(1)?).map(|c| c.0);
    }
    chapters.iter().find(|(_, t)| t.to_lowercase().contains(&v)).map(|c| c.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_notation() {
        use TimeSpec::*;
        assert_eq!(parse_time("90"), Some(Absolute(90.0)));
        assert_eq!(parse_time("1:30"), Some(Absolute(90.0)));
        assert_eq!(parse_time("01:02:03"), Some(Absolute(3723.0)));
        assert_eq!(parse_time("+30"), Some(Relative(30.0)));
        assert_eq!(parse_time("-10"), Some(Relative(-10.0)));
        assert_eq!(parse_time("-1:00"), Some(Relative(-60.0)));
        assert_eq!(parse_time("5m"), Some(Absolute(300.0)));
        assert_eq!(parse_time("+90s"), Some(Relative(90.0)));
        assert_eq!(parse_time("1h"), Some(Absolute(3600.0)));
        for bad in ["", "+", "abc", "1:xx", "--5", "NaN", "inf", "1:inf"] {
            assert_eq!(parse_time(bad), None, "{bad:?}");
        }
        assert_eq!(fmt_hms(4323.9), "01:12:03");
        assert_eq!(fmt_hms(-5.0), "00:00:00");
    }

    fn chapters() -> Vec<(f64, String)> {
        vec![(0.0, "Opening".into()), (300.0, "Act Two".into()), (900.0, "Credits".into())]
    }

    #[test]
    fn seeks_resolve_times_and_chapters() {
        let ch = chapters();
        let at = |v: &str, pos: f64| resolve_seek(v, pos, Some(1000.0), &ch);
        assert_eq!(at("+30", 100.0), Ok(130.0));
        assert_eq!(at("-500", 100.0), Ok(0.0)); // clamped at the start
        assert_eq!(at("02:00:00", 0.0), Ok(999.0)); // and short of the end
        assert_eq!(at("act two", 0.0), Ok(300.0));
        assert_eq!(at("CREDITS", 0.0), Ok(900.0));
        assert_eq!(at("chapter 2", 0.0), Ok(300.0));
        assert_eq!(at("next chapter", 310.0), Ok(900.0));
        assert_eq!(at("next chapter", 299.8), Ok(900.0)); // just after a chapter seek
        assert_eq!(at("previous chapter", 400.0), Ok(300.0)); // restart this one
        assert_eq!(at("previous chapter", 301.0), Ok(0.0)); // near its start: go back one
        assert!(at("chapter 9", 0.0).is_err());
        assert!(at("next chapter", 950.0).is_err());
        let err = at("the bit with the boat", 0.0).unwrap_err();
        assert!(err.contains("Act Two"), "{err}");
        let err = resolve_seek("intro", 0.0, None, &[]).unwrap_err();
        assert!(err.contains("no chapters"), "{err}");
    }
}
