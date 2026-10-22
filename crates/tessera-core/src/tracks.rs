//! Per-item memory of the audio and subtitle tracks the user picked.
//!
//! mpv plays a server's file directly, so whatever Plex or Jellyfin
//! remember about a viewer's track choices never reaches it — every open
//! starts from the file's own default flags. Local and SMB files have no
//! server to remember anything at all. So the choice is kept here, keyed by
//! `MediaItem::id`, and put back whenever that item loads again.
//!
//! A track is remembered by what it *is* (language, title, codec) as well as
//! by mpv's id for it. The id is what normally matches — same file, same
//! streams — but an external subtitle file dropped next to a film later
//! shifts the numbering, and then the description is what finds it again.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// How many items are remembered before the least recently set are
/// dropped. Far more films than anyone re-watches; it only bounds the file.
pub const MAX_ITEMS: usize = 2000;

/// Which of an item's track lists a pick is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Audio,
    Subtitle,
}

/// What the user asked for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Choice {
    Off,
    Track {
        id: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lang: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codec: Option<String>,
    },
}

/// One audio or subtitle track as mpv lists it, for matching against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Track {
    pub id: i64,
    pub lang: Option<String>,
    pub title: Option<String>,
    pub codec: Option<String>,
}

/// Which track a remembered choice means among `tracks`: `None`
/// when there is nothing to do (the track it named is gone, so the file's
/// default stands), `Some(None)` for off, `Some(Some(id))` for a track.
pub fn pick(choice: &Choice, tracks: &[Track]) -> Option<Option<i64>> {
    let Choice::Track { id, lang, title, codec } = choice else {
        return Some(None);
    };
    let named = |t: &&Track| t.lang == *lang && t.title == *title;
    let same = |t: &&Track| named(t) && t.codec == *codec;
    tracks
        .iter()
        .find(|t| t.id == *id && same(t))
        .or_else(|| tracks.iter().find(same))
        // The codec can differ between a server's remux and the original;
        // the language and title are what the user chose by.
        .or_else(|| tracks.iter().find(named))
        .map(|t| Some(t.id))
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Entry {
    /// Either may be unset: a pick of one says nothing about the other,
    /// whose file default should stand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    audio: Option<Choice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sub: Option<Choice>,
    /// Unix seconds when it was last set, for eviction.
    at: u64,
}

impl Entry {
    fn slot(&mut self, kind: Kind) -> &mut Option<Choice> {
        match kind {
            Kind::Audio => &mut self.audio,
            Kind::Subtitle => &mut self.sub,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct TrackMemory {
    entries: HashMap<String, Entry>,
}

impl TrackMemory {
    pub fn recall(&self, key: &str, kind: Kind) -> Option<&Choice> {
        let e = self.entries.get(key)?;
        match kind {
            Kind::Audio => e.audio.as_ref(),
            Kind::Subtitle => e.sub.as_ref(),
        }
    }

    /// Records a choice. `now` is Unix seconds, passed in so this stays
    /// testable.
    pub fn remember(&mut self, key: &str, kind: Kind, choice: Choice, now: u64) {
        let e = self.entries.entry(key.to_owned()).or_default();
        *e.slot(kind) = Some(choice);
        e.at = now;
        if self.entries.len() > MAX_ITEMS {
            let mut by_age: Vec<(u64, String)> =
                self.entries.iter().map(|(k, e)| (e.at, k.clone())).collect();
            by_age.sort();
            for (_, k) in by_age.into_iter().take(self.entries.len() - MAX_ITEMS) {
                self.entries.remove(&k);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(&self.entries).unwrap_or_else(|_| "{}".into())
    }

    /// Parses what [`to_json`](Self::to_json) wrote. Entries that don't
    /// parse are an error rather than skipped: the caller starts over with
    /// an empty memory, which loses nothing worse than a few picks.
    pub fn from_json(text: &str) -> Result<TrackMemory, String> {
        serde_json::from_str(text).map(|entries| TrackMemory { entries }).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: i64, lang: &str, title: Option<&str>, codec: &str) -> Track {
        Track { id, lang: Some(lang.into()), title: title.map(Into::into), codec: Some(codec.into()) }
    }

    fn chose(t: &Track) -> Choice {
        Choice::Track { id: t.id, lang: t.lang.clone(), title: t.title.clone(), codec: t.codec.clone() }
    }

    #[test]
    fn off_is_off_whatever_the_file_has() {
        assert_eq!(pick(&Choice::Off, &[track(1, "eng", None, "subrip")]), Some(None));
        assert_eq!(pick(&Choice::Off, &[]), Some(None));
    }

    #[test]
    fn a_track_is_found_by_id_then_by_what_it_is() {
        let eng = track(1, "eng", None, "subrip");
        let sdh = track(2, "eng", Some("SDH"), "subrip");
        let fre = track(3, "fre", None, "ass");
        let tracks = [eng.clone(), sdh.clone(), fre.clone()];
        assert_eq!(pick(&chose(&sdh), &tracks), Some(Some(2)));
        // An external file added later pushed every embedded track up one.
        let shifted: Vec<Track> =
            tracks.iter().map(|t| Track { id: t.id + 1, ..t.clone() }).collect();
        assert_eq!(pick(&chose(&sdh), &shifted), Some(Some(3)));
        // Same language and title, different codec: still the one meant.
        let remuxed = [track(1, "fre", None, "subrip")];
        assert_eq!(pick(&chose(&fre), &remuxed), Some(Some(1)));
        // Gone entirely: leave the file's default alone.
        assert_eq!(pick(&chose(&fre), &[eng]), None);
    }

    #[test]
    fn the_id_alone_does_not_match_a_different_track() {
        // Id 2 now belongs to something else; English SDH is at 5.
        let tracks = [track(2, "ger", None, "subrip"), track(5, "eng", Some("SDH"), "subrip")];
        let sdh = track(2, "eng", Some("SDH"), "subrip");
        assert_eq!(pick(&chose(&sdh), &tracks), Some(Some(5)));
    }

    #[test]
    fn memory_round_trips_and_forgets_the_oldest() {
        let mut m = TrackMemory::default();
        m.remember("local|/a.mkv", Kind::Subtitle, Choice::Off, 10);
        m.remember("plex|7", Kind::Subtitle, chose(&track(4, "eng", Some("Forced"), "pgs")), 20);
        m.remember("plex|7", Kind::Audio, chose(&track(2, "jpn", None, "ac3")), 21);
        let back = TrackMemory::from_json(&m.to_json()).unwrap();
        assert_eq!(back.recall("local|/a.mkv", Kind::Subtitle), Some(&Choice::Off));
        // Picking subtitles says nothing about audio.
        assert_eq!(back.recall("local|/a.mkv", Kind::Audio), None);
        assert_eq!(back.recall("plex|7", Kind::Subtitle), m.recall("plex|7", Kind::Subtitle));
        assert_eq!(back.recall("plex|7", Kind::Audio), m.recall("plex|7", Kind::Audio));
        assert!(back.recall("plex|7", Kind::Audio).is_some());
        assert_eq!(back.recall("plex|8", Kind::Subtitle), None);
        // A file written before audio was remembered still reads.
        let old = r#"{"local|/b.mkv":{"sub":{"kind":"off"},"at":5}}"#;
        let old = TrackMemory::from_json(old).unwrap();
        assert_eq!(old.recall("local|/b.mkv", Kind::Subtitle), Some(&Choice::Off));
        assert_eq!(old.recall("local|/b.mkv", Kind::Audio), None);

        let mut full = TrackMemory::default();
        for n in 0..MAX_ITEMS as u64 + 3 {
            full.remember(&format!("local|/{n}.mkv"), Kind::Subtitle, Choice::Off, n);
        }
        assert_eq!(full.len(), MAX_ITEMS);
        assert_eq!(full.recall("local|/0.mkv", Kind::Subtitle), None);
        assert_eq!(full.recall("local|/2.mkv", Kind::Subtitle), None);
        assert!(full.recall("local|/3.mkv", Kind::Subtitle).is_some());
    }
}
