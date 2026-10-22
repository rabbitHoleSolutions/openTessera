//! What survives a restart: settings.conf, queue.json, and where they live.

use tessera_core::PlaybackQueue;

use crate::overlay;

/// `~/.config/opentessera`, where settings, source credentials and the
/// plex.tv device id live.
pub fn config_dir() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".config/opentessera"))
        .unwrap_or_default()
}

/// The persisted queue. Kept out of settings.conf: it is a few hundred
/// items of churn, and settings.conf is hand-editable.
pub fn queue_path() -> std::path::PathBuf {
    config_dir().join("queue.json")
}

/// Restores the saved queue, or None if there isn't one (or it is
/// unreadable — a corrupt queue file is not worth refusing to start over).
pub fn load_queue() -> Option<(Vec<tessera_core::MediaItem>, usize)> {
    let text = std::fs::read_to_string(queue_path()).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| eprintln!("warning: queue.json unreadable ({e}) — starting empty"))
        .ok()?;
    let items: Vec<tessera_core::MediaItem> =
        serde_json::from_value(v.get("items")?.clone()).ok()?;
    let cursor = v.get("cursor").and_then(serde_json::Value::as_u64).unwrap_or(0) as usize;
    (!items.is_empty()).then_some((items, cursor))
}

/// Writes the queue out. Silent on success; a failure here must never take
/// the wall down, so it only warns.
pub fn save_queue(queue: &PlaybackQueue) {
    let dir = config_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let (items, cursor) = queue.snapshot();
    let body = serde_json::json!({ "cursor": cursor, "items": items });
    match serde_json::to_string(&body) {
        Ok(text) => {
            // Private: every remote item's uri carries its server token.
            if let Err(e) = write_private(&queue_path(), &text) {
                eprintln!("warning: could not save the queue: {e}");
            }
        }
        Err(e) => eprintln!("warning: could not serialise the queue: {e}"),
    }
}

/// The audio and subtitle tracks picked for each item (`tessera_core::tracks`).
pub fn tracks_path() -> std::path::PathBuf {
    config_dir().join("tracks.json")
}

/// Restores the remembered track picks; an unreadable file starts over
/// empty, which costs no more than a few re-picks.
pub fn load_tracks() -> tessera_core::tracks::TrackMemory {
    let Ok(text) = std::fs::read_to_string(tracks_path()) else {
        return Default::default();
    };
    tessera_core::tracks::TrackMemory::from_json(&text).unwrap_or_else(|e| {
        eprintln!("warning: tracks.json unreadable ({e}) — track picks start over");
        Default::default()
    })
}

/// Written on each pick: a click, a few a session, and a pick lost to a
/// crash is exactly the annoyance this file exists to end.
pub fn save_tracks(tracks: &tessera_core::tracks::TrackMemory) {
    if std::fs::create_dir_all(config_dir()).is_err() {
        return;
    }
    // Private: a URL item is keyed by its URL, which can carry a token.
    if let Err(e) = write_private(&tracks_path(), &tracks.to_json()) {
        eprintln!("warning: could not save track picks: {e}");
    }
}

/// Writes a file only its owner can read.
///
/// Both files this is used for carry live server tokens: settings.conf in
/// its `sources = [...]` JSON, and queue.json inside every item's `uri` and
/// `thumb_url` (a Plex/Jellyfin direct-play URL *is* `...?api_key=...`).
/// The default umask leaves them 0644, which on any shared machine hands
/// every local account a working key. Proper secret storage is a bigger
/// job — docs/ROADMAP.md §9 — but the mode costs nothing.
///
/// `mode()` only applies when the file is created, so an existing file
/// written before this change is tightened explicitly.
pub fn write_private(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    file.write_all(text.as_bytes())
}

pub fn load_settings() -> overlay::UiSettings {
    let mut s = overlay::UiSettings::default();
    let path = std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".config/opentessera/settings.conf"))
        .unwrap_or_default();
    if let Ok(text) = std::fs::read_to_string(&path) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                let (k, v) = (k.trim(), v.trim());
                match k {
                    "ui_scale" => {
                        if let Ok(f) = v.parse::<f32>() {
                            s.ui_scale = f.clamp(0.75, 3.0);
                        }
                    }
                    "cache_mb" => {
                        if let Ok(n) = v.parse::<u32>() {
                            s.cache_mb = n.clamp(64, 8192);
                        }
                    }
                    "max_streams" => {
                        if let Ok(n) = v.parse::<u32>() {
                            s.max_streams = n.clamp(1, 128);
                        }
                    }
                    "monitor_scale" => {
                        match serde_json::from_str::<std::collections::BTreeMap<String, f32>>(v) {
                            Ok(map) => {
                                s.monitor_scale =
                                    map.into_iter().map(|(k, v)| (k, v.clamp(0.75, 3.0))).collect()
                            }
                            Err(e) => {
                                eprintln!("warning: bad monitor_scale line in settings.conf: {e}")
                            }
                        }
                    }
                    "audio" => s.audio = v == "true" || v == "yes" || v == "on",
                    "audio_follows" => s.audio_follows = v == "true" || v == "yes" || v == "on",
                    "retain_queue" => s.retain_queue = v == "true" || v == "yes" || v == "on",
                    "hide_after_secs" => {
                        // `is_finite`: "nan" parses, survives clamp, and
                        // Duration::from_secs_f32 panics on it.
                        if let Some(n) = v.parse::<f32>().ok().filter(|n| n.is_finite()) {
                            s.hide_after_secs = n.clamp(0.5, 60.0);
                        }
                    }
                    "fade_secs" => {
                        if let Some(n) = v.parse::<f32>().ok().filter(|n| n.is_finite()) {
                            s.fade_secs = n.clamp(0.0, 10.0);
                        }
                    }
                    "sources" => match serde_json::from_str::<Vec<tessera_core::sources::Source>>(v) {
                        Ok(list) => s.sources = list,
                        Err(e) => eprintln!("warning: bad sources line in settings.conf: {e}"),
                    },
                    _ => {}
                }
            }
        }
    }
    s
}

pub fn save_settings(s: &overlay::UiSettings) {
    let Some(home) = std::env::var_os("HOME") else { return };
    let dir = std::path::PathBuf::from(home).join(".config/opentessera");
    if std::fs::create_dir_all(&dir).is_err() {
        eprintln!("warning: cannot create {}", dir.display());
        return;
    }
    let sources_json = serde_json::to_string(&s.sources).unwrap_or_else(|_| "[]".into());
    let scales_json = serde_json::to_string(&s.monitor_scale).unwrap_or_else(|_| "{}".into());
    let text = format!(
        "# openTessera settings (also editable at runtime: S sources, C config)\n\
         # ui_scale is the fallback; monitor_scale remembers each display by name.\n\
         ui_scale = {}\nmonitor_scale = {}\ncache_mb = {}\nmax_streams = {}\naudio = {}\naudio_follows = {}\nretain_queue = {}\nhide_after_secs = {}\nfade_secs = {}\nsources = {}\n",
        s.ui_scale, scales_json, s.cache_mb, s.max_streams, s.audio, s.audio_follows, s.retain_queue, s.hide_after_secs, s.fade_secs,
        sources_json
    );
    // Private: the sources line holds Plex tokens and Jellyfin API keys.
    let path = dir.join("settings.conf");
    if let Err(e) = write_private(&path, &text) {
        eprintln!("warning: could not save settings: {e}");
        return;
    }
    println!("settings: saved to {}", path.display());
}

pub fn dump_dir() -> Option<std::path::PathBuf> {
    let dir = std::path::PathBuf::from(std::env::var("TESSERA_DUMP_DIR").ok()?);
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}
