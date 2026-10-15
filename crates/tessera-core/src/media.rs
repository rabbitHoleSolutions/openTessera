use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    Local,
    Smb,
    Jellyfin,
    Plex,
    /// A live camera feed. Everything about it is an ordinary item —
    /// queued, tiled, pinned, dragged between screens — except that it
    /// never ends, which is what [`SourceKind::is_live`] exists to say.
    Camera,
    /// A recorded stream opened by URL (http/https) — seekable, but there
    /// is no file to delete. Live protocols open as `Camera` instead.
    Url,
}

impl SourceKind {
    pub fn label(&self) -> &'static str {
        match self {
            SourceKind::Local => "local",
            SourceKind::Smb => "smb",
            SourceKind::Jellyfin => "jellyfin",
            SourceKind::Plex => "plex",
            SourceKind::Camera => "camera",
            SourceKind::Url => "url",
        }
    }

    /// A stream with no end and no seeking.
    ///
    /// This is the distinction that actually matters to playback: buffering
    /// a live feed is just latency, end-of-file never arrives so
    /// auto-advance and looping are meaningless, and the scrub bar has
    /// nothing to scrub. Kept on the kind rather than guessed from the URI
    /// scheme so a future source cannot forget to declare it.
    pub fn is_live(&self) -> bool {
        matches!(self, SourceKind::Camera)
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

    /// An item for a URL handed over directly (the MCP `tessera_open`).
    /// None for anything that isn't a scheme mpv streams: `file://` and
    /// friends go through `from_path`, which knows what is on disk.
    pub fn from_url(url: &str) -> Option<MediaItem> {
        let (scheme, rest) = url.split_once("://")?;
        let kind = match scheme.to_ascii_lowercase().as_str() {
            "http" | "https" => SourceKind::Url,
            "rtsp" | "rtsps" | "rtmp" | "rtmps" | "srt" | "udp" | "rtp" => SourceKind::Camera,
            _ => return None,
        };
        let rest = rest.split(['?', '#']).next().unwrap_or(rest);
        // The file name if there is one (extension dropped), else the host.
        let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
        let title = match path.rsplit('/').find(|s| !s.is_empty()) {
            Some(file) => file.rsplit_once('.').filter(|(stem, _)| !stem.is_empty()).map_or(file, |(stem, _)| stem),
            None => host,
        }
        .to_string();
        Some(MediaItem {
            id: format!("url|{url}"),
            title,
            uri: url.to_string(),
            kind,
            width: None,
            height: None,
            fps: None,
            codec: None,
            size: None,
            modified: None,
            thumb_url: None,
            source: None,
        })
    }

    /// True for an item that plays forever (see [`SourceKind::is_live`]).
    pub fn is_live(&self) -> bool {
        self.kind.is_live()
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
            | "m2v" | "vob" | "3gp"
    )
}

/// The video files under `paths`. A file is taken as given; a folder is
/// walked **recursively** — a library is `Movies/`, `Shows/<show>/<season>/`,
/// and a folder source that only read its top level found nothing in one.
///
/// Skipped: hidden entries (`.DS_Store`, and macOS `._name.mkv` AppleDouble
/// files, which carry a video extension and are not videos), NAS trash and
/// thumbnail folders (`#recycle`, `@eaDir`), and anything past `MAX_DEPTH`.
/// Symlinks are followed, with each real directory visited once, so a link
/// back up the tree is not an infinite walk. Within a folder, files come
/// before subfolders, each sorted by name.
pub fn collect_items(paths: &[std::path::PathBuf]) -> Vec<MediaItem> {
    let mut items = Vec::new();
    let mut visited = std::collections::HashSet::new();
    for p in paths {
        // A stream URL given where a path was expected (the command line,
        // a hand-off from a later launch) is one item, not a missing file.
        if let Some(item) = p.to_str().and_then(MediaItem::from_url) {
            items.push(item);
        } else if p.is_dir() {
            walk(p, 0, &mut visited, &mut items);
        } else if p.is_file() {
            items.push(MediaItem::from_path(p));
        } else {
            eprintln!("skipping {}: not found", p.display());
        }
    }
    items
}

/// Deeper than any real library; a guard against pathological trees.
const MAX_DEPTH: usize = 16;

fn walk(
    dir: &Path,
    depth: usize,
    visited: &mut std::collections::HashSet<std::path::PathBuf>,
    items: &mut Vec<MediaItem>,
) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(real) = std::fs::canonicalize(dir) else { return };
    if !visited.insert(real) {
        return;
    }
    let mut entries: Vec<std::path::PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| !skipped_name(n)))
            .collect(),
        Err(err) => {
            eprintln!("cannot read dir {}: {err}", dir.display());
            return;
        }
    };
    entries.sort();
    let (dirs, files): (Vec<_>, Vec<_>) = entries.into_iter().partition(|p| p.is_dir());
    for f in files {
        if f.is_file() && is_video_file(&f) {
            items.push(MediaItem::from_path(&f));
        }
    }
    for d in dirs {
        walk(&d, depth + 1, visited, items);
    }
}

fn skipped_name(name: &str) -> bool {
    name.starts_with('.')
        || name.starts_with('#')
        || name.starts_with('@')
        || name.eq_ignore_ascii_case("lost+found")
        || name.eq_ignore_ascii_case("$RECYCLE.BIN")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_are_walked_recursively_skipping_junk() {
        let root = std::env::temp_dir().join(format!("tessera-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mk = |rel: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"x").unwrap();
        };
        mk("top.mkv");
        mk("notes.txt");
        mk("Shows/Show/Season 1/S01E01.mkv");
        mk("Shows/Show/Season 1/._S01E01.mkv"); // AppleDouble junk
        mk("Movies/Heat (1995).avi");
        mk("Movies/clip.M2V");
        mk("#recycle/deleted.mp4");
        mk("@eaDir/thumb.mp4");
        mk(".hidden/secret.mp4");
        // A link back up the tree must not loop forever.
        std::os::unix::fs::symlink(&root, root.join("Movies/loop")).unwrap();

        let titles: Vec<String> = collect_items(std::slice::from_ref(&root)).into_iter().map(|m| m.title).collect();
        // Files before subfolders, each by name; the linked-back root is
        // already visited, so nothing appears twice.
        assert_eq!(titles, ["top", "Heat (1995)", "clip", "S01E01"]);
        // A file is taken as given.
        assert_eq!(collect_items(&[root.join("notes.txt")]).len(), 1);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn urls_become_items_by_scheme() {
        let m = MediaItem::from_url("https://example.org/films/Heat.1995.mkv?token=x").unwrap();
        assert_eq!((m.kind, m.title.as_str()), (SourceKind::Url, "Heat.1995"));
        assert!(!m.is_live());
        let m = MediaItem::from_url("rtsp://10.0.0.5:554/stream1").unwrap();
        assert_eq!((m.kind, m.title.as_str()), (SourceKind::Camera, "stream1"));
        assert!(m.is_live());
        // Bare host: still has a name.
        assert_eq!(MediaItem::from_url("http://cam.local").unwrap().title, "cam.local");
        // Not a streaming scheme, or not a URL at all.
        assert!(MediaItem::from_url("file:///etc/passwd").is_none());
        assert!(MediaItem::from_url("/home/x/a.mkv").is_none());
    }

    #[test]
    fn a_url_among_paths_is_an_item_not_a_missing_file() {
        let items = collect_items(&[std::path::PathBuf::from("rtsps://nvr.local:7441/abc")]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, SourceKind::Camera);
    }
}
