//! Single-instance ownership, and the hand-off from later launches.
//!
//! Two of the app's central invariants are *process*-scoped, not
//! window-scoped: there is one [`PlaybackQueue`] and one [`DecodeBudget`]
//! per process. A second launch therefore does not merely duplicate a
//! window — it quietly gets its own of each, so admission control stops
//! meaning anything (two processes each admitting a full ceiling's worth of
//! streams against one GPU), and both processes write settings.conf and
//! queue.json wholesale from their own copy, silently dropping whatever the
//! other added. That last one is not theoretical: a stale instance erased a
//! configured source when an unrelated setting was toggled.
//!
//! So a second launch does not become a second app. It hands its paths to
//! the running one, asks it to come to the front, and exits.
//!
//! **Mechanism.** A Unix socket, *connect-first*. Connecting proves an
//! instance is alive and listening; failing to connect means the socket is
//! stale or absent, so this process takes it over. There is deliberately no
//! lock file: a lock can be inherited from a process that died holding it,
//! whereas a dead instance simply refuses connections, which is exactly the
//! signal wanted.
//!
//! **The socket path is keyed on the config directory**, so isolating HOME
//! isolates the instance with it. A sandboxed run (its own HOME, its own
//! settings.conf) then coexists with the real app rather than fighting it
//! for the socket — which is also how this module gets tested.
//!
//! [`PlaybackQueue`]: tessera_core::PlaybackQueue
//! [`DecodeBudget`]: tessera_core::DecodeBudget

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

/// Set to run a second instance against the same config anyway. Only for
/// development — it reinstates every hazard above, including the settings
/// clobber. Isolating HOME is the safe way to get two instances.
const ALLOW_MULTI: &str = "TESSERA_ALLOW_MULTI";

/// What `acquire` decided.
pub enum Acquired {
    /// This process owns the instance. Drain `opens` once a frame.
    Primary(Opens),
    /// A running instance took the paths; this process has nothing to do
    /// and should exit successfully.
    HandedOff,
}

/// One hand-off from a later launch: the files it was asked to open,
/// already absolute. Empty means "no files" — someone launched the app
/// again with no arguments (the dock icon, usually), which still means
/// "bring the window to the front".
pub type Open = Vec<PathBuf>;

/// Receives hand-offs from later launches.
pub struct Opens {
    rx: mpsc::Receiver<Open>,
    socket: PathBuf,
}

impl Opens {
    /// Everything received since the last call. Never blocks — the accept
    /// loop lives on its own thread, the same rule every other blocking
    /// thing in this app follows.
    pub fn poll(&mut self) -> Vec<Open> {
        self.rx.try_iter().collect()
    }
}

impl Drop for Opens {
    fn drop(&mut self) {
        // Tidy exit. A crash leaves the file behind instead, which the
        // connect-first probe treats as stale and reclaims.
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Claims the instance, or hands `paths` to whoever already holds it.
///
/// Call before any expensive setup: the whole point is that a second launch
/// costs a socket round trip rather than a GL context and a wall of mpv
/// instances.
pub fn acquire(config_dir: &Path, paths: &[PathBuf]) -> Acquired {
    if std::env::var_os(ALLOW_MULTI).is_some() {
        println!("instance: {ALLOW_MULTI} set — not claiming single-instance ownership");
        return Acquired::Primary(Opens {
            rx: mpsc::channel().1, // nothing will ever arrive on it
            socket: PathBuf::new(),
        });
    }
    let socket = socket_path(config_dir);
    let absolute = absolutize(paths);

    // Connect-first. A live instance answers; a stale socket file does not.
    if let Some(()) = hand_off(&socket, &absolute) {
        return Acquired::HandedOff;
    }
    // Nobody home. Clear any stale file and take the socket.
    let _ = std::fs::remove_file(&socket);
    if let Some(dir) = socket.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let listener = match UnixListener::bind(&socket) {
        Ok(l) => l,
        Err(e) => {
            // Lost a race with another launch starting at the same moment,
            // or the socket directory is unusable. Try once more to hand
            // off; failing that, run on without ownership rather than
            // refusing to start at all.
            if let Some(()) = hand_off(&socket, &absolute) {
                return Acquired::HandedOff;
            }
            eprintln!("warning: cannot listen on {}: {e} — running without single-instance ownership", socket.display());
            return Acquired::Primary(Opens { rx: mpsc::channel().1, socket: PathBuf::new() });
        }
    };

    let (tx, rx) = mpsc::channel::<Open>();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            if serve(stream, &tx).is_err() {
                break; // the app is gone
            }
        }
    });
    Acquired::Primary(Opens { rx, socket })
}

/// Reads one request and acknowledges it. Errors only when the receiving
/// end has gone away, which ends the accept loop.
fn serve(stream: UnixStream, tx: &mpsc::Sender<Open>) -> Result<(), mpsc::SendError<Open>> {
    let mut line = String::new();
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return Ok(()),
    });
    if reader.read_line(&mut line).is_err() {
        return Ok(());
    }
    let paths = parse_request(&line);
    // Acknowledged before the app has acted on it: the sender only needs to
    // know it reached a live instance, and making it wait for a tile to
    // load would hold up its exit for no reason.
    let mut stream = stream;
    let _ = stream.write_all(b"ok\n");
    let _ = stream.flush();
    tx.send(paths)
}

/// Sends `paths` to a running instance. `None` if there isn't one.
fn hand_off(socket: &Path, paths: &[PathBuf]) -> Option<()> {
    let mut stream = UnixStream::connect(socket).ok()?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .ok()?;
    let body = build_request(paths);
    stream.write_all(body.as_bytes()).ok()?;
    stream.flush().ok()?;
    // Wait for the ack, so "handed off" is something we observed rather
    // than something we hope happened — a half-open socket would otherwise
    // look like success and the user's file would vanish into nothing.
    let mut ack = String::new();
    BufReader::new(&stream).read_line(&mut ack).ok()?;
    if ack.trim() != "ok" {
        return None;
    }
    if paths.is_empty() {
        println!("instance: already running — bringing it to the front");
    } else {
        println!(
            "instance: already running — handed it {} file(s) to play",
            paths.len()
        );
    }
    Some(())
}

/// One JSON line. Deliberately trivial and versionless: both ends ship in
/// the same binary, so there is no version skew to negotiate.
fn build_request(paths: &[PathBuf]) -> String {
    let list: Vec<String> = paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    let mut s = serde_json::json!({ "paths": list }).to_string();
    s.push('\n');
    s
}

fn parse_request(line: &str) -> Open {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|v| {
            Some(
                v.get("paths")?
                    .as_array()?
                    .iter()
                    .filter_map(|p| p.as_str())
                    .map(PathBuf::from)
                    .collect(),
            )
        })
        .unwrap_or_default()
}

/// Resolves paths against *this* process's working directory, since the
/// running instance's is almost certainly different.
fn absolutize(paths: &[PathBuf]) -> Vec<PathBuf> {
    let cwd = std::env::current_dir().ok();
    paths
        .iter()
        .map(|p| {
            std::fs::canonicalize(p).unwrap_or_else(|_| match (&cwd, p.is_absolute()) {
                (Some(cwd), false) => cwd.join(p),
                _ => p.clone(),
            })
        })
        .collect()
}

/// `$XDG_RUNTIME_DIR/opentessera-<key>.sock`, falling back to /tmp.
///
/// The key is derived from the config directory rather than from the user
/// id, so two configs are two instances — see the module comment.
fn socket_path(config_dir: &Path) -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    dir.join(format!("opentessera-{}.sock", key_for(config_dir)))
}

/// A short, stable, filename-safe digest of a path. FNV-1a: this only has
/// to separate one config directory from another on one machine, so a
/// cryptographic hash would be dead weight.
fn key_for(config_dir: &Path) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in config_dir.as_os_str().as_encoded_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips_including_the_empty_case() {
        let paths = vec![PathBuf::from("/media/a b.mkv"), PathBuf::from("/media/ünï.mp4")];
        assert_eq!(parse_request(&build_request(&paths)), paths);
        // No files still has to be a valid request: it means "come to the
        // front", which is what a second click on the dock icon wants.
        assert!(parse_request(&build_request(&[])).is_empty());
    }

    #[test]
    fn junk_on_the_socket_is_not_a_crash() {
        assert!(parse_request("").is_empty());
        assert!(parse_request("not json\n").is_empty());
        assert!(parse_request("{}\n").is_empty());
        assert!(parse_request(r#"{"paths":"nope"}"#).is_empty());
        assert!(parse_request(r#"{"paths":[1,2]}"#).is_empty());
    }

    #[test]
    fn separate_config_dirs_are_separate_instances() {
        // The property a sandboxed run depends on: its own HOME must not
        // collide with the real app's socket.
        let a = socket_path(Path::new("/home/someone/.config/opentessera"));
        let b = socket_path(Path::new("/tmp/sandbox/.config/opentessera"));
        assert_ne!(a, b);
        // ...and stable across calls, or a restart could not find itself.
        assert_eq!(a, socket_path(Path::new("/home/someone/.config/opentessera")));
        assert!(a.file_name().unwrap().to_string_lossy().ends_with(".sock"));
    }

    #[test]
    fn a_stale_socket_file_does_not_look_like_a_live_instance() {
        // Nothing is listening on this path, so a hand-off must fail and
        // let the caller reclaim it.
        let dir = std::env::temp_dir().join(format!("tessera-inst-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stale = dir.join("stale.sock");
        std::fs::write(&stale, b"not a socket").unwrap();
        assert!(hand_off(&stale, &[]).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_live_listener_accepts_a_hand_off() {
        let dir = std::env::temp_dir().join(format!("tessera-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("live.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        let (tx, rx) = mpsc::channel::<Open>();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(1) {
                let _ = serve(stream.unwrap(), &tx);
            }
        });
        let want = vec![PathBuf::from("/media/x.mkv")];
        assert!(hand_off(&sock, &want).is_some(), "live listener should answer");
        assert_eq!(rx.recv().unwrap(), want);
        std::fs::remove_dir_all(&dir).ok();
    }
}
