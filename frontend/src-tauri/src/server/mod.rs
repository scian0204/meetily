//! Headless HTTP server powering the self-hosted web deployment.
//!
//! The desktop app talks to the Rust core over Tauri IPC. This module exposes the
//! same core over HTTP so a browser can drive it:
//!
//! - `POST /api/invoke/:cmd` mirrors `invoke()` (see [`dispatch`])
//! - `GET  /api/events`      mirrors Tauri events as SSE
//! - `WS   /api/audio/:id`   receives browser-captured PCM for live transcription
//! - `POST /api/upload`      transcribes an uploaded audio file
//! - everything else falls through to the static Next.js export
//!
//! Nothing here is used by the desktop binary.

pub mod audio;
pub mod dispatch;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path as UrlPath, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{from_fn_with_state, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{broadcast, Semaphore};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};
use tower_http::services::{ServeDir, ServeFile};

use crate::database::manager::DatabaseManager;
use crate::parakeet_engine::ParakeetEngine;
use crate::whisper_engine::whisper_engine::WhisperEngine;

pub const SESSION_COOKIE: &str = "meetily_session";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Error returned to the browser as `{"error": "..."}` so the shim can rethrow it
/// and existing frontend `try/catch` blocks keep working unchanged.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self { status, message: message.into() }
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::internal(e.to_string())
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        ApiError::internal(e.to_string())
    }
}

impl From<String> for ApiError {
    fn from(e: String) -> Self {
        ApiError::internal(e)
    }
}

impl From<std::io::Error> for ApiError {
    fn from(e: std::io::Error) -> Self {
        ApiError::internal(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// One Tauri-style event, forwarded to every connected browser over SSE.
#[derive(Debug, Clone, Serialize)]
pub struct EventEnvelope {
    pub event: String,
    pub payload: Value,
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// `None` disables auth. Only allowed on loopback or with an explicit override.
    pub password: Option<String>,
    pub cookie_secure: bool,
    pub session_ttl: Duration,
    /// Overrides the endpoint stored in settings so compose can point at the ollama service.
    pub ollama_endpoint: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            password: None,
            cookie_secure: false,
            session_ttl: Duration::from_secs(60 * 60 * 24 * 30),
            ollama_endpoint: None,
        }
    }
}

pub struct ServerState {
    pub db: DatabaseManager,
    pub whisper: Arc<WhisperEngine>,
    pub parakeet: Arc<ParakeetEngine>,
    pub events: broadcast::Sender<EventEnvelope>,
    /// Login sessions: token -> expiry. Memory only, so a restart just forces re-login
    /// and there is no signing key to manage or leak.
    pub sessions: DashMap<String, Instant>,
    /// Active browser recordings: session id -> live transcription state.
    pub live: DashMap<String, Arc<audio::LiveSession>>,
    /// ponytail: one inference slot for the whole server. Swap for an engine pool if
    /// concurrent meetings need to transcribe in parallel.
    pub transcribe_lock: Arc<Semaphore>,
    pub data_dir: PathBuf,
    pub http: reqwest::Client,
    pub config: ServerConfig,
}

pub type SharedState = Arc<ServerState>;

impl ServerState {
    pub async fn new(data_dir: PathBuf, config: ServerConfig) -> anyhow::Result<SharedState> {
        std::fs::create_dir_all(&data_dir)?;
        let models_dir = data_dir.join("models");
        std::fs::create_dir_all(&models_dir)?;
        std::fs::create_dir_all(data_dir.join("recordings"))?;

        let db_path = data_dir.join("meeting_minutes.sqlite").to_string_lossy().to_string();
        let db = DatabaseManager::new(&db_path, &db_path).await?;

        let whisper = Arc::new(WhisperEngine::new_with_models_dir(Some(models_dir.clone()))?);
        let parakeet = Arc::new(ParakeetEngine::new_with_models_dir(Some(models_dir))?);

        let (events, _) = broadcast::channel(1024);

        Ok(Arc::new(ServerState {
            db,
            whisper,
            parakeet,
            events,
            sessions: DashMap::new(),
            live: DashMap::new(),
            transcribe_lock: Arc::new(Semaphore::new(1)),
            data_dir,
            http: reqwest::Client::new(),
            config,
        }))
    }

    /// Broadcast a Tauri-style event. An error just means nobody is listening.
    pub fn emit(&self, event: &str, payload: Value) {
        let _ = self.events.send(EventEnvelope { event: event.to_string(), payload });
    }

    pub fn pool(&self) -> &sqlx::SqlitePool {
        self.db.pool()
    }

    pub fn meeting_dir(&self, name: &str) -> PathBuf {
        self.data_dir.join("recordings").join(sanitize_path_component(name))
    }
}

/// Keep untrusted ids/names from escaping the recordings directory.
pub fn sanitize_path_component(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let trimmed = cleaned.trim_matches('_').to_string();
    if trimmed.is_empty() {
        "untitled".to_string()
    } else {
        trimmed.chars().take(80).collect()
    }
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub password: String,
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn new_token() -> String {
    let bytes: [u8; 32] = rand::random();
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        if k.trim() == name {
            Some(v.trim().to_string())
        } else {
            None
        }
    })
}

impl ServerState {
    fn auth_required(&self) -> bool {
        self.config.password.is_some()
    }

    fn session_valid(&self, token: &str) -> bool {
        let alive = match self.sessions.get(token) {
            Some(entry) => *entry.value() > Instant::now(),
            None => return false,
        };
        if !alive {
            self.sessions.remove(token);
        }
        alive
    }
}

async fn login(
    State(state): State<SharedState>,
    Json(body): Json<LoginRequest>,
) -> Result<Response, ApiError> {
    let Some(expected) = state.config.password.as_ref() else {
        // Auth disabled: nothing to log into.
        return Ok(Json(json!({ "ok": true, "authRequired": false })).into_response());
    };
    if !constant_time_eq(body.password.as_bytes(), expected.as_bytes()) {
        log::warn!("[server] rejected login attempt");
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "Invalid password"));
    }

    let token = new_token();
    state.sessions.insert(token.clone(), Instant::now() + state.config.session_ttl);

    let max_age = state.config.session_ttl.as_secs();
    let mut cookie =
        format!("{}={}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}", SESSION_COOKIE, token, max_age);
    if state.config.cookie_secure {
        cookie.push_str("; Secure");
    }

    let mut response = Json(json!({ "ok": true })).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        cookie.parse().map_err(|_| ApiError::internal("failed to build session cookie"))?,
    );
    Ok(response)
}

async fn logout(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if let Some(token) = cookie_value(&headers, SESSION_COOKIE) {
        state.sessions.remove(&token);
    }
    let mut response = Json(json!({ "ok": true })).into_response();
    if let Ok(value) = format!("{}=; HttpOnly; Path=/; Max-Age=0", SESSION_COOKIE).parse() {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

async fn auth_status(State(state): State<SharedState>, headers: HeaderMap) -> Json<Value> {
    let authenticated = if state.auth_required() {
        cookie_value(&headers, SESSION_COOKIE).map(|t| state.session_valid(&t)).unwrap_or(false)
    } else {
        true
    };
    Json(json!({ "authRequired": state.auth_required(), "authenticated": authenticated }))
}

async fn require_auth(
    State(state): State<SharedState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if !state.auth_required() {
        return Ok(next.run(request).await);
    }
    let ok = cookie_value(request.headers(), SESSION_COOKIE)
        .map(|t| state.session_valid(&t))
        .unwrap_or(false);
    if ok {
        Ok(next.run(request).await)
    } else {
        Err(ApiError::new(StatusCode::UNAUTHORIZED, "Not authenticated"))
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `invoke()` bridge. The body is the args object the frontend already sends.
async fn invoke(
    State(state): State<SharedState>,
    UrlPath(cmd): UrlPath<String>,
    body: Option<Json<Value>>,
) -> Result<Json<Value>, ApiError> {
    let args = body.map(|Json(v)| v).unwrap_or(Value::Null);
    let value = dispatch::dispatch(&state, &cmd, args).await?;
    Ok(Json(json!({ "ok": value })))
}

async fn events(
    State(state): State<SharedState>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let stream = BroadcastStream::new(state.events.subscribe()).filter_map(|item| match item {
        Ok(envelope) => match Event::default().json_data(&envelope) {
            Ok(event) => Some(Ok(event)),
            Err(e) => {
                log::warn!("[server] failed to serialize event {}: {}", envelope.event, e);
                None
            }
        },
        // Lagged receiver: the browser refetches state on reconnect, so dropping is fine.
        Err(_) => None,
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Build the app. `web_root` is the directory of the Next.js static export;
/// pass `None` to serve the API only (tests).
pub fn router(state: SharedState, web_root: Option<PathBuf>) -> Router {
    let protected = Router::new()
        .route("/invoke/:cmd", post(invoke))
        .route("/events", get(events))
        .route("/upload", post(audio::upload))
        .route("/audio/:session_id", get(audio::live_ws))
        .route("/logout", post(logout))
        .route_layer(from_fn_with_state(state.clone(), require_auth));

    let public = Router::new()
        .route("/login", post(login))
        .route("/auth/status", get(auth_status));

    let api = protected.merge(public).with_state(state);
    let app = Router::new().nest("/api", api);

    match web_root {
        // Static export: unknown paths fall back to index.html so client routing works.
        Some(root) => {
            let index = root.join("index.html");
            app.fallback_service(ServeDir::new(root).fallback(ServeFile::new(index)))
        }
        None => app,
    }
}
