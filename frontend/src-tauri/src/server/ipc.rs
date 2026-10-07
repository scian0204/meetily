//! Browser <-> app bridge on `/api/ws`: invokes, events, ordering and reconnects.
//!
//! Text JSON frames (the client is `frontend/web-runtime/runtime.js`):
//! - client -> server
//!   - `{"t":"invoke","id","cmd","args"}`: run a command through the real IPC handler
//!   - `{"t":"listen","event"}`: start forwarding an app event to browsers
//!   - `{"t":"emit","event","payload"}`: emit an app-wide event (echoed to every browser)
//!   - `{"t":"resume","ids":[..]}`: re-attach invokes still pending after a reconnect
//! - server -> client
//!   - `{"t":"hello","boot","seq"}`: first frame; `seq` is the last event emitted so far
//!   - `{"t":"event","seq","event","payload"}`
//!   - `{"t":"result","id","ok":true,"value"}` / `{"t":"result","id","ok":false,"error"}`
//!   - `{"t":"unknown","id"}`: this server never got that invoke; the client re-sends it
//!   - `{"t":"gap"}`: events since the reconnect point were evicted and are missing
//!
//! A reconnecting tab opens `/api/ws?boot=<boot>&since=<seq>` and receives the events it
//! missed from a replay log before live ones, so no transcript update is lost to a blip.

use super::{Server, Shared};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    response::Response,
};
use dashmap::{mapref::entry::Entry, DashMap};
use futures_util::{SinkExt, StreamExt};
use rand::RngCore;
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::VecDeque,
    path::{Component, Path},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tauri::{ipc::InvokeResponseBody, Emitter, Listener};
use tokio::sync::{broadcast, mpsc};

/// A local app URL, so plugin commands pass the "main" capability's ACL.
const APP_URL: &str = if cfg!(windows) {
    "http://tauri.localhost"
} else {
    "tauri://localhost"
};

/// Results nobody has received yet are kept this long for a reconnecting tab.
const RESULT_TTL: Duration = Duration::from_secs(15 * 60);
/// Delivered results are kept briefly too: "delivered" only means queued on a socket
/// that may have died before the browser read it.
const DELIVERED_TTL: Duration = Duration::from_secs(120);
/// Event frames kept for replay (~ minutes of a busy recording).
const EVENT_LOG_FRAMES: usize = 8192;
const LIVE_EVENT_CAPACITY: usize = 4096;

/// Commands the browser runtime handles itself, or that would act on the server
/// machine rather than the user's (dialogs, file managers, raw file I/O, telemetry).
const BROWSER_ONLY: &[&str] = &[
    "read_audio_file",
    "save_transcript",
    "open_external_url",
    "open_meeting_folder",
    "open_recordings_folder",
    "open_models_folder",
    "open_parakeet_models_folder",
    "open_database_folder",
    "open_system_settings",
    "select_and_validate_audio_command",
    "select_legacy_database_path",
    "check_homebrew_database",
    "show_console",
    "hide_console",
    "toggle_console",
    "init_analytics",
];

/// The only plugin namespaces a browser may reach. Everything else is refused: process
/// exit/restart, the updater, native dialogs and desktop notifications on the server.
const SERVER_PLUGINS: &[&str] = &[
    "plugin:store|",
    "plugin:path|",
    "plugin:app|",
    "plugin:resources|",
];

/// App events fanned out to browsers, with a replay log for reconnecting tabs.
pub struct EventHub {
    /// Random per process: a tab reconnecting to a restarted server must not replay.
    boot: String,
    log: Mutex<EventLog>,
    live: broadcast::Sender<(u64, Arc<str>)>,
}

#[derive(Default)]
struct EventLog {
    last_seq: u64,
    frames: VecDeque<(u64, Arc<str>)>,
}

/// What a new connection starts from.
struct Subscription {
    live: broadcast::Receiver<(u64, Arc<str>)>,
    hello: String,
    gap: bool,
    replay: Vec<(u64, Arc<str>)>,
}

impl EventHub {
    pub fn new() -> Self {
        let mut bytes = [0u8; 8];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        Self {
            boot: bytes.iter().map(|b| format!("{b:02x}")).collect(),
            log: Mutex::new(EventLog::default()),
            live: broadcast::channel(LIVE_EVENT_CAPACITY).0,
        }
    }

    fn publish(&self, name: &str, payload: &str) {
        let mut log = self.log.lock().unwrap();
        log.last_seq += 1;
        let seq = log.last_seq;
        let frame: Arc<str> = event_frame(seq, name, payload).into();
        log.frames.push_back((seq, frame.clone()));
        if log.frames.len() > EVENT_LOG_FRAMES {
            log.frames.pop_front();
        }
        // Sent under the lock, so live frames go out in sequence order.
        let _ = self.live.send((seq, frame));
    }

    /// Subscribes to live events and, for a reconnect to this same process, collects the
    /// logged events after `since`. Done under the log lock so nothing falls in between.
    fn subscribe(&self, resume: &ResumeQuery) -> Subscription {
        let log = self.log.lock().unwrap();
        let live = self.live.subscribe();
        let hello = format!(
            r#"{{"t":"hello","boot":{},"seq":{}}}"#,
            Value::String(self.boot.clone()),
            log.last_seq
        );
        let (gap, replay) = match (&resume.boot, resume.since) {
            (Some(boot), Some(since)) if *boot == self.boot && since < log.last_seq => {
                let oldest = log.frames.front().map_or(log.last_seq + 1, |(seq, _)| *seq);
                let replay = log.frames.iter().filter(|(seq, _)| *seq > since).cloned().collect();
                (oldest > since + 1, replay)
            }
            _ => (false, Vec::new()),
        };
        Subscription { live, hello, gap, replay }
    }
}

/// Invoke outcomes, kept across reconnects so a dropped socket never loses the
/// result of a long command (stop_recording can take minutes).
#[derive(Default)]
pub struct Results(DashMap<String, Slot>);

enum Slot {
    /// Still running; deliver to this connection.
    Pending(mpsc::UnboundedSender<String>),
    /// Finished. `delivered`: handed to a socket at least once.
    Done { frame: String, at: Instant, delivered: bool },
}

impl Results {
    fn start(&self, id: &str, conn: &mpsc::UnboundedSender<String>) {
        self.0.insert(id.to_string(), Slot::Pending(conn.clone()));
    }

    fn finish(&self, id: &str, frame: String) {
        if let Entry::Occupied(mut slot) = self.0.entry(id.to_string()) {
            let delivered =
                matches!(slot.get(), Slot::Pending(conn) if conn.send(frame.clone()).is_ok());
            slot.insert(Slot::Done { frame, at: Instant::now(), delivered });
        }
    }

    fn resume(&self, id: &str, conn: &mpsc::UnboundedSender<String>) {
        match self.0.entry(id.to_string()) {
            Entry::Occupied(mut slot) => match slot.get_mut() {
                Slot::Pending(waiting) => *waiting = conn.clone(),
                Slot::Done { frame, at, delivered } => {
                    // The tab still waits, so any earlier delivery never arrived: send again.
                    *delivered = conn.send(frame.clone()).is_ok() || *delivered;
                    *at = Instant::now();
                }
            },
            Entry::Vacant(_) => {
                let _ = conn.send(format!(r#"{{"t":"unknown","id":{}}}"#, Value::String(id.into())));
            }
        }
    }

    fn sweep(&self) {
        self.0.retain(|_, slot| match slot {
            Slot::Pending(_) => true,
            Slot::Done { at, delivered, .. } => {
                at.elapsed() < if *delivered { DELIVERED_TTL } else { RESULT_TTL }
            }
        });
    }
}

impl Server {
    /// Bridge an app event to every browser. Idempotent.
    fn forward_event(&self, name: &str) {
        if !valid_event_name(name) {
            log::warn!("ignoring invalid event name {name:?}");
            return;
        }
        if !self.listened.lock().unwrap().insert(name.to_string()) {
            return;
        }
        let hub = self.events.clone();
        let event_name = name.to_string();
        self.app.listen_any(name.to_string(), move |event| {
            // Runs inside tauri's emit with its handler table locked: never block here.
            hub.publish(&event_name, event.payload());
        });
    }
}

/// tauri's event-name rule; `listen_any` panics on anything else.
fn valid_event_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '/' | ':' | '_'))
}

fn result_frame(id: &str, outcome: Result<String, Value>) -> String {
    let id = Value::String(id.to_string());
    match outcome {
        Ok(json) => format!(r#"{{"t":"result","id":{id},"ok":true,"value":{json}}}"#),
        Err(error) => format!(r#"{{"t":"result","id":{id},"ok":false,"error":{error}}}"#),
    }
}

fn event_frame(seq: u64, name: &str, payload: &str) -> String {
    let name = Value::String(name.to_string());
    let payload = if payload.is_empty() { "null" } else { payload };
    format!(r#"{{"t":"event","seq":{seq},"event":{name},"payload":{payload}}}"#)
}

#[derive(Deserialize, Default)]
pub struct ResumeQuery {
    boot: Option<String>,
    since: Option<u64>,
}

pub async fn socket(
    State(server): State<Shared>,
    Query(resume): Query<ResumeQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    // Invoke args can be large (a full transcript on save).
    ws.max_message_size(256 << 20)
        .on_upgrade(move |socket| connection(server, socket, resume))
}

async fn connection(server: Shared, socket: WebSocket, resume: ResumeQuery) {
    let (mut sink, mut stream) = socket.split();
    let (conn, mut outbox) = mpsc::unbounded_channel::<String>();
    let Subscription { mut live, hello, gap, replay } = server.events.subscribe(&resume);

    let writer = tokio::spawn(async move {
        let mut first = vec![hello];
        if gap {
            log::warn!("reconnecting browser missed events that are no longer logged");
            first.push(r#"{"t":"gap"}"#.to_string());
        }
        let mut sent_seq = replay.last().map_or(0, |(seq, _)| *seq);
        first.extend(replay.into_iter().map(|(_, frame)| frame.to_string()));
        for frame in first {
            if sink.send(Message::Text(frame)).await.is_err() {
                return;
            }
        }
        loop {
            let frame = tokio::select! {
                // Biased: an event emitted while a command ran always goes out before that
                // command's result (the UI expects e.g. recording-stopped first).
                biased;
                event = live.recv() => match event {
                    Ok((seq, _)) if seq <= sent_seq => continue, // already replayed
                    Ok((seq, frame)) => {
                        sent_seq = seq;
                        frame.to_string()
                    }
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        // The tab reconnects with `since` and gets the rest from the log.
                        log::warn!("browser fell {missed} events behind; reconnecting it");
                        let _ = sink.send(Message::Close(None)).await;
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                frame = outbox.recv() => match frame {
                    Some(frame) => frame,
                    None => break,
                },
            };
            if sink.send(Message::Text(frame)).await.is_err() {
                break;
            }
        }
    });

    while let Some(Ok(message)) = stream.next().await {
        match message {
            Message::Text(text) => handle(&server, &conn, &text),
            Message::Close(_) => break,
            _ => {}
        }
    }
    writer.abort();
}

#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
enum ClientMessage {
    Invoke {
        id: String,
        cmd: String,
        #[serde(default = "empty_args")]
        args: Value,
    },
    Listen {
        event: String,
    },
    Emit {
        event: String,
        #[serde(default)]
        payload: Value,
    },
    Resume {
        ids: Vec<String>,
    },
}

fn empty_args() -> Value {
    Value::Object(Default::default())
}

fn handle(server: &Shared, conn: &mpsc::UnboundedSender<String>, text: &str) {
    let message = match serde_json::from_str::<ClientMessage>(text) {
        Ok(message) => message,
        Err(e) => {
            log::warn!("unreadable message from browser: {e}");
            return;
        }
    };
    match message {
        ClientMessage::Invoke { id, cmd, mut args } => {
            let paths = Paths {
                data: &server.data_dir,
                uploads: &server.uploads_dir,
                recordings: &server.recordings_dir,
            };
            if let Err(error) = guard(&cmd, &mut args, &paths) {
                log::warn!("refused `{cmd}`: {error}");
                let _ = conn.send(result_frame(&id, Err(Value::String(error))));
                return;
            }
            server.results.start(&id, conn);
            let server = server.clone();
            tauri::async_runtime::spawn(async move {
                let outcome = invoke(&server, cmd, args).await;
                server.results.finish(&id, result_frame(&id, outcome));
            });
        }
        ClientMessage::Listen { event } => server.forward_event(&event),
        ClientMessage::Emit { event, payload } => {
            if !valid_event_name(&event) {
                log::warn!("ignoring emit of invalid event name {event:?}");
            } else if let Err(e) = server.app.emit(&event, payload) {
                log::warn!("emit `{event}` failed: {e}");
            }
        }
        ClientMessage::Resume { ids } => {
            for id in ids {
                server.results.resume(&id, conn);
            }
        }
    }
}

/// Runs one command exactly as the desktop webview would, through tauri's IPC handler
/// (argument parsing, managed state, ACL), and returns the raw JSON value or error.
async fn invoke(server: &Server, cmd: String, args: Value) -> Result<String, Value> {
    let webview = server.webview.clone();
    let request = tauri::webview::InvokeRequest {
        cmd: cmd.clone(),
        callback: tauri::ipc::CallbackFn(0),
        error: tauri::ipc::CallbackFn(1),
        url: APP_URL.parse().expect("static app URL"),
        body: tauri::ipc::InvokeBody::Json(args),
        headers: Default::default(),
        invoke_key: tauri::test::INVOKE_KEY.to_string(),
    };
    // get_ipc_response blocks until the command finishes; async commands run on
    // tauri's runtime meanwhile, sync ones on this blocking thread.
    let response = tauri::async_runtime::spawn_blocking(move || {
        tauri::test::get_ipc_response(&webview, request)
    })
    .await;
    match response {
        Ok(Ok(InvokeResponseBody::Json(json))) => Ok(json),
        // No command returns raw bytes today; a number array is the closest JSON form.
        Ok(Ok(InvokeResponseBody::Raw(bytes))) => {
            Ok(serde_json::to_string(&bytes).unwrap_or_else(|_| "null".into()))
        }
        Ok(Err(error)) => Err(error),
        Err(e) => {
            log::error!("command `{cmd}` panicked: {e}");
            Err(Value::String(format!("`{cmd}` failed unexpectedly on the server")))
        }
    }
}

pub fn spawn_housekeeping(server: Shared) {
    const FLUSH_EVENT: &str = "meetily-server:flush";
    server.app.listen_any(FLUSH_EVENT, |_| {});
    tauri::async_runtime::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        for round in 1u64.. {
            tick.tick().await;
            // tauri defers an emit that races a running handler and replays it only on
            // the next emit that reaches a handler. This keeps such events from stalling.
            let _ = server.app.emit(FLUSH_EVENT, ());
            if round % 240 == 0 {
                server.results.sweep();
                super::media::sweep_uploads(&server.uploads_dir);
            }
        }
    });
}

struct Paths<'a> {
    data: &'a Path,
    uploads: &'a Path,
    recordings: &'a Path,
}

/// Validates (and for two commands rewrites) client arguments before dispatch.
/// Paths a browser sends must point into the upload or recordings roots.
fn guard(cmd: &str, args: &mut Value, paths: &Paths) -> Result<(), String> {
    let foreign_plugin =
        cmd.starts_with("plugin:") && !SERVER_PLUGINS.iter().any(|prefix| cmd.starts_with(prefix));
    if BROWSER_ONLY.contains(&cmd) || foreign_plugin {
        return Err(format!("`{cmd}` is not available in the web version"));
    }
    match cmd {
        // Stores resolve their path against app data, but an absolute path or `..` would
        // escape it: allow only the app's own `<name>.json` files.
        "plugin:store|load" | "plugin:store|get_store" => require_store_name(args),
        "validate_audio_file_command" => require_within(args, "path", paths.uploads),
        "start_import_audio_command" => require_within(args, "sourcePath", paths.uploads),
        "detect_legacy_database" => require_within(args, "selectedPath", paths.uploads),
        "import_and_initialize_database" => require_within(args, "legacyDbPath", paths.uploads),
        "start_retranscription_command" => {
            require_within(args, "meetingFolderPath", paths.recordings)
        }
        "recover_audio_from_checkpoints" | "cleanup_checkpoints" | "has_audio_checkpoints" => {
            require_within(args, "meetingFolder", paths.recordings)
        }
        "api_save_transcript" => match args.get("folderPath") {
            None | Some(Value::Null) => Ok(()),
            Some(_) => require_within(args, "folderPath", paths.recordings),
        },
        "stop_recording" => {
            // save_path only drives a create_dir_all of its parent: keep it in app data.
            if let Some(stop_args) = args.get_mut("args").and_then(Value::as_object_mut) {
                let save_path = paths.data.join("recording.wav");
                stop_args.insert("save_path".into(), save_path.to_string_lossy().into());
            }
            Ok(())
        }
        "set_recording_preferences" => {
            // Meetings are always saved under the recordings root (MEETILY_RECORDINGS_DIR).
            if let Some(prefs) = args.get_mut("preferences").and_then(Value::as_object_mut) {
                prefs.insert("save_folder".into(), paths.recordings.to_string_lossy().into());
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn require_store_name(args: &Value) -> Result<(), String> {
    let name = args.get("path").and_then(Value::as_str).unwrap_or_default();
    let mut parts = Path::new(name).components();
    let single_file = matches!(parts.next(), Some(Component::Normal(_))) && parts.next().is_none();
    if single_file && name.ends_with(".json") && !name.contains(['/', '\\']) {
        Ok(())
    } else {
        Err(format!("store `{name}` is not allowed: use a plain <name>.json"))
    }
}

fn require_within(args: &Value, key: &str, root: &Path) -> Result<(), String> {
    let raw = args
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing `{key}`"))?;
    if within(Path::new(raw), root) {
        Ok(())
    } else {
        Err(format!("`{key}` must be inside {}", root.display()))
    }
}

/// `root` must be canonical.
fn within(path: &Path, root: &Path) -> bool {
    match path.canonicalize() {
        Ok(real) => real.starts_with(root),
        // Not on disk: judge the path as written, refusing any `..`.
        Err(_) => {
            path.is_absolute()
                && !path.components().any(|c| matches!(c, Component::ParentDir))
                && path.starts_with(root)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn roots() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let uploads = tmp.path().join("uploads");
        let recordings = tmp.path().join("recordings");
        std::fs::create_dir_all(uploads.join("abc")).unwrap();
        std::fs::create_dir_all(recordings.join("Meeting 1")).unwrap();
        std::fs::write(uploads.join("abc/talk.mp3"), b"x").unwrap();
        (tmp, uploads.canonicalize().unwrap(), recordings.canonicalize().unwrap())
    }

    #[test]
    fn guard_confines_client_paths() {
        let (tmp, uploads, recordings) = roots();
        let paths = Paths { data: tmp.path(), uploads: &uploads, recordings: &recordings };
        let upload = uploads.join("abc/talk.mp3").to_string_lossy().to_string();
        let meeting = recordings.join("Meeting 1").to_string_lossy().to_string();

        assert!(guard("validate_audio_file_command", &mut json!({"path": upload}), &paths).is_ok());
        assert!(guard("validate_audio_file_command", &mut json!({"path": "/etc/passwd"}), &paths).is_err());
        let escape = format!("{}/../../etc/passwd", uploads.display());
        assert!(guard("start_import_audio_command", &mut json!({"sourcePath": escape}), &paths).is_err());
        assert!(guard("has_audio_checkpoints", &mut json!({"meetingFolder": meeting}), &paths).is_ok());
        let missing = recordings.join("gone").to_string_lossy().to_string();
        assert!(guard("has_audio_checkpoints", &mut json!({"meetingFolder": missing}), &paths).is_ok());
        assert!(guard("api_save_transcript", &mut json!({"folderPath": null}), &paths).is_ok());
        assert!(guard("api_save_transcript", &mut json!({"folderPath": "/tmp"}), &paths).is_err());
        assert!(guard("api_get_meetings", &mut json!({}), &paths).is_ok());
    }

    #[test]
    fn guard_limits_plugins_and_store_paths() {
        let (tmp, uploads, recordings) = roots();
        let paths = Paths { data: tmp.path(), uploads: &uploads, recordings: &recordings };
        for cmd in ["plugin:process|exit", "plugin:updater|check", "plugin:dialog|open", "plugin:notification|notify", "plugin:window|close", "plugin:fs|read_file"] {
            assert!(guard(cmd, &mut json!({}), &paths).is_err(), "{cmd} must be refused");
        }
        for cmd in ["plugin:path|resolve_directory", "plugin:app|version", "plugin:resources|close", "plugin:store|set"] {
            assert!(guard(cmd, &mut json!({}), &paths).is_ok(), "{cmd} must be allowed");
        }
        assert!(guard("plugin:store|load", &mut json!({"path": "preferences.json"}), &paths).is_ok());
        for bad in ["meeting_minutes.sqlite", "/etc/passwd.json", "../x.json", "a/b.json", "a\\b.json", ""] {
            assert!(guard("plugin:store|load", &mut json!({"path": bad}), &paths).is_err(), "{bad}");
            assert!(guard("plugin:store|get_store", &mut json!({"path": bad}), &paths).is_err(), "{bad}");
        }
    }

    #[test]
    fn guard_refuses_server_side_actions_and_rewrites_paths() {
        let (tmp, uploads, recordings) = roots();
        let paths = Paths { data: tmp.path(), uploads: &uploads, recordings: &recordings };
        for cmd in ["read_audio_file", "open_meeting_folder", "init_analytics", "check_homebrew_database"] {
            assert!(guard(cmd, &mut json!({}), &paths).is_err(), "{cmd} must be refused");
        }

        let mut stop = json!({"args": {"save_path": "/etc/cron.d/x"}});
        guard("stop_recording", &mut stop, &paths).unwrap();
        assert_eq!(stop["args"]["save_path"], json!(tmp.path().join("recording.wav").to_string_lossy()));

        let mut prefs = json!({"preferences": {"save_folder": "/", "auto_save": true}});
        guard("set_recording_preferences", &mut prefs, &paths).unwrap();
        assert_eq!(prefs["preferences"]["save_folder"], json!(recordings.to_string_lossy()));
        assert_eq!(prefs["preferences"]["auto_save"], json!(true));
    }

    #[test]
    fn frames_embed_raw_json() {
        assert_eq!(
            result_frame("a\"1", Ok(r#"{"x":1}"#.into())),
            r#"{"t":"result","id":"a\"1","ok":true,"value":{"x":1}}"#
        );
        assert_eq!(
            result_frame("2", Err(Value::String("No recording in progress".into()))),
            r#"{"t":"result","id":"2","ok":false,"error":"No recording in progress"}"#
        );
        assert_eq!(event_frame(7, "x", ""), r#"{"t":"event","seq":7,"event":"x","payload":null}"#);
        assert!(valid_event_name("tauri://drag-drop") && !valid_event_name("a b") && !valid_event_name(""));
    }

    #[test]
    fn results_survive_reconnects() {
        let results = Results::default();

        // Finished while the tab's socket was gone.
        let (old, old_rx) = mpsc::unbounded_channel();
        results.start("1", &old);
        drop(old_rx);
        results.finish("1", "done".into());
        let (new, mut new_rx) = mpsc::unbounded_channel();
        results.resume("1", &new);
        assert_eq!(new_rx.try_recv().unwrap(), "done");

        // Queued on a socket that died before the browser read it: resume sends it again.
        let (live, _live_rx) = mpsc::unbounded_channel();
        results.start("2", &live);
        results.finish("2", "two".into());
        results.resume("2", &new);
        assert_eq!(new_rx.try_recv().unwrap(), "two");

        // Never received: the client is told to re-send.
        results.resume("3", &new);
        assert_eq!(new_rx.try_recv().unwrap(), r#"{"t":"unknown","id":"3"}"#);
    }

    #[test]
    fn reconnects_replay_missed_events() {
        let hub = EventHub::new();
        hub.publish("a", "1");
        hub.publish("b", "2");
        let boot = Some(hub.boot.clone());

        // Fresh page: no replay; hello carries the current sequence number.
        let fresh = hub.subscribe(&ResumeQuery::default());
        assert!(fresh.hello.contains(r#""seq":2"#) && fresh.replay.is_empty() && !fresh.gap);

        let again = hub.subscribe(&ResumeQuery { boot: boot.clone(), since: Some(1) });
        assert_eq!(again.replay.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(), vec![2]);
        assert!(!again.gap);

        // Another server process: nothing to replay.
        let other = hub.subscribe(&ResumeQuery { boot: Some("old".into()), since: Some(1) });
        assert!(other.replay.is_empty());

        // Evicted history is reported as a gap.
        for i in 0..EVENT_LOG_FRAMES {
            hub.publish("c", &i.to_string());
        }
        let late = hub.subscribe(&ResumeQuery { boot, since: Some(1) });
        assert!(late.gap && late.replay.len() == EVENT_LOG_FRAMES);
    }
}
