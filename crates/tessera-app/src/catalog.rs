//! On-demand source listing for the library browser.
//!
//! `Source::list()` blocks — an HTTP round trip for Plex/Jellyfin, a
//! recursive walk for local folders — so it can never run on the render
//! thread. Requests go out to a worker and results come back through a
//! channel that the browser drains each frame, exactly like [`ThumbCache`]
//! does for posters.
//!
//! Startup enumeration is separate and stays where it is (main.rs, before
//! the event loop): the wall is meant to start playing immediately. This
//! catalog is only what the browser reads.
//!
//! [`ThumbCache`]: crate::thumbs::ThumbCache

use std::collections::HashMap;
use std::sync::mpsc;

use tessera_core::sources::Source;
use tessera_core::MediaItem;

/// Where one source's listing has got to.
pub enum Listing {
    Loading,
    Ready(Vec<MediaItem>),
    Failed(String),
}

struct Request {
    key: String,
    source: Source,
}

struct Response {
    key: String,
    result: Result<Vec<MediaItem>, String>,
}

/// Per-source listings, fetched in the background and cached until asked to
/// refresh. Keyed by the source's label, which is stable and unique enough
/// for a hand-maintained source list.
pub struct SourceCatalog {
    listings: HashMap<String, Listing>,
    /// Bumped whenever any listing is replaced or dropped. The library
    /// tab's row cache keys on it, which is what lets that cache survive
    /// across frames without ever going stale against a re-listing.
    generation: u64,
    tx: mpsc::Sender<Request>,
    rx: mpsc::Receiver<Response>,
}

impl SourceCatalog {
    pub fn new() -> SourceCatalog {
        let (tx_req, rx_req) = mpsc::channel::<Request>();
        let (tx_done, rx_done) = mpsc::channel::<Response>();
        // One worker: listings are chunky and mostly network-bound, and
        // serializing them keeps a wall of sources from opening a dozen
        // simultaneous connections to the same server.
        std::thread::spawn(move || {
            while let Ok(req) = rx_req.recv() {
                let result = req.source.list().map_err(|e| e.to_string());
                if tx_done.send(Response { key: req.key, result }).is_err() {
                    break;
                }
            }
        });
        SourceCatalog { listings: HashMap::new(), generation: 0, tx: tx_req, rx: rx_done }
    }

    /// Takes delivery of anything the worker finished. Call once per frame
    /// before reading listings.
    pub fn poll(&mut self) {
        while let Ok(done) = self.rx.try_recv() {
            let state = match done.result {
                Ok(items) => Listing::Ready(items),
                Err(e) => Listing::Failed(e),
            };
            self.listings.insert(done.key, state);
            self.generation += 1;
        }
    }

    /// Changes whenever any listing does. Anything caching a derived view
    /// of a listing can hold this and rebuild only when it moves.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The listing for a source, requesting it the first time it is asked
    /// for. Returns `Loading` until the worker answers.
    pub fn get(&mut self, source: &Source) -> &Listing {
        let key = source.label();
        if !self.listings.contains_key(&key) {
            self.listings.insert(key.clone(), Listing::Loading);
            let _ = self.tx.send(Request { key: key.clone(), source: source.clone() });
        }
        // Key is present either way by this point.
        self.listings.get(&key).unwrap_or(&Listing::Loading)
    }

    /// Drops a cached listing so the next `get` re-fetches it.
    pub fn refresh(&mut self, source: &Source) {
        self.listings.remove(&source.label());
        self.generation += 1;
    }
}

impl Default for SourceCatalog {
    fn default() -> SourceCatalog {
        SourceCatalog::new()
    }
}
