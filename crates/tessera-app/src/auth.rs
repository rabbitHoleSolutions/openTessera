//! Server sign-in, off the render thread.
//!
//! Wraps `tessera_core::signin` — every call there blocks (UDP listen, HTTP,
//! and in Plex's case a poll loop that can run for minutes while the user
//! walks to their phone). Requests go to a worker; the settings panel drains
//! events each frame and renders whatever state they leave behind.
//!
//! One flow is live at a time, which is what a single add-source form can
//! express anyway.

use std::sync::mpsc;
use std::time::Duration;

use tessera_core::signin::{self, DiscoveredServer, PlexServer};

/// How long to listen for Jellyfin discovery replies.
const DISCOVER_WINDOW: Duration = Duration::from_secs(2);
/// How often to ask plex.tv whether the user has entered the code yet.
const PLEX_POLL: Duration = Duration::from_secs(2);

enum Request {
    Discover,
    JellyfinLogin { url: String, username: String, password: String },
    PlexLink { client_id: String },
}

enum Event {
    Discovered(Vec<DiscoveredServer>),
    JellyfinToken { url: String, token: String },
    /// The 4-character code to enter at plex.tv/link.
    PlexCode(String),
    /// Linked, servers listed, each with the first candidate URL that
    /// actually answered (None if none did).
    PlexServers(Vec<(PlexServer, Option<String>)>),
    Failed(String),
}

/// A Plex server offered to the user after linking.
pub struct PlexChoice {
    pub name: String,
    pub owned: bool,
    pub token: String,
    /// The candidate that answered, if any. When None the server was found
    /// but nothing reachable — offer it anyway with its first candidate,
    /// since a firewall at add time doesn't mean it's wrong forever.
    pub reachable: Option<String>,
    pub fallback: String,
}

/// What the sign-in UI is currently showing. Drained from the worker each
/// frame; the settings panel reads it and never blocks.
#[derive(Default)]
pub struct AuthState {
    /// A request is in flight.
    pub busy: bool,
    /// Last failure, shown until the next request starts.
    pub error: String,
    /// Jellyfin servers found on the LAN.
    pub discovered: Vec<DiscoveredServer>,
    /// True once a scan has completed, so "none found" can be distinguished
    /// from "not scanned yet".
    pub scanned: bool,
    /// The plex.tv code, while waiting for the user to enter it.
    pub plex_code: Option<String>,
    /// Servers to pick from once the link succeeds.
    pub plex_servers: Vec<PlexChoice>,
}

pub struct Auth {
    pub state: AuthState,
    /// A finished Jellyfin sign-in waiting to be turned into a Source by
    /// the caller, which owns the rest of the form.
    pending_token: Option<(String, String)>,
    tx: mpsc::Sender<Request>,
    rx: mpsc::Receiver<Event>,
}

impl Auth {
    pub fn new() -> Auth {
        let (tx_req, rx_req) = mpsc::channel::<Request>();
        let (tx_ev, rx_ev) = mpsc::channel::<Event>();
        std::thread::spawn(move || {
            while let Ok(req) = rx_req.recv() {
                let sent = match req {
                    Request::Discover => match signin::jellyfin_discover(DISCOVER_WINDOW) {
                        Ok(list) => tx_ev.send(Event::Discovered(list)),
                        Err(e) => tx_ev.send(Event::Failed(e.to_string())),
                    },
                    Request::JellyfinLogin { url, username, password } => {
                        match signin::jellyfin_login(&url, &username, &password) {
                            Ok(token) => tx_ev.send(Event::JellyfinToken { url, token }),
                            Err(e) => tx_ev.send(Event::Failed(e.to_string())),
                        }
                    }
                    Request::PlexLink { client_id } => plex_link(&tx_ev, &client_id),
                };
                if sent.is_err() {
                    break; // the app is gone
                }
            }
        });
        Auth { state: AuthState::default(), pending_token: None, tx: tx_req, rx: rx_ev }
    }

    /// Applies anything the worker finished. Call once per frame before
    /// reading `state`.
    pub fn poll(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Event::Discovered(list) => {
                    self.state.discovered = list;
                    self.state.scanned = true;
                    self.state.busy = false;
                }
                Event::JellyfinToken { url, token } => {
                    self.state.busy = false;
                    self.pending_token = Some((url, token));
                }
                Event::PlexCode(code) => {
                    // Still busy: the worker is now polling for the user.
                    self.state.plex_code = Some(code);
                }
                Event::PlexServers(list) => {
                    self.state.busy = false;
                    self.state.plex_code = None;
                    self.state.plex_servers = list
                        .into_iter()
                        .map(|(s, reachable)| PlexChoice {
                            fallback: s.candidates.first().cloned().unwrap_or_default(),
                            name: s.name,
                            owned: s.owned,
                            token: s.token,
                            reachable,
                        })
                        .collect();
                }
                Event::Failed(e) => {
                    self.state.busy = false;
                    self.state.plex_code = None;
                    self.state.error = e;
                }
            }
        }
    }

    /// A completed Jellyfin sign-in, taken once. The caller turns it into a
    /// `Source` (it owns the library filter the form collected).
    pub fn take_jellyfin_token(&mut self) -> Option<(String, String)> {
        self.pending_token.take()
    }

    pub fn discover(&mut self) {
        self.start(Request::Discover);
    }

    pub fn jellyfin_login(&mut self, url: &str, username: &str, password: &str) {
        self.start(Request::JellyfinLogin {
            url: url.trim().to_owned(),
            username: username.trim().to_owned(),
            password: password.to_owned(),
        });
    }

    pub fn plex_link(&mut self, client_id: &str) {
        self.state.plex_servers.clear();
        self.start(Request::PlexLink { client_id: client_id.to_owned() });
    }

    /// Forgets an in-progress or finished flow (the user changed their mind
    /// or picked a server). The worker may still be polling plex.tv; its
    /// result is simply ignored.
    pub fn reset(&mut self) {
        self.state = AuthState::default();
        self.pending_token = None;
    }

    fn start(&mut self, req: Request) {
        self.state.busy = true;
        self.state.error.clear();
        if self.tx.send(req).is_err() {
            self.state.busy = false;
            self.state.error = "sign-in worker stopped".into();
        }
    }
}

impl Default for Auth {
    fn default() -> Auth {
        Auth::new()
    }
}

/// The whole plex.tv link: mint a PIN, report the code immediately so the
/// user can start typing it, then poll until it is claimed or expires, then
/// list the account's servers and probe which address actually answers.
fn plex_link(tx: &mpsc::Sender<Event>, client_id: &str) -> Result<(), mpsc::SendError<Event>> {
    let pin = match signin::plex_create_pin(client_id) {
        Ok(p) => p,
        Err(e) => return tx.send(Event::Failed(e.to_string())),
    };
    tx.send(Event::PlexCode(pin.code.clone()))?;

    let deadline = std::time::Instant::now() + Duration::from_secs(pin.expires_in);
    let token = loop {
        if std::time::Instant::now() >= deadline {
            return tx.send(Event::Failed(
                "the plex.tv code expired — try linking again".into(),
            ));
        }
        std::thread::sleep(PLEX_POLL);
        match signin::plex_check_pin(client_id, pin.id) {
            Ok(Some(t)) => break t,
            Ok(None) => continue,
            Err(e) => return tx.send(Event::Failed(e.to_string())),
        }
    };

    let servers = match signin::plex_servers(client_id, &token) {
        Ok(s) => s,
        Err(e) => return tx.send(Event::Failed(e.to_string())),
    };
    if servers.is_empty() {
        return tx.send(Event::Failed("that Plex account has no servers".into()));
    }
    let probed = servers
        .into_iter()
        .map(|s| {
            let reachable = signin::plex_first_reachable(&s);
            (s, reachable)
        })
        .collect();
    tx.send(Event::PlexServers(probed))
}
