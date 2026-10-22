//! Remote media sources: Plex and Jellyfin.
//!
//! Each source lists movies and turns them into [`MediaItem`]s whose `uri`
//! is a direct-stream URL the server will serve over HTTP with range
//! support — mpv consumes it exactly like a local file (seek included).
//! No transcoding is requested; if a client can't direct-play a file the
//! server still serves the original bytes, which is what a video wall wants.
//!
//! Credentials are read from `~/.config/opentessera/sources.conf`
//! (`key = value` lines) and overridden by the `TESSERA_PLEX_URL` /
//! `TESSERA_PLEX_TOKEN` / `TESSERA_JELLYFIN_URL` / `TESSERA_JELLYFIN_KEY`
//! environment variables.

use std::path::PathBuf;

use serde_json::Value;

use crate::media::{MediaItem, SourceKind};

#[derive(Debug)]
pub enum SourceError {
    Http(String),
    Json(String),
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceError::Http(e) => write!(f, "http: {e}"),
            SourceError::Json(e) => write!(f, "json: {e}"),
        }
    }
}

/// Everything needed to reach one remote server.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub url: String,
    pub token: String,
}

/// Parsed `sources.conf` (env vars already applied).
#[derive(Clone, Debug, Default)]
pub struct SourceConfig {
    pub plex: Option<ServerConfig>,
    pub jellyfin: Option<ServerConfig>,
}

impl SourceConfig {
    /// Reads `~/.config/opentessera/sources.conf`, then applies env overrides.
    /// Missing file is not an error (empty config).
    pub fn load() -> SourceConfig {
        let mut kv: Vec<(String, String)> = Vec::new();
        if let Some(path) = config_path() {
            if let Ok(text) = std::fs::read_to_string(path) {
                for line in text.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    if let Some((k, v)) = line.split_once('=') {
                        kv.push((k.trim().to_owned(), v.trim().to_owned()));
                    }
                }
            }
        }
        let get = |names: &[&str]| -> Option<String> {
            for n in names {
                if let Ok(v) = std::env::var(n) {
                    if !v.is_empty() {
                        return Some(v);
                    }
                }
            }
            kv.iter().find(|(k, _)| names.contains(&k.as_str())).map(|(_, v)| v.clone())
        };
        SourceConfig {
            plex: pair(get(&["TESSERA_PLEX_URL", "plex_url"]), get(&["TESSERA_PLEX_TOKEN", "plex_token"])),
            jellyfin: pair(
                get(&["TESSERA_JELLYFIN_URL", "jellyfin_url"]),
                get(&["TESSERA_JELLYFIN_KEY", "jellyfin_key"]),
            ),
        }
    }
}

fn pair(url: Option<String>, token: Option<String>) -> Option<ServerConfig> {
    match (url, token) {
        (Some(url), Some(token)) => Some(ServerConfig {
            url: url.trim_end_matches('/').to_owned(),
            token,
        }),
        (Some(url), None) => {
            eprintln!(
                "sources: {} given but no token (set it in ~/.config/opentessera/sources.conf or the matching env var)",
                url
            );
            None
        }
        (None, _) => None,
    }
}

/// One library on a server, as offered by the add-source picker.
///
/// `movies` is what the lister would actually return for it. It is shown
/// (choosing between "Movies" and "Movies Demo" is much easier with the
/// counts in front of you) and, for Jellyfin, it is also the filter — see
/// [`JellyfinSource::libraries`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibraryInfo {
    pub name: String,
    pub movies: u64,
}

/// One configured media source: a folder on disk, or a Plex/Jellyfin
/// server. Sources persist in settings.conf (as one JSON line) and are
/// editable in the settings panel; listing them is shared between startup
/// and (future) runtime re-scans. Camera sources will join this enum.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Source {
    /// A folder (or single file) on local disk.
    Folder { path: String },
    /// A Plex server: direct-play URLs, token auth, optional library filter.
    Plex {
        url: String,
        token: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        library: Option<String>,
    },
    /// A Jellyfin server: direct-play URLs, API-key auth, optional library
    /// filter.
    Jellyfin {
        url: String,
        api_key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        library: Option<String>,
    },
    /// Live camera feeds, one item per stream.
    ///
    /// Deliberately a list of plain stream URLs rather than a client for
    /// any particular NVR. A camera source is the one place where the
    /// vendor API buys least: it costs an API key, a self-signed-certificate
    /// bypass and a parser per vendor, and all it returns is a URL that mpv
    /// was always going to open anyway. Taking the URL directly works with
    /// every camera that speaks RTSP.
    ///
    /// The cost is that the list is assembled rather than enumerated —
    /// which is what network discovery is for: it finds the cameras and
    /// names them, and only the stream path has to be supplied.
    Cameras {
        /// What to call this group in the source list.
        label: String,
        streams: Vec<CameraStream>,
    },
}

/// One camera: a name to show and a stream to open.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CameraStream {
    pub name: String,
    /// Anything mpv can open — `rtsp://`, `rtsps://`, an MJPEG `http://`
    /// endpoint, an HLS playlist.
    pub url: String,
}

impl Source {
    /// Short human label for logs and the settings panel.
    pub fn label(&self) -> String {
        let lib = |l: &Option<String>| {
            l.as_deref()
                .map(|l| format!(" ({l})"))
                .unwrap_or_default()
        };
        match self {
            Source::Folder { path } => format!("folder {path}"),
            Source::Plex { url, library, .. } => format!("plex {url}{}", lib(library)),
            Source::Jellyfin { url, library, .. } => format!("jellyfin {url}{}", lib(library)),
            Source::Cameras { label, streams } => {
                format!("cameras {label} ({})", streams.len())
            }
        }
    }

    /// Lists the source's items. Servers produce direct-play URIs mpv
    /// consumes like local files; folders go through the media scanner.
    pub fn list(&self) -> Result<Vec<MediaItem>, SourceError> {
        let mut items = match self {
            Source::Folder { path } => {
                crate::media::collect_items(&[PathBuf::from(path)])
            }
            Source::Plex { url, token, library } => PlexSource {
                base: url.clone(),
                token: token.clone(),
            }
            .list(library.as_deref())?,
            Source::Jellyfin { url, api_key, library } => JellyfinSource {
                base: url.clone(),
                api_key: api_key.clone(),
            }
            .list(library.as_deref())?,
            // No round trip: the streams *are* the listing. A camera that
            // is unplugged still belongs in the list — it is the wall that
            // discovers that, by failing to open it, not this.
            Source::Cameras { streams, .. } => streams.iter().map(camera_item).collect(),
        };
        // Stamped here, in the one place every listing goes through, so no
        // path can forget and leave an item that delete can't route.
        let label = self.label();
        for item in items.iter_mut() {
            item.source = Some(label.clone());
        }
        Ok(items)
    }
}

/// One camera stream as a queueable item.
///
/// Dimensions are left unknown on purpose: unlike a Plex listing there is
/// no metadata to read without opening the stream, so the decode budget
/// admits it on the placeholder and corrects itself from the real numbers
/// the moment mpv reports them (`TileEvent::Loaded`).
fn camera_item(cam: &CameraStream) -> MediaItem {
    MediaItem {
        // Keyed on the URL, so renaming a camera does not orphan its place
        // in a saved queue.
        id: format!("camera|{}", cam.url),
        title: cam.name.clone(),
        uri: cam.url.clone(),
        kind: SourceKind::Camera,
        width: None,
        height: None,
        fps: None,
        codec: None,
        size: None,
        modified: None,
        thumb_url: None,
        source: None,
    }
}

fn config_path() -> Option<PathBuf> {
    std::env::var_os("TESSERA_SOURCES_CONF")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/opentessera/sources.conf")))
}

// ---------------------------------------------------------------------------
// HTTP + JSON plumbing
// ---------------------------------------------------------------------------

/// The shared HTTP client for every server call.
///
/// The timeouts are the point. ureq applies none by default, and
/// `catalog.rs` runs a single listing worker deliberately — so one server
/// that accepts a connection and then goes quiet blocks *every* source's
/// listing for the life of the process, with the browser stuck on
/// "listing…" and no way back short of a restart. `timeout_read` is per
/// socket read rather than for the whole body, so a large-but-progressing
/// library listing is not cut off.
fn agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(5))
            .timeout_read(std::time::Duration::from_secs(20))
            .build()
    })
}

/// An HTTP failure as text a person can be shown — and never the URL.
///
/// ureq's own `Display` leads with the full request URL, and for every
/// server here the query *is* the credential (`?api_key=`,
/// `?X-Plex-Token=`). It put a live Jellyfin key into the delete toast, the
/// log and the library tab's "couldn't list this source". The status and
/// the server's own first line are the useful part anyway.
pub(crate) fn http_error(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            let first: String = body.trim().lines().next().unwrap_or("").chars().take(200).collect();
            // An HTML error page says nothing a toast can use, and nor
            // does Jellyfin's stock 500 body — its reason is only in its
            // own log.
            if first.is_empty() || first.starts_with('<') || first == "Error processing request." {
                format!("the server answered {code}")
            } else {
                format!("the server answered {code}: {first}")
            }
        }
        ureq::Error::Transport(t) => match t.message() {
            Some(m) => format!("{}: {m}", t.kind()),
            None => t.kind().to_string(),
        },
    }
}

/// Why a server refused a delete, when the status alone can say. Both of
/// these are settings on the server, which Tessera cannot change.
fn delete_error(e: ureq::Error, kind: SourceKind) -> String {
    let code = match &e {
        ureq::Error::Status(code, _) => Some(*code),
        ureq::Error::Transport(_) => None,
    };
    match (kind, code) {
        // Observed: Jellyfin in Docker with the movies volume mounted `:ro`
        // answers 500 with a body of "Error processing request." and keeps
        // the item; "Read-only file system" is only in its own log.
        (SourceKind::Jellyfin, Some(500)) => {
            "jellyfin couldn't delete the file (server error 500). Its log says why — usually the media folder is mounted read-only, e.g. a Docker `:ro` volume.".to_owned()
        }
        (SourceKind::Jellyfin, Some(401 | 403)) => {
            "jellyfin refused: this key isn't allowed to delete. Check \"allow media deletion\" for the user in jellyfin.".to_owned()
        }
        (SourceKind::Plex, Some(401 | 403)) => {
            "plex refused. Turn on \"allow media deletion\" in the plex server's library settings.".to_owned()
        }
        _ => http_error(e),
    }
}

fn get_json(url: &str, headers: &[(&str, &str)]) -> Result<Value, SourceError> {
    let mut req = agent().get(url).set("Accept", "application/json");
    for (k, v) in headers {
        req = req.set(k, v);
    }
    let resp = req.call().map_err(|e| SourceError::Http(http_error(e)))?;
    resp.into_json::<Value>()
        .map_err(|e| SourceError::Json(e.to_string()))
}

// ---------------------------------------------------------------------------
// Plex
// ---------------------------------------------------------------------------

/// A Plex server. Lists every movie library and every movie in it.
#[derive(Clone, Debug)]
pub struct PlexSource {
    pub base: String,
    pub token: String,
}

impl PlexSource {
    /// The titles of this server's movie libraries, for the add-source
    /// picker. Deliberately the same section filter [`PlexSource::list`]
    /// applies, so everything offered here is something `list` can actually
    /// return items for — a picker that can suggest a library the lister
    /// then rejects would be worse than no picker.
    pub fn libraries(&self) -> Result<Vec<LibraryInfo>, SourceError> {
        let auth = format!("X-Plex-Token={}", self.token);
        let sections = get_json(&format!("{}/library/sections?{auth}", self.base), &[])?;
        let mut out = Vec::new();
        for (key, name) in movie_sections(&sections) {
            // Container-Size=0 asks for the count without the metadata —
            // the full listing of a large section is megabytes, and the
            // picker only needs the number.
            let counted = get_json(
                &format!("{}/library/sections/{key}/all?{auth}&X-Plex-Container-Start=0&X-Plex-Container-Size=0", self.base),
                &[],
            )?;
            let movies = counted
                .pointer("/MediaContainer/totalSize")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            out.push(LibraryInfo { name, movies });
        }
        Ok(out)
    }

    /// Lists every movie library. `library` restricts to one section by
    /// title (case-insensitive).
    pub fn list(&self, library: Option<&str>) -> Result<Vec<MediaItem>, SourceError> {
        let auth = format!("X-Plex-Token={}", self.token);
        let sections =
            get_json(&format!("{}/library/sections?{auth}", self.base), &[])?;
        let mut items = Vec::new();
        let dirs = sections
            .pointer("/MediaContainer/Directory")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for dir in dirs {
            if dir.get("type").and_then(Value::as_str) != Some("movie") {
                continue;
            }
            let key = match dir.get("key").and_then(Value::as_str) {
                Some(k) => k.to_owned(),
                None => continue,
            };
            let lib_title = dir.get("title").and_then(Value::as_str).unwrap_or("?").to_owned();
            if let Some(want) = library {
                if !lib_title.eq_ignore_ascii_case(want) {
                    continue;
                }
            }
            let section = get_json(&format!("{}/library/sections/{key}/all?{auth}", self.base), &[])?;
            items.extend(plex_items_from_section(&section, &self.base, &self.token));
        }
        // Only a naming mistake is an error: a library that exists and is
        // genuinely empty is not.
        if let Some(want) = library {
            let available = movie_section_titles(&sections);
            if !available.iter().any(|t| t.eq_ignore_ascii_case(want)) {
                return Err(SourceError::Http(format!(
                    "no plex movie library named {want:?} (have: {})",
                    available.join(", ")
                )));
            }
        }
        Ok(items)
    }
}

/// `(section key, title)` for every movie section in a `/library/sections`
/// response. Plex's `type` is authoritative here, unlike Jellyfin's
/// CollectionType — see [`JellyfinSource::libraries`].
fn movie_sections(sections: &Value) -> Vec<(String, String)> {
    sections
        .pointer("/MediaContainer/Directory")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|d| d.get("type").and_then(Value::as_str) == Some("movie"))
        .filter_map(|d| {
            Some((
                d.get("key").and_then(Value::as_str)?.to_owned(),
                d.get("title").and_then(Value::as_str)?.to_owned(),
            ))
        })
        .collect()
}

fn movie_section_titles(sections: &Value) -> Vec<String> {
    movie_sections(sections).into_iter().map(|(_, t)| t).collect()
}

/// Maps one Plex library-section listing (`/library/sections/{id}/all`)
/// into MediaItems. Direct-play URI: `{base}{part.key}?X-Plex-Token=...`.
///
/// One item per *version*, not per film. Plex merges every file it
/// matches to the same movie into one entry with several `Media`, and
/// reading only the first hid the rest: the library showed one title, the
/// wall could only ever play one copy, and the trash — which deletes the
/// whole entry — took every copy with it. A single-version item keeps the
/// plain `plex|{ratingKey}` id, so saved queues and thumbnails are
/// unchanged; only a merged one gains `|{mediaId}` (see
/// [`crate::info::plex_ids`]).
fn plex_items_from_section(json: &Value, base: &str, token: &str) -> Vec<MediaItem> {
    let metadata = json
        .pointer("/MediaContainer/Metadata")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut items = Vec::new();
    for m in metadata {
        // Trimmed: Plex keeps a leading space from a file name like
        // "(artist) - song.mpg", which sorted those entries ahead of
        // everything else.
        let base_title = m.get("title").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()).unwrap_or("?");
        let title = base_title.to_owned();
        let year = m.get("year").and_then(Value::as_i64);
        let rating_key = m.get("ratingKey").and_then(Value::as_str).unwrap_or("").to_owned();
        let thumb_url = m
            .get("thumb")
            .and_then(Value::as_str)
            .map(|t| format!("{base}{t}?X-Plex-Token={token}"));
        let title = match year {
            Some(y) => format!("{title} ({y})"),
            None => title,
        };
        let medias = m.get("Media").and_then(Value::as_array).cloned().unwrap_or_default();
        let several = medias.len() > 1;
        for media in &medias {
            let part = media.pointer("/Part/0").cloned().unwrap_or(Value::Null);
            let part_key = match part.get("key").and_then(Value::as_str) {
                Some(k) => k,
                None => continue,
            };
            // A merged entry is only as right as Plex's match, and a
            // version whose file names something else was mismatched
            // into it — 23 different music videos under one film, in a
            // real library. That row goes by its file, without the
            // entry's poster (it is of the wrong film; the thumbnailer
            // takes a frame from the file instead).
            let own = several
                .then(|| part.get("file").and_then(Value::as_str))
                .flatten()
                .and_then(|f| mismatched_version_title(base_title, year, f));
            let (title, thumb_url) = match own {
                Some(t) => (t, None),
                None => (title.clone(), thumb_url.clone()),
            };
            let id = match media.get("id").map(|v| v.to_string()) {
                Some(media_id) if several => format!("plex|{rating_key}|{}", media_id.trim_matches('"')),
                _ => format!("plex|{rating_key}"),
            };
            let dim = |k: &str| media.get(k).and_then(Value::as_i64).map(|v| v as u32);
            items.push(MediaItem {
                id,
                title,
                uri: format!("{base}{part_key}?X-Plex-Token={token}"),
                kind: SourceKind::Plex,
                // `width`/`height`, not `videoWidth`/`videoHeight`: the
                // latter are not Plex fields, and reading them left every
                // Plex item's size unknown — so each one entered the
                // decode budget as the 1080p30 placeholder, 4K included.
                width: dim("width"),
                height: dim("height"),
                fps: plex_frame_rate(media.get("videoFrameRate").and_then(Value::as_str)),
                codec: media.get("videoCodec").and_then(Value::as_str).map(str::to_owned),
                size: part.get("size").and_then(Value::as_i64).map(|v| v.max(0) as u64),
                // When it joined the library — what "date" ordering means
                // to a person. Parts carry no timestamp of their own.
                modified: m.get("addedAt").and_then(Value::as_i64).map(|v| v.max(0) as u64),
                thumb_url,
                source: None,
            });
        }
    }
    items.sort_by_key(|a| a.title.to_lowercase());
    items
}

/// The title a version of a merged Plex entry should go by, when its file
/// says it is something other than the entry: `None` when the file agrees
/// (same title, and the same year or none), which keeps Plex's tidier name
/// for what is genuinely a second copy.
fn mismatched_version_title(entry: &str, year: Option<i64>, file: &str) -> Option<String> {
    let parsed = crate::title::parse_title(file);
    let key = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect::<String>();
    let same_year = match (parsed.year, year) {
        (Some(a), Some(b)) => a as i64 == b,
        _ => true,
    };
    if parsed.title.is_empty() || (key(&parsed.title) == key(entry) && same_year) {
        return None;
    }
    Some(parsed.display())
}

/// Plex reports frame rates as labels: "24p", "PAL", "NTSC", "60p"...
fn plex_frame_rate(label: Option<&str>) -> Option<f64> {
    match label? {
        "PAL" => Some(25.0),
        "NTSC" => Some(29.97),
        "24p" => Some(24.0),
        "25p" => Some(25.0),
        "30p" => Some(30.0),
        "50p" => Some(50.0),
        "60p" => Some(60.0),
        other => {
            let digits: String = other.chars().filter(|c| c.is_ascii_digit() || *c == '.').collect();
            digits.parse::<f64>().ok().filter(|v| *v > 0.0 && *v <= 240.0)
        }
    }
}

// ---------------------------------------------------------------------------
// Jellyfin
// ---------------------------------------------------------------------------

/// A Jellyfin server. Lists every movie across all libraries.
#[derive(Clone, Debug)]
pub struct JellyfinSource {
    pub base: String,
    pub api_key: String,
}

impl JellyfinSource {
    /// This server's libraries, for the add-source picker.
    ///
    /// Jellyfin's `CollectionType` cannot be the filter the way Plex's
    /// `type` can: a perfectly ordinary movie library reports
    /// `CollectionType: null` (observed on a real server holding 30
    /// movies), so filtering on it hides exactly the library the user
    /// wants. The reliable question is the one the lister itself asks —
    /// how many `Movie` items does this folder hold — so each folder is
    /// counted with `Limit=0`, which returns `TotalRecordCount` and no
    /// items. A folder is offered if it holds movies, or if it is declared
    /// a movie library and merely happens to be empty.
    pub fn libraries(&self) -> Result<Vec<LibraryInfo>, SourceError> {
        let folders = get_json(
            &format!("{}/Library/MediaFolders?EnableImages=false&api_key={}", self.base, self.api_key),
            &[],
        )?;
        let mut out = Vec::new();
        for f in folders.get("Items").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default() {
            let (Some(name), Some(id)) = (
                f.get("Name").and_then(Value::as_str),
                f.get("Id").and_then(Value::as_str),
            ) else {
                continue;
            };
            let declared_movies =
                f.get("CollectionType").and_then(Value::as_str) == Some("movies");
            let counted = get_json(
                &format!(
                    "{}/Items?ParentId={id}&Recursive=true&IncludeItemTypes=Movie&Limit=0&api_key={}",
                    self.base, self.api_key
                ),
                &[],
            )?;
            let movies = counted
                .get("TotalRecordCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if movies > 0 || declared_movies {
                out.push(LibraryInfo { name: name.to_owned(), movies });
            }
        }
        Ok(out)
    }

    /// Lists every movie across all libraries. `library` restricts to one
    /// library by name (case-insensitive).
    pub fn list(&self, library: Option<&str>) -> Result<Vec<MediaItem>, SourceError> {
        let parent = match library {
            Some(want) => {
                let folders = get_json(
                    &format!("{}/Library/MediaFolders?EnableImages=false&api_key={}", self.base, self.api_key),
                    &[],
                )?;
                let items = folders.get("Items").and_then(Value::as_array).cloned().unwrap_or_default();
                let hit = items
                    .iter()
                    .find(|f| f.get("Name").and_then(Value::as_str).map(|n| n.eq_ignore_ascii_case(want)).unwrap_or(false));
                match hit {
                    Some(f) => f.get("Id").and_then(Value::as_str).unwrap_or_default().to_owned(),
                    None => {
                        let available: Vec<String> = items
                            .iter()
                            .filter_map(|f| f.get("Name").and_then(Value::as_str))
                            .map(str::to_owned)
                            .collect();
                        return Err(SourceError::Http(format!(
                            "no jellyfin library named {:?} (have: {})",
                            want,
                            available.join(", ")
                        )));
                    }
                }
            }
            None => String::new(),
        };
        let parent_q = if parent.is_empty() {
            "Recursive=true&".to_owned()
        } else {
            format!("ParentId={parent}&Recursive=true&")
        };
        let url = format!(
            "{}/Items?{}IncludeItemTypes=Movie&Fields=MediaSources&api_key={}",
            self.base, parent_q, self.api_key
        );
        let json = get_json(&url, &[])?;
        Ok(jellyfin_items_from_query(&json, &self.base, &self.api_key))
    }
}

/// Maps a Jellyfin `/Items` response into MediaItems. Direct-play URI:
/// `{base}/Videos/{id}/stream?static=true&api_key=...` (original file bytes).
fn jellyfin_items_from_query(json: &Value, base: &str, api_key: &str) -> Vec<MediaItem> {
    let items_json = json
        .get("Items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut items = Vec::new();
    for i in items_json {
        let id = match i.get("Id").and_then(Value::as_str) {
            Some(id) => id.to_owned(),
            None => continue,
        };
        let title = i.get("Name").and_then(Value::as_str).unwrap_or("?").to_owned();
        let year = i.get("ProductionYear").and_then(Value::as_i64);
        let title = match year {
            Some(y) => format!("{title} ({y})"),
            None => title,
        };
        let stream = i.pointer("/MediaSources/0").cloned().unwrap_or(Value::Null);
        let size = stream.get("Size").and_then(Value::as_i64).map(|v| v.max(0) as u64);
        // The stream of Type "Video", not merely the first one: Jellyfin
        // guarantees no order, and an audio stream in slot 0 leaves every
        // dimension None — which silently hands the decode budget the
        // 1080p30 placeholder for a file that might be 4K.
        let video = stream
            .pointer("/MediaStreams")
            .and_then(Value::as_array)
            .and_then(|list| {
                list.iter()
                    .find(|s| s.get("Type").and_then(Value::as_str) == Some("Video"))
            })
            .cloned()
            .unwrap_or(Value::Null);
        let thumb_url = format!("{base}/Items/{id}/Images/Primary?maxWidth=160&quality=90&api_key={api_key}");
        items.push(MediaItem {
            id: format!("jellyfin|{id}"),
            title,
            uri: format!("{base}/Videos/{id}/stream?static=true&api_key={api_key}"),
            kind: SourceKind::Jellyfin,
            width: video.get("Width").and_then(Value::as_i64).map(|v| v as u32),
            height: video.get("Height").and_then(Value::as_i64).map(|v| v as u32),
            fps: video.get("RealFrameRate").and_then(Value::as_f64).filter(|v| *v > 0.0),
            codec: video.get("Codec").and_then(Value::as_str).map(str::to_owned),
            size,
            modified: None,
            thumb_url: Some(thumb_url),
            source: None,
        });
    }
    items.sort_by_key(|a| a.title.to_lowercase());
    items
}

/// The server an item came from: its base URL and credential.
///
/// `item.source` is stamped by [`Source::list`]; sources.conf is only a
/// fallback for items listed through the --plex/--jellyfin flags, which
/// never had a configured Source. Shared by delete and details, because
/// both have to reach the server that actually holds the file — guessing
/// is wrong the moment two servers are configured.
fn server_for(item: &MediaItem, configured: &[Source]) -> Result<(String, String), String> {
    let owner = item
        .source
        .as_deref()
        .and_then(|label| configured.iter().find(|s| s.label() == label));
    let (base, key) = match (item.kind, owner) {
        (SourceKind::Jellyfin, Some(Source::Jellyfin { url, api_key, .. })) => (url.clone(), api_key.clone()),
        (SourceKind::Plex, Some(Source::Plex { url, token, .. })) => (url.clone(), token.clone()),
        (SourceKind::Jellyfin, _) => {
            let srv = SourceConfig::load()
                .jellyfin
                .ok_or("can't tell which Jellyfin server owns this item")?;
            (srv.url, srv.token)
        }
        (SourceKind::Plex, _) => {
            let srv = SourceConfig::load()
                .plex
                .ok_or("can't tell which Plex server owns this item")?;
            (srv.url, srv.token)
        }
        (kind, _) => return Err(format!("a {} item has no server", kind.label())),
    };
    Ok((base.trim_end_matches('/').to_owned(), key))
}

/// Everything the item's source can say about it, for the info panel. A
/// server round trip (or a stat for a local file), so call it from a
/// worker — never the render thread.
pub fn item_details(item: &MediaItem, configured: &[Source]) -> Result<Vec<crate::info::Section>, String> {
    match item.kind {
        SourceKind::Local => Ok(crate::info::local(std::path::Path::new(&item.uri))),
        SourceKind::Jellyfin => {
            let (base, key) = server_for(item, configured)?;
            let id = item.id.strip_prefix("jellyfin|").ok_or("bad jellyfin item id")?;
            // /Items?Ids= rather than /Items/{id}: the latter wants a user
            // id, which an API key does not have.
            let json = get_json(
                &format!("{base}/Items?Ids={id}&Fields=MediaSources,Path,DateCreated&api_key={key}"),
                &[],
            )
            .map_err(|e| e.to_string())?;
            let found = json.pointer("/Items/0").ok_or("the server no longer has this item")?;
            Ok(crate::info::jellyfin(found))
        }
        SourceKind::Plex => {
            let (base, token) = server_for(item, configured)?;
            let (rating_key, media) = crate::info::plex_ids(&item.id).ok_or("bad plex item id")?;
            let json = get_json(&format!("{base}/library/metadata/{rating_key}?X-Plex-Token={token}"), &[])
                .map_err(|e| e.to_string())?;
            let meta = json
                .pointer("/MediaContainer/Metadata/0")
                .ok_or("the server no longer has this item")?;
            Ok(crate::info::plex(meta, media))
        }
        // Nothing to ask: the listing said all there is.
        SourceKind::Smb | SourceKind::Url | SourceKind::Camera => Ok(Vec::new()),
    }
}

/// Deletes a media item at its source: removes the local file, or issues
/// the server's delete API (Jellyfin `DELETE /Items/{id}`, Plex
/// `DELETE /library/metadata/{ratingKey}`, or `.../media/{mediaId}` for one
/// version of a merged item).
pub fn delete_item(item: &MediaItem, configured: &[Source]) -> Result<(), String> {
    // This erases the file, on the server's disk for Plex and Jellyfin —
    // not just the library entry — so it has to reach the server the item
    // actually came from (see `server_for`).
    match item.kind {
        SourceKind::Local => std::fs::remove_file(&item.uri).map_err(|e| e.to_string()),
        SourceKind::Jellyfin => {
            let (base, key) = server_for(item, configured)?;
            let id = item.id.strip_prefix("jellyfin|").ok_or("bad jellyfin item id")?;
            let url = format!("{base}/Items/{id}?api_key={key}");
            agent().delete(&url).call().map(|_| ()).map_err(|e| delete_error(e, SourceKind::Jellyfin))
        }
        SourceKind::Plex => {
            let (base, token) = server_for(item, configured)?;
            let (key, media) = crate::info::plex_ids(&item.id).ok_or("bad plex item id")?;
            // DELETE /library/metadata/{ratingKey} — the trailing "/delete"
            // this carried over from the Quest app is not a Plex endpoint,
            // so the request 404'd and nothing was ever removed. Matches
            // Plex's documented API, but is NOT verified against a live
            // server: this erases the file on the server's disk, so try it
            // on something expendable first.
            let url = match media {
                // One version of a merged item: only that file. The whole
                // entry would take every copy with it, which is the very
                // thing a person deleting "the big one" does not want.
                Some(media) => format!("{base}/library/metadata/{key}/media/{media}?X-Plex-Token={token}"),
                None => {
                    // An id without a version may predate the split (a
                    // saved queue) and still name a merged item. Refuse
                    // rather than erase copies nobody was looking at.
                    let meta = get_json(&format!("{base}/library/metadata/{key}?X-Plex-Token={token}"), &[])
                        .map_err(|e| e.to_string())?;
                    let versions = meta
                        .pointer("/MediaContainer/Metadata/0/Media")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len);
                    if versions > 1 {
                        return Err(format!(
                            "this plex item holds {versions} versions — refresh the library and delete the one you mean"
                        ));
                    }
                    format!("{base}/library/metadata/{key}?X-Plex-Token={token}")
                }
            };
            agent().delete(&url).call().map(|_| ()).map_err(|e| delete_error(e, SourceKind::Plex))
        }
        SourceKind::Smb => Err("smb delete not supported (mount it locally instead)".into()),
        SourceKind::Url => Err("nothing to delete: this was opened by URL".into()),
        // There is no file to remove, and the trash sits right there on the
        // transport next to a live feed.
        SourceKind::Camera => Err("a camera feed is live — there is nothing to delete".into()),
    }
}

// ---------------------------------------------------------------------------
// Tests (JSON mapping is pure; no server needed)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plex_mapping() {
        let json: Value = serde_json::json!({
            "MediaContainer": { "Metadata": [
                {
                    "title": "Gravity", "year": 2013, "ratingKey": "123",
                    "thumb": "/library/metadata/123/thumb/1700000000",
                    "addedAt": 1700000000u64,
                    "Media": [{
                        "id": 77, "width": 3840, "height": 1080,
                        "videoFrameRate": "NTSC", "videoCodec": "hevc",
                        "Part": [{ "key": "/library/parts/9/42/file.mkv", "size": 9000000000u64 }]
                    }]
                },
                { "title": "No Media", "ratingKey": "124" }
            ]}
        });
        let items = plex_items_from_section(&json, "http://x:32400", "tok");
        assert_eq!(items.len(), 1);
        let it = &items[0];
        assert_eq!(it.title, "Gravity (2013)");
        assert_eq!(it.id, "plex|123");
        assert_eq!(it.uri, "http://x:32400/library/parts/9/42/file.mkv?X-Plex-Token=tok");
        assert_eq!(it.kind, SourceKind::Plex);
        assert_eq!(it.width, Some(3840));
        assert_eq!(it.fps, Some(29.97));
        assert_eq!(it.codec.as_deref(), Some("hevc"));
        assert_eq!(it.size, Some(9_000_000_000));
        assert_eq!(it.modified, Some(1_700_000_000));
        assert_eq!(it.thumb_url.as_deref(), Some("http://x:32400/library/metadata/123/thumb/1700000000?X-Plex-Token=tok"));
    }

    #[test]
    fn plex_lists_each_version_of_a_merged_item() {
        // Plex merges every file matched to one film into a single entry;
        // a real library had 21 of them. Each copy is its own item, so
        // each can be played, inspected and deleted on its own.
        let json: Value = serde_json::json!({
            "MediaContainer": { "Metadata": [{
                "title": "Alita: Battle Angel", "year": 2019, "ratingKey": "55",
                "Media": [
                    { "id": 4806, "width": 1920, "height": 1080,
                      "Part": [{ "key": "/library/parts/4962/1/file.mkv", "size": 22010512751u64 }] },
                    { "id": 5046, "width": 3840, "height": 1600,
                      "Part": [{ "key": "/library/parts/5200/1/file.mkv", "size": 13665808326u64 }] }
                ]
            }]}
        });
        let items = plex_items_from_section(&json, "http://x:32400", "tok");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, "plex|55|4806");
        assert_eq!(items[1].id, "plex|55|5046");
        assert_eq!(items[1].width, Some(3840));
        assert!(items[1].uri.contains("/parts/5200/"));
        assert_eq!(items[0].title, items[1].title);
    }

    #[test]
    fn a_version_mismatched_into_an_entry_goes_by_its_file() {
        // From a real library: Plex matched a folder of music videos to
        // one film, and Home Alone 2 to Home Alone.
        let json: Value = serde_json::json!({
            "MediaContainer": { "Metadata": [{
                "title": " Home Alone", "year": 1990, "ratingKey": "7", "thumb": "/library/metadata/7/thumb/1",
                "Media": [
                    { "id": 1, "Part": [{ "key": "/library/parts/1/1/a.mkv",
                        "file": "/media/Kids Movies/Home.Alone.1990.720p.BRRip.x264-x0r.mkv" }] },
                    { "id": 2, "Part": [{ "key": "/library/parts/2/1/b.mp4",
                        "file": "/media/Kids Movies/Home Alone 1992 2 Lost in New York Macaulay Culkin 720p.mp4" }] },
                    { "id": 3, "Part": [{ "key": "/library/parts/3/1/c.mpg",
                        "file": "/media/Music Videos/(ace of base) - beautiful life - vsn.mpg" }] },
                    { "id": 4, "Part": [{ "key": "/library/parts/4/1/d.mkv",
                        "file": "/media/Kids Movies/home_alone.mkv" }] }
                ]
            }]}
        });
        let items = plex_items_from_section(&json, "http://x:32400", "tok");
        let by_id = |id: &str| items.iter().find(|i| i.id == id).unwrap();
        // Copies of the film keep Plex's name and poster.
        assert_eq!(by_id("plex|7|1").title, "Home Alone (1990)");
        assert_eq!(by_id("plex|7|4").title, "Home Alone (1990)");
        assert!(by_id("plex|7|1").thumb_url.is_some());
        // The others say what they are, without the wrong poster.
        assert_eq!(by_id("plex|7|2").title, "Home Alone (1992)");
        assert_eq!(by_id("plex|7|3").title, "ace of base beautiful life vsn");
        assert_eq!(by_id("plex|7|3").thumb_url, None);
    }

    #[test]
    fn jellyfin_mapping() {
        let json: Value = serde_json::json!({
            "TotalRecordCount": 1,
            "Items": [
                {
                    "Name": "Gravity", "ProductionYear": 2013, "Id": "abc123",
                    "MediaSources": [{
                        "Size": 12345,
                        "MediaStreams": [
                            { "Type": "Video", "Width": 1920, "Height": 800, "RealFrameRate": 23.976, "Codec": "h264" }
                        ]
                    }]
                }
            ]
        });
        let items = jellyfin_items_from_query(&json, "http://x:8096", "key");
        assert_eq!(items.len(), 1);
        let it = &items[0];
        assert_eq!(it.title, "Gravity (2013)");
        assert_eq!(it.id, "jellyfin|abc123");
        assert_eq!(it.uri, "http://x:8096/Videos/abc123/stream?static=true&api_key=key");
        assert_eq!(it.kind, SourceKind::Jellyfin);
        assert_eq!(it.width, Some(1920));
        assert_eq!(it.fps, Some(23.976));
        assert_eq!(it.size, Some(12345));
        assert_eq!(
            it.thumb_url.as_deref(),
            Some("http://x:8096/Items/abc123/Images/Primary?maxWidth=160&quality=90&api_key=key")
        );
    }

    #[test]
    fn jellyfin_picks_the_video_stream_not_the_first_one() {
        // Audio in slot 0 is perfectly normal in a Jellyfin response.
        let json: Value = serde_json::json!({
            "Items": [{
                "Name": "Dune", "Id": "d1",
                "MediaSources": [{
                    "MediaStreams": [
                        { "Type": "Audio", "Codec": "eac3", "Channels": 6 },
                        { "Type": "Video", "Width": 3840, "Height": 2160,
                          "RealFrameRate": 24.0, "Codec": "hevc" },
                        { "Type": "Subtitle", "Codec": "subrip" }
                    ]
                }]
            }]
        });
        let it = &jellyfin_items_from_query(&json, "http://x:8096", "k")[0];
        assert_eq!(it.width, Some(3840));
        assert_eq!(it.height, Some(2160));
        assert_eq!(it.fps, Some(24.0));
        assert_eq!(it.codec.as_deref(), Some("hevc"));
        // No video track at all is still not a panic, just unknown.
        let none: Value = serde_json::json!({
            "Items": [{ "Name": "Audiobook", "Id": "a1",
                        "MediaSources": [{ "MediaStreams": [{ "Type": "Audio" }] }] }]
        });
        assert_eq!(jellyfin_items_from_query(&none, "http://x:8096", "k")[0].width, None);
    }

    #[test]
    fn http_errors_never_carry_the_url() {
        // The query is the credential; this is what leaked into a toast.
        let e = agent().get("http://127.0.0.1:1/Items/x?api_key=sekrit").call().unwrap_err();
        let shown = http_error(e);
        assert!(!shown.contains("sekrit") && !shown.contains("api_key"), "{shown}");

        // Jellyfin's real 500 body, which says nothing; the hint does.
        let resp = ureq::Response::new(500, "Internal Server Error", "Error processing request.").unwrap();
        assert_eq!(http_error(ureq::Error::Status(500, resp)), "the server answered 500");
        let resp = ureq::Response::new(500, "Internal Server Error", "Error processing request.").unwrap();
        let shown = delete_error(ureq::Error::Status(500, resp), SourceKind::Jellyfin);
        assert!(shown.contains("read-only"), "{shown}");
        let resp = ureq::Response::new(400, "Bad Request", "Item not found").unwrap();
        assert_eq!(http_error(ureq::Error::Status(400, resp)), "the server answered 400: Item not found");

        let page = ureq::Response::new(404, "Not Found", "<html><body>nope</body></html>").unwrap();
        assert_eq!(http_error(ureq::Error::Status(404, page)), "the server answered 404");
    }

    #[test]
    fn delete_local_item_removes_file() {
        let dir = std::env::temp_dir().join(format!("tessera-del-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("doomed.mp4");
        std::fs::write(&path, b"not really a video").unwrap();
        let item = crate::MediaItem::from_path(&path);
        assert!(path.exists());
        delete_item(&item, &[]).unwrap();
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delete_routes_to_the_server_the_item_came_from() {
        let a = Source::Jellyfin {
            url: "http://a:8096".into(),
            api_key: "key-a".into(),
            library: Some("Movies".into()),
        };
        let b = Source::Jellyfin {
            url: "http://b:8096".into(),
            api_key: "key-b".into(),
            library: Some("Shows".into()),
        };
        // An item stamped by `list` carries the label of its own source...
        let mut item = MediaItem::from_path(std::path::Path::new("/x.mkv"));
        item.kind = SourceKind::Jellyfin;
        item.id = "jellyfin|zzz".into();
        item.source = Some(b.label());

        let owner = item
            .source
            .as_deref()
            .and_then(|l| [a.clone(), b.clone()].into_iter().find(|s| s.label() == l));
        // ...and resolves to B, not to whichever server happens to be first.
        assert!(matches!(owner, Some(Source::Jellyfin { ref url, .. }) if url == "http://b:8096"));

        // An unstamped item (listed via --jellyfin, no configured Source)
        // matches nothing and falls back to sources.conf.
        item.source = None;
        let owner = item
            .source
            .as_deref()
            .and_then(|l| [a, b].into_iter().find(|s| s.label() == l));
        assert!(owner.is_none());
    }

    #[test]
    fn movie_sections_are_offered_and_others_are_not() {
        let sections: Value = serde_json::json!({
            "MediaContainer": { "Directory": [
                { "key": "1", "type": "movie", "title": "_sx2" },
                { "key": "2", "type": "movie", "title": "Movies" },
                { "key": "3", "type": "show",  "title": "TV" },
                { "key": "4", "type": "movie", "title": "Movies Demo" },
            ]}
        });
        // The picker offers exactly what the lister can serve: movie
        // sections, in server order, with TV left out.
        assert_eq!(movie_section_titles(&sections), ["_sx2", "Movies", "Movies Demo"]);
        assert!(movie_section_titles(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn plex_sections_carry_their_keys() {
        let sections: Value = serde_json::json!({
            "MediaContainer": { "Directory": [
                { "key": "1", "type": "movie", "title": "_sx2" },
                { "key": "9", "type": "show",  "title": "TV" },
                { "key": "2", "type": "movie", "title": "Movies" },
            ]}
        });
        assert_eq!(
            movie_sections(&sections),
            [("1".to_owned(), "_sx2".to_owned()), ("2".to_owned(), "Movies".to_owned())]
        );
    }

    #[test]
    fn jellyfin_offers_a_library_that_declares_no_collection_type() {
        // The rule this encodes, from a real server: `Movies` holds 30
        // films and reports CollectionType null, while `Playlists` is
        // typed and holds none. Filtering on the type would offer exactly
        // the wrong one of those two.
        let folder = |name: &str, ct: Option<&str>| {
            let mut m = serde_json::Map::new();
            m.insert("Name".into(), Value::from(name));
            m.insert("Id".into(), Value::from(format!("id-{name}")));
            if let Some(ct) = ct {
                m.insert("CollectionType".into(), Value::from(ct));
            }
            Value::Object(m)
        };
        let offered = |f: &Value, movies: u64| {
            let declared = f.get("CollectionType").and_then(Value::as_str) == Some("movies");
            movies > 0 || declared
        };
        assert!(offered(&folder("Movies", None), 30), "untyped but full");
        assert!(!offered(&folder("Playlists", Some("playlists")), 0), "typed, empty, not movies");
        assert!(offered(&folder("_sx2", Some("movies")), 4279), "typed and full");
        assert!(offered(&folder("New", Some("movies")), 0), "declared movies, merely empty");
        assert!(!offered(&folder("Music", Some("music")), 0), "not a movie library");
    }

    #[test]
    fn plex_frame_rate_labels() {
        assert_eq!(plex_frame_rate(Some("PAL")), Some(25.0));
        assert_eq!(plex_frame_rate(Some("NTSC")), Some(29.97));
        assert_eq!(plex_frame_rate(Some("24p")), Some(24.0));
        assert_eq!(plex_frame_rate(Some("nonsense")), None);
        assert_eq!(plex_frame_rate(None), None);
    }

    #[test]
    fn source_json_roundtrip() {
        let sources = vec![
            Source::Folder { path: "/media/movies".into() },
            Source::Jellyfin {
                url: "http://localhost:8096".into(),
                api_key: "k".into(),
                library: Some("Movies".into()),
            },
            Source::Plex {
                url: "http://nas:32400".into(),
                token: "t".into(),
                library: None,
            },
        ];
        let json = serde_json::to_string(&sources).unwrap();
        // Optional library is skipped when None.
        assert!(!json.contains("library\":null"));
        assert!(!json.contains("library") || json.matches("library").count() == 1);
        let back: Vec<Source> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, sources);
        // One settings.conf line must be enough: no newlines in the JSON.
        assert!(!json.contains('\n'));
    }

    #[test]
    fn source_labels() {
        assert_eq!(Source::Folder { path: "/m".into() }.label(), "folder /m");
        assert_eq!(
            Source::Jellyfin {
                url: "http://h:8096".into(),
                api_key: "k".into(),
                library: Some("Movies".into())
            }
            .label(),
            "jellyfin http://h:8096 (Movies)"
        );
    }

    #[test]
    fn config_parse_and_env_override() {
        // TESSERA_SOURCES_CONF redirects the config path for tests.
        let dir = std::env::temp_dir().join(format!("tessera-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let conf = dir.join("sources.conf");
        std::fs::write(&conf, "# comment\nplex_url = http://a:32400\nplex_token = t1\n\n").unwrap();
        // SAFETY: tests run in one thread per binary by default; serialize via env lock.
        let guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("TESSERA_SOURCES_CONF", &conf);
        std::env::remove_var("TESSERA_PLEX_URL");
        std::env::remove_var("TESSERA_PLEX_TOKEN");
        let cfg = SourceConfig::load();
        assert_eq!(cfg.plex.as_ref().unwrap().url, "http://a:32400");
        assert_eq!(cfg.plex.as_ref().unwrap().token, "t1");
        assert!(cfg.jellyfin.is_none());

        std::env::set_var("TESSERA_PLEX_URL", "http://b:32400/");
        let cfg = SourceConfig::load();
        assert_eq!(cfg.plex.as_ref().unwrap().url, "http://b:32400"); // trailing slash trimmed
        std::env::remove_var("TESSERA_PLEX_URL");
        std::env::remove_var("TESSERA_SOURCES_CONF");
        drop(guard);
        std::fs::remove_dir_all(&dir).ok();
    }

    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());
}

#[cfg(test)]
mod camera_source_tests {
    use super::*;

    fn cams() -> Source {
        Source::Cameras {
            label: "Outside".into(),
            streams: vec![
                CameraStream { name: "Front".into(), url: "rtsps://192.168.1.1:7441/abc?enableSrtp".into() },
                CameraStream { name: "Driveway".into(), url: "rtsp://192.168.1.50:554/s0".into() },
            ],
        }
    }

    #[test]
    fn each_camera_becomes_a_queueable_item() {
        // The whole point: a camera is to the queue what a film is.
        let items = cams().list().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "Front");
        assert_eq!(items[0].uri, "rtsps://192.168.1.1:7441/abc?enableSrtp");
        assert_eq!(items[0].kind, SourceKind::Camera);
        assert!(items[0].is_live(), "a camera never ends");
        // Stamped with its source, like every other listing, so routing works.
        assert_eq!(items[0].source.as_deref(), Some("cameras Outside (2)"));
        // Ids key on the URL, so renaming a camera keeps its place in a
        // saved queue.
        assert_eq!(items[1].id, "camera|rtsp://192.168.1.50:554/s0");
    }

    #[test]
    fn a_camera_is_not_something_you_can_delete() {
        let item = &cams().list().unwrap()[0];
        let err = delete_item(item, &[]).unwrap_err();
        assert!(err.contains("live"), "got {err}");
    }

    #[test]
    fn recorded_items_are_not_live() {
        assert!(!MediaItem::from_path(std::path::Path::new("/m/a.mkv")).is_live());
    }

    #[test]
    fn a_camera_source_round_trips_through_settings() {
        let json = serde_json::to_string(&vec![cams()]).unwrap();
        assert!(!json.contains('\n'), "settings.conf is one line per key");
        let back: Vec<Source> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, vec![cams()]);
    }

    #[test]
    fn an_empty_camera_group_is_a_listing_of_nothing_not_an_error() {
        let empty = Source::Cameras { label: "None".into(), streams: vec![] };
        assert!(empty.list().unwrap().is_empty());
    }
}
