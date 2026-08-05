//! Integration tests for the self-hosted web server.
//!
//! These drive the axum router directly (no socket, no models), covering the parts
//! that are easiest to get wrong: the auth gate, the error contract, and one database
//! round trip through the dispatch table.

use std::time::Duration;

use app_lib::server::{router, ServerConfig, ServerState, SharedState};
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

const PASSWORD: &str = "test-password";

async fn state_with_auth(dir: &std::path::Path) -> SharedState {
    ServerState::new(
        dir.to_path_buf(),
        ServerConfig {
            password: Some(PASSWORD.to_string()),
            cookie_secure: false,
            session_ttl: Duration::from_secs(60),
            ollama_endpoint: None,
        },
    )
    .await
    .expect("server state")
}

fn post(uri: &str, body: Value, cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    builder.body(Body::from(body.to_string())).expect("request")
}

async fn read_json(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("body");
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

#[tokio::test]
async fn rejects_unauthenticated_requests() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = state_with_auth(dir.path()).await;
    let app = router(state, None);

    let response = app
        .oneshot(post("/api/invoke/api_get_meetings", json!({}), None))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(read_json(response).await["error"], "Not authenticated");
}

#[tokio::test]
async fn rejects_a_wrong_password() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = state_with_auth(dir.path()).await;
    let app = router(state, None);

    let response = app
        .oneshot(post("/api/login", json!({ "password": "nope" }), None))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn login_then_save_and_read_a_meeting() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = state_with_auth(dir.path()).await;
    let app = router(state, None);

    // 1. Log in and keep the session cookie.
    let response = app
        .clone()
        .oneshot(post("/api/login", json!({ "password": PASSWORD }), None))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .expect("session cookie")
        .to_string();

    // 2. An empty database lists no meetings.
    let response = app
        .clone()
        .oneshot(post("/api/invoke/api_get_meetings", json!({}), Some(&cookie)))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(read_json(response).await["ok"], json!([]));

    // 3. Save a transcript the way the frontend does.
    let response = app
        .clone()
        .oneshot(post(
            "/api/invoke/api_save_transcript",
            json!({
                "meetingTitle": "Weekly Sync",
                "folderPath": Value::Null,
                "transcripts": [{
                    "id": "seg_0",
                    "text": "hello there",
                    "timestamp": "00:00:00",
                    "audio_start_time": 0.0,
                    "audio_end_time": 1.5,
                    "duration": 1.5
                }]
            }),
            Some(&cookie),
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let saved = read_json(response).await;
    let meeting_id = saved["ok"]["meeting_id"].as_str().expect("meeting id").to_string();
    assert!(meeting_id.starts_with("meeting-"), "got {}", meeting_id);

    // 4. It comes back, and the transcript column is renamed to `text` for the UI.
    let response = app
        .clone()
        .oneshot(post("/api/invoke/api_get_meetings", json!({}), Some(&cookie)))
        .await
        .expect("response");
    assert_eq!(read_json(response).await["ok"][0]["title"], "Weekly Sync");

    let response = app
        .clone()
        .oneshot(post(
            "/api/invoke/api_get_meeting_transcripts",
            json!({ "meetingId": meeting_id, "limit": 10, "offset": 0 }),
            Some(&cookie),
        ))
        .await
        .expect("response");
    let page = read_json(response).await;
    assert_eq!(page["ok"]["total_count"], 1);
    assert_eq!(page["ok"]["has_more"], false);
    assert_eq!(page["ok"]["transcripts"][0]["text"], "hello there");
}

#[tokio::test]
async fn unknown_commands_are_reported_not_swallowed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = ServerState::new(dir.path().to_path_buf(), ServerConfig::default())
        .await
        .expect("server state");
    let app = router(state, None);

    let response = app
        .oneshot(post("/api/invoke/definitely_not_a_command", json!({}), None))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = read_json(response).await;
    assert!(
        body["error"].as_str().unwrap_or_default().contains("definitely_not_a_command"),
        "the error should name the command, got {}",
        body
    );
}

#[tokio::test]
async fn auth_status_reports_whether_a_password_is_set() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = ServerState::new(dir.path().to_path_buf(), ServerConfig::default())
        .await
        .expect("server state");
    let app = router(state, None);

    let response = app
        .oneshot(Request::builder().uri("/api/auth/status").body(Body::empty()).unwrap())
        .await
        .expect("response");
    let body = read_json(response).await;
    assert_eq!(body["authRequired"], false);
    assert_eq!(body["authenticated"], true);
}
