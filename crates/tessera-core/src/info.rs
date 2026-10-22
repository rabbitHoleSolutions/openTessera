//! Everything there is to know about one item, as rows for the info panel.
//!
//! Three sources, in increasing cost: what the listing already carried
//! ([`listing`] — free, shown the moment the panel opens), what the server
//! knows about the file ([`crate::sources::item_details`] — one HTTP round
//! trip, so it runs on a worker), and for a playing tile what mpv is
//! actually doing with it (assembled by the app, which owns the player).
//!
//! The reason this exists is telling two copies of a film apart: the
//! library shows two identical titles, and the only safe way to delete
//! the right one is to see its path, size and format first.

use serde_json::Value;

use crate::media::{MediaItem, SourceKind};

/// One block of the panel: a heading and label/value rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub title: String,
    pub rows: Vec<(String, String)>,
}

impl Section {
    pub fn new(title: &str) -> Section {
        Section { title: title.to_owned(), rows: Vec::new() }
    }

    /// Adds a row, skipping an empty value — a server that omits a field
    /// should make the row disappear, not print a blank.
    pub fn row(&mut self, label: &str, value: impl Into<String>) {
        let value = value.into();
        if !value.trim().is_empty() {
            self.rows.push((label.to_owned(), value));
        }
    }

    fn opt(&mut self, label: &str, value: Option<String>) {
        if let Some(v) = value {
            self.row(label, v);
        }
    }
}

/// What the listing already knows. Shown immediately, and kept for any
/// item the server can't say more about (a camera, a URL).
pub fn listing(item: &MediaItem) -> Vec<Section> {
    let mut s = Section::new("item");
    match item.kind {
        SourceKind::Local => s.row("path", item.uri.clone()),
        SourceKind::Plex | SourceKind::Jellyfin => {}
        _ => s.row("address", redact_uri(&item.uri)),
    }
    s.opt("size", item.size.map(human_size));
    s.opt("picture", picture(item.width, item.height, item.codec.as_deref(), item.fps));
    if let Some(v) = plex_ids(&item.id).and_then(|(_, m)| m) {
        s.row("version", format!("one of several on this plex item (media {v})"));
    }
    vec![s]
}

/// "3836×2072 · 4K · hevc · 23.976 fps", from whatever is known.
fn picture(w: Option<u32>, h: Option<u32>, codec: Option<&str>, fps: Option<f64>) -> Option<String> {
    let mut parts = Vec::new();
    if let (Some(w), Some(h)) = (w, h) {
        parts.push(format!("{w}×{h}"));
        parts.push(resolution_label(w, h).to_owned());
    }
    if let Some(c) = codec {
        parts.push(c.to_owned());
    }
    if let Some(f) = fps {
        parts.push(format!("{} fps", trim_float(f)));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// The name a person uses for a frame size. Judged on width as much as
/// height: a 2.39:1 film in 4K is 3840×1606, which by height alone is
/// "1080p" and is exactly the confusion the panel is there to clear up.
pub fn resolution_label(w: u32, h: u32) -> &'static str {
    match (w, h) {
        (w, h) if w >= 7000 || h >= 4000 => "8K",
        (w, h) if w >= 3400 || h >= 2000 => "4K",
        (w, h) if w >= 2400 || h >= 1300 => "1440p",
        (w, h) if w >= 1800 || h >= 1000 => "1080p",
        (w, h) if w >= 1200 || h >= 700 => "720p",
        _ => "SD",
    }
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{} {}", bytes, UNITS[0])
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// Bits per second, as a person reads a bitrate.
pub fn human_bitrate(bps: f64) -> String {
    if bps >= 1_000_000.0 {
        format!("{:.1} Mb/s", bps / 1_000_000.0)
    } else {
        format!("{:.0} kb/s", bps / 1000.0)
    }
}

/// h:mm:ss, or m:ss under an hour.
pub fn human_duration(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}")
    } else {
        format!("{m}:{sec:02}")
    }
}

/// Seconds since the epoch, as a UTC date. Days only: when a file was
/// added is the question, and a clock time in the wrong zone would be
/// worse than none.
pub fn human_date(epoch: u64) -> String {
    // Civil-from-days (Howard Hinnant), so a date needs no chrono.
    let z = (epoch / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn trim_float(v: f64) -> String {
    let s = format!("{v:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// A URL with anything secret taken out: the query (a Plex or Jellyfin
/// URL *is* its token) and any `user:password@`, which a camera URL
/// usually carries. Anything that is not a URL comes back as it was.
pub fn redact_uri(uri: &str) -> String {
    let Some((scheme, rest)) = uri.split_once("://") else {
        return uri.to_owned();
    };
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let rest = match rest.split_once('/') {
        Some((auth, path)) => format!("{}/{path}", auth.rsplit('@').next().unwrap_or(auth)),
        None => rest.rsplit('@').next().unwrap_or(rest).to_owned(),
    };
    format!("{scheme}://{rest}")
}

/// `plex|{ratingKey}` or `plex|{ratingKey}|{mediaId}` — the second form
/// names one version of an item Plex has merged several files into.
pub fn plex_ids(id: &str) -> Option<(&str, Option<&str>)> {
    let rest = id.strip_prefix("plex|")?;
    Some(match rest.split_once('|') {
        Some((rk, media)) => (rk, Some(media)),
        None => (rest, None),
    })
}

// ---------------------------------------------------------------------------
// Local files
// ---------------------------------------------------------------------------

/// A local file: what the filesystem says. The container's own contents
/// need a demuxer, which is mpv — the playing-tile section covers that.
pub fn local(path: &std::path::Path) -> Vec<Section> {
    let mut s = Section::new("file");
    s.row("path", path.to_string_lossy());
    match std::fs::metadata(path) {
        Ok(m) => {
            s.row("size", human_size(m.len()));
            let modified = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
            s.opt("modified", modified.map(|d| human_date(d.as_secs())));
        }
        Err(e) => s.row("error", e.to_string()),
    }
    if let Some(ext) = path.extension() {
        s.row("container", ext.to_string_lossy().to_lowercase());
    }
    vec![s]
}

// ---------------------------------------------------------------------------
// Jellyfin
// ---------------------------------------------------------------------------

/// One item from `/Items?Ids={id}&Fields=MediaSources,Path,DateCreated`.
///
/// Jellyfin's `DisplayTitle` is used as-is for tracks: it is already the
/// phrase a person wants ("English - Dolby Digital Plus + Dolby Atmos -
/// 5.1 - Default"), and it comes in the server's UI language, so it is
/// something egui's fonts can draw.
pub fn jellyfin(item: &Value) -> Vec<Section> {
    let src = item.pointer("/MediaSources/0").unwrap_or(&Value::Null);
    let str_of = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    let num = |v: &Value, k: &str| v.get(k).and_then(Value::as_f64);

    let mut file = Section::new("file");
    file.opt("path", str_of(src, "Path").or_else(|| str_of(item, "Path")));
    file.opt("size", src.get("Size").and_then(Value::as_u64).map(human_size));
    file.opt("container", str_of(src, "Container"));
    file.opt("bitrate", num(src, "Bitrate").filter(|b| *b > 0.0).map(human_bitrate));
    let ticks = num(src, "RunTimeTicks").or_else(|| num(item, "RunTimeTicks"));
    file.opt("length", ticks.map(|t| human_duration(t / 10_000_000.0)));
    // ISO 8601; the date part is all that matters here.
    file.opt("added", str_of(item, "DateCreated").map(|d| d.chars().take(10).collect()));
    let mut out = vec![file];

    let streams = src.get("MediaStreams").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let of_type = |t: &'static str| streams.iter().filter(move |s| s.get("Type").and_then(Value::as_str) == Some(t));

    let mut video = Section::new("video");
    for v in of_type("Video") {
        video.opt("stream", str_of(v, "DisplayTitle"));
        video.opt(
            "picture",
            picture(
                v.get("Width").and_then(Value::as_u64).map(|w| w as u32),
                v.get("Height").and_then(Value::as_u64).map(|h| h as u32),
                str_of(v, "Codec").as_deref(),
                num(v, "RealFrameRate"),
            ),
        );
        let mut detail = Vec::new();
        if let Some(p) = str_of(v, "Profile") {
            detail.push(p);
        }
        if let Some(b) = v.get("BitDepth").and_then(Value::as_u64) {
            detail.push(format!("{b}-bit"));
        }
        // VideoRangeType is the specific one ("DOVIWithHDR10Plus"); the
        // plain range ("HDR") only when that is missing.
        match (str_of(v, "VideoRangeType"), str_of(v, "VideoRange")) {
            (Some(t), _) if t != "Unknown" => detail.push(t),
            (_, Some(r)) if r != "Unknown" => detail.push(r),
            _ => {}
        }
        video.row("format", detail.join(" · "));
        video.opt("bitrate", num(v, "BitRate").filter(|b| *b > 0.0).map(human_bitrate));
    }
    if !video.rows.is_empty() {
        out.push(video);
    }

    for (kind, title) in [("Audio", "audio"), ("Subtitle", "subtitles")] {
        let mut s = Section::new(title);
        for (i, t) in of_type(kind).enumerate() {
            let mut label = str_of(t, "DisplayTitle")
                .or_else(|| str_of(t, "Codec"))
                .unwrap_or_else(|| "?".to_owned());
            if t.get("IsExternal").and_then(Value::as_bool) == Some(true) {
                label.push_str(" · external file");
            }
            if t.get("IsForced").and_then(Value::as_bool) == Some(true) && !label.contains("Forced") {
                label.push_str(" · forced");
            }
            s.row(&(i + 1).to_string(), label);
        }
        if !s.rows.is_empty() {
            out.push(s);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Plex
// ---------------------------------------------------------------------------

/// One `Metadata` entry from `/library/metadata/{ratingKey}`, and the
/// version (`Media`) of it to describe — the first if none is named.
///
/// Track labels are built from codes rather than Plex's `displayTitle`,
/// which names each language in its own script ("Русский", "中文",
/// "ไทย"). egui's bundled fonts have no CJK, Thai or Devanagari, so those
/// would draw as empty boxes; "rus · DTS 5.1" at least says what it is.
pub fn plex(meta: &Value, media_id: Option<&str>) -> Vec<Section> {
    let medias = meta.get("Media").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let id_of = |m: &Value| m.get("id").map(|v| v.to_string().trim_matches('"').to_owned());
    let media = media_id
        .and_then(|want| medias.iter().find(|m| id_of(m).as_deref() == Some(want)))
        .or(medias.first())
        .unwrap_or(&Value::Null);
    let part = media.pointer("/Part/0").unwrap_or(&Value::Null);
    let str_of = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    let num = |v: &Value, k: &str| v.get(k).and_then(Value::as_f64);

    let mut file = Section::new("file");
    file.opt("path", str_of(part, "file"));
    file.opt("size", part.get("size").and_then(Value::as_u64).map(human_size));
    file.opt("container", str_of(media, "container").or_else(|| str_of(part, "container")));
    // Plex reports kilobits.
    file.opt("bitrate", num(media, "bitrate").filter(|b| *b > 0.0).map(|b| human_bitrate(b * 1000.0)));
    file.opt("length", num(media, "duration").or_else(|| num(meta, "duration")).map(|ms| human_duration(ms / 1000.0)));
    file.opt("added", meta.get("addedAt").and_then(Value::as_u64).map(human_date));
    if medias.len() > 1 {
        let pos = medias.iter().position(|m| std::ptr::eq(m, media)).unwrap_or(0);
        file.row("version", format!("{} of {} on this plex item", pos + 1, medias.len()));
    }
    let mut out = vec![file];

    let streams = part.get("Stream").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let of_type = |t: i64| streams.iter().filter(move |s| s.get("streamType").and_then(Value::as_i64) == Some(t));

    let mut video = Section::new("video");
    for v in of_type(1) {
        video.opt(
            "picture",
            picture(
                v.get("width").and_then(Value::as_u64).map(|w| w as u32),
                v.get("height").and_then(Value::as_u64).map(|h| h as u32),
                str_of(v, "codec").as_deref(),
                num(v, "frameRate"),
            ),
        );
        let mut detail = Vec::new();
        if let Some(p) = str_of(v, "profile") {
            detail.push(p);
        }
        if let Some(b) = v.get("bitDepth").and_then(Value::as_u64) {
            detail.push(format!("{b}-bit"));
        }
        if v.get("DOVIPresent").and_then(Value::as_bool) == Some(true) {
            detail.push("Dolby Vision".to_owned());
        }
        match str_of(v, "colorTrc").as_deref() {
            Some("smpte2084") => detail.push("HDR10".to_owned()),
            Some("arib-std-b67") => detail.push("HLG".to_owned()),
            _ => {}
        }
        video.row("format", detail.join(" · "));
        video.opt("bitrate", num(v, "bitrate").filter(|b| *b > 0.0).map(|b| human_bitrate(b * 1000.0)));
    }
    if video.rows.is_empty() {
        // An unanalysed file has no streams yet; the Media summary still
        // has the basics.
        video.opt(
            "picture",
            picture(
                media.get("width").and_then(Value::as_u64).map(|w| w as u32),
                media.get("height").and_then(Value::as_u64).map(|h| h as u32),
                str_of(media, "videoCodec").as_deref(),
                None,
            ),
        );
    }
    if !video.rows.is_empty() {
        out.push(video);
    }

    for (kind, title) in [(2, "audio"), (3, "subtitles")] {
        let mut s = Section::new(title);
        for (i, t) in of_type(kind).enumerate() {
            s.row(&(i + 1).to_string(), plex_track_label(t));
        }
        if !s.rows.is_empty() {
            out.push(s);
        }
    }
    out
}

/// "eng · DTS-HD MA 7.1 · default". The codec-and-layout part is lifted
/// from the parenthesis of `displayTitle` ("English (DTS-HD MA 7.1)"),
/// which is always Latin, rather than rebuilt from `codec`/`profile`,
/// which say "dca"/"ma" for the same thing.
fn plex_track_label(t: &Value) -> String {
    let str_of = |k: &str| t.get(k).and_then(Value::as_str);
    let mut parts: Vec<String> = Vec::new();
    if let Some(lang) = str_of("languageCode") {
        parts.push(lang.to_owned());
    }
    let paren = str_of("displayTitle")
        .and_then(|d| d.rsplit_once('(').map(|(_, r)| r.trim_end_matches(')').trim().to_owned()))
        .filter(|p| !p.is_empty());
    match paren {
        Some(p) => parts.push(p),
        None => {
            if let Some(c) = str_of("codec") {
                parts.push(c.to_uppercase());
            }
        }
    }
    if let Some(title) = str_of("title") {
        // A track's own title is free text and may be in any script; it
        // is only worth showing when egui can draw it.
        if title.chars().all(|c| (c as u32) < 0x0370) {
            parts.push(title.to_owned());
        }
    }
    if t.get("forced").and_then(Value::as_bool) == Some(true) {
        parts.push("forced".to_owned());
    }
    if t.get("default").and_then(Value::as_bool) == Some(true) {
        parts.push("default".to_owned());
    }
    if t.get("key").and_then(Value::as_str).is_some() {
        // Only sidecar subtitles have a key of their own.
        parts.push("external file".to_owned());
    }
    if parts.is_empty() {
        "?".to_owned()
    } else {
        parts.join(" · ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(sections: &[Section], title: &str) -> Vec<(String, String)> {
        sections.iter().find(|s| s.title == title).map(|s| s.rows.clone()).unwrap_or_default()
    }

    fn get<'a>(rows: &'a [(String, String)], label: &str) -> Option<&'a str> {
        rows.iter().find(|(l, _)| l == label).map(|(_, v)| v.as_str())
    }

    #[test]
    fn jellyfin_tells_the_two_civil_wars_apart() {
        // Trimmed from a real server, where the same film exists twice and
        // the library showed two identical titles.
        let item = serde_json::json!({
            "Name": "Civil War", "DateCreated": "2024-06-01T23:07:40.0000000Z",
            "MediaSources": [{
                "Path": "/media/movies/Civil.War.(2024).(2160p.iT.WEB-DL.Hybrid.H265.DV.HDR10+.DDP.Atmos.5.1.English.-.HONE).mkv",
                "Container": "mkv", "Size": 19775934786u64, "Bitrate": 25013513,
                "RunTimeTicks": 65252270000u64,
                "MediaStreams": [
                    { "Type": "Video", "Codec": "hevc", "Profile": "Main 10",
                      "DisplayTitle": "4K HEVC Dolby Vision Profile 8.1 (HDR10)",
                      "Width": 3836, "Height": 2072, "RealFrameRate": 23.976025, "BitDepth": 10,
                      "VideoRange": "HDR", "VideoRangeType": "DOVIWithHDR10Plus", "BitRate": 24245513 },
                    { "Type": "Audio", "Codec": "eac3",
                      "DisplayTitle": "English - Dolby Digital Plus + Dolby Atmos - 5.1 - Default" },
                    { "Type": "Subtitle", "Codec": "subrip", "DisplayTitle": "English - SUBRIP" },
                    { "Type": "Subtitle", "Codec": "subrip", "DisplayTitle": "Spanish - SUBRIP", "IsExternal": true }
                ]
            }]
        });
        let s = jellyfin(&item);
        let file = rows(&s, "file");
        assert!(get(&file, "path").unwrap().ends_with("HONE).mkv"));
        assert_eq!(get(&file, "size"), Some("18.4 GB"));
        assert_eq!(get(&file, "bitrate"), Some("25.0 Mb/s"));
        assert_eq!(get(&file, "length"), Some("1:48:45"));
        assert_eq!(get(&file, "added"), Some("2024-06-01"));
        let video = rows(&s, "video");
        assert_eq!(get(&video, "picture"), Some("3836×2072 · 4K · hevc · 23.976 fps"));
        assert_eq!(get(&video, "format"), Some("Main 10 · 10-bit · DOVIWithHDR10Plus"));
        assert_eq!(rows(&s, "audio").len(), 1);
        assert_eq!(get(&rows(&s, "subtitles"), "2"), Some("Spanish - SUBRIP · external file"));
    }

    fn alita() -> Value {
        serde_json::json!({
            "title": "Alita: Battle Angel", "addedAt": 1_700_000_000u64, "duration": 7317270,
            "Media": [
                { "id": 4806, "bitrate": 24064, "container": "mkv", "duration": 7317270,
                  "width": 1920, "height": 1080, "videoCodec": "h264",
                  "Part": [{ "file": "/v/Alita.3D.Half-SBS.mkv", "size": 22010512751u64,
                    "Stream": [
                      { "streamType": 1, "codec": "h264", "width": 1920, "height": 1080,
                        "frameRate": 23.976, "bitDepth": 8, "profile": "high", "colorTrc": "bt709", "bitrate": 21152 },
                      { "streamType": 2, "codec": "dca", "languageCode": "eng",
                        "displayTitle": "English (DTS-HD MA 7.1)", "default": true },
                      { "streamType": 2, "codec": "dca", "languageCode": "rus",
                        "displayTitle": "Русский (DTS 5.1)" },
                      { "streamType": 3, "codec": "pgs", "languageCode": "zho", "displayTitle": "中文" }
                    ] }] },
                { "id": 5046, "bitrate": 14000, "container": "mkv", "width": 3840, "height": 1600,
                  "videoCodec": "hevc",
                  "Part": [{ "file": "/v/Alita.2160p.mkv", "size": 13665808326u64 }] }
            ]
        })
    }

    #[test]
    fn plex_describes_the_version_it_was_asked_for() {
        let first = plex(&alita(), None);
        let file = rows(&first, "file");
        assert_eq!(get(&file, "path"), Some("/v/Alita.3D.Half-SBS.mkv"));
        assert_eq!(get(&file, "bitrate"), Some("24.1 Mb/s"));
        assert_eq!(get(&file, "version"), Some("1 of 2 on this plex item"));
        assert_eq!(get(&rows(&first, "video"), "format"), Some("high · 8-bit"));

        let second = plex(&alita(), Some("5046"));
        let file = rows(&second, "file");
        assert_eq!(get(&file, "path"), Some("/v/Alita.2160p.mkv"));
        assert_eq!(get(&file, "version"), Some("2 of 2 on this plex item"));
        // No streams analysed yet: the Media summary stands in.
        assert_eq!(get(&rows(&second, "video"), "picture"), Some("3840×1600 · 4K · hevc"));
    }

    #[test]
    fn plex_track_labels_stay_in_a_script_egui_can_draw() {
        let s = plex(&alita(), None);
        let audio = rows(&s, "audio");
        assert_eq!(get(&audio, "1"), Some("eng · DTS-HD MA 7.1 · default"));
        assert_eq!(get(&audio, "2"), Some("rus · DTS 5.1"));
        // No parenthesis to lift from: the code and the codec.
        assert_eq!(get(&rows(&s, "subtitles"), "1"), Some("zho · PGS"));
    }

    #[test]
    fn a_wide_4k_film_is_called_4k() {
        assert_eq!(resolution_label(3840, 1606), "4K");
        assert_eq!(resolution_label(1920, 800), "1080p");
        assert_eq!(resolution_label(1280, 534), "720p");
        assert_eq!(resolution_label(720, 480), "SD");
    }

    #[test]
    fn redaction_keeps_the_place_and_drops_the_secrets() {
        assert_eq!(
            redact_uri("rtsp://admin:hunter2@192.168.1.5:554/s0?x=1"),
            "rtsp://192.168.1.5:554/s0"
        );
        assert_eq!(redact_uri("http://nas:8096/Videos/x/stream?api_key=k"), "http://nas:8096/Videos/x/stream");
        assert_eq!(redact_uri("/m/a.mkv"), "/m/a.mkv");
    }

    #[test]
    fn plex_ids_name_a_version_when_there_is_one() {
        assert_eq!(plex_ids("plex|123"), Some(("123", None)));
        assert_eq!(plex_ids("plex|123|4806"), Some(("123", Some("4806"))));
        assert_eq!(plex_ids("jellyfin|abc"), None);
    }

    #[test]
    fn formatting() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(3_500_000_000), "3.3 GB");
        assert_eq!(human_bitrate(768_000.0), "768 kb/s");
        assert_eq!(human_duration(65.0), "1:05");
        assert_eq!(human_date(0), "1970-01-01");
        assert_eq!(human_date(1_700_000_000), "2023-11-14");
        assert_eq!(human_date(951_782_400), "2000-02-29");
    }
}
