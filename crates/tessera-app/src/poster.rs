//! Thumbnails for anything without a poster: files and live cameras,
//! extracted once and cached on disk by fingerprint.
//!
//! Plex and Jellyfin hand over a poster URL; a file on disk has nothing, so
//! until now every local item drew a placeholder. A frame out of the file
//! itself is the obvious substitute, and libmpv — already a hard dependency
//! — will produce one with no display, no GL context and no external tool:
//! `vo=image` writes a frame to a directory and `frames=1` stops after the
//! first. Measured at 0.37s for a 77 GB 4K HEVC remux on cold storage, so
//! one worker keeps up comfortably with what a scrolling list asks for.
//!
//! **Where the cache lives.** `$XDG_CACHE_HOME/opentessera/thumbs`, one
//! JPEG per fingerprint. Deliberately *not* alongside the user's media —
//! nobody wants this app strewing dotfiles through their library — and
//! deliberately not in the config directory either, because this is
//! regenerable data and config is not. Deleting the whole directory is a
//! supported thing to do; it costs a re-extract and nothing else.
//!
//! **The fingerprint** is a hash of path, size and mtime. Content hashing a
//! 77 GB file to decide whether to draw a 9 KB image is absurd, and those
//! three are already carried on every local [`MediaItem`] — so a cache
//! *lookup* costs no I/O at all, which is what makes it usable while
//! scrolling. The trade is that moving or re-encoding a file re-extracts;
//! for a cache that is the right way round.

use std::path::{Path, PathBuf};

use tessera_core::{MediaItem, SourceKind};

/// Longest a single extraction may take before it is abandoned. A file on
/// a network mount that has gone away must not wedge the worker for every
/// item behind it.
const DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// Width thumbnails are scaled to. The library row draws at 64pt and the
/// queue strip at 84pt, so this is generous for both while staying small
/// enough that a few thousand of them are megabytes, not gigabytes.
const WIDTH: u32 = 320;

/// Where in the file to sample. Far enough in to be past logos and black
/// leader, not so far as to be a spoiler.
const SEEK: &str = "25%";

/// How often a live source's thumbnail is refreshed.
///
/// A camera's picture is only worth anything if it is roughly current, but
/// this is a thumbnail, not a feed: hourly costs one short connection per
/// camera per hour and keeps the browser honest about what the camera can
/// see. Refreshes are opportunistic — driven by something actually asking
/// for the thumbnail, never by a timer of their own — so cameras nobody is
/// looking at cost nothing.
pub const LIVE_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

/// True when a frame can be extracted from this item: a file on disk, or a
/// live stream, or a server item with no poster of its own.
pub fn extractable(item: &MediaItem) -> bool {
    // A server item normally has a poster and never gets here; one without
    // (a version mismatched into someone else's Plex entry) is a file over
    // HTTP, which mpv reads as readily as a local one.
    item.kind == SourceKind::Local
        || item.is_live()
        || (matches!(item.kind, SourceKind::Plex | SourceKind::Jellyfin) && item.thumb_url.is_none())
}

/// How long a cached thumbnail stays good. Live sources age out; a file's
/// frame is as valid as the fingerprint that named it, which is to say
/// until the file itself changes.
pub fn max_age(item: &MediaItem) -> Option<std::time::Duration> {
    item.is_live().then_some(LIVE_TTL)
}

/// Cache key for a local item: path, size and mtime.
///
/// FNV-1a — this only has to tell one file apart from another on one
/// machine, and it has to be cheap enough to run while scrolling.
pub fn fingerprint(item: &MediaItem) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for b in bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(0x1000_0000_01b3);
        }
    };
    eat(item.uri.as_bytes());
    eat(&item.size.unwrap_or(0).to_le_bytes());
    eat(&item.modified.unwrap_or(0).to_le_bytes());
    format!("{h:016x}")
}

/// `$XDG_CACHE_HOME/opentessera/thumbs`, else `~/.cache/opentessera/thumbs`.
pub fn cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("opentessera/thumbs"))
}

fn cached_path(fingerprint: &str) -> Option<PathBuf> {
    Some(cache_dir()?.join(format!("{fingerprint}.jpg")))
}

/// The cached JPEG for this fingerprint, if one has already been made and
/// is no older than `max_age` (unbounded when None).
pub fn cached(fingerprint: &str, max_age: Option<std::time::Duration>) -> Option<Vec<u8>> {
    let path = cached_path(fingerprint)?;
    if let Some(max) = max_age {
        let age = std::fs::metadata(&path).ok()?.modified().ok()?.elapsed().ok()?;
        if age > max {
            return None;
        }
    }
    let bytes = std::fs::read(path).ok()?;
    (!bytes.is_empty()).then_some(bytes)
}

/// Extracts a frame from `source` and stores it under `fingerprint`,
/// returning the JPEG bytes. `None` when the file has no video, cannot be
/// read, or takes longer than [`DEADLINE`].
///
/// Runs on the thumbnail worker: it creates its own mpv instance, which
/// never gets a render context and so is free of the GL-thread rule that
/// governs playback tiles.
pub fn generate(
    lib: &std::sync::Arc<tessera_mpv::MpvLib>,
    source: &str,
    live: bool,
    fingerprint: &str,
) -> Option<Vec<u8>> {
    let dest = cached_path(fingerprint)?;
    let dir = dest.parent()?.to_path_buf();
    std::fs::create_dir_all(&dir).ok()?;

    // mpv names its output by frame number inside an output *directory*, so
    // each extraction gets its own and the result is renamed into place.
    // The rename is what makes a cache hit mean "complete": a killed
    // process leaves a stray temp directory, never a truncated JPEG that
    // would be served forever after.
    let scratch = dir.join(format!(
        "tmp-{}-{}",
        std::process::id(),
        fingerprint
    ));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).ok()?;

    let out = extract(lib, source, live, &scratch);
    let result = out.and_then(|frame| {
        std::fs::rename(&frame, &dest).ok()?;
        std::fs::read(&dest).ok()
    });
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

/// Runs mpv headlessly and returns the frame it wrote, if any.
fn extract(
    lib: &std::sync::Arc<tessera_mpv::MpvLib>,
    source: &str,
    live: bool,
    outdir: &Path,
) -> Option<PathBuf> {
    let outdir_s = outdir.to_str()?;
    let scale = format!("scale={WIDTH}:-2");
    let mut opts: Vec<(&str, &str)> = vec![
        ("vo", "image"),
        ("vo-image-format", "jpg"),
        ("vo-image-jpeg-quality", "82"),
        ("vo-image-outdir", outdir_s),
        // One frame, then mpv reports end-of-file and we are done.
        ("frames", "1"),
        ("vf", &scale),
        // No audio pipeline, no hardware decoder: this runs on a worker
        // thread alongside a wall of real players, and must not compete
        // with them for either.
        ("audio", "no"),
        ("hwdec", "no"),
        ("terminal", "no"),
        ("msg-level", "all=no"),
    ];
    if live {
        // No `start`: there is nothing to seek into, and asking would
        // either be ignored or wait forever. The first frame is the only
        // frame that means anything on a live feed anyway.
        opts.push(("profile", "low-latency"));
        opts.push(("rtsp-transport", "tcp"));
        opts.push(("network-timeout", "10"));
    } else {
        opts.push(("start", SEEK));
    }
    let mpv = tessera_mpv::Mpv::new(lib, &opts).ok()?;
    mpv.load_file(source, "replace").ok()?;

    let deadline = std::time::Instant::now() + DEADLINE;
    loop {
        if std::time::Instant::now() >= deadline {
            return None;
        }
        match mpv.poll_event() {
            Some(tessera_mpv::MpvEvent::EndFile { .. }) | Some(tessera_mpv::MpvEvent::Shutdown) => {
                break
            }
            Some(_) => continue,
            None => std::thread::sleep(std::time::Duration::from_millis(10)),
        }
    }
    // Whatever it wrote — the name is mpv's to choose.
    std::fs::read_dir(outdir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("jpg")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn item(uri: &str, size: Option<u64>, modified: Option<u64>) -> MediaItem {
        let mut m = MediaItem::from_path(Path::new(uri));
        m.size = size;
        m.modified = modified;
        m
    }

    #[test]
    fn fingerprint_changes_with_each_input_and_only_those() {
        let base = item("/media/a.mkv", Some(100), Some(5));
        let same = item("/media/a.mkv", Some(100), Some(5));
        assert_eq!(fingerprint(&base), fingerprint(&same), "stable across calls");

        // Any of the three moving is a different file as far as the cache
        // is concerned — re-encoding in place is the case that matters.
        assert_ne!(fingerprint(&base), fingerprint(&item("/media/b.mkv", Some(100), Some(5))));
        assert_ne!(fingerprint(&base), fingerprint(&item("/media/a.mkv", Some(101), Some(5))));
        assert_ne!(fingerprint(&base), fingerprint(&item("/media/a.mkv", Some(100), Some(6))));

        // Missing metadata must not panic or collapse everything together.
        assert_ne!(
            fingerprint(&item("/media/a.mkv", None, None)),
            fingerprint(&item("/media/b.mkv", None, None))
        );
    }

    #[test]
    fn the_cache_never_lands_next_to_the_media() {
        // The whole point of a central cache: nothing is written into the
        // directory the video lives in.
        let dir = cache_dir().expect("HOME is set in tests");
        assert!(dir.ends_with("opentessera/thumbs"), "got {}", dir.display());
        let p = cached_path("deadbeef").unwrap();
        assert!(p.starts_with(&dir));
        assert!(p.to_string_lossy().ends_with("deadbeef.jpg"));
    }

    #[test]
    fn a_missing_or_empty_cache_entry_is_a_miss() {
        assert!(cached("definitely-not-a-real-fingerprint-0000", None).is_none());
    }

    #[test]
    fn a_cache_entry_older_than_its_ttl_is_a_miss() {
        // What makes a camera thumbnail refresh: the bytes are on disk and
        // perfectly readable, but too old to still be telling the truth.
        let dir = cache_dir().unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let fp = format!("ttl-test-{}", std::process::id());
        let path = dir.join(format!("{fp}.jpg"));
        std::fs::write(&path, b"pretend jpeg").unwrap();

        assert!(cached(&fp, None).is_some(), "no age limit: always a hit");
        assert!(cached(&fp, Some(LIVE_TTL)).is_some(), "just written: fresh");
        assert!(
            cached(&fp, Some(std::time::Duration::ZERO)).is_none(),
            "nothing is newer than zero seconds old"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn files_live_streams_and_posterless_server_items_are_candidates() {
        assert!(extractable(&item("/media/a.mkv", None, None)));
        let mut cam = item("/x", None, None);
        cam.kind = SourceKind::Camera;
        assert!(extractable(&cam), "a camera frame is extractable too");
        let mut remote = item("/x", None, None);
        remote.kind = SourceKind::Plex;
        remote.thumb_url = Some("http://x/thumb".into());
        assert!(!extractable(&remote), "a server's poster is used instead");
        // A version mismatched into another film's Plex entry has its
        // poster taken away, and is a file like any other.
        remote.thumb_url = None;
        assert!(extractable(&remote));
    }

    #[test]
    fn only_live_thumbnails_go_stale() {
        // A file's frame is good until the file changes, which the
        // fingerprint already captures; a camera's is good for an hour.
        assert_eq!(max_age(&item("/media/a.mkv", Some(1), Some(2))), None);
        let mut cam = item("/x", None, None);
        cam.kind = SourceKind::Camera;
        assert_eq!(max_age(&cam), Some(LIVE_TTL));
    }

    #[test]
    fn a_camera_fingerprint_is_stable_and_keyed_on_its_url() {
        // No size or mtime to hash, so the URL carries it — and the entry
        // is overwritten in place on refresh rather than orphaned hourly.
        let mut a = item("rtsps://host:7441/alias", None, None);
        a.kind = SourceKind::Camera;
        let b = a.clone();
        assert_eq!(fingerprint(&a), fingerprint(&b));
        let mut other = a.clone();
        other.uri = "rtsps://host:7441/other".into();
        assert_ne!(fingerprint(&a), fingerprint(&other));
    }
}
