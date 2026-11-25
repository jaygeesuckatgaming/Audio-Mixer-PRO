//! Local control surface: a loopback HTTP API the Stream Deck plug-in (or a
//! script) uses to read the console's state and drive it.
//!
//! Two endpoints, deliberately small:
//!
//!   * `GET  /state`   -> JSON snapshot the console publishes every ~150 ms;
//!   * `POST /command` -> one command, queued for the UI thread.
//!
//! Why a queue and not direct mutation: every control lives in `MixerApp` and
//! is pushed to the audio engine once per frame by `sync_audio_controls`. The
//! server thread therefore only *queues* a [`Command`]; the UI thread drains
//! the queue at the top of each frame and applies it exactly as a click would.
//!
//! Security: the listener is bound to 127.0.0.1 only and every request must
//! carry the token from `control.token` next to the scene. The plug-in runs as
//! the same user and reads the file itself, so nothing has to be configured,
//! and a web page in a browser cannot reach it (no CORS, and it cannot read the
//! file to learn the token).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::audio::MAX_STRIPS;
use eframe::egui;

/// Port the plug-in looks for first. Chosen above the usual 8000/8080 crowd and
/// below the Windows dynamic range.
pub const DEFAULT_PORT: u16 = 8765;

/// What the UI thread is asked to do. `None` for a boolean means "toggle".
///
/// Only mute exists today; the enum is the extension point, so adding solo,
/// gain, transport or scene recall later is one variant plus one match arm -
/// no change to the wire format or the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Mute one strip (`strip` is 0-based, matching the console's own index).
    StripMute { strip: usize, value: Option<bool> },
    /// Mute every active strip. Toggling mutes all unless all are already
    /// muted, in which case it unmutes them.
    MuteAll { value: Option<bool> },
}

/// Data shared between the UI thread and the server thread.
pub struct Shared {
    commands: Mutex<Vec<Command>>,
    state: Mutex<String>,
    repaint: Mutex<Option<egui::Context>>,
}

impl Shared {
    fn new() -> Self {
        Self {
            commands: Mutex::new(Vec::new()),
            state: Mutex::new(r#"{"seq":0,"strips":[]}"#.to_string()),
            repaint: Mutex::new(None),
        }
    }

    /// Replace the snapshot served by `GET /state`.
    pub fn publish(&self, state: String) {
        if let Ok(mut slot) = self.state.lock() {
            *slot = state;
        }
    }

    /// Hand the UI thread a context so a command can wake it even when the
    /// window is unfocused.
    pub fn attach_ui(&self, ctx: egui::Context) {
        if let Ok(mut slot) = self.repaint.lock() {
            *slot = Some(ctx);
        }
    }

    pub fn take_commands(&self) -> Vec<Command> {
        self.commands
            .lock()
            .map(|mut queue| std::mem::take(&mut *queue))
            .unwrap_or_default()
    }

    /// Queue a command from inside the process (a future hotkey path can use
    /// this without going through HTTP).
    pub fn push(&self, command: Command) {
        if let Ok(mut queue) = self.commands.lock() {
            queue.push(command);
        }
        if let Ok(slot) = self.repaint.lock() {
            if let Some(ctx) = slot.as_ref() {
                ctx.request_repaint();
            }
        }
    }

    fn state_or_default(&self) -> String {
        self.state
            .lock()
            .map(|state| state.clone())
            .unwrap_or_else(|_| r#"{"seq":0,"strips":[]}"#.to_string())
    }
}

/// A running listener. Dropping it stops the thread.
pub struct Server {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
    port: u16,
    token: String,
}

impl Server {
    /// Bind `127.0.0.1:port` (port 0 picks a free one) and start serving.
    pub fn start(port: u16) -> Result<Self, String> {
        let token = load_or_create_token()?;
        let listener = TcpListener::bind(("127.0.0.1", port))
            .map_err(|err| format!("could not listen on port {port}: {err}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|err| format!("could not set the listener non-blocking: {err}"))?;
        let port = listener
            .local_addr()
            .map(|addr| addr.port())
            .unwrap_or(port);
        let shared = Arc::new(Shared::new());
        let stop = Arc::new(AtomicBool::new(false));

        let thread_shared = Arc::clone(&shared);
        let thread_stop = Arc::clone(&stop);
        let thread_token = token.clone();
        let thread = std::thread::Builder::new()
            .name("control-api".to_string())
            .spawn(move || serve(listener, thread_stop, thread_shared, thread_token))
            .map_err(|err| format!("could not start the control thread: {err}"))?;

        Ok(Self {
            stop,
            thread: Some(thread),
            shared,
            port,
            token,
        })
    }

    pub fn shared(&self) -> Arc<Shared> {
        Arc::clone(&self.shared)
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Where the token lives; the plug-in reads the same file.
    pub fn token_path() -> PathBuf {
        crate::settings_file().with_file_name("control.token")
    }

    #[allow(dead_code)]
    pub fn token(&self) -> &str {
        &self.token
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            // The loop wakes every 20 ms, so this join is short and never
            // blocks the UI for long.
            let _ = thread.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Wire format
// ---------------------------------------------------------------------------

/// `{ "cmd": "strip.mute", "strip": 1, "value": true }` — `strip` is 1-based on
/// the wire (it is a human/debug surface, and the console labels strips from 1)
/// and is converted to the internal 0-based index here.
pub fn parse_command(body: &str) -> Result<Command, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("invalid JSON: {err}"))?;
    let cmd = value
        .get("cmd")
        .and_then(|cmd| cmd.as_str())
        .ok_or_else(|| "missing \"cmd\"".to_string())?
        .to_string();
    let toggle = match value.get("value") {
        Some(serde_json::Value::Bool(flag)) => Some(*flag),
        None | Some(serde_json::Value::Null) => None,
        Some(_) => return Err("\"value\" must be true or false".to_string()),
    };
    match cmd.as_str() {
        "strip.mute" => {
            let number = value
                .get("strip")
                .and_then(|strip| strip.as_u64())
                .ok_or_else(|| "missing \"strip\"".to_string())?;
            if number == 0 || number as usize > MAX_STRIPS {
                return Err(format!("strip must be 1..{MAX_STRIPS}"));
            }
            Ok(Command::StripMute {
                strip: number as usize - 1,
                value: toggle,
            })
        }
        "mute.all" => Ok(Command::MuteAll { value: toggle }),
        other => Err(format!("unknown command \"{other}\"")),
    }
}

// ---------------------------------------------------------------------------
// Server thread
// ---------------------------------------------------------------------------

fn serve(listener: TcpListener, stop: Arc<AtomicBool>, shared: Arc<Shared>, token: String) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                handle(&mut stream, &token, &shared);
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// Largest request accepted. A status poll is a few bytes; a command is under
/// a hundred. Anything bigger is not ours.
const MAX_REQUEST: usize = 8 * 1024;

fn handle(stream: &mut TcpStream, token: &str, shared: &Shared) {
    let Some(request) = read_request(stream) else {
        respond(stream, "400 Bad Request", r#"{"ok":false,"error":"malformed request"}"#);
        return;
    };

    // The plug-in may probe `/` first; answering with the routes helps when
    // someone points a browser at the port while debugging.
    if request.method == "GET" && request.path == "/" {
        respond(
            stream,
            "200 OK",
            r#"{"name":"Audio Mixer PRO control","routes":["GET /state","POST /command"]}"#,
        );
        return;
    }

    if request.token != token {
        respond(
            stream,
            "403 Forbidden",
            r#"{"ok":false,"error":"missing or wrong X-AudioMixer-Token"}"#,
        );
        return;
    }

    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/state") => {
            let state = shared.state_or_default();
            respond(stream, "200 OK", &state);
        }
        ("POST", "/command") => match parse_command(&request.body) {
            Ok(command) => {
                shared.push(command);
                respond(stream, "200 OK", r#"{"ok":true,"queued":true}"#);
            }
            Err(err) => {
                let body = serde_json::json!({ "ok": false, "error": err }).to_string();
                respond(stream, "400 Bad Request", &body);
            }
        },
        _ => respond(
            stream,
            "404 Not Found",
            r#"{"ok":false,"error":"no such route"}"#,
        ),
    }
}

struct Request {
    method: String,
    path: String,
    token: String,
    body: String,
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];

    // Read until the blank line that ends the headers.
    let header_end = loop {
        if let Some(at) = find(&buffer, b"\r\n\r\n") {
            break at + 4;
        }
        if buffer.len() > MAX_REQUEST {
            return None;
        }
        match stream.read(&mut chunk) {
            Ok(0) => return None,
            Ok(count) => buffer.extend_from_slice(&chunk[..count]),
            Err(_) => return None,
        }
    };

    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = head.lines();
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_string();
    let path = request_line.next()?.split('?').next()?.to_string();

    let mut token = String::new();
    let mut content_length = 0usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "x-audiomixer-token" => token = value.to_string(),
            "content-length" => content_length = value.parse().unwrap_or(0),
            _ => {}
        }
    }
    if content_length > MAX_REQUEST {
        return None;
    }

    let mut body = buffer[header_end..].to_vec();
    while body.len() < content_length {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => body.extend_from_slice(&chunk[..count]),
            Err(_) => break,
        }
    }
    body.truncate(content_length);

    Some(Request {
        method,
        path,
        token,
        body: String::from_utf8_lossy(&body).to_string(),
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let head = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

// ---------------------------------------------------------------------------
// Token
// ---------------------------------------------------------------------------

fn load_or_create_token() -> Result<String, String> {
    let path = Server::token_path();
    if let Ok(text) = std::fs::read_to_string(&path) {
        let token = text.trim().to_string();
        if token.len() >= 16 {
            return Ok(token);
        }
    }
    let token = random_hex();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&path, &token)
        .map_err(|err| format!("could not write {}: {err}", path.display()))?;
    Ok(token)
}

/// 32 hex characters seeded from the OS-randomised hasher, the clock and the
/// process id. This is a loopback token, not a key: it only has to be
/// unguessable from another process on the same machine within a session.
fn random_hex() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    let mut out = String::with_capacity(32);
    for round in 0..2u32 {
        let mut hasher = RandomState::new().build_hasher();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        hasher.write_u128(nanos);
        hasher.write_u32(std::process::id());
        hasher.write_u32(round);
        out.push_str(&format!("{:016x}", hasher.finish()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_strip_mute_with_explicit_value() {
        let command = parse_command(r#"{"cmd":"strip.mute","strip":3,"value":true}"#).unwrap();
        assert_eq!(
            command,
            Command::StripMute {
                strip: 2,
                value: Some(true)
            }
        );
    }

    #[test]
    fn parses_strip_mute_toggle() {
        let command = parse_command(r#"{"cmd":"strip.mute","strip":1}"#).unwrap();
        assert_eq!(
            command,
            Command::StripMute {
                strip: 0,
                value: None
            }
        );
    }

    #[test]
    fn parses_mute_all() {
        let command = parse_command(r#"{"cmd":"mute.all","value":false}"#).unwrap();
        assert_eq!(command, Command::MuteAll { value: Some(false) });
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse_command("not json").is_err());
        assert!(parse_command(r#"{"cmd":"strip.mute"}"#).is_err());
        assert!(parse_command(r#"{"cmd":"strip.mute","strip":0}"#).is_err());
        assert!(parse_command(r#"{"cmd":"strip.mute","strip":99}"#).is_err());
        assert!(parse_command(r#"{"cmd":"nope"}"#).is_err());
        assert!(parse_command(r#"{"cmd":"mute.all","value":"yes"}"#).is_err());
    }

    #[test]
    fn finds_the_header_break() {
        assert_eq!(find(b"GET / HTTP/1.1\r\n\r\n", b"\r\n\r\n"), Some(14));
        assert_eq!(find(b"nothing here", b"\r\n\r\n"), None);
    }
}
