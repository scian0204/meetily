//! meetily-server: the desktop app's Rust core, run headless behind a browser UI.
//!
//! The same Tauri app (plugins, managed state, every command) is built on tauri's
//! MockRuntime instead of a webview. The browser loads the static Next.js export
//! plus `/__meetily/runtime.js`, which implements `window.__TAURI_INTERNALS__` over
//! a WebSocket, so the unmodified UI calls the real command handlers here.
//! Microphone / tab audio streams in over a second WebSocket and replaces the cpal
//! devices (`audio::web_source`). Transcription, summaries, ffmpeg and model
//! downloads all run on this machine.
//!
//! Environment:
//! - `MEETILY_BIND`: listen address (default `127.0.0.1:8080`)
//! - `MEETILY_WEB_ROOT`: static UI directory (default `<exe dir>/web`)
//! - `MEETILY_PASSWORD`: login password; required unless bound to loopback
//! - `MEETILY_ALLOW_NO_AUTH=1`: allow a non-loopback bind without a password
//! - `MEETILY_COOKIE_SECURE=1`: mark the session cookie `Secure` (HTTPS proxy in front)
//! - `MEETILY_ALLOWED_HOSTS`: without a password, host names accepted besides loopback
//!   names and IP addresses (DNS-rebinding protection), comma-separated
//! - `MEETILY_RECORDINGS_DIR`: meeting folders (default `<app data>/recordings`)
//!
//! App data (DB, models, settings) lives in `$XDG_DATA_HOME` or `$HOME/.local/share`,
//! under `com.meetily.ai`.

mod auth;
mod ipc;
mod media;
mod web;

use anyhow::Context;
use axum::{
    routing::{get, post, put},
    Router,
};
use std::{
    collections::HashSet,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tauri::{test::MockRuntime, AppHandle, Manager, WebviewWindow};

/// Must match `identifier` in tauri.conf.json: tauri derives app_data_dir from it.
const IDENTIFIER: &str = "com.meetily.ai";

pub struct Server {
    app: AppHandle<MockRuntime>,
    /// The mock "main" window every invoke is dispatched through.
    webview: WebviewWindow<MockRuntime>,
    /// App events fanned out to every connected browser, with a replay log.
    events: Arc<ipc::EventHub>,
    /// Event names already bridged from the app to `events`.
    listened: Mutex<HashSet<String>>,
    results: ipc::Results,
    auth: auth::Auth,
    web_root: PathBuf,
    data_dir: PathBuf,
    uploads_dir: PathBuf,
    recordings_dir: PathBuf,
}

type Shared = Arc<Server>;

pub fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    if let Err(e) = run() {
        log::error!("meetily-server: {e:#}");
        std::process::exit(1);
    }
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn run() -> anyhow::Result<()> {
    let bind: SocketAddr = env("MEETILY_BIND")
        .unwrap_or_else(|| "127.0.0.1:8080".into())
        .parse()
        .context("invalid MEETILY_BIND")?;
    let password = env("MEETILY_PASSWORD");
    if password.is_none()
        && !bind.ip().is_loopback()
        && env("MEETILY_ALLOW_NO_AUTH").as_deref() != Some("1")
    {
        anyhow::bail!(
            "refusing to listen on {bind} without MEETILY_PASSWORD \
             (set MEETILY_ALLOW_NO_AUTH=1 to run unauthenticated)"
        );
    }

    let data_dir = dirs::data_dir()
        .context("no data directory: set HOME or XDG_DATA_HOME")?
        .join(IDENTIFIER);
    let recordings_dir = match env("MEETILY_RECORDINGS_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => {
            let dir = data_dir.join("recordings");
            // Still single-threaded here, so mutating the environment is sound.
            // get_default_recordings_folder() reads it for every recording/import.
            std::env::set_var("MEETILY_RECORDINGS_DIR", &dir);
            dir
        }
    };
    let uploads_dir = data_dir.join("uploads");
    for dir in [&data_dir, &recordings_dir, &uploads_dir] {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let web_root = env("MEETILY_WEB_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(default_web_root);
    if !web_root.join("index.html").is_file() {
        log::warn!(
            "No web UI at {} (set MEETILY_WEB_ROOT to the Next.js `out` directory)",
            web_root.display()
        );
    }

    crate::audio::web_source::enable();

    let mut app = crate::configure(tauri::test::mock_builder())
        .setup(|app| {
            crate::init_core(app.handle());
            Ok(())
        })
        .build(tauri::generate_context!())
        .context("failed to build the Meetily app")?;
    // Runs setup once: creates the mock "main" window, opens the DB, starts engines.
    // Never call app.run() on MockRuntime: its loop serves one main-thread task per second.
    #[allow(deprecated)]
    app.run_iteration(|_, _| {});
    let webview = app
        .get_webview_window("main")
        .context("mock main window missing")?;
    let app_handle = app.handle().clone();
    if app_handle.path().app_data_dir().ok().as_deref() != Some(data_dir.as_path()) {
        log::warn!("app data dir differs from {}", data_dir.display());
    }

    let server = Arc::new(Server {
        app: app_handle,
        webview,
        events: Arc::new(ipc::EventHub::new()),
        listened: Mutex::new(HashSet::new()),
        results: ipc::Results::default(),
        auth: auth::Auth::new(
            password,
            env("MEETILY_COOKIE_SECURE").as_deref() == Some("1"),
            env("MEETILY_ALLOWED_HOSTS")
                .map(|hosts| hosts.split(',').map(|h| h.trim().to_string()).filter(|h| !h.is_empty()).collect())
                .unwrap_or_default(),
        ),
        web_root,
        // Canonical roots: the path guards compare canonicalized client paths against them.
        recordings_dir: recordings_dir.canonicalize()?,
        uploads_dir: uploads_dir.canonicalize()?,
        data_dir,
    });

    let result = tauri::async_runtime::block_on(serve(server, bind));
    drop(app);
    result
}

fn default_web_root() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("web")))
        .unwrap_or_else(|| PathBuf::from("web"))
}

async fn serve(server: Shared, bind: SocketAddr) -> anyhow::Result<()> {
    ipc::spawn_housekeeping(server.clone());

    let router = Router::new()
        .route("/api/ws", get(ipc::socket))
        .route("/api/audio", get(media::audio_socket))
        .route(
            "/api/upload",
            put(media::upload).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route("/api/meetings/:id/files/", get(media::meeting_files))
        .route("/api/meetings/:id/files/:name", get(media::meeting_file))
        .route("/api/session", get(|| async { axum::http::StatusCode::NO_CONTENT }))
        .route("/api/login", post(auth::login))
        .route("/api/logout", post(auth::logout))
        .route("/login", get(auth::login_page))
        .route("/__meetily/runtime.js", get(web::runtime_js))
        .route("/__meetily/audio-worklet.js", get(web::worklet_js))
        .fallback(web::static_files)
        .layer(axum::middleware::from_fn_with_state(server.clone(), auth::guard))
        .with_state(server.clone());

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("cannot bind {bind}"))?;
    log::info!("meetily-server listening on http://{bind}");

    // Graceful shutdown waits for open connections, and browsers keep WebSockets open:
    // give in-flight requests a few seconds, then stop regardless.
    let server_future = axum::serve(listener, router).with_graceful_shutdown(shutdown_signal());
    tokio::select! {
        result = server_future => result?,
        _ = async {
            shutdown_signal().await;
            tokio::time::sleep(Duration::from_secs(3)).await;
        } => log::info!("closing remaining connections"),
    }

    log::info!("shutting down");
    crate::shutdown_cleanup(&server.app).await;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(e) => {
                log::warn!("SIGTERM handler unavailable: {e}");
                std::future::pending::<()>().await
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
