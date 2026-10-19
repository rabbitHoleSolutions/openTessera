//! Friendlier ways to add a server than typing a token.
//!
//! Three flows, ported from the Quest app (`JellyfinDiscovery.kt`,
//! `JellyfinMediaSource.authenticate`, `PlexLinkClient.kt`):
//!
//! - **Jellyfin discovery** — broadcast on UDP 7359 and collect the replies.
//! - **Jellyfin sign-in** — username + password, exchanged once for an
//!   access token. The password is never stored: the token it mints is what
//!   ends up in the source, so this is purely an add-time convenience.
//! - **Plex link** — mint a PIN at plex.tv, show the user a short code to
//!   enter at plex.tv/link, poll until it yields an account token, then list
//!   the account's servers with their LAN addresses.
//!
//! Every network call here blocks; callers run them off the render thread.
//! The parsing is split out and unit-tested, which is where the fiddly parts
//! actually live (connection preference order, missing fields, IPv6).
//!
//! NB: all of these still end with a long-lived token written to disk in
//! plaintext. That is a known gap with its own roadmap entry (docs/ROADMAP.md
//! §9) — these flows only mean the user never has to handle the token.

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::sources::SourceError;

// ---------------------------------------------------------------------------
// Jellyfin: LAN discovery
// ---------------------------------------------------------------------------

/// A server that answered the discovery broadcast. `address` is a full base
/// URL straight from the server's own config, e.g. `http://192.168.1.14:8096`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredServer {
    pub name: String,
    pub address: String,
}

/// The shared HTTP client for the sign-in calls. Same reason as
/// [`crate::sources`]': these all run on the single auth worker, and ureq
/// has no default read timeout, so a server that accepts the connection
/// and then stalls would hang the worker — and with it every later
/// sign-in request — indefinitely.
fn agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout_read(Duration::from_secs(15))
            .build()
    })
}

/// Jellyfin's discovery port and probe string. Servers listen by default, so
/// unlike Quick Connect this needs no server-side setup.
pub const JELLYFIN_DISCOVERY_PORT: u16 = 7359;
const JELLYFIN_PROBE: &[u8] = b"Who is JellyfinServer?";
/// A single UDP datagram on Wi-Fi is best-effort, so the probe is repeated
/// across the listening window.
const RESEND: Duration = Duration::from_millis(700);

/// Broadcasts and collects answers for `timeout`. The full window is always
/// waited out — more than one server can answer. Blocking.
pub fn jellyfin_discover(timeout: Duration) -> Result<Vec<DiscoveredServer>, SourceError> {
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| SourceError::Http(e.to_string()))?;
    socket
        .set_broadcast(true)
        .map_err(|e| SourceError::Http(e.to_string()))?;
    socket
        .set_read_timeout(Some(Duration::from_millis(300)))
        .map_err(|e| SourceError::Http(e.to_string()))?;
    let target: SocketAddr = format!("255.255.255.255:{JELLYFIN_DISCOVERY_PORT}")
        .parse()
        .map_err(|_| SourceError::Http("bad broadcast address".into()))?;

    let deadline = Instant::now() + timeout;
    let mut last_send: Option<Instant> = None;
    let mut found: Vec<DiscoveredServer> = Vec::new();
    let mut buf = [0u8; 8 * 1024];
    while Instant::now() < deadline {
        if last_send.is_none_or(|t| t.elapsed() >= RESEND) {
            let _ = socket.send_to(JELLYFIN_PROBE, target);
            last_send = Some(Instant::now());
        }
        let Ok((n, _)) = socket.recv_from(&mut buf) else {
            continue; // read timeout: keep waiting out the window
        };
        let body = String::from_utf8_lossy(&buf[..n]);
        if let Some(srv) = parse_discovery_reply(&body) {
            // First answer for an address wins; a server can reply to every
            // repeat of the probe.
            if !found.iter().any(|f| f.address == srv.address) {
                found.push(srv);
            }
        }
    }
    Ok(found)
}

/// Parses one discovery reply; None for anything malformed.
pub fn parse_discovery_reply(body: &str) -> Option<DiscoveredServer> {
    let v: Value = serde_json::from_str(body).ok()?;
    let address = v.get("Address").and_then(Value::as_str)?.trim().to_owned();
    if address.is_empty() {
        return None;
    }
    let name = v
        .get("Name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&address)
        .to_owned();
    Some(DiscoveredServer { name, address })
}

// ---------------------------------------------------------------------------
// Cameras: Ubiquiti LAN discovery
// ---------------------------------------------------------------------------

/// A camera (or other Ubiquiti device) that answered the discovery probe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredCamera {
    /// The device's own name — "Front", "Driveway" — which is exactly what
    /// the user calls it, so it becomes the item title.
    pub name: String,
    pub address: String,
    /// e.g. "UVC G4 Dome". Shown to tell two cameras apart.
    pub model: String,
}

/// Ubiquiti's device-discovery port. Answers carry the device's name,
/// model and address with **no credentials at all**, which is what makes
/// this worth doing: the tedious half of adding cameras is knowing what is
/// out there and what it is called.
pub const UBIQUITI_DISCOVERY_PORT: u16 = 10001;
/// Both protocol generations; older firmware answers only the first.
const UBIQUITI_PROBES: [&[u8]; 2] = [&[0x01, 0x00, 0x00, 0x00], &[0x02, 0x08, 0x00, 0x00]];

/// Broadcasts and collects answers for `timeout`. Blocking.
///
/// Everything that answers is returned, not just cameras — a controller is
/// a useful thing to see in the list too, since on Protect it is the host
/// the streams actually come from.
pub fn ubiquiti_discover(timeout: Duration) -> Result<Vec<DiscoveredCamera>, SourceError> {
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| SourceError::Http(e.to_string()))?;
    socket.set_broadcast(true).map_err(|e| SourceError::Http(e.to_string()))?;
    socket
        .set_read_timeout(Some(Duration::from_millis(300)))
        .map_err(|e| SourceError::Http(e.to_string()))?;
    let target: SocketAddr = format!("255.255.255.255:{UBIQUITI_DISCOVERY_PORT}")
        .parse()
        .map_err(|_| SourceError::Http("bad broadcast address".into()))?;

    let deadline = Instant::now() + timeout;
    let mut last_send: Option<Instant> = None;
    let mut found: Vec<DiscoveredCamera> = Vec::new();
    let mut buf = [0u8; 8 * 1024];
    while Instant::now() < deadline {
        if last_send.is_none_or(|t| t.elapsed() >= RESEND) {
            for probe in UBIQUITI_PROBES {
                let _ = socket.send_to(probe, target);
            }
            last_send = Some(Instant::now());
        }
        let Ok((n, addr)) = socket.recv_from(&mut buf) else {
            continue; // read timeout: keep waiting out the window
        };
        if let Some(dev) = parse_ubiquiti_reply(&buf[..n], &addr.ip().to_string()) {
            if !found.iter().any(|f| f.address == dev.address) {
                found.push(dev);
            }
        }
    }
    found.sort_by_key(|a| a.name.to_lowercase());
    Ok(found)
}

/// Parses one discovery reply.
///
/// The payload is a 4-byte header then type/length/value records, with
/// big-endian lengths: 0x0b is the device name and 0x0c the model. Anything
/// unrecognised is skipped rather than fatal — firmware versions differ in
/// which records they send.
///
/// **The address comes from the packet, not the payload.** Records of type
/// 0x02 carry a MAC and an IPv4 address, one per interface, and a gateway
/// duly reports its WAN address among them — which showed up in testing as
/// a public IP listed alongside the cameras as though it were another
/// device. A device is reachable at the address it answered from, so that
/// is the only one worth having, and using it also collapses a
/// multi-interface device's several replies into one entry.
pub fn parse_ubiquiti_reply(body: &[u8], from: &str) -> Option<DiscoveredCamera> {
    let mut name = String::new();
    let mut model = String::new();
    let address = from.to_owned();
    let mut i = 4usize;
    while i + 3 <= body.len() {
        let kind = body[i];
        let len = u16::from_be_bytes([body[i + 1], body[i + 2]]) as usize;
        let start = i + 3;
        let end = start.checked_add(len)?;
        if end > body.len() {
            break; // truncated record: keep whatever was already read
        }
        let value = &body[start..end];
        match kind {
            0x0b => name = String::from_utf8_lossy(value).trim_end_matches('\0').to_owned(),
            0x0c => model = String::from_utf8_lossy(value).trim_end_matches('\0').to_owned(),
            _ => {}
        }
        i = end;
    }
    if name.is_empty() {
        name = address.clone();
    }
    Some(DiscoveredCamera { name, address, model })
}

// ---------------------------------------------------------------------------
// Jellyfin: username + password
// ---------------------------------------------------------------------------

/// Exchanges a username and password for an access token, which is what the
/// source stores. The password is not kept.
pub fn jellyfin_login(base: &str, username: &str, password: &str) -> Result<String, SourceError> {
    let base = base.trim_end_matches('/');
    let body = serde_json::json!({ "Username": username, "Pw": password });
    let resp = agent()
        .post(&format!("{base}/Users/AuthenticateByName"))
        .set("Accept", "application/json")
        .set("X-Emby-Authorization", JELLYFIN_AUTH_HEADER)
        .send_json(body)
        .map_err(|e| SourceError::Http(friendly_http(e)))?;
    let v: Value = resp
        .into_json()
        .map_err(|e| SourceError::Json(e.to_string()))?;
    parse_login_token(&v)
}

/// Jellyfin wants a client identity on the auth request; without it the
/// server rejects the login outright.
const JELLYFIN_AUTH_HEADER: &str =
    "MediaBrowser Client=\"openTessera\", Device=\"desktop\", DeviceId=\"opentessera\", Version=\"1.0\"";

/// Pulls the access token out of an AuthenticateByName response.
pub fn parse_login_token(v: &Value) -> Result<String, SourceError> {
    match v.get("AccessToken").and_then(Value::as_str) {
        Some(t) if !t.trim().is_empty() => Ok(t.to_owned()),
        _ => Err(SourceError::Http(
            "signed in, but the server returned no access token".into(),
        )),
    }
}

// ---------------------------------------------------------------------------
// Plex: plex.tv device link
// ---------------------------------------------------------------------------

const PLEX_TV: &str = "https://plex.tv/api/v2";

/// One PIN from plex.tv: show `code` to the user, poll by `id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlexPin {
    pub id: i64,
    pub code: String,
    pub expires_in: u64,
}

/// A server on the linked account. `candidates` are base URLs in preference
/// order: plain `http://ip:port` LAN connections first (no DNS, no TLS, and
/// they keep working if the LAN loses internet), then the `plex.direct`
/// HTTPS URIs for servers that demand secure connections, then remote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlexServer {
    pub name: String,
    pub owned: bool,
    /// Per-server token — shared servers get their own; owned servers fall
    /// back to the account token.
    pub token: String,
    pub candidates: Vec<String>,
}

/// Asks plex.tv for a linking PIN. Blocking.
pub fn plex_create_pin(client_id: &str) -> Result<PlexPin, SourceError> {
    let resp = plex_req(agent().post(&format!("{PLEX_TV}/pins")), client_id)
        .send_form(&[("strong", "false")])
        .map_err(|e| SourceError::Http(friendly_http(e)))?;
    let v: Value = resp
        .into_json()
        .map_err(|e| SourceError::Json(e.to_string()))?;
    parse_pin(&v)
}

/// The account token once the user has entered the code, else None. Blocking.
pub fn plex_check_pin(client_id: &str, pin_id: i64) -> Result<Option<String>, SourceError> {
    let resp = plex_req(agent().get(&format!("{PLEX_TV}/pins/{pin_id}")), client_id)
        .call()
        .map_err(|e| SourceError::Http(friendly_http(e)))?;
    let v: Value = resp
        .into_json()
        .map_err(|e| SourceError::Json(e.to_string()))?;
    Ok(parse_pin_token(&v))
}

/// The account's servers, owned first. Blocking.
pub fn plex_servers(client_id: &str, account_token: &str) -> Result<Vec<PlexServer>, SourceError> {
    let resp = plex_req(
        agent().get(&format!("{PLEX_TV}/resources?includeHttps=1&includeRelay=0")),
        client_id,
    )
    .set("X-Plex-Token", account_token)
    .call()
    .map_err(|e| SourceError::Http(friendly_http(e)))?;
    let v: Value = resp
        .into_json()
        .map_err(|e| SourceError::Json(e.to_string()))?;
    Ok(parse_servers(&v, account_token))
}

/// The first of a server's candidate URLs that answers an authenticated
/// `/library/sections`, or None. Short timeouts: a stale remote address, or
/// a cert-required server refusing plain http, should cost seconds.
pub fn plex_first_reachable(server: &PlexServer) -> Option<String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(4))
        .timeout_read(Duration::from_secs(8))
        .build();
    server
        .candidates
        .iter()
        .find(|base| {
            agent
                .get(&format!("{base}/library/sections"))
                .set("Accept", "application/json")
                .set("X-Plex-Token", &server.token)
                .call()
                .is_ok()
        })
        .cloned()
}

fn plex_req(req: ureq::Request, client_id: &str) -> ureq::Request {
    req.set("Accept", "application/json")
        .set("X-Plex-Product", "openTessera")
        .set("X-Plex-Client-Identifier", client_id)
        .set("X-Plex-Device-Name", "desktop")
}

pub fn parse_pin(v: &Value) -> Result<PlexPin, SourceError> {
    let id = v
        .get("id")
        .and_then(Value::as_i64)
        .ok_or_else(|| SourceError::Json("plex.tv pin response had no id".into()))?;
    let code = v
        .get("code")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .ok_or_else(|| SourceError::Json("plex.tv pin response had no code".into()))?
        .to_owned();
    let expires_in = v.get("expiresIn").and_then(Value::as_u64).unwrap_or(900);
    Ok(PlexPin { id, code, expires_in })
}

pub fn parse_pin_token(v: &Value) -> Option<String> {
    v.get("authToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
}

/// Turns plex.tv's `/resources` list into servers with usable base URLs.
/// Relay connections are dropped — they proxy through plex.tv and can never
/// direct-play, which is the whole point of this app.
pub fn parse_servers(v: &Value, account_token: &str) -> Vec<PlexServer> {
    let mut out: Vec<PlexServer> = v
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|dev| {
            let provides = dev.get("provides").and_then(Value::as_str).unwrap_or("");
            if !provides.split(',').any(|p| p.trim() == "server") {
                return None;
            }
            let name = dev.get("name").and_then(Value::as_str)?.to_owned();
            let conns: Vec<&Value> = dev
                .get("connections")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .filter(|c| c.get("relay").and_then(Value::as_bool) != Some(true))
                .collect();
            fn is_local(c: &Value) -> bool {
                c.get("local").and_then(Value::as_bool) == Some(true)
            }

            let mut candidates: Vec<String> = Vec::new();
            let mut push = |u: String| {
                if !u.is_empty() && !candidates.contains(&u) {
                    candidates.push(u);
                }
            };
            // LAN address:port first...
            for c in conns.iter().filter(|c| is_local(c)) {
                if let (Some(addr), Some(port)) = (
                    c.get("address").and_then(Value::as_str),
                    c.get("port").and_then(Value::as_u64),
                ) {
                    let host = if addr.contains(':') {
                        format!("[{addr}]") // bracket IPv6
                    } else {
                        addr.to_owned()
                    };
                    push(format!("http://{host}:{port}"));
                }
            }
            // ...then the LAN plex.direct https URIs...
            for c in conns.iter().filter(|c| is_local(c)) {
                if let Some(uri) = c.get("uri").and_then(Value::as_str) {
                    push(uri.to_owned());
                }
            }
            // ...then anything remote.
            for c in conns.iter().filter(|c| !is_local(c)) {
                if let Some(uri) = c.get("uri").and_then(Value::as_str) {
                    push(uri.to_owned());
                }
            }
            if candidates.is_empty() {
                return None;
            }
            Some(PlexServer {
                name,
                owned: dev.get("owned").and_then(Value::as_bool) == Some(true),
                token: dev
                    .get("accessToken")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                    .unwrap_or(account_token)
                    .to_owned(),
                candidates,
            })
        })
        .collect();
    out.sort_by_key(|s| !s.owned); // owned first, order otherwise preserved
    out
}

/// A stable per-install identifier. plex.tv keys the PIN and the account's
/// device list on it, so it must not change between creating and checking a
/// PIN — nor across runs, or every launch registers a new "device".
pub fn plex_client_id(config_dir: &std::path::Path) -> String {
    let path = config_dir.join("plex-client-id");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    let id = pseudo_uuid();
    let _ = std::fs::create_dir_all(config_dir);
    let _ = std::fs::write(&path, &id);
    id
}

/// A UUID-shaped random identifier. plex.tv only needs it to be stable and
/// unique to this install, so the clock and the address of a heap allocation
/// are enough entropy — not worth a uuid dependency.
fn pseudo_uuid() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let boxed = Box::new(0u8);
    let addr = &*boxed as *const u8 as usize;
    let a = (nanos as u64) ^ (addr as u64).rotate_left(17);
    let b = (nanos >> 64) as u64 ^ (addr as u64).rotate_left(43) ^ 0x9e37_79b9_7f4a_7c15;
    format!("{:016x}-{:016x}", a, b)
}

/// ureq's errors stringify to something unhelpfully long; keep the status
/// and the server's own message, which is usually the useful part.
fn friendly_http(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            let body = body.trim();
            let hint = match code {
                401 | 403 => " — wrong username or password?",
                _ => "",
            };
            if body.is_empty() {
                format!("HTTP {code}{hint}")
            } else {
                format!("HTTP {code}{hint}: {}", truncate(body, 160))
            }
        }
        other => other.to_string(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    s.chars().take(max).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn discovery_reply_falls_back_to_the_address_for_a_name() {
        let r = parse_discovery_reply(r#"{"Address":"http://10.0.0.5:8096","Name":"Attic"}"#).unwrap();
        assert_eq!(r.name, "Attic");
        assert_eq!(r.address, "http://10.0.0.5:8096");

        // A server with no (or blank) name is still perfectly usable.
        let r = parse_discovery_reply(r#"{"Address":"http://10.0.0.6:8096","Name":"  "}"#).unwrap();
        assert_eq!(r.name, "http://10.0.0.6:8096");
    }

    #[test]
    fn discovery_reply_rejects_junk() {
        assert!(parse_discovery_reply("not json").is_none());
        assert!(parse_discovery_reply("{}").is_none());
        assert!(parse_discovery_reply(r#"{"Address":""}"#).is_none());
    }

    #[test]
    fn login_token_is_required_to_be_non_empty() {
        assert_eq!(parse_login_token(&json!({"AccessToken":"abc"})).unwrap(), "abc");
        assert!(parse_login_token(&json!({"AccessToken":""})).is_err());
        assert!(parse_login_token(&json!({"User":{"Id":"x"}})).is_err());
    }

    #[test]
    fn pin_parsing_needs_an_id_and_a_code() {
        let p = parse_pin(&json!({"id": 42, "code": "AB12", "expiresIn": 600})).unwrap();
        assert_eq!(p, PlexPin { id: 42, code: "AB12".into(), expires_in: 600 });
        // expiresIn is optional and defaults.
        assert_eq!(parse_pin(&json!({"id": 1, "code": "ZZZZ"})).unwrap().expires_in, 900);
        assert!(parse_pin(&json!({"code": "AB12"})).is_err());
        assert!(parse_pin(&json!({"id": 42})).is_err());
    }

    #[test]
    fn pin_token_is_null_until_the_user_enters_the_code() {
        assert!(parse_pin_token(&json!({"authToken": null})).is_none());
        assert!(parse_pin_token(&json!({"authToken": ""})).is_none());
        assert_eq!(parse_pin_token(&json!({"authToken": "tok"})).unwrap(), "tok");
    }

    #[test]
    fn servers_prefer_lan_addresses_then_lan_uris_then_remote() {
        let v = json!([{
            "provides": "server",
            "name": "Basement",
            "owned": true,
            "connections": [
                {"local": false, "uri": "https://remote.plex.direct:32400"},
                {"local": true,  "address": "192.168.1.9", "port": 32400,
                 "uri": "https://192-168-1-9.abc.plex.direct:32400"},
            ],
        }]);
        let s = parse_servers(&v, "acct");
        assert_eq!(s.len(), 1);
        assert_eq!(
            s[0].candidates,
            [
                "http://192.168.1.9:32400",
                "https://192-168-1-9.abc.plex.direct:32400",
                "https://remote.plex.direct:32400",
            ]
        );
        // No per-server accessToken: falls back to the account token.
        assert_eq!(s[0].token, "acct");
    }

    #[test]
    fn servers_drop_relays_and_non_servers_and_bracket_ipv6() {
        let v = json!([
            {"provides": "player", "name": "A phone", "connections": []},
            {"provides": "server", "name": "Relayed", "connections": [
                {"local": false, "relay": true, "uri": "https://relay.plex.direct"}]},
            {"provides": "server,player", "name": "Six", "accessToken": "own",
             "connections": [{"local": true, "address": "fd00::1", "port": 32400}]},
        ]);
        let s = parse_servers(&v, "acct");
        // The player is skipped, and the relay-only server has no usable
        // candidate left so it drops out entirely.
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].name, "Six");
        assert_eq!(s[0].candidates, ["http://[fd00::1]:32400"]);
        assert_eq!(s[0].token, "own");
    }

    #[test]
    fn owned_servers_sort_first() {
        let v = json!([
            {"provides": "server", "name": "Shared", "owned": false,
             "connections": [{"local": true, "address": "10.0.0.2", "port": 32400}]},
            {"provides": "server", "name": "Mine", "owned": true,
             "connections": [{"local": true, "address": "10.0.0.3", "port": 32400}]},
        ]);
        let s = parse_servers(&v, "acct");
        assert_eq!(s.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), ["Mine", "Shared"]);
    }

    #[test]
    fn client_id_is_stable_across_calls() {
        let dir = std::env::temp_dir().join(format!("tessera-signin-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let a = plex_client_id(&dir);
        let b = plex_client_id(&dir);
        assert_eq!(a, b, "plex.tv keys the PIN on this — it must not change");
        assert!(!a.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod camera_tests {
    use super::*;

    /// Builds a reply in the wire format: 4-byte header then TLV records.
    fn tlv(records: &[(u8, Vec<u8>)]) -> Vec<u8> {
        let mut b = vec![0x01, 0x00, 0x00, 0x00];
        for (t, v) in records {
            b.push(*t);
            b.extend_from_slice(&(v.len() as u16).to_be_bytes());
            b.extend_from_slice(v);
        }
        b
    }

    #[test]
    fn a_real_camera_reply_yields_its_name_model_and_address() {
        // Shaped after the actual answers from a UVC G4 Dome on a live LAN.
        let body = tlv(&[
            (0x02, vec![0x78, 0x45, 0x58, 0xc5, 0x2a, 0xc1, 192, 168, 1, 115]),
            (0x0b, b"Front".to_vec()),
            (0x0c, b"UVC G4 Dome".to_vec()),
            (0x03, b"UVC.S5L.v5.4.132".to_vec()),
        ]);
        let d = parse_ubiquiti_reply(&body, "192.168.1.115").unwrap();
        assert_eq!(d.name, "Front");
        assert_eq!(d.model, "UVC G4 Dome");
        assert_eq!(d.address, "192.168.1.115");
    }

    #[test]
    fn a_reply_with_no_name_falls_back_to_the_address() {
        // A controller answered with only a hostname and no model; other
        // firmware sends neither, and the address is always usable.
        let body = tlv(&[(0x02, vec![0, 0, 0, 0, 0, 0, 192, 168, 1, 1])]);
        let d = parse_ubiquiti_reply(&body, "192.168.1.1").unwrap();
        assert_eq!(d.name, "192.168.1.1");
        assert!(d.model.is_empty());
    }

    #[test]
    fn a_gateways_wan_address_is_not_mistaken_for_another_device() {
        // Observed on a real UDM Pro: it reports every interface, WAN
        // included, so trusting the payload listed a public IP among the
        // cameras. The address it answered from is the one that is real.
        let body = tlv(&[
            (0x02, vec![0, 0, 0, 0, 0, 0, 192, 168, 1, 1]),
            (0x02, vec![0, 0, 0, 0, 0, 0, 162, 156, 176, 174]),
            (0x0b, b"UCTRL-7845".to_vec()),
        ]);
        let d = parse_ubiquiti_reply(&body, "192.168.1.1").unwrap();
        assert_eq!(d.address, "192.168.1.1", "must not adopt the WAN address");
        assert_eq!(d.name, "UCTRL-7845");
    }

    #[test]
    fn a_truncated_record_keeps_what_was_already_read_instead_of_panicking() {
        let mut body = tlv(&[(0x0b, b"Driveway".to_vec())]);
        body.extend_from_slice(&[0x0c, 0xff, 0xff]); // claims 65535 bytes, sends none
        let d = parse_ubiquiti_reply(&body, "10.0.0.9").unwrap();
        assert_eq!(d.name, "Driveway");
        assert_eq!(d.address, "10.0.0.9");
        // And pure junk must not panic either.
        assert!(parse_ubiquiti_reply(&[], "10.0.0.9").is_some());
        assert!(parse_ubiquiti_reply(&[0xff; 3], "10.0.0.9").is_some());
    }
}
