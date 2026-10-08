//! Thumbnail cache: obtains thumbnails on a background thread; completed
//! images are uploaded as egui textures (per-screen — texture handles
//! belong to one GL context) when polled during the egui pass.
//!
//! Two ways to get one, both off the render thread and both landing in the
//! same in-memory cache: a remote poster is fetched over HTTP, and a local
//! file has a frame extracted from it and cached on disk by fingerprint
//! (see [`crate::poster`]). Local items used to have neither and always
//! drew a placeholder.

use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::sync::mpsc;

use egui::{ColorImage, Context, TextureHandle, TextureOptions};
use tessera_core::MediaItem;

enum State {
    Requested,
    /// Nothing could be obtained. `at` so a live source can be tried again
    /// later — a camera that was unreachable an hour ago may be back.
    Failed { at: std::time::Instant },
    /// `at` is when this image was obtained, which is what makes a live
    /// thumbnail expire; `refreshing` stops a stale one asking again every
    /// frame while its replacement is in flight.
    Ready { tex: TextureHandle, at: std::time::Instant, refreshing: bool },
}

/// Where one thumbnail is to come from.
enum Want {
    /// A poster URL on a Plex/Jellyfin server.
    Url(String),
    /// A frame out of a file or a live stream, cached on disk under this
    /// fingerprint.
    Frame { source: String, live: bool, fingerprint: String },
}

struct FetchMsg {
    id: String,
    want: Want,
}

struct DoneMsg {
    id: String,
    img: Option<ColorImage>,
}

/// How many entries a cache keeps before the oldest are dropped.
///
/// A `State::Ready` holds a live GPU texture, and nothing used to evict:
/// scrolling a few thousand Plex posters uploaded one texture each and kept
/// every one of them, per screen, for the life of the process. The bound is
/// comfortably above what any view can show at once (the library lists at
/// most `MAX_ROWS`, the hover strip a dozen), so eviction never touches
/// something on screen and cannot thrash.
const MAX_ENTRIES: usize = 512;

pub struct ThumbCache {
    states: HashMap<String, State>,
    /// Insertion order, for the eviction above.
    order: VecDeque<String>,
    tx: mpsc::Sender<FetchMsg>,
    rx: mpsc::Receiver<DoneMsg>,
}

impl ThumbCache {
    pub fn new() -> ThumbCache {
        let (tx_req, rx_req) = mpsc::channel::<FetchMsg>();
        let (tx_done, rx_done) = mpsc::channel::<DoneMsg>();
        std::thread::spawn(move || {
            let agent = ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(10))
                .build();
            // Its own handle on libmpv, rather than threading the main
            // one through Screen::new and create_screen: this is a dlopen
            // of a library already in the process, so it costs a refcount.
            // If it fails, remote posters still work and local items keep
            // their placeholder.
            let mpv = tessera_mpv::MpvLib::load()
                .map_err(|e| eprintln!("thumbs: no libmpv for frame extraction ({e})"))
                .ok();
            while let Ok(msg) = rx_req.recv() {
                let img = match msg.want {
                    Want::Url(url) => fetch(&agent, &url),
                    Want::Frame { source, live, fingerprint } => {
                        // The disk cache is consulted here rather than on
                        // the render thread: a hit is a file read, which is
                        // still I/O.
                        let max_age = live.then_some(crate::poster::LIVE_TTL);
                        let bytes = crate::poster::cached(&fingerprint, max_age)
                            .or_else(|| {
                                let lib = mpv.as_ref()?;
                                crate::poster::generate(lib, &source, live, &fingerprint)
                            })
                            // A camera that has gone offline should keep
                            // showing the picture it had, stale, rather
                            // than falling back to a grey placeholder: the
                            // old frame still says what the camera watches.
                            .or_else(|| crate::poster::cached(&fingerprint, None));
                        bytes.as_deref().and_then(decode)
                    }
                };
                if tx_done.send(DoneMsg { id: msg.id, img }).is_err() {
                    break;
                }
            }
        });
        ThumbCache { states: HashMap::new(), order: VecDeque::new(), tx: tx_req, rx: rx_done }
    }

    /// Drains completed fetches and uploads them as textures.
    fn poll(&mut self, ctx: &Context) {
        while let Ok(done) = self.rx.try_recv() {
            let now = std::time::Instant::now();
            match done.img {
                Some(img) => {
                    let tex = ctx.load_texture(format!("thumb-{}", done.id), img, TextureOptions::default());
                    self.remember(done.id, State::Ready { tex, at: now, refreshing: false });
                }
                None => {
                    // Keep a picture we already have rather than replacing
                    // it with a failure: a refresh that could not reach the
                    // camera has lost nothing.
                    let keep = matches!(self.states.get(&done.id), Some(State::Ready { .. }));
                    if keep {
                        if let Some(State::Ready { refreshing, .. }) = self.states.get_mut(&done.id) {
                            *refreshing = false;
                        }
                    } else {
                        self.remember(done.id, State::Failed { at: now });
                    }
                }
            }
        }
    }

    /// Records a state, evicting the oldest entries past [`MAX_ENTRIES`].
    /// Dropping a `Ready` drops its `TextureHandle`, which is what actually
    /// releases the GPU memory.
    fn remember(&mut self, id: String, state: State) {
        // Only a genuinely new key extends the queue — a Requested entry
        // turning into Ready must not be queued for eviction twice.
        if self.states.insert(id.clone(), state).is_none() {
            self.order.push_back(id);
        }
        while self.order.len() > MAX_ENTRIES {
            if let Some(oldest) = self.order.pop_front() {
                self.states.remove(&oldest);
            }
        }
    }

    /// The texture for an item if it is ready (a cloned handle — cheap).
    ///
    /// Requests one on first sight and returns None until it arrives, so a
    /// caller draws a placeholder for a frame or two rather than blocking.
    /// An item with neither a poster URL nor a local file fails immediately
    /// and is not retried every frame.
    ///
    /// Deliberately takes the whole item: the fingerprint needs its size
    /// and mtime, which are already on it, so a lookup costs no I/O.
    pub fn get(&mut self, ctx: &Context, item: &MediaItem) -> Option<TextureHandle> {
        self.poll(ctx);
        let stale = crate::poster::max_age(item);
        // A live thumbnail that has aged out is refreshed *while still being
        // shown*: the request goes out and the previous frame is returned,
        // so an hourly refresh never blinks a camera back to a placeholder.
        // Opportunistic by construction — this only runs because something
        // asked to draw the thumbnail.
        let mut refresh = false;
        match self.states.get_mut(&item.id) {
            Some(State::Ready { tex, at, refreshing }) => {
                let tex = tex.clone();
                if let Some(max) = stale {
                    if !*refreshing && at.elapsed() > max {
                        *refreshing = true;
                        refresh = true;
                    }
                }
                if refresh {
                    self.request(item);
                }
                return Some(tex);
            }
            Some(State::Failed { at }) => {
                // Only live sources are worth retrying; a file with no video
                // will not grow one.
                if stale.is_some_and(|max| at.elapsed() > max) {
                    self.states.remove(&item.id);
                } else {
                    return None;
                }
            }
            Some(State::Requested) => return None,
            None => {}
        }
        self.request(item);
        None
    }

    /// Dispatches a thumbnail request, or records that there is nothing to
    /// ask for.
    fn request(&mut self, item: &MediaItem) {
        let want = match item.thumb_url.as_deref() {
            Some(url) => Some(Want::Url(url.to_owned())),
            None if crate::poster::extractable(item) => Some(Want::Frame {
                source: item.uri.clone(),
                live: item.is_live(),
                fingerprint: crate::poster::fingerprint(item),
            }),
            None => None,
        };
        match want {
            Some(want) => {
                let _ = self.tx.send(FetchMsg { id: item.id.clone(), want });
                // A refresh keeps its Ready entry (and its picture); a first
                // request has none to keep.
                if !matches!(self.states.get(&item.id), Some(State::Ready { .. })) {
                    self.remember(item.id.clone(), State::Requested);
                }
            }
            None => self.remember(item.id.clone(), State::Failed { at: std::time::Instant::now() }),
        }
    }
}

impl Default for ThumbCache {
    fn default() -> ThumbCache {
        ThumbCache::new()
    }
}

fn fetch(agent: &ureq::Agent, url: &str) -> Option<ColorImage> {
    let resp = agent.get(url).call().ok()?;
    let mut bytes = Vec::new();
    resp.into_reader()
        .take(4 << 20)
        .read_to_end(&mut bytes)
        .ok()?;
    decode(&bytes)
}

fn decode(bytes: &[u8]) -> Option<ColorImage> {
    let img = image::load_from_memory(bytes).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some(ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba))
}
