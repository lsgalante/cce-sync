//! `cce-sync daemon`: keeps the vault folder in step with the server.
//!
//! Three things wake a pass: the folder changing (a debounced watcher), the
//! server announcing a new seq on the live socket, and a timer (every five
//! minutes, and sooner after a failure, backing off). Passes run one at a
//! time on this thread; the watcher and the socket only send events.
//!
//! The daemon answers on `$XDG_RUNTIME_DIR/cce/cce-sync.sock`, one line in
//! and one out: `status` (a JSON [`DaemonStatus`]), `sync` (pass now). The
//! socket is also the single-instance lock: a second daemon finds it
//! answering and exits.

use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::http::Client;
use crate::local::{wanted, Folder};
use crate::pass::{self, Saved};

/// How long the folder must be quiet before a pass, and the longest a
/// stream of changes can hold one off.
const QUIET: Duration = Duration::from_millis(600);
const QUIET_CAP: Duration = Duration::from_secs(4);
/// The safety-net pass, for anything the watcher or the socket missed.
const PERIODIC: Duration = Duration::from_secs(300);
const BACKOFF_MAX: Duration = Duration::from_secs(300);
/// A keep-alive on the live socket; the server answers `pong`.
const PING_EVERY: Duration = Duration::from_secs(30);

enum Event {
    Local,
    Remote(u64),
    Now,
}

/// What `cce-sync status` and a status-bar segment read.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DaemonStatus {
    /// `starting`, `synced`, `pending`, `offline`.
    pub state: String,
    pub server: String,
    pub vault: String,
    pub seq: u64,
    pub pending: usize,
    /// Unix ms of the last pass that finished.
    pub last_pass: Option<i64>,
    pub last_error: Option<String>,
    /// Conflict copies written since the daemon started: (path, copy).
    pub conflicts: Vec<(String, String)>,
    /// Files the server refuses, with why.
    pub refused: Vec<String>,
    pub live: bool,
}

pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("cce")
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join("cce-sync.sock")
}

/// Ask a running daemon something. `None` when none answers.
pub fn ask(line: &str) -> Option<String> {
    let mut s = UnixStream::connect(socket_path()).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    s.write_all(format!("{line}\n").as_bytes()).ok()?;
    let mut reply = String::new();
    std::io::BufRead::read_line(&mut std::io::BufReader::new(s), &mut reply).ok()?;
    Some(reply.trim().to_string())
}

/// Where the state for this (server, vault) pair lives.
pub fn state_path(server: &str, vault: &Path) -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"));
    let id = cce_sync::proto::hash(format!("{server}\n{}", vault.display()).as_bytes());
    base.join("cce/sync").join(format!("{}.json", &id[..16]))
}

pub fn load(path: &Path, server: &str, vault: &Path) -> Saved {
    let fresh = || Saved { server: server.to_string(), vault: vault.display().to_string(), ..Saved::default() };
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<Saved>(&bytes) {
            Ok(s) if s.server == server && s.vault == vault.display().to_string() => s,
            Ok(_) => fresh(),
            Err(e) => {
                log(&format!("state {}: {e}; starting over (the first pass adopts identical files)", path.display()));
                fresh()
            }
        },
        Err(_) => fresh(),
    }
}

pub fn save(path: &Path, saved: &Saved) -> Result<(), String> {
    let bytes = serde_json::to_vec(saved).map_err(|e| e.to_string())?;
    cce_vault::write::atomic_write(path, &bytes).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn log(msg: &str) {
    eprintln!("cce-sync: {msg}");
}

fn notify(summary: &str, body: &str) {
    let _ = std::process::Command::new("notify-send")
        .args(["-a", "cce-sync", summary, body])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// Claim the status socket, or fail if a daemon already answers on it.
fn claim() -> Result<UnixListener, String> {
    let path = socket_path();
    if ask("ping").is_some() {
        return Err(format!("a daemon is already running ({})", path.display()));
    }
    std::fs::create_dir_all(runtime_dir()).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&path); // a crashed daemon's leftover
    UnixListener::bind(&path).map_err(|e| format!("{}: {e}", path.display()))
}

fn serve(listener: UnixListener, status: Arc<Mutex<DaemonStatus>>, tx: Sender<Event>) {
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            let Some(line) = cce_core::ipc::read_request_line(&conn, 4096, Duration::from_secs(2)) else { continue };
            let reply = match line.trim() {
                "ping" => "pong".to_string(),
                "status" => serde_json::to_string(&*status.lock().unwrap()).unwrap_or_default(),
                "sync" => {
                    let _ = tx.send(Event::Now);
                    "ok".to_string()
                }
                other => format!("error unknown command {other}"),
            };
            let _ = conn.write_all(format!("{reply}\n").as_bytes());
        }
    });
}

fn watch(root: &Path, tx: Sender<Event>) -> Result<notify::RecommendedWatcher, String> {
    use notify::Watcher;
    let base = root.to_path_buf();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else { return };
        if matches!(ev.kind, notify::EventKind::Access(_)) {
            return;
        }
        // Our own writes go through hidden temps, which this skips; their
        // final rename does wake a pass, which then finds nothing to do.
        let relevant = ev.paths.iter().any(|p| {
            p.strip_prefix(&base).ok().and_then(|r| r.to_str()).is_some_and(|r| r.is_empty() || wanted(r))
        });
        if relevant {
            let _ = tx.send(Event::Local);
        }
    })
    .map_err(|e| e.to_string())?;
    watcher.watch(root, notify::RecursiveMode::Recursive).map_err(|e| e.to_string())?;
    Ok(watcher)
}

/// The live socket: every notice is a `Remote(seq)` event. Reconnects with
/// backoff; the server sends the current seq on connect, so a reconnect
/// after a gap triggers a catch-up pass by itself.
fn live(url: String, auth: String, tx: Sender<Event>, status: Arc<Mutex<DaemonStatus>>) {
    use tungstenite::client::IntoClientRequest;
    use tungstenite::stream::MaybeTlsStream;
    use tungstenite::Message;
    std::thread::spawn(move || {
        let mut backoff = Duration::from_secs(1);
        loop {
            let mut req = match url.as_str().into_client_request() {
                Ok(r) => r,
                Err(e) => return log(&format!("live: {e}")),
            };
            if let Ok(v) = auth.parse() {
                req.headers_mut().insert("authorization", v);
            }
            match tungstenite::connect(req) {
                Ok((mut ws, _)) => {
                    backoff = Duration::from_secs(1);
                    let timeout = Some(PING_EVERY);
                    let _ = match ws.get_mut() {
                        MaybeTlsStream::Plain(s) => s.set_read_timeout(timeout),
                        MaybeTlsStream::Rustls(s) => s.get_mut().set_read_timeout(timeout),
                        _ => Ok(()),
                    };
                    status.lock().unwrap().live = true;
                    loop {
                        match ws.read() {
                            Ok(Message::Text(t)) => {
                                if let Ok(n) = serde_json::from_str::<cce_sync::proto::LiveNotice>(t.as_str()) {
                                    if tx.send(Event::Remote(n.seq)).is_err() {
                                        return;
                                    }
                                }
                            }
                            Ok(Message::Close(_)) => break,
                            Ok(_) => {}
                            Err(tungstenite::Error::Io(e))
                                if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
                            {
                                if ws.send(Message::text("ping")).is_err() {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    status.lock().unwrap().live = false;
                }
                Err(e) => log(&format!("live: {e}")),
            }
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
    });
}

/// Wait out a burst of folder events: until it has been quiet for
/// [`QUIET`], at most [`QUIET_CAP`]. A remote notice or a `sync` ends the
/// wait at once.
fn settle(rx: &Receiver<Event>) {
    let started = Instant::now();
    while started.elapsed() < QUIET_CAP {
        match rx.recv_timeout(QUIET) {
            Ok(Event::Local) => continue,
            Ok(_) | Err(_) => return,
        }
    }
}

pub fn run(http: Client, root: PathBuf) -> Result<(), String> {
    let listener = claim()?;
    let folder = Folder { root: root.clone() };
    let state_file = state_path(http.server(), &root);
    let mut saved = load(&state_file, http.server(), &root);
    let status = Arc::new(Mutex::new(DaemonStatus {
        state: "starting".into(),
        server: http.server().to_string(),
        vault: root.display().to_string(),
        seq: saved.sync.seq,
        ..DaemonStatus::default()
    }));
    let (tx, rx) = channel();
    serve(listener, status.clone(), tx.clone());
    let _watcher = watch(&root, tx.clone())?;
    live(http.live_url(), http.auth().to_string(), tx.clone(), status.clone());
    log(&format!("syncing {} with {} (state {})", root.display(), http.server(), state_file.display()));

    let mut wait = Duration::ZERO; // the first pass runs at once
    let mut failures = 0u32;
    loop {
        match rx.recv_timeout(wait) {
            Ok(Event::Local) => settle(&rx),
            Ok(Event::Remote(seq)) if seq <= saved.sync.seq && saved.sync.listed && failures == 0 => continue,
            Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err("event channel closed".into()),
        }
        while rx.try_recv().is_ok() {} // anything queued is covered by this pass
        match pass::run(&http, &folder, &mut saved) {
            Ok(report) => {
                failures = 0;
                wait = PERIODIC;
                if let Err(e) = save(&state_file, &saved) {
                    log(&e);
                }
                if report.wrote_anything() || report.uploaded + report.deleted > 0 {
                    log(&format!(
                        "pass: {} down, {} removed, {} up ({} merged), {} deleted, {} conflicts",
                        report.downloaded,
                        report.removed,
                        report.uploaded,
                        report.merged,
                        report.deleted,
                        report.conflicts.len()
                    ));
                }
                for (path, copy) in &report.conflicts {
                    log(&format!("conflict: {path}: this device's version kept as {copy}"));
                    notify(&format!("Sync conflict in {path}"), &format!("This device's version is kept as {copy}"));
                }
                let mut s = status.lock().unwrap();
                for r in &report.refused {
                    if !s.refused.contains(r) {
                        log(&format!("refused: {r}"));
                        notify("The vault server refused a file", r);
                    }
                }
                s.state = if report.pending > 0 { "pending".into() } else { "synced".into() };
                s.seq = saved.sync.seq;
                s.pending = report.pending;
                s.last_pass = Some(now_ms());
                s.last_error = None;
                s.conflicts.extend(report.conflicts);
                s.refused = report.refused;
            }
            Err(e) => {
                failures += 1;
                wait = (Duration::from_secs(5) * 2u32.saturating_pow(failures - 1)).min(BACKOFF_MAX);
                log(&format!("pass failed ({e}); next try in {}s", wait.as_secs()));
                let _ = save(&state_file, &saved);
                let mut s = status.lock().unwrap();
                s.state = "offline".into();
                s.last_error = Some(e);
            }
        }
    }
}
