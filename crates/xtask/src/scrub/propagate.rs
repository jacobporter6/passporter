//! "Redacted once, redacted everywhere": the second pass over each file.
//!
//! A secret is often recognisable in only one of its copies. A `Read` tool
//! result carries the file twice: parsed in `toolUseResult.file.content`,
//! where a `client_secret` key gives it away, and line-numbered in
//! `message.content`, where no rule can tell. The first pass collects the
//! values it redacted; this pass replaces their other occurrences in the
//! same file.
//!
//! Only values that look like secrets are propagated, so a project or
//! branch name that once sat under a secret-looking key is not wiped from
//! every path in the file:
//!
//! - values a token rule recognised by shape (provider prefixes, JWTs,
//!   private keys, high-entropy values, …), or
//! - values of at least 8 characters, from at least two character classes,
//!   with at least 3 bits/char of entropy, and not made only of lower-case
//!   letters and separators.
//!
//! Matching is done on the JSON-escaped spelling and only at token
//! boundaries: a match touching an identifier character (`[A-Za-z0-9_]`),
//! or starting inside an escape sequence, is left alone. Values made only
//! of number characters are never propagated. As a last guard, a line that
//! was valid JSON must still be valid after the pass; if it would not be,
//! every string value containing a secret is replaced whole instead.

use std::collections::BTreeSet;
use std::io::{self, BufRead, Write};

use aho_corasick::{AhoCorasick, MatchKind};
use serde_json::Value;

use super::rules::{Counts, Found, MARKER, entropy, not_placeholder};
use super::value::json_escape;
use super::{Line, read_line};

/// Shortest value propagated on shape alone.
const MIN_LEN: usize = 8;

/// Minimum entropy (bits per char) for values not recognised by shape.
const MIN_ENTROPY: f64 = 3.0;

/// Most distinct values collected per file.
const MAX_SECRETS: usize = 100_000;

/// Rules that recognise a secret by its own shape, so the value itself is
/// known to be a credential wherever it appears.
const SHAPE_RULES: &[&str] = &[
    "private-key",
    "anthropic",
    "openai",
    "github",
    "gitlab",
    "huggingface",
    "npm",
    "tailscale",
    "age",
    "docker",
    "google-oauth",
    "sendgrid",
    "aws-access-key-id",
    "aws-secret-key",
    "cloudflare-tunnel",
    "cloudflare",
    "slack",
    "google-api-key",
    "stripe",
    "jwt",
    "azure-key",
    "discord-webhook",
    "telegram",
    "high-entropy",
];

/// The rule name counted for propagated redactions.
pub(crate) const PROPAGATED: &str = "propagated";

/// The secrets redacted in one file so far.
#[derive(Debug, Default)]
pub(crate) struct Secrets {
    /// As they appear in raw JSON text.
    escaped: BTreeSet<String>,
    /// As decoded string values, for the whole-string fallback.
    plain: BTreeSet<String>,
}

impl Secrets {
    /// Adds every value in `found` that looks like a secret.
    pub(crate) fn add(&mut self, found: &[Found]) {
        for f in found {
            if self.escaped.len() >= MAX_SECRETS {
                return;
            }
            if propagatable(f) {
                self.escaped.insert(json_escape(&f.secret));
                self.plain.insert(f.secret.clone());
            }
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.escaped.is_empty()
    }

    /// Copies `input` to `output`, replacing other occurrences of the
    /// collected values with a marker. Adds the replacements to `counts` and
    /// returns how many distinct values were replaced at least once.
    pub(crate) fn rewrite(
        &self,
        mut input: impl BufRead,
        mut output: impl Write,
        cap: usize,
        counts: &mut Counts,
    ) -> io::Result<u64> {
        let build = |patterns: &BTreeSet<String>| {
            AhoCorasick::builder()
                .match_kind(MatchKind::LeftmostLongest)
                .build(patterns)
                .map_err(io::Error::other)
        };
        let escaped = build(&self.escaped)?;
        let plain = build(&self.plain)?;
        let mut used = BTreeSet::new();
        let mut line = Vec::new();
        loop {
            match read_line(&mut input, &mut line, cap)? {
                Line::Eof => break,
                // Pass one already dropped oversized lines.
                Line::Oversized => {}
                Line::Read { newline } => {
                    let (out, hits) = replace_line(&line, &escaped, &plain, &mut used);
                    output.write_all(out.as_deref().unwrap_or(&line))?;
                    if hits > 0 {
                        *counts.entry(PROPAGATED).or_default() += hits;
                    }
                    if newline {
                        output.write_all(b"\n")?;
                    }
                }
            }
        }
        output.flush()?;
        Ok(used.len() as u64)
    }
}

fn marker() -> String {
    format!("[REDACTED:{PROPAGATED}]")
}

/// One line through the pass: `(replacement, hits)`, `None` if unchanged.
fn replace_line(
    line: &[u8],
    escaped: &AhoCorasick,
    plain: &AhoCorasick,
    used: &mut BTreeSet<usize>,
) -> (Option<Vec<u8>>, u64) {
    let marker = marker();
    let mut out = Vec::new();
    let mut last = 0;
    let mut hits = 0;
    for m in escaped.find_iter(line) {
        if !at_token_boundary(line, m.start(), m.end()) {
            continue;
        }
        out.extend_from_slice(line.get(last..m.start()).unwrap_or_default());
        out.extend_from_slice(marker.as_bytes());
        last = m.end();
        hits += 1;
        used.insert(m.pattern().as_usize());
    }
    if hits == 0 {
        return (None, 0);
    }
    out.extend_from_slice(line.get(last..).unwrap_or_default());

    // Invariant: valid JSON stays valid JSON.
    let Ok(mut value) = serde_json::from_slice::<Value>(line) else {
        return (Some(out), hits);
    };
    if serde_json::from_slice::<Value>(&out).is_ok() {
        return (Some(out), hits);
    }
    let whole = redact_strings(&mut value, plain);
    (Some(value.to_string().into_bytes()), whole)
}

/// Replaces every string (value or key) containing a secret with the marker.
fn redact_strings(value: &mut Value, plain: &AhoCorasick) -> u64 {
    match value {
        Value::String(s) if plain.is_match(s.as_str()) => {
            *s = marker();
            1
        }
        Value::Array(items) => items.iter_mut().map(|v| redact_strings(v, plain)).sum(),
        Value::Object(map) => {
            let mut n = 0;
            let old = std::mem::take(map);
            for (k, mut v) in old {
                n += redact_strings(&mut v, plain);
                let k = if plain.is_match(k.as_str()) {
                    n += 1;
                    marker()
                } else {
                    k
                };
                map.insert(k, v);
            }
            n
        }
        _ => 0,
    }
}

/// Whether `line[start..end]` stands alone: not glued to an identifier
/// character on either side, and not starting inside an escape sequence
/// (an odd run of backslashes before it). A letter after a backslash, as in
/// `\n` or `\t`, ends an escape and counts as a boundary.
fn at_token_boundary(line: &[u8], start: usize, end: usize) -> bool {
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let backslashes_before = |i: usize| {
        line.get(..i)
            .unwrap_or_default()
            .iter()
            .rev()
            .take_while(|&&b| b == b'\\')
            .count()
    };
    if backslashes_before(start) % 2 == 1 {
        return false;
    }
    let before_ok = match start.checked_sub(1).and_then(|i| line.get(i)) {
        None => true,
        Some(&b) if !ident(b) => true,
        Some(_) => backslashes_before(start - 1) % 2 == 1,
    };
    let after_ok = line.get(end).is_none_or(|&b| !ident(b));
    before_ok && after_ok
}

/// Whether a redacted value is worth hunting for elsewhere in its file.
fn propagatable(f: &Found) -> bool {
    let value = f.secret.as_str();
    if value.starts_with(MARKER)
        || !not_placeholder(value)
        || value
            .chars()
            .all(|c| matches!(c, '0'..='9' | '.' | 'e' | 'E' | '+' | '-'))
    {
        return false;
    }
    if SHAPE_RULES.contains(&f.rule) {
        return value.len() >= 6;
    }
    let wordy = value
        .chars()
        .all(|c| c.is_ascii_lowercase() || matches!(c, '-' | '_' | '.' | '/' | ' '));
    let classes = [
        value.chars().any(|c| c.is_ascii_lowercase()),
        value.chars().any(|c| c.is_ascii_uppercase()),
        value.chars().any(|c| c.is_ascii_digit()),
        value.chars().any(|c| !c.is_ascii_alphanumeric()),
    ]
    .iter()
    .filter(|&&b| b)
    .count();
    !wordy && value.len() >= MIN_LEN && classes >= 2 && entropy(value) >= MIN_ENTROPY
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(rule: &'static str, secret: &str) -> Found {
        Found {
            rule,
            secret: secret.to_owned(),
            quoted: true,
        }
    }

    fn rewrite(secrets: &Secrets, input: &str) -> (String, Counts, u64) {
        let mut out = Vec::new();
        let mut counts = Counts::new();
        let used = secrets
            .rewrite(input.as_bytes(), &mut out, 1 << 20, &mut counts)
            .expect("rewrite");
        (String::from_utf8(out).expect("utf8"), counts, used)
    }

    #[test]
    fn only_secret_looking_values_propagate() {
        for (rule, value, want) in [
            ("secret-field", "Kq7-zP2x-Lm9w", true),
            ("secret-field", "login", false),
            ("secret-field", "feature-branch-name", false),
            ("password", "home/automation", false),
            ("secret-field", "Abc12", false),
            ("secret-field", "12345678", false),
            ("github", "ghp_short", true),
            ("secret-field", "aaaaaaaaAAAAAAAA", false),
        ] {
            assert_eq!(propagatable(&found(rule, value)), want, "{value}");
        }
    }

    #[test]
    fn replaces_whole_tokens_only() {
        let mut secrets = Secrets::default();
        secrets.add(&[found("secret-field", "Kq7-zP2x-Lm9w")]);
        let input = concat!(
            r#"{"a":"Kq7-zP2x-Lm9w here","b":"x_Kq7-zP2x-Lm9w","c":"Kq7-zP2x-Lm9wTail"}"#,
            "\n",
            r#"{"d":"     3\tKq7-zP2x-Lm9w","e":"/w/Kq7-zP2x-Lm9w/f","n":1234567}"#
        );
        let (out, counts, used) = rewrite(&secrets, input);
        assert!(out.contains(r#""b":"x_Kq7-zP2x-Lm9w""#), "glued: {out}");
        assert!(out.contains(r#""c":"Kq7-zP2x-Lm9wTail""#), "glued: {out}");
        assert!(out.contains(r#""a":"[REDACTED:propagated] here""#), "{out}");
        assert!(
            out.contains(r#"\t[REDACTED:propagated]""#),
            "after an escape: {out}"
        );
        assert!(out.contains("/w/[REDACTED:propagated]/f"), "{out}");
        assert_eq!(counts.get(PROPAGATED), Some(&3));
        assert_eq!(used, 1);
        for line in out.lines() {
            serde_json::from_str::<Value>(line).expect("still JSON");
        }
    }

    /// A value starting with an escape letter (`n…`) must not be cut out of
    /// `\n…`, which would leave a lone backslash: the escape check skips it,
    /// and the JSON guard catches anything that still breaks.
    #[test]
    fn never_splits_an_escape_or_breaks_json() {
        let mut secrets = Secrets::default();
        secrets.add(&[found("secret-field", "nQ7xP2-Lm9w")]);
        let (out, _, _) = rewrite(&secrets, r#"{"a":"line\nQ7xP2-Lm9w","b":"nQ7xP2-Lm9w"}"#);
        serde_json::from_str::<Value>(&out).expect("still JSON");
        assert!(out.contains(r#""b":"[REDACTED:propagated]""#), "{out}");

        // A secret whose escaped spelling ends in a backslash pair: replacing
        // it raw would orphan the escape; the guard redacts the string whole.
        let mut secrets = Secrets::default();
        secrets.add(&[found("high-entropy", "Qx7\\")]);
        secrets.add(&[found("high-entropy", "Zk9pL2mW\\")]);
        let line = r#"{"a":"Zk9pL2mW\\\"quoted\"","b":"ok"}"#;
        serde_json::from_str::<Value>(line).expect("input is JSON");
        let (out, _, _) = rewrite(&secrets, line);
        serde_json::from_str::<Value>(&out).expect("still JSON");
        assert!(!out.contains("Zk9pL2mW"), "{out}");
        assert!(out.contains(r#""b":"ok""#), "{out}");
    }

    /// The guard itself: a replacement that would break the line falls back
    /// to replacing whole string values.
    #[test]
    fn a_breaking_replacement_falls_back_to_whole_strings() {
        let escaped = AhoCorasick::new([r#"" x"#]).expect("matcher");
        let plain = AhoCorasick::new([" x"]).expect("matcher");
        let mut used = BTreeSet::new();
        let line = br#"{"a":" x ","b":"keep"}"#;
        let (out, hits) = replace_line(line, &escaped, &plain, &mut used);
        let out = String::from_utf8(out.expect("changed")).expect("utf8");
        assert_eq!(out, r#"{"a":"[REDACTED:propagated]","b":"keep"}"#);
        assert_eq!(hits, 1);
    }
}
