//! Auth, static files, headers and limits, over real HTTP.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use bridge::server::{AuthFuture, AuthRejection, AuthRequest, Authenticator};

use crate::harness::{Bridge, CuttableHerdr, SENTINEL, TOKEN};

const CSP: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; \
    connect-src 'self'; img-src 'self'; object-src 'none'; base-uri 'none'; \
    form-action 'self'; frame-ancestors 'none'";

fn assert_security_headers(response: &crate::harness::HttpResponse) {
    assert_eq!(response.header("content-security-policy"), Some(CSP));
    assert_eq!(response.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(response.header("referrer-policy"), Some("no-referrer"));
}

#[tokio::test]
async fn api_requests_without_valid_credentials_are_refused() {
    let bridge = Bridge::start(CuttableHerdr::new()).await;
    for headers in [
        &[][..],
        &[("authorization", "Bearer wrong")][..],
        &[("authorization", TOKEN)][..],
    ] {
        let response = bridge.http("GET", "/v1/agents", headers).await;
        assert_eq!(response.status, 401, "{headers:?}");
        assert_eq!(response.header("www-authenticate"), Some("Bearer"));
        assert!(!response.body.contains('['), "no agent data leaks");
        assert_security_headers(&response);
    }
    // Unknown API paths are refused before they are looked up.
    assert_eq!(bridge.http("GET", "/v1/nope", &[]).await.status, 401);
    assert_eq!(bridge.get_authed("/v1/nope").await.status, 404);
    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn api_responses_are_never_cached() {
    let bridge = Bridge::start(CuttableHerdr::new()).await;
    let response = bridge.get_authed("/v1/agents").await;
    assert_eq!(response.status, 200);
    assert_eq!(response.header("cache-control"), Some("no-store"));
    assert_security_headers(&response);
    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn the_version_is_public_and_never_cached() {
    let bridge = Bridge::start(CuttableHerdr::new()).await;
    let response = bridge.http("GET", "/v1/version", &[]).await;
    assert_eq!(response.status, 200, "no credentials needed");
    assert_eq!(response.header("cache-control"), Some("no-store"));
    assert_security_headers(&response);
    let version: protocol::BridgeVersion = serde_json::from_str(&response.body).expect("json");
    assert_eq!(version, bridge::version::BUILD.wire());
    // Only GET, and nothing else under /v1 opens up with it.
    assert_eq!(bridge.http("POST", "/v1/version", &[]).await.status, 405);
    assert_eq!(bridge.http("GET", "/v1/versions", &[]).await.status, 401);
    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn serves_the_shell_with_security_headers() {
    let bridge = Bridge::start(CuttableHerdr::new()).await;
    let response = bridge.http("GET", "/", &[]).await;
    assert_eq!(response.status, 200);
    assert!(response.body.contains("<title>shell</title>"));
    assert_eq!(response.header("content-type"), Some("text/html"));
    assert_eq!(response.header("cache-control"), Some("no-cache"));
    assert_security_headers(&response);
    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn the_service_worker_is_always_revalidated() {
    let bridge = Bridge::start(CuttableHerdr::new()).await;
    let response = bridge.http("GET", "/sw.js", &[]).await;
    assert_eq!(response.status, 200);
    assert_eq!(response.body, "// service worker");
    assert_eq!(response.header("cache-control"), Some("no-cache"));
    assert!(
        response
            .header("content-type")
            .is_some_and(|t| t.contains("javascript")),
        "{:?}",
        response.header("content-type")
    );
    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn deep_links_fall_back_to_the_shell() {
    let bridge = Bridge::start(CuttableHerdr::new()).await;
    for path in [
        "/agent/w1:p1~9ca09978-ef89-46fd-9a85-118b84bee7b4",
        "/settings",
        "/pair",
    ] {
        let response = bridge.http("GET", path, &[]).await;
        assert_eq!(response.status, 200, "{path}");
        assert!(response.body.contains("<title>shell</title>"), "{path}");
        assert_security_headers(&response);
    }
    let browser = bridge
        .http("GET", "/agent/w1.p1", &[("accept", "text/html,*/*;q=0.8")])
        .await;
    assert_eq!(browser.status, 200, "a navigation even with a dot");
    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn missing_assets_and_api_paths_never_get_the_shell() {
    let bridge = Bridge::start(CuttableHerdr::new()).await;
    let missing = bridge.http("GET", "/missing-1234.js", &[]).await;
    assert_eq!(missing.status, 404);
    assert!(!missing.body.contains("shell"));
    for path in ["/v1", "/v1/", "/v1/agents/x/thread"] {
        let response = bridge.http("GET", path, &[("accept", "text/html")]).await;
        assert!(
            !response.body.contains("shell"),
            "{path}: {}",
            response.status
        );
        assert_ne!(response.status, 200, "{path}");
    }
    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn path_traversal_is_refused() {
    let bridge = Bridge::start(CuttableHerdr::new()).await;
    // The sentinel really is one level above the static dir.
    let leak = bridge.http("GET", "/leakdir/secret.txt", &[]).await;
    assert!(!leak.body.contains(SENTINEL));
    for path in [
        "/../secret.txt",
        "/%2e%2e/secret.txt",
        "/..%2fsecret.txt",
        "/%2e%2e%2fsecret.txt",
        "//../secret.txt",
        "/sw.js/../../secret.txt",
    ] {
        let response = bridge.http("GET", path, &[]).await;
        assert!(!response.body.contains(SENTINEL), "{path}");
        assert_ne!(response.status, 200, "{path}");
    }
    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn symlinks_out_of_the_static_dir_are_refused() {
    let bridge = Bridge::start(CuttableHerdr::new()).await;
    for path in ["/leak.txt", "/leakdir/secret.txt", "/leakdir", "/leakdir/"] {
        let response = bridge.http("GET", path, &[("accept", "text/html")]).await;
        assert_eq!(response.status, 404, "{path}");
        assert!(!response.body.contains(SENTINEL), "{path}");
    }
    // Ordinary files next to the links still work.
    assert_eq!(bridge.http("GET", "/app-1234.js", &[]).await.status, 200);
    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn oversized_bodies_are_refused() {
    let bridge = Bridge::start(CuttableHerdr::new()).await;
    // Over the largest body any route takes (a photo): refused before
    // auth, without reading it.
    let way_too_big = (10 * 1024 * 1024 + 1).to_string();
    let response = bridge
        .http("POST", "/v1/agents", &[("content-length", &way_too_big)])
        .await;
    assert_eq!(response.status, 413);
    // Over the ordinary limit: refused by the route.
    let body = "x".repeat(64 * 1024 + 1);
    let response = bridge.post_authed("/v1/actions", &body).await;
    assert_eq!(response.status, 413);
    bridge.stop().await.expect("clean stop");
}

/// Records the client address each request resolved to.
#[derive(Debug, Default)]
struct RecordingAuth {
    seen: Mutex<Vec<IpAddr>>,
}

impl Authenticator for RecordingAuth {
    fn authenticate<'a>(&'a self, request: AuthRequest<'a>) -> AuthFuture<'a> {
        assert_eq!(request.uri.path(), "/v1/agents", "the URI the client sent");
        self.seen.lock().expect("lock").push(request.client.0);
        Box::pin(async { Err(AuthRejection::Unauthenticated) })
    }
}

async fn client_seen(trusted_proxy: Option<IpAddr>) -> IpAddr {
    let auth = Arc::new(RecordingAuth::default());
    let bridge = Bridge::start_full(CuttableHerdr::new(), auth.clone(), |s| {
        s.server.trusted_proxy = trusted_proxy;
    })
    .await;
    bridge
        .http(
            "GET",
            "/v1/agents",
            &[("x-forwarded-for", "1.2.3.4, 198.51.100.2")],
        )
        .await;
    bridge.stop().await.expect("clean stop");
    let seen = auth.seen.lock().expect("lock");
    seen[0]
}

#[tokio::test]
async fn forwarded_for_is_trusted_only_from_the_configured_proxy() {
    let loopback: IpAddr = "127.0.0.1".parse().expect("ip");
    assert_eq!(
        client_seen(Some(loopback)).await,
        "198.51.100.2".parse::<IpAddr>().expect("ip"),
        "the proxy's right-most entry"
    );
    assert_eq!(client_seen(None).await, loopback, "header ignored");
    let other: IpAddr = "192.0.2.46".parse().expect("ip");
    assert_eq!(
        client_seen(Some(other)).await,
        loopback,
        "peer is not the proxy"
    );
}

#[tokio::test]
async fn a_static_dir_deployed_after_start_is_served() {
    let tmp = tempfile::tempdir().expect("tmp");
    let later = tmp.path().join("web-later");
    let static_dir = later.clone();
    let bridge = Bridge::start_with(CuttableHerdr::new(), move |s| {
        s.server.static_dir = static_dir;
    })
    .await;
    assert_eq!(bridge.http("GET", "/", &[]).await.status, 404);
    std::fs::create_dir(&later).expect("deploy");
    std::fs::write(later.join("index.html"), "<title>late shell</title>").expect("index");
    let response = bridge.http("GET", "/agent/x", &[]).await;
    assert_eq!(response.status, 200);
    assert!(response.body.contains("late shell"));
    bridge.stop().await.expect("clean stop");
}
