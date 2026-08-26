//! Thumbnail cache: fetches and decodes remote thumbnails on a background
//! thread; completed images are uploaded as egui textures (per-screen —
//! texture handles belong to one GL context) when polled during the egui
//! pass. Local items have no URL and render as placeholders.

use std::collections::HashMap;
use std::io::Read;
use std::sync::mpsc;

use egui::{ColorImage, Context, TextureHandle, TextureOptions};

enum State {
    Requested,
    Failed,
    Ready(TextureHandle),
}

struct FetchMsg {
    id: String,
    url: String,
}

struct DoneMsg {
    id: String,
    img: Option<ColorImage>,
}

pub struct ThumbCache {
    states: HashMap<String, State>,
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
            while let Ok(msg) = rx_req.recv() {
                let img = fetch(&agent, &msg.url);
                if tx_done.send(DoneMsg { id: msg.id, img }).is_err() {
                    break;
                }
            }
        });
        ThumbCache { states: HashMap::new(), tx: tx_req, rx: rx_done }
    }

    /// Drains completed fetches and uploads them as textures.
    fn poll(&mut self, ctx: &Context) {
        while let Ok(done) = self.rx.try_recv() {
            match done.img {
                Some(img) => {
                    let tex = ctx.load_texture(format!("thumb-{}", done.id), img, TextureOptions::default());
                    self.states.insert(done.id, State::Ready(tex));
                }
                None => {
                    self.states.insert(done.id, State::Failed);
                }
            }
        }
    }

    /// Returns the texture for an item if it's ready (cloned handle — cheap).
    /// Requests the URL on first sight; None-url items fail immediately so
    /// callers can draw a placeholder without retrying every frame.
    pub fn get(&mut self, ctx: &Context, id: &str, url: Option<&str>) -> Option<TextureHandle> {
        self.poll(ctx);
        if let Some(State::Ready(t)) = self.states.get(id) {
            return Some(t.clone());
        }
        if !self.states.contains_key(id) {
            let state = match url {
                Some(url) => {
                    let _ = self.tx.send(FetchMsg { id: id.to_owned(), url: url.to_owned() });
                    State::Requested
                }
                None => State::Failed,
            };
            self.states.insert(id.to_owned(), state);
        }
        None
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
    let img = image::load_from_memory(&bytes).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some(ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba))
}
