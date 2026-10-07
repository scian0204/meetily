//! The static Next.js export, with the Tauri runtime polyfill injected into every page.

use super::Shared;
use axum::{
    body::Body,
    extract::{Request, State},
    http::{
        header::{CACHE_CONTROL, CONTENT_TYPE},
        StatusCode,
    },
    response::{Html, IntoResponse, Response},
};
use std::path::{Component, Path, PathBuf};
use tower_http::services::ServeDir;

const RUNTIME_JS: &str = include_str!("../../../web-runtime/runtime.js");
const WORKLET_JS: &str = include_str!("../../../web-runtime/audio-worklet.js");
/// First thing in <head>, so `window.__TAURI_INTERNALS__` exists before any app chunk runs.
const RUNTIME_TAG: &str = r#"<script src="/__meetily/runtime.js"></script>"#;

pub async fn runtime_js() -> Response {
    javascript(RUNTIME_JS)
}

pub async fn worklet_js() -> Response {
    javascript(WORKLET_JS)
}

fn javascript(source: &'static str) -> Response {
    (
        [(CONTENT_TYPE, "text/javascript; charset=utf-8"), (CACHE_CONTROL, "no-cache")],
        source,
    )
        .into_response()
}

pub async fn static_files(State(server): State<Shared>, request: Request) -> Response {
    let path = request.uri().path().to_owned();
    if path.split('/').any(|segment| segment == "..") || path.contains('\\') {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if let Some(page) = html_page(&server.web_root, &path) {
        return html(&page, StatusCode::OK).await;
    }
    let is_route = !path.rsplit('/').next().unwrap_or_default().contains('.');
    if is_route {
        // Next's 404 page renders the app shell, so it needs the runtime too.
        return html(&server.web_root.join("404.html"), StatusCode::NOT_FOUND).await;
    }
    match ServeDir::new(&server.web_root).try_call(request).await {
        Ok(response) => response.map(Body::new),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// Next's static export writes `/settings` as `settings.html`; Tauri resolves that
/// implicitly, a plain file server does not.
fn html_page(root: &Path, url_path: &str) -> Option<PathBuf> {
    let relative = url_path.trim_start_matches('/');
    // Only plain names: a Windows drive prefix (`/C:/...`) would replace `root` on join.
    if Path::new(relative).components().any(|part| !matches!(part, Component::Normal(_))) {
        return None;
    }
    let candidates = if relative.is_empty() || relative.ends_with('/') {
        vec![format!("{relative}index.html")]
    } else if relative.ends_with(".html") {
        vec![relative.to_string()]
    } else {
        vec![format!("{relative}.html"), format!("{relative}/index.html")]
    };
    candidates
        .into_iter()
        .map(|candidate| root.join(candidate))
        .find(|candidate| candidate.is_file())
}

async fn html(file: &Path, status: StatusCode) -> Response {
    match tokio::fs::read_to_string(file).await {
        Ok(page) => (status, [(CACHE_CONTROL, "no-cache")], Html(inject_runtime(&page))).into_response(),
        Err(e) => {
            log::warn!("cannot read {}: {e}", file.display());
            (status, "Meetily web UI not found on the server (check MEETILY_WEB_ROOT)").into_response()
        }
    }
}

fn inject_runtime(page: &str) -> String {
    match page.find("<head>") {
        Some(index) => {
            let at = index + "<head>".len();
            format!("{}{RUNTIME_TAG}{}", &page[..at], &page[at..])
        }
        None => format!("{RUNTIME_TAG}{page}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_goes_first_in_head() {
        assert_eq!(
            inject_runtime(r#"<html><head><script src="/_next/a.js"></script></head>"#),
            format!(r#"<html><head>{RUNTIME_TAG}<script src="/_next/a.js"></script></head>"#)
        );
        assert!(inject_runtime("<p>x</p>").starts_with(RUNTIME_TAG));
    }

    #[test]
    fn routes_resolve_to_exported_pages() {
        let root = tempfile::tempdir().unwrap();
        for file in ["index.html", "settings.html", "notes/abc.html"] {
            let path = root.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "x").unwrap();
        }
        let page = |url: &str| html_page(root.path(), url).map(|p| p.strip_prefix(root.path()).unwrap().to_path_buf());
        assert_eq!(page("/"), Some(PathBuf::from("index.html")));
        assert_eq!(page("/settings"), Some(PathBuf::from("settings.html")));
        assert_eq!(page("/notes/abc"), Some(PathBuf::from("notes/abc.html")));
        assert_eq!(page("/missing"), None);
        assert_eq!(page("/../index"), None);
    }
}
