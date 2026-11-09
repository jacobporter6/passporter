//! Query strings of the read endpoints: `key=value` pairs joined by `&`.
//! Anything malformed, and any key the endpoint does not know, is a 400.

use std::str::FromStr;

use axum::http::StatusCode;

/// The `key=value` pairs of `query` (empty pairs skipped); a pair without
/// `=` is a 400.
pub(super) fn pairs(query: Option<&str>) -> impl Iterator<Item = Result<(&str, &str), StatusCode>> {
    query
        .unwrap_or_default()
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| pair.split_once('=').ok_or(StatusCode::BAD_REQUEST))
}

/// `value` parsed as a number, or a 400.
pub(super) fn number<T: FromStr>(value: &str) -> Result<T, StatusCode> {
    value.parse().map_err(|_| StatusCode::BAD_REQUEST)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_split_on_ampersands_and_equals() {
        let got: Result<Vec<_>, _> = pairs(Some("a=1&&b=")).collect();
        assert_eq!(got, Ok(vec![("a", "1"), ("b", "")]));
        assert_eq!(pairs(None).count(), 0);
        assert!(pairs(Some("a")).any(|p| p.is_err()));
        assert_eq!(number::<u32>("7"), Ok(7));
        assert_eq!(number::<u32>("-1"), Err(StatusCode::BAD_REQUEST));
    }
}
