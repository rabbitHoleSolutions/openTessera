//! What a file name says about what it is: show, season and episode for TV,
//! title and year for a film.
//!
//! Release names are a convention, not a format — `Some.Show.S01E02.
//! MULTi.1080p.WEB.H264-GROUP`, `Heat (1995).mkv`, `the_wire_1x05.avi` — so
//! this is a tolerant scan rather than a grammar: tokens up to the first
//! marker (SxxEyy, a year, or a release tag) are the title. Anything it
//! can't place is simply left out; a wrong guess would be worse than none,
//! because an assistant reads this aloud.

/// Parsed from a file name. `title` is the show for an episode.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedTitle {
    pub title: String,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub year: Option<u32>,
}

impl ParsedTitle {
    /// One line a person (or a voice) can use: "Some Show S01E02",
    /// "Heat (1995)".
    pub fn display(&self) -> String {
        let mut s = self.title.clone();
        if let Some(y) = self.year {
            s.push_str(&format!(" ({y})"));
        }
        match (self.season, self.episode) {
            (Some(se), Some(ep)) => s.push_str(&format!(" S{se:02}E{ep:02}")),
            (None, Some(ep)) => s.push_str(&format!(" E{ep:02}")),
            _ => {}
        }
        s
    }
}

/// Parses a file name or path; the directory and extension are ignored.
pub fn parse_title(name: &str) -> ParsedTitle {
    let file = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let stem = match file.rsplit_once('.') {
        // Only strip something that looks like an extension; "Mr. Robot"
        // has a dot that is not one.
        Some((stem, ext)) if (2..=4).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphanumeric()) => stem,
        _ => file,
    };
    let tokens: Vec<&str> = stem
        .split(['.', '_', ' '])
        .filter(|t| !t.is_empty())
        .collect();

    let mut out = ParsedTitle::default();
    let mut title_end = tokens.len();
    for (i, tok) in tokens.iter().enumerate() {
        let bare = tok.trim_matches(|c| matches!(c, '(' | ')' | '[' | ']'));
        if let Some((se, ep)) = season_episode(bare) {
            out.season = se;
            out.episode = Some(ep);
            title_end = title_end.min(i);
            break;
        }
        // A year needs a title before it — "1917.mkv" is a film called
        // 1917, not a film from 1917 with no name.
        if i > 0 && out.year.is_none() && is_year(bare) {
            out.year = bare.parse().ok();
            title_end = title_end.min(i);
            // Keep going: "Doctor.Who.2005.S01E01" has both.
            continue;
        }
        if i > 0 && is_release_tag(bare) {
            title_end = title_end.min(i);
            break;
        }
    }
    out.title = tokens[..title_end]
        .iter()
        .map(|t| t.trim_matches(|c| matches!(c, '(' | ')' | '[' | ']' | '-')))
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if out.title.is_empty() {
        out.title = stem.to_string();
    }
    out
}

/// `S01E02` (also `s1e2`, `S01E02E03` → first episode) or `1x02`.
fn season_episode(tok: &str) -> Option<(Option<u32>, u32)> {
    let lower = tok.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix('s') {
        let (season, rest) = rest.split_once('e')?;
        let ep: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !season.is_empty() && season.chars().all(|c| c.is_ascii_digit()) && !ep.is_empty() {
            return Some((season.parse().ok(), ep.parse().ok()?));
        }
        return None;
    }
    let (season, ep) = lower.split_once('x')?;
    let digits = |s: &str| !s.is_empty() && s.len() <= 3 && s.chars().all(|c| c.is_ascii_digit());
    // "x264" has no season, and a resolution like "1920x1080" is too long
    // on both sides to be one.
    if digits(season) && digits(ep) && season.len() <= 2 {
        return Some((season.parse().ok(), ep.parse().ok()?));
    }
    None
}

fn is_year(tok: &str) -> bool {
    tok.len() == 4
        && tok.chars().all(|c| c.is_ascii_digit())
        && matches!(tok.parse::<u32>(), Ok(1900..=2099))
}

/// Where the release junk starts: quality, source, codec, audio.
fn is_release_tag(tok: &str) -> bool {
    let t = tok.to_ascii_lowercase();
    let resolution = t.len() >= 4 && t.ends_with('p') && t[..t.len() - 1].chars().all(|c| c.is_ascii_digit());
    resolution
        || matches!(
            t.as_str(),
            "4k" | "uhd" | "hdr" | "hdr10" | "10bit" | "x264" | "x265" | "h264" | "h265" | "hevc" | "avc" | "xvid"
                | "web" | "webrip" | "web-dl" | "webdl" | "bluray" | "blu-ray" | "brrip" | "bdrip" | "dvdrip"
                | "hdtv" | "remux" | "proper" | "repack" | "multi" | "aac" | "ac3" | "dts" | "ddp5" | "atmos"
                | "extended" | "unrated" | "internal"
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(name: &str) -> ParsedTitle {
        parse_title(name)
    }

    #[test]
    fn scene_episode_names() {
        let t = p("/home/x/Videos/Some.Show.S01E02.MULTi.1080p.WEB.H264-GROUP.mkv");
        assert_eq!(t.title, "Some Show");
        assert_eq!((t.season, t.episode, t.year), (Some(1), Some(2), None));
        assert_eq!(t.display(), "Some Show S01E02");
        let t = p("the_wire_1x05.avi");
        assert_eq!((t.title.as_str(), t.season, t.episode), ("the wire", Some(1), Some(5)));
        let t = p("Show Name - s3e12 - Episode Title.mp4");
        assert_eq!((t.title.as_str(), t.season, t.episode), ("Show Name", Some(3), Some(12)));
        // A year and an episode marker together.
        let t = p("Doctor.Who.2005.S01E01.Rose.720p.mkv");
        assert_eq!((t.title.as_str(), t.year, t.season, t.episode), ("Doctor Who", Some(2005), Some(1), Some(1)));
    }

    #[test]
    fn films() {
        let t = p("Heat (1995).mkv");
        assert_eq!((t.title.as_str(), t.year), ("Heat", Some(1995)));
        assert_eq!(t.display(), "Heat (1995)");
        let t = p("Blade.Runner.2049.2017.2160p.UHD.BluRay.x265.mkv");
        // The first year-shaped token after the title is the year; a film
        // titled with a number reads oddly here, which is the trade for not
        // needing a database.
        assert_eq!((t.title.as_str(), t.year), ("Blade Runner", Some(2049)));
        let t = p("1917.mkv");
        assert_eq!((t.title.as_str(), t.year), ("1917", None));
        let t = p("Some.Film.1080p.x264.mkv");
        assert_eq!((t.title.as_str(), t.year, t.episode), ("Some Film", None, None));
    }

    #[test]
    fn things_that_are_not_markers() {
        // x264 is a codec, not season x episode; a resolution is not one.
        assert_eq!(p("Film.x264.mkv").title, "Film");
        assert_eq!(p("Clip 1920x1080.mp4").episode, None);
        // A dot inside a title is not an extension.
        assert_eq!(p("Mr. Robot").title, "Mr Robot");
        // Nothing parseable still names the thing.
        assert_eq!(p("cam-front.mp4").title, "cam-front");
    }
}
