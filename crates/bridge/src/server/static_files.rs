//! Serves the PWA's build output (`crates/web/dist`), per `crates/web/README.md`:
//!
//! - files are served as they are, with `Cache-Control: no-cache` so the
//!   browser always revalidates. That is required for `/sw.js` (a stale
//!   worker would pin an old app) and harmless for the rest, which the
//!   service worker caches itself;
//! - an unknown path that a browser navigates to gets `/index.html` (the SPA
//!   fallback), so deep links like `/agent/<id>` work before the service
//!   worker is installed. A request is a navigation when it accepts
//!   `text/html` or its last segment has no file extension; a missing asset
//!   such as `/missing.js` stays a 404 rather than turning into HTML.
//!
//! The `/v1` prefix belongs to the API: anything under it that reaches this
//! handler (e.g. `/v1/`, which the nested router does not match) is a 404,
//! never the shell.
//!
//! `ServeDir` follows symlinks, so before serving, the requested file (and
//! for a directory, its `index.html`; for a navigation, the shell) is
//! resolved and must lie inside the canonical static dir. A symlink pointing
//! out of the build is a 404.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use percent_encoding::percent_decode_str;
use tower::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};

use super::AppState;

/// The SPA shell, relative to the static dir.
const INDEX: &str = "index.html";

/// The static dir and the services that serve it, built once.
#[derive(Debug, Clone)]
pub(crate) struct StaticFiles {
    /// The static dir as configured.
    dir: Arc<Path>,
    /// The static dir, canonicalized once it exists (it may be deployed after
    /// the bridge starts).
    root: Arc<OnceLock<PathBuf>>,
    files: ServeDir,
    files_or_shell: ServeDir<ServeFile>,
}

impl StaticFiles {
    pub(crate) fn new(dir: &Path) -> Self {
        let files = ServeDir::new(dir).append_index_html_on_directories(true);
        let files_or_shell = files.clone().fallback(ServeFile::new(dir.join(INDEX)));
        let root = OnceLock::new();
        if let Ok(real) = std::fs::canonicalize(dir) {
            let _ = root.set(real);
        }
        Self {
            dir: dir.into(),
            root: Arc::new(root),
            files,
            files_or_shell,
        }
    }

    /// The canonical static dir, or `None` while it does not exist.
    async fn root(&self) -> Option<&Path> {
        if let Some(root) = self.root.get() {
            return Some(root);
        }
        let real = tokio::fs::canonicalize(&*self.dir).await.ok()?;
        Some(self.root.get_or_init(|| real))
    }

    /// Whether `path`, if it exists, resolves to a location inside `root`
    /// (following every symlink). Missing paths pass: `ServeDir` answers them.
    async fn stays_inside(root: &Path, path: &Path) -> bool {
        let Ok(real) = tokio::fs::canonicalize(path).await else {
            return true;
        };
        if !real.starts_with(root) {
            return false;
        }
        if tokio::fs::metadata(&real).await.is_ok_and(|m| m.is_dir()) {
            return Box::pin(Self::stays_inside(root, &real.join(INDEX))).await;
        }
        true
    }

    /// The file a request path names, as `ServeDir` would look it up, or
    /// `None` when `ServeDir` itself refuses the path (`..`, bad encoding).
    fn file_for(&self, uri_path: &str) -> Option<PathBuf> {
        let decoded = percent_decode_str(uri_path).decode_utf8().ok()?;
        let mut path = self.dir.to_path_buf();
        for segment in decoded.split('/') {
            match segment {
                "" | "." => {}
                ".." => return None,
                s if s.contains('\\') => return None,
                s => path.push(s),
            }
        }
        Some(path)
    }
}

/// Fallback handler for every non-`/v1` path.
pub(crate) async fn serve(State(state): State<AppState>, request: Request) -> Response {
    if is_api_path(request.uri().path()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let files = &state.files;
    let Some(root) = files.root().await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(path) = files.file_for(request.uri().path())
        && !StaticFiles::stays_inside(root, &path).await
    {
        tracing::warn!(
            path = request.uri().path(),
            "refusing a symlink out of the static dir"
        );
        return StatusCode::NOT_FOUND.into_response();
    }
    let shell =
        is_navigation(&request) && StaticFiles::stays_inside(root, &files.dir.join(INDEX)).await;
    let mut response = if shell {
        files
            .files_or_shell
            .clone()
            .oneshot(request)
            .await
            .map(|r| r.map(Body::new).into_response())
    } else {
        files
            .files
            .clone()
            .oneshot(request)
            .await
            .map(|r| r.map(Body::new).into_response())
    }
    .unwrap_or_else(|never| match never {});
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

fn is_api_path(path: &str) -> bool {
    path == "/v1" || path.starts_with("/v1/")
}

/// A GET/HEAD that a browser would answer with a page.
fn is_navigation(request: &Request) -> bool {
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        return false;
    }
    let accepts_html = request
        .headers()
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| v.contains("text/html"));
    let last_segment = request.uri().path().rsplit('/').next().unwrap_or_default();
    accepts_html || Path::new(last_segment).extension().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: Method, path: &str, accept: Option<&str>) -> Request {
        let mut builder = Request::builder().method(method).uri(path);
        if let Some(accept) = accept {
            builder = builder.header(header::ACCEPT, accept);
        }
        builder.body(Body::empty()).expect("request")
    }

    #[test]
    fn navigations_are_recognized() {
        assert!(is_navigation(&request(
            Method::GET,
            "/agent/w1:p1~abc",
            None
        )));
        assert!(is_navigation(&request(Method::GET, "/", None)));
        assert!(is_navigation(&request(Method::HEAD, "/settings", None)));
        assert!(is_navigation(&request(
            Method::GET,
            "/agent/w1.p1",
            Some("text/html,application/xhtml+xml")
        )));
    }

    #[test]
    fn the_api_prefix_is_recognized() {
        assert!(is_api_path("/v1"));
        assert!(is_api_path("/v1/"));
        assert!(is_api_path("/v1/agents/x"));
        assert!(!is_api_path("/v1x"));
        assert!(!is_api_path("/agent/v1"));
    }

    #[test]
    fn assets_and_other_methods_are_not_navigations() {
        assert!(!is_navigation(&request(Method::GET, "/missing.js", None)));
        assert!(!is_navigation(&request(
            Method::GET,
            "/a.wasm",
            Some("*/*")
        )));
        assert!(!is_navigation(&request(Method::POST, "/agent/x", None)));
    }
}
