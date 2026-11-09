//! The browser [`Transport`]: `fetch`, same origin only, from the page or
//! the service worker.

use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    AbortSignal, Headers, Request, RequestCache, RequestCredentials, RequestInit, RequestRedirect,
    Response,
};

use super::{ApiError, BearerToken, HttpRequest, HttpResponse, Transport};
use crate::platform::{Global, js_error_message};

/// Sends requests with `fetch`. Paths are relative to the app's origin,
/// cookies are never sent (auth is the bearer token alone), redirects are
/// errors (the bridge never redirects API calls, and following one could
/// carry the token elsewhere), and the HTTP cache is bypassed (API data is
/// cached deliberately, in `IndexedDB`, never implicitly).
#[derive(Debug, Clone, Copy, Default)]
pub struct FetchTransport;

impl Transport for FetchTransport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, ApiError> {
        send(request, None).await
    }
}

/// [`FetchTransport`] that gives up on a request, body included, after a
/// number of milliseconds: a [`ApiError::Transport`] then, as if the
/// connection dropped. For work that holds a lock across bridge calls
/// (the push wake, the push switch), so a hung tunnel can't hold it.
#[derive(Debug, Clone, Copy)]
pub struct TimedFetch(pub u32);

impl Transport for TimedFetch {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, ApiError> {
        send(request, Some(self.0)).await
    }
}

async fn send(request: HttpRequest, timeout_ms: Option<u32>) -> Result<HttpResponse, ApiError> {
    let body = request.body.as_deref().map(JsValue::from_str);
    fetch_raw(RawRequest {
        method: request.method.as_str(),
        path: &request.path,
        headers: &request.headers,
        auth: request.auth.as_ref(),
        body: body.as_ref(),
        timeout_ms,
    })
    .await
    .map_err(|e| ApiError::Transport(js_error_message(&e)))
}

#[wasm_bindgen]
extern "C" {
    /// `AbortSignal.timeout(ms)`; throws where it doesn't exist.
    #[wasm_bindgen(js_namespace = AbortSignal, js_name = timeout, catch)]
    fn abort_after(ms: u32) -> Result<AbortSignal, JsValue>;
}

/// One request for [`fetch_raw`]: like [`HttpRequest`], but with any body
/// `fetch` accepts (JSON text, audio bytes).
#[derive(Debug)]
pub(crate) struct RawRequest<'a> {
    /// The method name.
    pub(crate) method: &'a str,
    /// Absolute path plus query.
    pub(crate) path: &'a str,
    /// Header names and values (never secrets).
    pub(crate) headers: &'a [(&'a str, &'a str)],
    /// The token to send as `Authorization: Bearer`, if any.
    pub(crate) auth: Option<&'a BearerToken>,
    /// The body, if any.
    pub(crate) body: Option<&'a JsValue>,
    /// Give up (as a transport error) if the whole exchange, body
    /// included, takes longer than this many milliseconds.
    pub(crate) timeout_ms: Option<u32>,
}

/// Sends `request` with the rules on [`FetchTransport`]: same origin, no
/// cookies, redirects are errors, no HTTP cache. The one place the app's
/// `fetch` options are set.
///
/// The timeout is an `AbortSignal.timeout` owned by the browser, so
/// dropping this future mid-request leaves nothing behind to fire into
/// freed Rust state.
pub(crate) async fn fetch_raw(request: RawRequest<'_>) -> Result<HttpResponse, JsValue> {
    let headers = Headers::new()?;
    for (name, value) in request.headers {
        headers.set(name, value)?;
    }
    if let Some(token) = request.auth {
        headers.set("Authorization", &token.header_value())?;
    }
    let init = RequestInit::new();
    init.set_method(request.method);
    init.set_headers(&headers);
    init.set_credentials(RequestCredentials::Omit);
    init.set_redirect(RequestRedirect::Error);
    init.set_cache(RequestCache::NoStore);
    if let Some(ms) = request.timeout_ms {
        // Aborts the body read too. Without `AbortSignal.timeout` (Safari
        // before 16), the request runs unbounded.
        init.set_signal(abort_after(ms).ok().as_ref());
    }
    if let Some(body) = request.body {
        init.set_body(body);
    }
    let request = Request::new_with_str_and_init(request.path, &init)?;

    let response: Response = JsFuture::from(Global::current().fetch(&request))
        .await?
        .dyn_into()?;
    // A status arrived, so this is a response even if the body read fails
    // (tunnel drop mid-body); `body: None` tells the caller which case it is.
    let body = match response.text() {
        Ok(text) => JsFuture::from(text).await.ok().and_then(|v| v.as_string()),
        Err(_) => None,
    };
    Ok(HttpResponse {
        status: response.status(),
        body,
    })
}
