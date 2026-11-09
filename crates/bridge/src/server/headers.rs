//! Response headers every reply carries, and the API's caching rule.

use axum::http::{HeaderName, HeaderValue, header};
use axum::response::Response;

/// The PWA's Content Security Policy, exactly as `crates/web/README.md`
/// specifies (that file and `index.html`'s `<meta>` tag mirror it).
/// `frame-ancestors` only works as a header, which is why this one matters.
pub const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; \
    script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; connect-src 'self'; \
    img-src 'self'; object-src 'none'; base-uri 'none'; form-action 'self'; \
    frame-ancestors 'none'";

const SECURITY_HEADERS: [(HeaderName, &str); 5] = [
    (header::CONTENT_SECURITY_POLICY, CONTENT_SECURITY_POLICY),
    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
    (header::REFERRER_POLICY, "no-referrer"),
    (
        HeaderName::from_static("cross-origin-opener-policy"),
        "same-origin",
    ),
    (
        HeaderName::from_static("cross-origin-resource-policy"),
        "same-origin",
    ),
];

/// Adds the security headers to any response (documents, assets, API).
pub(crate) async fn security(mut response: Response) -> Response {
    let headers = response.headers_mut();
    for (name, value) in SECURITY_HEADERS {
        headers.insert(name, HeaderValue::from_static(value));
    }
    response
}

/// API responses carry live agent state: never cache them anywhere.
pub(crate) async fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csp_matches_the_web_readme_policy() {
        let directives: Vec<&str> = CONTENT_SECURITY_POLICY.split(';').map(str::trim).collect();
        assert_eq!(
            directives,
            [
                "default-src 'self'",
                "script-src 'self' 'wasm-unsafe-eval'",
                "style-src 'self'",
                "connect-src 'self'",
                "img-src 'self'",
                "object-src 'none'",
                "base-uri 'none'",
                "form-action 'self'",
                "frame-ancestors 'none'",
            ]
        );
    }

    #[test]
    fn csp_matches_what_trunk_serves_in_dev() {
        let trunk = include_str!("../../../web/Trunk.toml");
        let line = format!("Content-Security-Policy = \"{CONTENT_SECURITY_POLICY}\"");
        assert!(
            trunk.contains(&line),
            "Trunk.toml's CSP drifted from the bridge's"
        );
    }

    #[test]
    fn every_header_value_is_valid() {
        for (_, value) in SECURITY_HEADERS {
            assert!(HeaderValue::from_str(value).is_ok(), "{value}");
        }
    }
}
