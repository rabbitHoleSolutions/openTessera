use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    Local,
    Smb,
    Jellyfin,
    Plex,
}

impl SourceKind {
    pub fn label(&self) -> &'static str {
        match self {
            SourceKind::Local => "local",
            SourceKind::Smb => "smb",
            SourceKind::Jellyfin => "jellyfin",
            SourceKind::Plex => "plex",
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct MediaItem {
    pub id: String,
    pub title: String,
    pub uri: String,
    pub kind: SourceKind,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    pub codec: Option<String>,
    pub size: Option<u64>,
    pub modified: Option<u64>,
    /// Remote thumbnail URL (Plex/Jellyfin image endpoint), if any.
    pub thumb_url: Option<String>,
    /// Which configured source produced this item ([`crate::sources::Source::label`]).
    /// Deleting routes through it: an item knows nothing about credentials,
    /// and without this the app has to guess which server owns the file —
    /// which is wrong the moment two servers are configured.
    #[serde(default)]
    pub source: Option<String>,
}

impl MediaItem {
    pub fn from_path(path: &Path) -> MediaItem {
        let uri = path.to_string_lossy().into_owned();
        let title = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| uri.clone());
        let (size, modified) = std::fs::metadata(path)
            .map(|m| (Some(m.len()), m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs())))
            .unwrap_or((None, None));
        MediaItem {
            id: format!("local|{uri}"),
            title,
            uri,
            kind: SourceKind::Local,
            width: None,
            height: None,
            fps: None,
            codec: None,
            size,
            modified,
            thumb_url: None,
            source: None,
        }
    }

    pub fn aspect(&self) -> Option<f32> {
        match (self.width, self.height) {
            (Some(w), Some(h)) if h > 0 => Some(w as f32 / h as f32),
            _ => None,
        }
    }
}

pub fn is_video_file(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "mp4" | "mkv" | "webm" | "avi" | "mov" | "ts" | "m2ts" | "mts" | "flv" | "wmv" | "mpg" | "mpeg" | "m4v" | "ogv"
    )
}

pub fn collect_items(paths: &[std::path::PathBuf]) -> Vec<MediaItem> {
    let mut items = Vec::new();
    for p in paths {
        if p.is_dir() {
            let mut entries: Vec<std::path::PathBuf> = match std::fs::read_dir(p) {
                Ok(rd) => rd.filter_map(|e| e.ok()).map(|e| e.path()).collect(),
                Err(err) => {
                    eprintln!("cannot read dir {}: {err}", p.display());
                    continue;
                }
            };
            entries.sort();
            for e in entries {
                if e.is_file() && is_video_file(&e) {
                    items.push(MediaItem::from_path(&e));
                }
            }
        } else if p.is_file() {
            items.push(MediaItem::from_path(p));
        } else {
            eprintln!("skipping {}: not found", p.display());
        }
    }
    items
}
