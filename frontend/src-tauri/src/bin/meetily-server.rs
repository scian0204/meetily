//! Headless Meetily server for the self-hosted web deployment.
//!
//! Serves the Next.js static export plus the HTTP/SSE/WebSocket API the browser
//! build talks to. The desktop binary (`meetily`) is unaffected.
//!
//! Environment:
//!   MEETILY_PASSWORD       shared login password. Required unless bound to loopback.
//!   MEETILY_BIND           listen address, default 0.0.0.0:8080
//!   MEETILY_DATA_DIR       sqlite + models + recordings, default /data
//!   MEETILY_WEB_ROOT       Next.js export directory, default ./web
//!   MEETILY_ALLOW_NO_AUTH  set to 1 to run without a password on a public bind
//!   MEETILY_COOKIE_SECURE  set to 1 when terminating TLS in front of the server
//!   OLLAMA_ENDPOINT        overrides the endpoint stored in settings

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use app_lib::server::{router, ServerConfig, ServerState};

fn env_flag(key: &str) -> bool {
    matches!(std::env::var(key).unwrap_or_default().as_str(), "1" | "true" | "yes")
}

fn env_string(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info,app_lib=info");
    }
    env_logger::init();

    let bind: SocketAddr = env_string("MEETILY_BIND")
        .unwrap_or_else(|| "0.0.0.0:8080".to_string())
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid MEETILY_BIND: {}", e))?;

    let password = env_string("MEETILY_PASSWORD");

    // Refuse to expose an unauthenticated instance beyond loopback: everything in the
    // database — every meeting transcript — is readable by anyone who can reach it.
    if password.is_none() && !bind.ip().is_loopback() && !env_flag("MEETILY_ALLOW_NO_AUTH") {
        anyhow::bail!(
            "MEETILY_PASSWORD is not set and {} is not loopback. Set MEETILY_PASSWORD, bind to \
             127.0.0.1, or set MEETILY_ALLOW_NO_AUTH=1 if the server is already protected by a \
             reverse proxy or a private network.",
            bind
        );
    }
    if password.is_none() {
        log::warn!("[server] running without authentication — every visitor sees every meeting");
    }

    let data_dir =
        PathBuf::from(env_string("MEETILY_DATA_DIR").unwrap_or_else(|| "/data".to_string()));
    let web_root =
        PathBuf::from(env_string("MEETILY_WEB_ROOT").unwrap_or_else(|| "web".to_string()));

    let config = ServerConfig {
        password,
        cookie_secure: env_flag("MEETILY_COOKIE_SECURE"),
        session_ttl: Duration::from_secs(60 * 60 * 24 * 30),
        ollama_endpoint: env_string("OLLAMA_ENDPOINT"),
    };

    let state = ServerState::new(data_dir.clone(), config).await?;
    log::info!("[server] data directory: {}", data_dir.display());

    let web_root = if web_root.join("index.html").exists() {
        log::info!("[server] serving web UI from {}", web_root.display());
        Some(web_root)
    } else {
        log::warn!("[server] no index.html under {} — serving the API only", web_root.display());
        None
    };

    let app = router(state, web_root);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    log::info!("[server] listening on http://{}", bind);
    axum::serve(listener, app).await?;
    Ok(())
}
