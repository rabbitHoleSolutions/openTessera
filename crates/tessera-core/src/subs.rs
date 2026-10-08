//! Subtitle cues as text: parsing, "where am I", search, and the time
//! notation an assistant speaks in.
//!
//! Everything is normalised to SRT before it gets here (ffmpeg converts any
//! text subtitle codec to it), so SRT is the only format parsed. The output
//! side is deliberately compact — `[01:12:03] line` — because whoever asks
//! re-reads every token and may speak the answer.

pub use crate::timecode::fmt_hms;

/// One subtitle cue. Times in seconds.
#[derive(Clone, Debug, PartialEq)]
pub struct Cue {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

/// Parses SRT. Tolerant: malformed blocks are skipped rather than failing
/// the whole track, and markup (`<i>`, `{\an8}`) is stripped.
pub fn parse_srt(srt: &str) -> Vec<Cue> {
    let srt = srt.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    let mut cues = Vec::new();
    for block in srt.split("\n\n") {
        let mut lines = block.lines().filter(|l| !l.trim().is_empty());
        let Some(mut line) = lines.next() else { continue };
        // The index line is optional in the wild.
        if !line.contains("-->") {
            match lines.next() {
                Some(l) => line = l,
                None => continue,
            }
        }
        let Some((a, b)) = line.split_once("-->") else { continue };
        let (Some(start), Some(end)) = (srt_time(a), srt_time(b)) else { continue };
        let text = lines.map(strip_markup).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ");
        if !text.is_empty() {
            cues.push(Cue { start, end, text });
        }
    }
    cues.sort_by(|a, b| a.start.total_cmp(&b.start));
    cues
}

/// `00:01:02,345` (or with a `.`), possibly followed by position junk.
fn srt_time(s: &str) -> Option<f64> {
    let s = s.split_whitespace().next()?.replace(',', ".");
    let mut parts = s.split(':');
    let h: f64 = parts.next()?.parse().ok()?;
    let m: f64 = parts.next()?.parse().ok()?;
    let sec: f64 = parts.next()?.parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + sec)
}

fn strip_markup(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut depth_angle = false;
    let mut depth_brace = false;
    for c in line.chars() {
        match c {
            '<' => depth_angle = true,
            '>' if depth_angle => depth_angle = false,
            '{' => depth_brace = true,
            '}' if depth_brace => depth_brace = false,
            _ if depth_angle || depth_brace => {}
            _ => out.push(c),
        }
    }
    out.trim().to_string()
}

/// Cues overlapping `[pos - back, pos + ahead]`.
pub fn around(cues: &[Cue], pos: f64, back: f64, ahead: f64) -> &[Cue] {
    let lo = cues.partition_point(|c| c.end < pos - back);
    let hi = cues.partition_point(|c| c.start <= pos + ahead);
    &cues[lo..hi.max(lo)]
}

/// Case- and punctuation-insensitive phrase search. A phrase split across
/// two cues still matches (reported at the first), since a line break in a
/// subtitle says nothing about where a sentence ends.
pub fn search<'a>(cues: &'a [Cue], query: &str, limit: usize) -> Vec<&'a Cue> {
    let q = normalize(query);
    if q.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (i, c) in cues.iter().enumerate() {
        let here = normalize(&c.text);
        let hit = here.contains(&q)
            || cues
                .get(i + 1)
                .is_some_and(|n| !normalize(&n.text).contains(&q) && format!("{here} {}", normalize(&n.text)).contains(&q));
        if hit {
            out.push(c);
            if out.len() >= limit {
                break;
            }
        }
    }
    out
}

fn normalize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() { c.to_lowercase().next().unwrap_or(c) } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// `[HH:MM:SS] text`, one cue per line.
pub fn format_cues<'a>(cues: impl IntoIterator<Item = &'a Cue>) -> String {
    cues.into_iter()
        .map(|c| format!("[{}] {}", fmt_hms(c.start), c.text))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRT: &str = "\u{feff}1\r\n00:00:33,075 --> 00:00:34,160\r\nEt merde !\r\n\r\n\
2\n00:00:42,835 --> 00:00:44,003\n<i>Réveille-toi !</i>\n\n\
3\n00:01:09,570 --> 00:01:11,280\n- Salut, papa.\n- Merde ! Salut.\n\n\
garbage block\n\n\
00:02:00.000 --> 00:02:02.000 X1:10\n{\\an8}Rosebud is\n\n\
5\n00:02:03,000 --> 00:02:05,000\nmy sled.\n";

    #[test]
    fn parses_srt_tolerantly() {
        let cues = parse_srt(SRT);
        assert_eq!(cues.len(), 5);
        assert_eq!(cues[0].start, 33.075);
        assert_eq!(cues[1].text, "Réveille-toi !");
        assert_eq!(cues[2].text, "- Salut, papa. - Merde ! Salut.");
        // No index line, dot separator, position junk, ASS override tag.
        assert_eq!(cues[3].text, "Rosebud is");
        assert_eq!(cues[3].start, 120.0);
    }

    #[test]
    fn around_takes_the_window() {
        let cues = parse_srt(SRT);
        let w = around(&cues, 70.0, 30.0, 5.0);
        let texts: Vec<&str> = w.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, ["Réveille-toi !", "- Salut, papa. - Merde ! Salut."]);
        assert!(around(&cues, 5000.0, 10.0, 10.0).is_empty());
        assert!(around(&[], 1.0, 1.0, 1.0).is_empty());
    }

    #[test]
    fn search_ignores_case_and_punctuation_and_cue_breaks() {
        let cues = parse_srt(SRT);
        let hits = search(&cues, "SALUT papa", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(fmt_hms(hits[0].start), "00:01:09");
        // Split across two cues: reported once, at the first.
        let hits = search(&cues, "rosebud is my sled", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].start, 120.0);
        assert!(search(&cues, "  ", 10).is_empty());
        assert_eq!(format_cues(search(&cues, "merde", 1)), "[00:00:33] Et merde !");
    }
}
