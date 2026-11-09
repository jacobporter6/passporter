//! Redacting JSON values, including JSON nested inside strings.
//!
//! Transcripts often carry text that is itself JSON or JSON-escaped: a tool
//! printing a config file, a log line with `{\"token\":\"…\"}`. Every string
//! is scanned as is, then:
//!
//! - if it parses as a JSON object or array, that document is scrubbed too
//!   (so secret *keys* inside it count), and
//! - if it contains backslash escapes, an unescaped copy is scanned (so
//!   `\"password\": \"two words\"` and `\nAIza…` are seen as written).
//!
//! Secrets found in such a copy are mapped back onto the original string, in
//! their verbatim or JSON-escaped spelling. If one cannot be located, the
//! whole string is replaced: losing a string beats leaking a secret.
//!
//! **Known gap:** a secret split across several JSON strings (e.g. streamed
//! in chunks, or a key and its value in separate array items) is invisible
//! to every rule, since each string is scanned on its own.

use std::borrow::Cow;

use serde_json::{Map, Value};

use super::rules::{Counts, Found, MARKER, Redactor, SECRET_FIELD, is_secret_key};

/// How deep strings-inside-strings are unpacked.
const MAX_DEPTH: usize = 4;

/// The rule name for a nested string whose secrets could not be mapped back.
const UNMAPPED: &str = "nested-unmapped";

/// Redacts every string in `value`, recording what it redacted in `found`.
///
/// Everything under a secret-looking key (`password`, `apiKey`, `secrets`,
/// …) is redacted whole: strings, numbers, and every value in a nested
/// array or object. So is the `value` of a name/value pair whose name is
/// secret-looking (`{"name": "DB_PASSWORD", "value": "…"}`, HAR headers).
/// Object keys go through the rules too.
pub(crate) fn scrub_value(
    value: &mut Value,
    key: Option<&str>,
    redactor: &Redactor,
    counts: &mut Counts,
    found: &mut Vec<Found>,
) {
    scrub_at(value, key, false, redactor, counts, found, 0);
}

fn scrub_at(
    value: &mut Value,
    key: Option<&str>,
    secret: bool,
    redactor: &Redactor,
    counts: &mut Counts,
    found: &mut Vec<Found>,
    depth: usize,
) {
    let secret = secret || key.is_some_and(is_secret_key);
    if secret && let Value::Number(n) = value {
        found.push(Found {
            rule: SECRET_FIELD,
            secret: n.to_string(),
            quoted: false,
        });
        *value = Value::String(format!("[REDACTED:{SECRET_FIELD}]"));
        *counts.entry(SECRET_FIELD).or_default() += 1;
        return;
    }
    match value {
        Value::String(s) if secret => redact_whole(s, counts, found),
        Value::String(s) => scrub_string(s, redactor, counts, found, depth),
        Value::Array(items) => {
            for item in items {
                scrub_at(item, key, secret, redactor, counts, found, depth);
            }
        }
        Value::Object(map) => scrub_object(map, secret, redactor, counts, found, depth),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn scrub_object(
    map: &mut Map<String, Value>,
    secret: bool,
    redactor: &Redactor,
    counts: &mut Counts,
    found: &mut Vec<Found>,
    depth: usize,
) {
    let named_secret = map.contains_key("value")
        && ["name", "key"]
            .iter()
            .filter_map(|k| map.get(*k).and_then(Value::as_str))
            .any(is_secret_key);
    for (k, v) in map.iter_mut() {
        let child_secret = secret || (named_secret && k == "value");
        scrub_at(v, Some(k), child_secret, redactor, counts, found, depth);
    }
    let renames: Vec<(String, String)> = map
        .keys()
        .filter_map(|k| match redactor.redact(k, counts, found) {
            Cow::Owned(redacted) => Some((k.clone(), redacted)),
            Cow::Borrowed(_) => None,
        })
        .collect();
    for (old, new) in renames {
        if let Some(v) = map.remove(&old) {
            map.insert(new, v);
        }
    }
}

/// Replaces a whole string value under a secret key.
fn redact_whole(s: &mut String, counts: &mut Counts, found: &mut Vec<Found>) {
    if s.is_empty() || s.starts_with(MARKER) {
        return;
    }
    found.push(Found {
        rule: SECRET_FIELD,
        secret: std::mem::replace(s, format!("[REDACTED:{SECRET_FIELD}]")),
        quoted: true,
    });
    *counts.entry(SECRET_FIELD).or_default() += 1;
}

fn scrub_string(
    s: &mut String,
    redactor: &Redactor,
    counts: &mut Counts,
    found: &mut Vec<Found>,
    depth: usize,
) {
    if let Cow::Owned(redacted) = redactor.redact(s, counts, found) {
        *s = redacted;
    }
    if depth >= MAX_DEPTH {
        return;
    }
    let trimmed = s.trim_start();
    let mut inner = Vec::new();
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && let Ok(mut nested) = serde_json::from_str::<Value>(s)
    {
        scrub_at(
            &mut nested,
            None,
            false,
            redactor,
            counts,
            &mut inner,
            depth + 1,
        );
    } else if s.contains('\\') {
        let mut decoded = unescape(s);
        if decoded != *s {
            scrub_string(&mut decoded, redactor, counts, &mut inner, depth + 1);
        }
    }
    if !inner.is_empty() {
        map_back(s, &inner, counts);
        found.extend(inner);
    }
}

/// Replaces each secret found in a decoded copy in the original string `s`.
fn map_back(s: &mut String, found: &[Found], counts: &mut Counts) {
    for f in found {
        let marker = format!("[REDACTED:{}]", f.rule);
        let escaped = json_escape(&f.secret);
        let spellings: Vec<(String, String)> = if f.quoted {
            vec![
                (format!("\"{}\"", f.secret), format!("\"{marker}\"")),
                (format!("\"{escaped}\""), format!("\"{marker}\"")),
                (format!("\\\"{escaped}\\\""), format!("\\\"{marker}\\\"")),
            ]
        } else {
            vec![(f.secret.clone(), marker.clone()), (escaped, marker)]
        };
        let mut hit = false;
        for (from, to) in spellings {
            if s.contains(&from) {
                *s = s.replace(&from, &to);
                hit = true;
            }
        }
        if !hit {
            *s = format!("[REDACTED:{UNMAPPED}]");
            *counts.entry(UNMAPPED).or_default() += 1;
            return;
        }
    }
}

/// `text` as it would appear inside a JSON string literal.
pub(crate) fn json_escape(text: &str) -> String {
    let quoted = serde_json::to_string(text).unwrap_or_default();
    quoted
        .strip_prefix('"')
        .and_then(|q| q.strip_suffix('"'))
        .unwrap_or_default()
        .to_owned()
}

/// Decodes JSON string escapes (`\"`, `\\`, `\n`, `\uXXXX`, …) leniently:
/// anything that is not a valid escape is kept as written.
pub(crate) fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let decoded = match chars.peek() {
            Some('"') => Some('"'),
            Some('\\') => Some('\\'),
            Some('/') => Some('/'),
            Some('n') => Some('\n'),
            Some('t') => Some('\t'),
            Some('r') => Some('\r'),
            Some('b') => Some('\u{8}'),
            Some('f') => Some('\u{c}'),
            _ => None,
        };
        if let Some(d) = decoded {
            chars.next();
            out.push(d);
            continue;
        }
        if chars.peek() == Some(&'u') {
            let hex: String = chars.clone().skip(1).take(4).collect();
            if let Some(ch) = u32::from_str_radix(&hex, 16)
                .ok()
                .filter(|_| hex.len() == 4)
                .and_then(char::from_u32)
            {
                for _ in 0..5 {
                    chars.next();
                }
                out.push(ch);
                continue;
            }
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scrub(json: &str) -> (String, Counts) {
        let redactor = Redactor::new().expect("rules");
        let mut value: Value = serde_json::from_str(json).expect("json");
        let mut counts = Counts::new();
        scrub_value(&mut value, None, &redactor, &mut counts, &mut Vec::new());
        (value.to_string(), counts)
    }

    #[test]
    fn unescape_is_lenient() {
        assert_eq!(unescape(r#"a\"b\\c\nd=e"#), "a\"b\\c\nd=e");
        assert_eq!(
            unescape(r"keep \q and \u12 as is"),
            r"keep \q and \u12 as is"
        );
    }

    #[test]
    fn nested_json_documents_get_key_context() {
        let inner = r#"{\"db\": {\"apiToken\": \"hunter2\"}, \"user\": \"app\"}"#;
        let (out, counts) = scrub(&format!(r#"{{"content": "{inner}"}}"#));
        assert!(!out.contains("hunter2"), "{out}");
        assert!(out.contains("app"), "{out}");
        assert_eq!(counts.get(SECRET_FIELD), Some(&1));
    }

    #[test]
    fn escaped_text_is_rescanned() {
        // The string itself contains backslash-escaped quotes (double-encoded).
        let (out, _) =
            scrub(r#"{"content": "log: {\\\"password\\\": \\\"two words here\\\"} done"}"#);
        assert!(!out.contains("two words"), "{out}");
        assert!(out.contains("log:"), "{out}");
    }

    #[test]
    fn account_identities_are_redacted() {
        let (out, _) = scrub(
            r#"{"creator_user_id": "user-FAKE0000example", "creator_account_id": "acct-FAKE0000example",
                "userId": "u-123456", "orgId": "org-abcdef", "author": "kept", "id": "kept-too"}"#,
        );
        for leaked in [
            "user-FAKE0000example",
            "acct-FAKE0000example",
            "u-123456",
            "org-abcdef",
        ] {
            assert!(!out.contains(leaked), "{leaked} in {out}");
        }
        assert!(out.contains("kept") && out.contains("kept-too"), "{out}");
    }

    #[test]
    fn whole_subtrees_under_secret_keys() {
        let (out, counts) = scrub(
            r#"{"password": 123456, "secrets": ["a1", {"x": "b2"}], "tokens": {"gh": "c3"},
                "usage": {"input_tokens": 12}}"#,
        );
        for leaked in ["123456", "a1", "b2", "c3"] {
            assert!(!out.contains(leaked), "{leaked} in {out}");
        }
        assert!(out.contains(r#""input_tokens":12"#), "{out}");
        assert_eq!(counts.get(SECRET_FIELD), Some(&4));
    }

    #[test]
    fn name_value_pairs() {
        let (out, _) = scrub(
            r#"{"env": [{"name": "DB_PASSWORD", "value": "hunter22"}, {"name": "PORT", "value": "5432"}],
                "headers": [{"name": "Authorization", "value": "Basic YWxhZGRpbg=="}],
                "cookies": [{"name": "sid", "value": "s3ss10n"}]}"#,
        );
        for leaked in ["hunter22", "YWxhZGRpbg", "s3ss10n"] {
            assert!(!out.contains(leaked), "{leaked} in {out}");
        }
        assert!(out.contains("5432"), "{out}");
    }

    #[test]
    fn object_keys_go_through_the_rules() {
        let token = format!("ghp_{}", "aZ3kQ9mX2pL7vR4tW8yB1nC6aZ3kQ9mX2pL7");
        let (out, counts) = scrub(&format!(r#"{{"{token}": 1, "plain": 2}}"#));
        assert!(!out.contains(&token), "{out}");
        assert!(out.contains("plain"), "{out}");
        assert_eq!(counts.get("github"), Some(&1));
    }

    #[test]
    fn short_values_under_secret_keys_are_redacted() {
        let (out, counts) = scrub(r#"{"password": "pw", "apiToken": "x", "maxTokens": "8"}"#);
        assert!(!out.contains("\"pw\""), "{out}");
        assert!(!out.contains("\"x\""), "{out}");
        assert!(out.contains("\"8\""), "{out}");
        assert_eq!(counts.get(SECRET_FIELD), Some(&2));
    }
}
