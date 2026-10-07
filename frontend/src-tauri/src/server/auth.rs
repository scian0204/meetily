//! Same-origin check plus optional password login with an in-memory session cookie.

use super::Shared;
use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::Next,
    response::{Html, IntoResponse, Redirect, Response},
    Json,
};
use rand::RngCore;
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

const COOKIE: &str = "meetily_session";
const SESSION_TTL: Duration = Duration::from_secs(30 * 24 * 3600);

pub struct Auth {
    password: Option<String>,
    secure_cookie: bool,
    /// Host names (besides loopback names and IP addresses) accepted without a password.
    allowed_hosts: Vec<String>,
    /// Session token -> expiry. In memory: a restart signs everyone out.
    sessions: Mutex<HashMap<String, Instant>>,
}

/// Serializes password checks: with the 1 s penalty below, the whole server tries at most
/// about one wrong password per second, however many connections an attacker opens.
static LOGIN_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

impl Auth {
    pub fn new(password: Option<String>, secure_cookie: bool, allowed_hosts: Vec<String>) -> Self {
        Self { password, secure_cookie, allowed_hosts, sessions: Mutex::new(HashMap::new()) }
    }

    fn signed_in(&self, headers: &HeaderMap) -> bool {
        let Some(token) = cookie(headers, COOKIE) else { return false };
        let mut sessions = self.sessions.lock().unwrap();
        match sessions.get(token) {
            Some(expiry) if *expiry > Instant::now() => true,
            Some(_) => {
                sessions.remove(token);
                false
            }
            None => false,
        }
    }
}

fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|pair| {
            let (key, value) = pair.trim().split_once('=')?;
            (key == name).then_some(value)
        })
}

/// Browsers send `Origin` on WebSocket handshakes and on cross-origin or unsafe
/// requests; it must name this host (directly or via a proxy's X-Forwarded-Host).
/// This blocks other sites from driving the API with the user's cookie, and from
/// reaching an unauthenticated loopback instance. Requests without `Origin` pass.
fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else { return true };
    let Some((_, authority)) = origin.to_str().ok().and_then(|o| o.split_once("://")) else {
        return false;
    };
    ["host", "x-forwarded-host"]
        .iter()
        .filter_map(|name| headers.get(*name).and_then(|value| value.to_str().ok()))
        .any(|host| host.eq_ignore_ascii_case(authority))
}

/// Without a password the server trusts whoever reaches it, so a page on another site must
/// not reach it through DNS rebinding (attacker.example resolving to 127.0.0.1): such a
/// request carries the attacker's name in Host. Loopback names and IP literals cannot be
/// rebound; other names must be listed in MEETILY_ALLOWED_HOSTS.
fn trusted_host(headers: &HeaderMap, allowed: &[String]) -> bool {
    let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) else {
        return false;
    };
    let name = match host.strip_prefix('[') {
        Some(bracketed) => bracketed.split(']').next().unwrap_or_default(), // [::1]:8080
        None => host.rsplit_once(':').map_or(host, |(name, _port)| name),
    }
    .to_ascii_lowercase();
    name == "localhost"
        || name.ends_with(".localhost")
        || name.parse::<std::net::IpAddr>().is_ok()
        || allowed.iter().any(|allowed| allowed.eq_ignore_ascii_case(&name))
}

pub async fn guard(State(server): State<Shared>, request: Request, next: Next) -> Response {
    if !same_origin(request.headers()) {
        return (StatusCode::FORBIDDEN, "cross-origin request refused").into_response();
    }
    let unauthenticated = server.auth.password.is_none();
    if unauthenticated && !trusted_host(request.headers(), &server.auth.allowed_hosts) {
        return (
            StatusCode::FORBIDDEN,
            "unknown host name: use localhost or an IP address, set MEETILY_ALLOWED_HOSTS, or set MEETILY_PASSWORD",
        )
            .into_response();
    }
    let path = request.uri().path();
    if unauthenticated
        || path == "/login"
        || path == "/api/login"
        || server.auth.signed_in(request.headers())
    {
        return next.run(request).await;
    }
    if path.starts_with("/api/") || path.starts_with("/__meetily/") {
        return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "login required" }))).into_response();
    }
    let target = request.uri().path_and_query().map_or("/", |pq| pq.as_str());
    let next_param: String = url::form_urlencoded::byte_serialize(target.as_bytes()).collect();
    Redirect::to(&format!("/login?next={next_param}")).into_response()
}

#[derive(Deserialize)]
pub struct LoginBody {
    password: String,
}

pub async fn login(State(server): State<Shared>, Json(body): Json<LoginBody>) -> Response {
    let auth = &server.auth;
    let Some(expected) = &auth.password else {
        return StatusCode::NO_CONTENT.into_response();
    };
    {
        let _gate = LOGIN_GATE.lock().await;
        if !constant_time_eq(body.password.as_bytes(), expected.as_bytes()) {
            // ponytail: global rate limit; a guessing flood also queues real sign-ins,
            // add per-client limits behind a proxy that forwards client addresses.
            tokio::time::sleep(Duration::from_secs(1)).await;
            return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "Wrong password" }))).into_response();
        }
    }

    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    {
        let mut sessions = auth.sessions.lock().unwrap();
        let now = Instant::now();
        sessions.retain(|_, expiry| *expiry > now);
        sessions.insert(token.clone(), now + SESSION_TTL);
    }
    let secure = if auth.secure_cookie { "; Secure" } else { "" };
    let cookie = format!(
        "{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{secure}",
        SESSION_TTL.as_secs()
    );
    (StatusCode::NO_CONTENT, [(header::SET_COOKIE, cookie)]).into_response()
}

pub async fn logout(State(server): State<Shared>, request: Request) -> Response {
    if let Some(token) = cookie(request.headers(), COOKIE) {
        server.auth.sessions.lock().unwrap().remove(token);
    }
    let expired = format!("{COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0");
    (StatusCode::NO_CONTENT, [(header::SET_COOKIE, expired)]).into_response()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub async fn login_page() -> Html<&'static str> {
    Html(LOGIN_PAGE)
}

const LOGIN_PAGE: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Meetily</title>
<style>
  body{margin:0;min-height:100vh;display:grid;place-items:center;background:#f6f7f9;font-family:system-ui,sans-serif;color:#111}
  form{background:#fff;padding:32px;border-radius:12px;box-shadow:0 2px 16px rgba(0,0,0,.08);width:min(320px,90vw)}
  h1{font-size:20px;margin:0 0 20px}
  input,button{width:100%;box-sizing:border-box;padding:10px 12px;font-size:15px;border-radius:8px}
  input{border:1px solid #ccc;margin-bottom:12px}
  button{border:0;background:#111;color:#fff;cursor:pointer}
  p{color:#b00020;font-size:14px;min-height:1em;margin:10px 0 0}
</style></head>
<body><form id="f"><h1>Meetily</h1>
<input id="pw" type="password" placeholder="Password" autocomplete="current-password" autofocus required>
<button>Sign in</button><p id="err"></p></form>
<script>
document.getElementById('f').addEventListener('submit', async (e) => {
  e.preventDefault();
  const err = document.getElementById('err');
  err.textContent = '';
  const res = await fetch('/api/login', {method: 'POST', headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({password: document.getElementById('pw').value})});
  if (!res.ok) { err.textContent = (await res.json().catch(() => ({}))).error || 'Sign-in failed'; return; }
  // Follow `next` only if it resolves to this origin (the URL parser drops tabs and
  // newlines, so a regex on the raw string is not enough).
  const next = new URL(new URLSearchParams(location.search).get('next') || '/', location.origin);
  location.href = next.origin === location.origin ? next.pathname + next.search + next.hash : '/';
});
</script></body></html>"#;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (key, value) in pairs {
            map.append(*key, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn origin_must_match_host() {
        assert!(same_origin(&headers(&[("host", "box:8080")])));
        assert!(same_origin(&headers(&[("host", "box:8080"), ("origin", "http://box:8080")])));
        assert!(same_origin(&headers(&[("host", "app:8080"), ("x-forwarded-host", "meet.example.com"), ("origin", "https://meet.example.com")])));
        assert!(!same_origin(&headers(&[("host", "box:8080"), ("origin", "https://evil.example")])));
        assert!(!same_origin(&headers(&[("host", "box:8080"), ("origin", "null")])));
    }

    #[test]
    fn unauthenticated_mode_accepts_only_unrebindable_hosts() {
        let allowed = vec!["meetily.lan".to_string()];
        for host in ["localhost:8080", "app.localhost", "127.0.0.1:8080", "[::1]:8080", "192.168.1.20:8080", "MEETILY.lan:8080"] {
            assert!(trusted_host(&headers(&[("host", host)]), &allowed), "{host}");
        }
        for host in ["rebind.attacker.example:8080", "localhost.attacker.example", "meetily.lan.evil:8080"] {
            assert!(!trusted_host(&headers(&[("host", host)]), &allowed), "{host}");
        }
        assert!(!trusted_host(&headers(&[]), &allowed));
    }

    #[test]
    fn session_cookie_round_trip() {
        let auth = Auth::new(Some("pw".into()), false, Vec::new());
        auth.sessions.lock().unwrap().insert("tok".into(), Instant::now() + SESSION_TTL);
        assert!(auth.signed_in(&headers(&[("cookie", "a=b; meetily_session=tok")])));
        assert!(!auth.signed_in(&headers(&[("cookie", "meetily_session=nope")])));
        assert!(!auth.signed_in(&headers(&[])));
        assert!(constant_time_eq(b"secret", b"secret") && !constant_time_eq(b"secret", b"secreT"));
    }
}
