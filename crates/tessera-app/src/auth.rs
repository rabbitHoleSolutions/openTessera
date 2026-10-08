//! Server sign-in, off the render thread.
//!
//! Wraps `tessera_core::signin` — every call there blocks (UDP listen, HTTP,
//! and in Plex's case a poll loop that can run for minutes while the user
//! walks to their phone). Requests go to a worker; the settings panel drains
//! events each frame and renders whatever state they leave behind.
//!
//! One flow is live at a time, which is what a single add-source form can
//! express anyway.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use tessera_core::signin::{self, DiscoveredCamera, DiscoveredServer, PlexServer};
use tessera_core::sources::LibraryInfo;

/// How long to listen for Jellyfin discovery replies.
const DISCOVER_WINDOW: Duration = Duration::from_secs(2);
/// How often to ask plex.tv whether the user has entered the code yet.
const PLEX_POLL: Duration = Duration::from_secs(2);
/// How long the poll loop sleeps between cancellation checks. Well under
/// [`PLEX_POLL`], so abandoning a link is felt immediately rather than up
/// to a full poll interval later.
const CANCEL_TICK: Duration = Duration::from_millis(250);

enum Request {
    Discover,
    /// Ubiquiti's LAN probe. Separate from Jellyfin discovery despite the
    /// same shape: different port, different payload, different answer.
    DiscoverCameras,
    JellyfinLogin { url: String, username: String, password: String },
    /// `generation` is the flow's ticket: the worker abandons the poll as
    /// soon as [`Auth::generation`] moves past it.
    PlexLink { client_id: String, generation: u64 },
    /// Which libraries a chosen server has, so the user can pick rather
    /// than type a name they have not been shown.
    Libraries { kind: ServerKind, base: String, token: String },
}

/// Which server API to ask for a library list. Both flows end in the same
/// picker, so this is the only thing that differs between them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerKind {
    Plex,
    Jellyfin,
}

enum Event {
    Discovered(Vec<DiscoveredServer>),
    CamerasFound(Vec<DiscoveredCamera>),
    JellyfinToken { url: String, token: String },
    /// The 4-character code to enter at plex.tv/link.
    PlexCode(String),
    /// Linked, servers listed, each with the first candidate URL that
    /// actually answered (None if none did).
    PlexServers(Vec<(PlexServer, Option<String>)>),
    /// The movie libraries on one server, with the credentials that reached
    /// it — the picker turns each ticked name into its own `Source`.
    Libraries { kind: ServerKind, base: String, token: String, found: Vec<LibraryInfo> },
    Failed(String),
}

/// A server whose libraries have been listed, ready to choose from.
pub struct ServerLibraries {
    pub kind: ServerKind,
    pub base: String,
    pub token: String,
    pub found: Vec<LibraryInfo>,
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
    /// The chosen server's libraries, once listed.
    pub libraries: Option<ServerLibraries>,
    /// Cameras (and other Ubiquiti devices) found on the LAN.
    pub cameras: Vec<DiscoveredCamera>,
    /// A camera scan has completed, so "none found" can be told apart from
    /// "not scanned yet".
    pub cameras_scanned: bool,
}

pub struct Auth {
    pub state: AuthState,
    /// A finished Jellyfin sign-in waiting to be turned into a Source by
    /// the caller, which owns the rest of the form.
    pending_token: Option<(String, String)>,
    tx: mpsc::Sender<Request>,
    rx: mpsc::Receiver<Event>,
    /// Bumped whenever a flow is started or abandoned, so the worker can
    /// tell that the flow it is in the middle of is no longer wanted.
    ///
    /// The plex.tv poll runs until the PIN expires — up to fifteen minutes.
    /// The worker is a single thread reading one channel, so without this
    /// `reset()` only cleared the UI: the thread stayed in the loop and
    /// every later request (a rescan, a second link attempt) sat unserved
    /// behind it, looking like the button had done nothing.
    generation: Arc<AtomicU64>,
}

impl Auth {
    pub fn new() -> Auth {
        let (tx_req, rx_req) = mpsc::channel::<Request>();
        let (tx_ev, rx_ev) = mpsc::channel::<Event>();
        let generation = Arc::new(AtomicU64::new(0));
        let worker_generation = generation.clone();
        std::thread::spawn(move || {
            while let Ok(req) = rx_req.recv() {
                let sent = match req {
                    Request::Discover => match signin::jellyfin_discover(DISCOVER_WINDOW) {
                        Ok(list) => tx_ev.send(Event::Discovered(list)),
                        Err(e) => tx_ev.send(Event::Failed(e.to_string())),
                    },
                    Request::DiscoverCameras => {
                        match signin::ubiquiti_discover(DISCOVER_WINDOW) {
                            Ok(list) => tx_ev.send(Event::CamerasFound(list)),
                            Err(e) => tx_ev.send(Event::Failed(e.to_string())),
                        }
                    }
                    Request::JellyfinLogin { url, username, password } => {
                        match signin::jellyfin_login(&url, &username, &password) {
                            Ok(token) => tx_ev.send(Event::JellyfinToken { url, token }),
                            Err(e) => tx_ev.send(Event::Failed(e.to_string())),
                        }
                    }
                    Request::PlexLink { client_id, generation } => {
                        plex_link(&tx_ev, &client_id, &worker_generation, generation)
                    }
                    Request::Libraries { kind, base, token } => {
                        let listed = match kind {
                            ServerKind::Plex => tessera_core::sources::PlexSource {
                                base: base.clone(),
                                token: token.clone(),
                            }
                            .libraries(),
                            ServerKind::Jellyfin => tessera_core::sources::JellyfinSource {
                                base: base.clone(),
                                api_key: token.clone(),
                            }
                            .libraries(),
                        };
                        match listed {
                            Ok(found) => tx_ev.send(Event::Libraries { kind, base, token, found }),
                            Err(e) => tx_ev.send(Event::Failed(e.to_string())),
                        }
                    }
                };
                if sent.is_err() {
                    break; // the app is gone
                }
            }
        });
        Auth { state: AuthState::default(), pending_token: None, tx: tx_req, rx: rx_ev, generation }
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
                Event::CamerasFound(list) => {
                    self.state.cameras = list;
                    self.state.cameras_scanned = true;
                    self.state.busy = false;
                }
                Event::Libraries { kind, base, token, found } => {
                    self.state.busy = false;
                    self.state.libraries = Some(ServerLibraries { kind, base, token, found });
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
        self.start(|_| Request::Discover);
    }

    /// Scans the LAN for Ubiquiti cameras.
    pub fn discover_cameras(&mut self) {
        self.start(|_| Request::DiscoverCameras);
    }

    pub fn jellyfin_login(&mut self, url: &str, username: &str, password: &str) {
        let (url, username, password) =
            (url.trim().to_owned(), username.trim().to_owned(), password.to_owned());
        self.start(move |_| Request::JellyfinLogin { url, username, password });
    }

    pub fn plex_link(&mut self, client_id: &str) {
        self.state.plex_servers.clear();
        self.state.libraries = None;
        let client_id = client_id.to_owned();
        self.start(move |generation| Request::PlexLink { client_id, generation });
    }

    /// Lists a server's movie libraries for the picker.
    pub fn list_libraries(&mut self, kind: ServerKind, base: &str, token: &str) {
        self.state.libraries = None;
        let (base, token) = (base.trim_end_matches('/').to_owned(), token.to_owned());
        self.start(move |_| Request::Libraries { kind, base, token });
    }

    /// Drops a listed library set without disturbing an in-flight flow —
    /// the picker's "back" action.
    pub fn clear_libraries(&mut self) {
        self.state.libraries = None;
    }

    /// Forgets an in-progress or finished flow (the user changed their mind
    /// or picked a server), and tells the worker to drop whatever it is in
    /// the middle of — see [`Auth::generation`].
    pub fn reset(&mut self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.state = AuthState::default();
        self.pending_token = None;
    }

    /// Starts a flow, retiring any flow already running. `make` receives
    /// the new generation so a long-running request can carry its own
    /// ticket.
    fn start(&mut self, make: impl FnOnce(u64) -> Request) {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        self.state.busy = true;
        self.state.error.clear();
        if self.tx.send(make(generation)).is_err() {
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
fn plex_link(
    tx: &mpsc::Sender<Event>,
    client_id: &str,
    current: &AtomicU64,
    generation: u64,
) -> Result<(), mpsc::SendError<Event>> {
    // True once the user has started another flow or reset this one. Every
    // send is gated on it as well as every sleep: an abandoned flow must
    // not push a stale error or a stale server list into the UI.
    let live = || current.load(Ordering::Relaxed) == generation;

    let pin = match signin::plex_create_pin(client_id) {
        Ok(p) => p,
        Err(e) if live() => return tx.send(Event::Failed(e.to_string())),
        Err(_) => return Ok(()),
    };
    if !live() {
        return Ok(());
    }
    tx.send(Event::PlexCode(pin.code.clone()))?;

    let deadline = std::time::Instant::now() + Duration::from_secs(pin.expires_in);
    let token = loop {
        if !live() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return tx.send(Event::Failed(
                "the plex.tv code expired — try linking again".into(),
            ));
        }
        // Slept in short slices rather than one PLEX_POLL block, so
        // abandoning the flow is noticed within CANCEL_TICK.
        let wake = std::time::Instant::now() + PLEX_POLL;
        while std::time::Instant::now() < wake {
            if !live() {
                return Ok(());
            }
            std::thread::sleep(CANCEL_TICK);
        }
        match signin::plex_check_pin(client_id, pin.id) {
            Ok(Some(t)) => break t,
            Ok(None) => continue,
            Err(e) if live() => return tx.send(Event::Failed(e.to_string())),
            Err(_) => return Ok(()),
        }
    };

    let servers = match signin::plex_servers(client_id, &token) {
        Ok(s) => s,
        Err(e) if live() => return tx.send(Event::Failed(e.to_string())),
        Err(_) => return Ok(()),
    };
    if !live() {
        return Ok(());
    }
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
    if !live() {
        return Ok(());
    }
    tx.send(Event::PlexServers(probed))
}
