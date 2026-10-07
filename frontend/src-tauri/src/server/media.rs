//! Browser audio ingest, uploads for audio import, and meeting file downloads.

use super::Shared;
use crate::audio::{web_source, RecordingDeviceType};
use axum::{
    body::Body,
    extract::{
        ws::{CloseFrame, Message, WebSocketUpgrade},
        Path as UrlPath, Query, Request, State,
    },
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    Json,
};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::Duration,
};
use tauri::Manager;
use tokio::io::AsyncWriteExt;
use tower_http::services::ServeFile;

/// The capture feeding the recorder: (socket id, capture token), and when it last sent audio.
static ACTIVE_CAPTURE: Mutex<Option<(u64, String)>> = Mutex::new(None);
static NEXT_AUDIO_SOCKET: AtomicU64 = AtomicU64::new(1);
static LAST_AUDIO_FRAME_MS: AtomicU64 = AtomicU64::new(0);
/// Close code telling a tab that another tab is already streaming (runtime.js AUDIO_BUSY).
const AUDIO_BUSY: u16 = 4409;
/// Another capture that has sent audio this recently is live and is not displaced.
const AUDIO_LIVE_WINDOW: Duration = Duration::from_secs(3);
/// A capturing browser sends a frame every 20 ms; a socket silent this long is dead
/// (half-open after a network change) and is closed.
const AUDIO_READ_TIMEOUT: Duration = Duration::from_secs(15);

/// Same cap as the import size check (audio/import.rs).
const MAX_UPLOAD_BYTES: u64 = 20 * 1024 * 1024 * 1024;
const UPLOAD_TTL: Duration = Duration::from_secs(6 * 3600);

#[derive(Deserialize)]
pub struct AudioQuery {
    /// Random per browser capture; its reconnects reuse it.
    #[serde(default)]
    capture: String,
}

pub async fn audio_socket(Query(query): Query<AudioQuery>, ws: WebSocketUpgrade) -> Response {
    ws.max_message_size(1 << 20).on_upgrade(|mut socket| async move {
        let id = NEXT_AUDIO_SOCKET.fetch_add(1, Ordering::SeqCst);
        if !claim_capture(id, &query.capture) {
            // A second tab (e.g. one re-attaching after load) must not hijack a live recording.
            log::info!("refusing a second browser audio socket while another capture streams");
            let close = CloseFrame { code: AUDIO_BUSY, reason: "another tab is streaming audio".into() };
            let _ = socket.send(Message::Close(Some(close))).await;
            return;
        }
        log::info!("browser audio connected");
        let connected = std::time::Instant::now();
        let mut samples_received = 0usize;
        while let Ok(Some(Ok(message))) = tokio::time::timeout(AUDIO_READ_TIMEOUT, socket.recv()).await {
            if !is_active(id) {
                log::info!("browser audio socket superseded by a newer one");
                break;
            }
            match message {
                Message::Binary(frame) => {
                    LAST_AUDIO_FRAME_MS.store(now_ms(), Ordering::SeqCst);
                    match decode_frame(&frame) {
                        Ok(decoded) => {
                            samples_received += decoded[0].1.len();
                            web_source::push_frame(decoded);
                        }
                        Err(e) => log::warn!("dropping audio frame: {e}"),
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
        {
            let mut active = ACTIVE_CAPTURE.lock().unwrap();
            if active.as_ref().is_some_and(|(socket, _)| *socket == id) {
                *active = None;
            }
        }
        // Less audio than connected time means the browser could not capture in real time.
        log::info!(
            "browser audio disconnected: {:.1}s of audio in {:.1}s",
            samples_received as f64 / web_source::WEB_SAMPLE_RATE as f64,
            connected.elapsed().as_secs_f64()
        );
    })
}

/// The same capture (a reconnect) always takes over; another one only once the active
/// capture has gone silent (its tab was closed or reloaded, or its network dropped).
fn claim_capture(socket: u64, token: &str) -> bool {
    let mut active = ACTIVE_CAPTURE.lock().unwrap();
    let silent_for = now_ms().saturating_sub(LAST_AUDIO_FRAME_MS.load(Ordering::SeqCst));
    let busy = match active.as_ref() {
        Some((_, active_token)) => {
            (token.is_empty() || active_token != token)
                && silent_for < AUDIO_LIVE_WINDOW.as_millis() as u64
        }
        None => false,
    };
    if !busy {
        *active = Some((socket, token.to_string()));
    }
    !busy
}

fn is_active(socket: u64) -> bool {
    ACTIVE_CAPTURE.lock().unwrap().as_ref().is_some_and(|(active, _)| *active == socket)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// Frame layout: byte 0 is a source mask (bit 0 microphone, bit 1 system/tab audio),
/// then little-endian i16 PCM at 48 kHz with the present sources interleaved in that order.
fn decode_frame(frame: &[u8]) -> Result<web_source::Frame, &'static str> {
    let (&mask, pcm) = frame.split_first().ok_or("empty frame")?;
    let sources: Vec<RecordingDeviceType> = [
        (1u8, RecordingDeviceType::Microphone),
        (2u8, RecordingDeviceType::System),
    ]
    .into_iter()
    .filter(|(bit, _)| mask & bit != 0)
    .map(|(_, source)| source)
    .collect();
    let count = sources.len();
    if count == 0 || pcm.len() % (2 * count) != 0 {
        return Err("malformed frame");
    }
    let mut channels = vec![Vec::with_capacity(pcm.len() / 2 / count); count];
    for (i, sample) in pcm.chunks_exact(2).enumerate() {
        // Same scaling as the cpal i16 path (audio/stream.rs).
        channels[i % count].push(i16::from_le_bytes([sample[0], sample[1]]) as f32 / i16::MAX as f32);
    }
    Ok(sources.into_iter().zip(channels).collect())
}

#[derive(Deserialize)]
pub struct UploadQuery {
    name: String,
}

/// `PUT /api/upload?name=<file name>` with the raw file as body; replies `{"path"}`.
/// Files keep their original name: import uses it as the meeting title and picks
/// the decoder by extension.
pub async fn upload(
    State(server): State<Shared>,
    Query(query): Query<UploadQuery>,
    body: Body,
) -> Response {
    let dir = server.uploads_dir.join(uuid::Uuid::new_v4().to_string());
    let path = dir.join(sanitize_file_name(&query.name));
    match write_upload(&dir, &path, body).await {
        Ok(bytes) => {
            log::info!("upload stored: {} ({bytes} bytes)", path.display());
            Json(json!({ "path": path })).into_response()
        }
        Err((status, error)) => {
            log::warn!("upload of {:?} failed: {error}", query.name);
            if let Err(e) = tokio::fs::remove_dir_all(&dir).await {
                log::warn!("could not remove partial upload {}: {e}", dir.display());
            }
            (status, Json(json!({ "error": error }))).into_response()
        }
    }
}

async fn write_upload(dir: &Path, path: &Path, body: Body) -> Result<u64, (StatusCode, String)> {
    let io = |e: std::io::Error| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    tokio::fs::create_dir_all(dir).await.map_err(io)?;
    let mut file = tokio::fs::File::create(path).await.map_err(io)?;
    let mut stream = body.into_data_stream();
    let mut written = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| (StatusCode::BAD_REQUEST, format!("upload interrupted: {e}")))?;
        written += chunk.len() as u64;
        if written > MAX_UPLOAD_BYTES {
            return Err((StatusCode::PAYLOAD_TOO_LARGE, "File is larger than 20 GB".into()));
        }
        file.write_all(&chunk).await.map_err(io)?;
    }
    file.flush().await.map_err(io)?;
    if written == 0 {
        return Err((StatusCode::BAD_REQUEST, "The file is empty".into()));
    }
    Ok(written)
}

/// Keeps the base name and extension, drops directories and characters that are
/// unsafe in file names.
fn sanitize_file_name(raw: &str) -> String {
    const MAX_CHARS: usize = 150;
    let base = raw.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| if matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.');
    if cleaned.is_empty() {
        return "upload".into();
    }
    if cleaned.chars().count() <= MAX_CHARS {
        return cleaned.to_string();
    }
    let (stem, ext) = cleaned.rsplit_once('.').unwrap_or((cleaned, ""));
    let ext: String = ext.chars().take(10).collect();
    let stem: String = stem.chars().take(MAX_CHARS - ext.len() - 1).collect();
    format!("{stem}.{ext}")
}

/// Removes upload folders older than UPLOAD_TTL. Import copies the file into the
/// meeting folder before decoding, so stale uploads are never needed again.
pub fn sweep_uploads(uploads_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(uploads_dir) else { return };
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|modified| modified.elapsed().unwrap_or_default() > UPLOAD_TTL)
            .unwrap_or(false);
        if stale {
            if let Err(e) = std::fs::remove_dir_all(entry.path()) {
                log::warn!("could not remove stale upload {}: {e}", entry.path().display());
            }
        }
    }
}

/// The meeting's folder from the DB, which must sit inside the recordings root.
async fn meeting_folder(server: &Shared, id: &str) -> Result<PathBuf, Response> {
    let fail = |status: StatusCode, message: String| (status, message).into_response();
    let state = server
        .app
        .try_state::<crate::state::AppState>()
        .ok_or_else(|| fail(StatusCode::SERVICE_UNAVAILABLE, "Database is not initialized yet".into()))?;
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT folder_path FROM meetings WHERE id = ?")
            .bind(id)
            .fetch_optional(state.db_manager.pool())
            .await
            .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let folder = row
        .and_then(|(folder,)| folder)
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, "This meeting has no recording folder".into()))?;
    let folder = PathBuf::from(folder)
        .canonicalize()
        .map_err(|_| fail(StatusCode::NOT_FOUND, "The recording folder no longer exists".into()))?;
    if !folder.starts_with(&server.recordings_dir) {
        return Err(fail(StatusCode::FORBIDDEN, "Recording folder is outside the recordings directory".into()));
    }
    Ok(folder)
}

/// Replaces "open meeting folder": lists the folder's files with download links
/// and a player for the recording.
pub async fn meeting_files(State(server): State<Shared>, UrlPath(id): UrlPath<String>) -> Response {
    let folder = match meeting_folder(&server, &id).await {
        Ok(folder) => folder,
        Err(response) => return response,
    };
    let mut names: Vec<String> = match std::fs::read_dir(&folder) {
        Ok(entries) => entries
            .flatten()
            .filter(|entry| entry.path().is_file())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| !name.starts_with('.'))
            .collect(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    names.sort();

    let title = folder.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let mut items = String::new();
    for name in &names {
        let href: String = url::form_urlencoded::byte_serialize(name.as_bytes()).collect::<String>().replace('+', "%20");
        let player = if crate::audio::constants::AUDIO_EXTENSIONS
            .iter()
            .any(|ext| name.to_lowercase().ends_with(&format!(".{ext}")))
        {
            format!(r#"<br><audio controls preload="metadata" src="{href}"></audio>"#)
        } else {
            String::new()
        };
        items.push_str(&format!(
            r#"<li><a href="{href}" download>{}</a>{player}</li>"#,
            escape_html(name)
        ));
    }
    if items.is_empty() {
        items.push_str("<li>No files yet.</li>");
    }
    Html(format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><title>{t}</title>
<style>body{{font-family:system-ui,sans-serif;max-width:720px;margin:40px auto;padding:0 16px;color:#111}}
li{{margin:12px 0}}audio{{margin-top:6px;width:100%}}</style></head>
<body><h1>{t}</h1><ul>{items}</ul></body></html>"#,
        t = escape_html(&title)
    ))
    .into_response()
}

pub async fn meeting_file(
    State(server): State<Shared>,
    UrlPath((id, name)): UrlPath<(String, String)>,
    request: Request,
) -> Response {
    let folder = match meeting_folder(&server, &id).await {
        Ok(folder) => folder,
        Err(response) => return response,
    };
    // Exactly one plain file name: no separators, no `..`, no Windows drive prefix.
    let mut parts = Path::new(&name).components();
    let plain = matches!(parts.next(), Some(std::path::Component::Normal(_))) && parts.next().is_none();
    if !plain || name.starts_with('.') || name.contains(['/', '\\', ':']) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let path = folder.join(&name);
    if !path.is_file() {
        return StatusCode::NOT_FOUND.into_response();
    }
    // ServeFile answers Range requests, so the audio player can seek.
    match ServeFile::new(path).try_call(request).await {
        Ok(response) => response.map(Body::new),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub(super) fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_split_into_sources() {
        // mic-only: two samples
        let frame = decode_frame(&[1, 0xff, 0x7f, 0x00, 0x00]).unwrap();
        assert_eq!(frame, vec![(RecordingDeviceType::Microphone, vec![1.0, 0.0])]);
        // mic + system interleaved: one frame each
        let frame = decode_frame(&[3, 0xff, 0x7f, 0x01, 0x80]).unwrap();
        assert_eq!(frame[0], (RecordingDeviceType::Microphone, vec![1.0]));
        assert_eq!(frame[1].0, RecordingDeviceType::System);
        assert!(frame[1].1[0] <= -1.0);
        assert!(decode_frame(&[]).is_err());
        assert!(decode_frame(&[0, 1, 2]).is_err());
        assert!(decode_frame(&[3, 1, 2]).is_err()); // half a stereo frame
    }

    #[test]
    fn a_capture_can_reconnect_but_not_be_hijacked() {
        *ACTIVE_CAPTURE.lock().unwrap() = None;
        assert!(claim_capture(1, "tab-a"));
        LAST_AUDIO_FRAME_MS.store(now_ms(), Ordering::SeqCst); // tab A is streaming
        assert!(!claim_capture(2, "tab-b"), "a second tab must not take over");
        assert!(!claim_capture(3, ""), "an untagged socket must not take over");
        assert!(claim_capture(4, "tab-a"), "tab A's own reconnect takes over at once");
        assert!(is_active(4) && !is_active(1));
        LAST_AUDIO_FRAME_MS.store(now_ms() - 10_000, Ordering::SeqCst); // tab A went silent
        assert!(claim_capture(5, "tab-b"));
        *ACTIVE_CAPTURE.lock().unwrap() = None;
    }

    #[test]
    fn upload_names_stay_inside_their_folder() {
        assert_eq!(sanitize_file_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_file_name("C:\\Users\\me\\talk.mp3"), "talk.mp3");
        assert_eq!(sanitize_file_name("..."), "upload");
        assert_eq!(sanitize_file_name("a<b>.wav"), "a_b_.wav");
        let long = format!("{}.m4a", "x".repeat(400));
        let kept = sanitize_file_name(&long);
        assert!(kept.ends_with(".m4a") && kept.chars().count() <= 150);
    }
}
