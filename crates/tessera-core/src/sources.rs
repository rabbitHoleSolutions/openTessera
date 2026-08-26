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

fn config_path() -> Option<PathBuf> {
    std::env::var_os("TESSERA_SOURCES_CONF")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/opentessera/sources.conf")))
}

// ---------------------------------------------------------------------------
// HTTP + JSON plumbing
// ---------------------------------------------------------------------------

fn get_json(url: &str, headers: &[(&str, &str)]) -> Result<Value, SourceError> {
    let mut req = ureq::get(url).set("Accept", "application/json");
    for (k, v) in headers {
        req = req.set(k, v);
    }
    let resp = req.call().map_err(|e| SourceError::Http(e.to_string()))?;
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
            items.extend(plex_items_from_section(&section, &self.base, &self.token, &lib_title));
        }
        if library.is_some() && items.is_empty() {
            let available: Vec<String> = sections
                .pointer("/MediaContainer/Directory")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .iter()
                .filter(|d| d.get("type").and_then(Value::as_str) == Some("movie"))
                .filter_map(|d| d.get("title").and_then(Value::as_str))
                .map(str::to_owned)
                .collect();
            return Err(SourceError::Http(format!(
                "no plex movie library named {:?} (have: {})",
                library.unwrap_or_default(),
                available.join(", ")
            )));
        }
        Ok(items)
    }
}

/// Maps one Plex library-section listing (`/library/sections/{id}/all`)
/// into MediaItems. Direct-play URI: `{base}{part.key}?X-Plex-Token=...`.
fn plex_items_from_section(json: &Value, base: &str, token: &str, lib: &str) -> Vec<MediaItem> {
    let metadata = json
        .pointer("/MediaContainer/Metadata")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut items = Vec::new();
    for m in metadata {
        let part = m.pointer("/Media/0/Part/0").cloned().unwrap_or(Value::Null);
        let part_key = match part.get("key").and_then(Value::as_str) {
            Some(k) => k,
            None => continue,
        };
        let title = m.get("title").and_then(Value::as_str).unwrap_or("?").to_owned();
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
        items.push(MediaItem {
            id: format!("plex|{rating_key}"),
            title,
            uri: format!("{base}{part_key}?X-Plex-Token={token}"),
            kind: SourceKind::Plex,
            width: m.pointer("/Media/0/videoWidth").and_then(Value::as_i64).map(|v| v as u32),
            height: m.pointer("/Media/0/videoHeight").and_then(Value::as_i64).map(|v| v as u32),
            fps: plex_frame_rate(m.pointer("/Media/0/videoFrameRate").and_then(Value::as_str)),
            codec: m.pointer("/Media/0/videoCodec").and_then(Value::as_str).map(str::to_owned),
            size: part.get("size").and_then(Value::as_i64).map(|v| v.max(0) as u64),
            modified: part.get("updatedAt").and_then(Value::as_i64).map(|v| v.max(0) as u64),
            thumb_url,
        });
        let _ = lib;
    }
    items.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
    items
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
        // Prefer the container's declared name; fall back to the original path.
        let size = stream.get("Size").and_then(Value::as_i64).map(|v| v.max(0) as u64);
        let video = stream
            .pointer("/MediaStreams/0")
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
        });
    }
    items.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
    items
}

/// Deletes a media item at its source: removes the local file, or issues
/// the server's delete API (Jellyfin `DELETE /Items/{id}`, Plex
/// `DELETE /library/metadata/{ratingKey}/delete`).
pub fn delete_item(item: &MediaItem) -> Result<(), String> {
    match item.kind {
        SourceKind::Local => std::fs::remove_file(&item.uri).map_err(|e| e.to_string()),
        SourceKind::Jellyfin => {
            let cfg = SourceConfig::load();
            let srv = cfg.jellyfin.ok_or("jellyfin server not configured")?;
            let id = item.id.strip_prefix("jellyfin|").ok_or("bad jellyfin item id")?;
            let url = format!("{}/Items/{id}?api_key={}", srv.url, srv.token);
            ureq::delete(&url).call().map(|_| ()).map_err(|e| e.to_string())
        }
        SourceKind::Plex => {
            let cfg = SourceConfig::load();
            let srv = cfg.plex.ok_or("plex server not configured")?;
            let key = item.id.strip_prefix("plex|").ok_or("bad plex item id")?;
            let url = format!("{}/library/metadata/{key}/delete?X-Plex-Token={}", srv.url, srv.token);
            ureq::delete(&url).call().map(|_| ()).map_err(|e| e.to_string())
        }
        SourceKind::Smb => Err("smb delete not supported (mount it locally instead)".into()),
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
                    "Media": [{
                        "videoWidth": 3840, "videoHeight": 1080,
                        "videoFrameRate": "NTSC", "videoCodec": "hevc",
                        "Part": [{ "key": "/library/parts/9/42/file.mkv", "size": 9000000000u64, "updatedAt": 1700000000u64 }]
                    }]
                },
                { "title": "No Media", "ratingKey": "124" }
            ]}
        });
        let items = plex_items_from_section(&json, "http://x:32400", "tok", "lib");
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
    fn delete_local_item_removes_file() {
        let dir = std::env::temp_dir().join(format!("tessera-del-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("doomed.mp4");
        std::fs::write(&path, b"not really a video").unwrap();
        let item = crate::MediaItem::from_path(&path);
        assert!(path.exists());
        delete_item(&item).unwrap();
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir).ok();
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
