//! Best-effort secret redaction rules.
//!
//! Each rule is a regex; when it has capture groups named `secret…` only the
//! one that matched is replaced (so `Authorization: Bearer <token>` keeps its
//! header), otherwise the whole match is. Matches become `[REDACTED:<rule>]`.
//!
//! The rules err towards redacting: a scrubbed corpus that lost a few
//! harmless strings is fine, a leaked token is not. Paths and IP addresses
//! are deliberately kept: the transcript reader needs realistic paths, and
//! home-network addresses are not secrets.
//!
//! Boundaries: provider tokens with a distinctive prefix (`AIza`, `ghp_`,
//! `glpat-`, …) match anywhere, with no `\b` before or after. A leading `\b`
//! misses keys glued to escape letters (`\nAIza…` in double-encoded JSON) or
//! URL encoding (`%3DAIza…`), and a trailing `\b` after a fixed-length body
//! misses keys ending in `-` or followed by more key characters. Bodies are
//! open-ended (`{35,}`), so a token glued to further key characters is
//! redacted together with them. Only short or common prefixes (`sk-`, `hf_`)
//! keep a leading `\b`.

use std::borrow::Cow;
use std::collections::BTreeMap;

use regex::{Captures, Regex, RegexSet};

/// Redactions made, by rule name.
pub(crate) type Counts = BTreeMap<&'static str, u64>;

/// The marker every redaction starts with. Text already carrying it is left
/// alone, so overlapping rules never count one secret twice.
pub(crate) const MARKER: &str = "[REDACTED";

/// Minimum Shannon entropy (bits per byte) for the generic key-like rule.
const MIN_ENTROPY: f64 = 3.5;

/// The same for values without digits.
const MIN_ENTROPY_LETTERS: f64 = 4.0;

/// A secret check applied to a candidate match.
type Check = fn(&str) -> bool;

/// Optional quote before a value, possibly backslash-escaped (`\"`) when the
/// text is JSON nested inside a JSON string.
macro_rules! q {
    () => {
        r#"\\?["']?"#
    };
}

/// A value: double-quoted (spaces allowed), single-quoted, or a bare word
/// of at least `$min` characters.
macro_rules! value {
    ($min:literal) => {
        concat!(
            r#"(?:\\?"(?P<secret>[^"\\\n]{1,200})\\?"|\\?'(?P<secret2>[^'\\\n]{1,200})\\?'|(?P<secret3>[^\s"',;\\]{"#,
            $min,
            r#",}))"#
        )
    };
}

const RULES: &[(&str, &str, Option<Check>)] = &[
    (
        "private-key",
        r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----.*?(?:-----END [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----|\z)",
        None,
    ),
    ("anthropic", r"sk-ant-[A-Za-z0-9_\-]{16,}", None),
    (
        "openai",
        r"\bsk-(?:proj-|svcacct-|admin-)?[A-Za-z0-9_\-]{20,}",
        None,
    ),
    (
        "github",
        r"gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{22,}",
        None,
    ),
    ("gitlab", r"glpat-[A-Za-z0-9_\-]{20,}", None),
    ("huggingface", r"\bhf_[A-Za-z0-9]{30,}", None),
    ("npm", r"npm_[A-Za-z0-9]{36,}", None),
    ("tailscale", r"tskey-[A-Za-z0-9\-]{16,}", None),
    ("age", r"AGE-SECRET-KEY-1[0-9A-Z]{20,}", None),
    ("docker", r"dckr_pat_[A-Za-z0-9_\-]{20,}", None),
    ("google-oauth", r"ya29\.[0-9A-Za-z_\-]{20,}", None),
    (
        "sendgrid",
        r"SG\.[A-Za-z0-9_\-]{16,}\.[A-Za-z0-9_\-]{16,}",
        None,
    ),
    (
        "aws-access-key-id",
        r"(?:AKIA|ASIA|ABIA|ACCA)[0-9A-Z]{16,}",
        None,
    ),
    (
        "aws-secret-key",
        r#"(?i)aws.{0,20}?secret.{0,20}?[\\"'\s:=]+(?P<secret>[A-Za-z0-9/+=]{40,})"#,
        None,
    ),
    ("cloudflare-tunnel", r"eyJhIjoi[A-Za-z0-9+/=_\-]{40,}", None),
    (
        "cloudflare",
        concat!(
            r#"(?i)\b(?:CF|CLOUDFLARE)_[A-Z0-9_]*(?:TOKEN|KEY|SECRET)"#,
            q!(),
            r#"\s*[:=]\s*"#,
            q!(),
            r#"(?P<secret>[A-Za-z0-9_\-]{20,})"#
        ),
        None,
    ),
    (
        "slack",
        r"xox[abposr]-[A-Za-z0-9\-]{10,}|https://hooks\.slack\.com/services/[A-Za-z0-9/]+",
        None,
    ),
    ("google-api-key", r"AIza[0-9A-Za-z_\-]{35,}", None),
    ("stripe", r"[rs]k_(?:live|test)_[0-9A-Za-z]{16,}", None),
    (
        "jwt",
        r"eyJ[A-Za-z0-9_\-]{8,}\.eyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]*",
        None,
    ),
    (
        // `scheme://user:password@host`. The user may be empty
        // (`redis://:pw@host`) or contain `@` (Azure's `admin@server`); the
        // password may contain `@`, `/` and any other non-space character.
        // The password is the shortest run followed by `@` plus a complete
        // host (name or `[v6]`) ending at a delimiter, so `p@ss@db:5432` and
        // `ab/cd@db/x` both split correctly.
        "url-credentials",
        r#"\b[a-zA-Z][a-zA-Z0-9+.\-]*://[^\s/:"'<>\\]*:(?P<secret>[^\s"'<>\\]+?)@(?:[A-Za-z0-9\-._~%]+|\[[0-9A-Fa-f:.]+\])(?:[:/?#\s"'<>\\]|\z)"#,
        Some(url_password),
    ),
    (
        "bearer",
        r"(?i)\bbearer\s+(?P<secret>[A-Za-z0-9\-._~+/]{8,}=*)",
        Some(not_placeholder),
    ),
    (
        "auth-header",
        concat!(
            r#"(?i)\b(?:authorization|proxy-authorization|x-api-key|api-key|x-auth-token|private-token|x-goog-api-key)"#,
            q!(),
            r#"\s*[:=]\s*"#,
            q!(),
            r#"(?:(?:basic|token|bearer)\s+)?(?P<secret>[^\s"',;\\]{8,})"#
        ),
        Some(not_placeholder),
    ),
    (
        "cookie",
        concat!(
            r#"(?i)\b(?:set-)?cookie"#,
            q!(),
            r#"\s*:\s*"#,
            q!(),
            r#"(?P<secret>[^\r\n"'\\]{6,})"#
        ),
        Some(not_placeholder),
    ),
    (
        "cli-credentials",
        concat!(
            r#"(?i)(?:^|\s)(?:--(?:password|passwd|pass|pwd|token|api-key|apikey|secret|client-secret|with-token|auth-token|access-token|cookie)(?:=|\s+)"#,
            q!(),
            r#"(?P<secret>[^\s"'\\]{4,})|(?:-u|--user)\s*"#,
            q!(),
            r#"[^\s:"'\\]*:(?P<secret2>[^\s"'\\]{3,}))"#
        ),
        Some(cli_value),
    ),
    (
        // `mysql -uroot -pS3cret`, `sshpass -p pw ssh …`, `docker login -p X`.
        "cli-password",
        concat!(
            r#"\b(?:mysql|mariadb|mysqldump|mysqladmin|mysqlsh)\b[^\n|;&]*?\s-p(?P<secret>[^\s"'\\-][^\s"'\\]*)"#,
            r#"|\bsshpass\s+(?:-[a-zA-Z]+\s+)*?-p\s*(?P<secret2>[^\s"'\\]+)"#,
            r#"|\b(?:docker|podman|nerdctl|helm)\b[^\n|;&]*?\blogin\b[^\n|;&]*?\s(?:-p|--password)[=\s]\s*(?P<secret3>[^\s"'\\]+)"#
        ),
        Some(cli_value),
    ),
    (
        // Azure storage / service bus connection strings and SAS signatures.
        "azure-key",
        r#"(?i)\b(?:AccountKey|SharedAccessKey|SharedAccessSignature)=(?P<secret>[^;\s"'\\]{8,})|[?&]sig=(?P<secret2>[^&\s"'\\]{16,})"#,
        None,
    ),
    (
        // `.netrc`: `machine host login user password secret`.
        "netrc",
        r#"(?i)\b(?:machine|login)\s+\S+(?:\s+login\s+\S+)?\s+password\s+(?P<secret>[^\s"'\\]+)"#,
        Some(not_placeholder),
    ),
    (
        "discord-webhook",
        r"https://(?:(?:ptb|canary)\.)?discord(?:app)?\.com/api/webhooks/\d+/[A-Za-z0-9_\-]+",
        None,
    ),
    ("telegram", r"\d{8,10}:[A-Za-z0-9_\-]{35,}", None),
    (
        // Upper-case names in .env, docker-compose and YAML: `DB_PASSWORD=x`,
        // `MYSQL_ROOT_PASSWORD: "x"`, `export MY_PW=x`. The secret word must
        // end a `_`-separated segment (or be one), so `MONKEY` does not count.
        "env-secret",
        concat!(
            r#"\b(?:[A-Z0-9]+_)*(?:[A-Z0-9]*(?:PASSWORD|PASSWD|SECRET|TOKEN|CREDENTIALS?|COOKIE|APIKEY)|KEY|PASS|PWD|PW|AUTH)(?:_[A-Z0-9]+)*"#,
            q!(),
            r#"\s*[:=]\s*"#,
            value!(4)
        ),
        Some(not_placeholder),
    ),
    (
        // Any casing, with optional `_`/`-`/`.` separated prefixes:
        // `password: x`, `db_password = "two words"`, `smtp.pass: x`.
        "password",
        concat!(
            r#"(?i)\b(?:[a-z0-9]+[_.\-])*(?:password|passwd|passphrase|pwd|pw|pass)\b"#,
            q!(),
            r#"\s*[:=]\s*"#,
            value!(4)
        ),
        Some(not_placeholder),
    ),
    (
        "high-entropy",
        concat!(
            r#"(?i)[\w.\-]*(?:secret|token|api[_\-]?key|access[_\-]?key|private[_\-]?key|credential|auth|passw(?:or)?d)[\w.\-]*"#,
            q!(),
            r#"\s*[:=]\s*"#,
            q!(),
            r#"(?P<secret>[A-Za-z0-9+/_\-.=]{16,})"#
        ),
        Some(high_entropy),
    ),
];

/// The rule name used for string values under secret-looking JSON keys.
pub(crate) const SECRET_FIELD: &str = "secret-field";

/// A secret a rule redacted, for mapping redactions made in a decoded or
/// parsed copy of a string back onto the original.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Found {
    pub(crate) rule: &'static str,
    pub(crate) secret: String,
    /// A whole JSON string value (under a secret key): only replace it where
    /// it appears quoted, since short values would match too broadly.
    pub(crate) quoted: bool,
}

/// One compiled rule.
#[derive(Debug)]
struct Rule {
    name: &'static str,
    regex: Regex,
    check: Option<Check>,
    /// Capture groups named `secret…`, in order.
    groups: Vec<String>,
}

/// The compiled rules.
#[derive(Debug)]
pub(crate) struct Redactor {
    rules: Vec<Rule>,
    /// All rule patterns at once: one pass finds the few rules worth running.
    prefilter: RegexSet,
}

impl Redactor {
    pub(crate) fn new() -> Result<Self, regex::Error> {
        let rules = RULES
            .iter()
            .map(|&(name, pattern, check)| {
                let regex = Regex::new(pattern)?;
                let groups = regex
                    .capture_names()
                    .flatten()
                    .filter(|n| n.starts_with("secret"))
                    .map(str::to_owned)
                    .collect();
                Ok(Rule {
                    name,
                    regex,
                    check,
                    groups,
                })
            })
            .collect::<Result<_, regex::Error>>()?;
        Ok(Self {
            rules,
            prefilter: RegexSet::new(RULES.iter().map(|(_, pattern, _)| pattern))?,
        })
    }

    /// Redacts every rule's matches in `text`, adding to `counts` and
    /// recording each redacted secret in `found`.
    pub(crate) fn redact<'a>(
        &self,
        text: &'a str,
        counts: &mut Counts,
        found: &mut Vec<Found>,
    ) -> Cow<'a, str> {
        let candidates = self.prefilter.matches(text);
        let mut text = Cow::Borrowed(text);
        for rule in candidates.iter().filter_map(|i| self.rules.get(i)) {
            let mut hits = Vec::new();
            let replaced = rule.regex.replace_all(&text, |caps: &Captures<'_>| {
                redact_match(rule, caps, &mut hits)
            });
            if !hits.is_empty() {
                text = Cow::Owned(replaced.into_owned());
                *counts.entry(rule.name).or_default() += hits.len() as u64;
                found.extend(hits.into_iter().map(|secret| Found {
                    rule: rule.name,
                    secret,
                    quoted: false,
                }));
            }
        }
        text
    }
}

/// The replacement for one match: the match with its secret group (or all
/// of it) swapped for the marker, or unchanged if the check rejects it.
fn redact_match(rule: &Rule, caps: &Captures<'_>, hits: &mut Vec<String>) -> String {
    let whole = &caps[0];
    let start = caps.get(0).map_or(0, |m| m.start());
    let secret = if rule.groups.is_empty() {
        caps.get(0)
    } else {
        rule.groups.iter().find_map(|g| caps.name(g))
    };
    let Some(secret) = secret else {
        return whole.to_owned();
    };
    let value = secret.as_str();
    if value.starts_with(MARKER) || rule.check.is_some_and(|check| !check(value)) {
        return whole.to_owned();
    }
    hits.push(value.to_owned());
    let (before, after) = (secret.start() - start, secret.end() - start);
    format!(
        "{}[REDACTED:{}]{}",
        whole.get(..before).unwrap_or_default(),
        rule.name,
        whole.get(after..).unwrap_or_default()
    )
}

/// Whether a JSON object key names a secret, in any casing convention:
/// `password`, `api_key`, `apiKey`, `X-Api-Key`, `sessionToken`,
/// `AWS_SECRET_ACCESS_KEY`, `cookie`, `auth`, `secrets`, … The key is split
/// into words; the last word (or last two, joined) decides, and a few
/// plurals count as a whole key. `max_tokens`, `author` or `key` alone do
/// not match.
pub(crate) fn is_secret_key(key: &str) -> bool {
    const LAST: &[&str] = &[
        "password",
        "passwd",
        "passphrase",
        "pass",
        "pwd",
        "secret",
        "token",
        "credential",
        "credentials",
        "cookie",
        "auth",
        "authorization",
        "apikey",
        "privatekey",
    ];
    const PAIRS: &[&str] = &[
        "apikey",
        "secretkey",
        "accesskey",
        "privatekey",
        "signingkey",
        "encryptionkey",
        "masterkey",
        // Account identities: not credentials, but they name the person
        // (e.g. Codex rollouts' `creator_user_id` / `creator_account_id`).
        "userid",
        "accountid",
        "orgid",
        "organizationid",
    ];
    /// Plurals that name secrets only as a whole key (`secrets`, but not
    /// `max_tokens`).
    const ALONE: &[&str] = &["secrets", "tokens", "passwords", "cookies", "apikeys"];
    let words = split_words(key);
    let Some(last) = words.last() else {
        return false;
    };
    if words.len() == 1 && ALONE.contains(&last.as_str()) {
        return true;
    }
    let pair = words
        .len()
        .checked_sub(2)
        .and_then(|i| words.get(i))
        .map(|w| format!("{w}{last}"));
    LAST.contains(&last.as_str()) || pair.is_some_and(|p| PAIRS.contains(&p.as_str()))
}

/// Lower-cased words of an identifier: split at non-alphanumerics and at
/// case boundaries (`apiKey` → api, key; `APIKey` → api, key).
fn split_words(key: &str) -> Vec<String> {
    let chars: Vec<char> = key.chars().collect();
    let mut words = Vec::new();
    let mut word = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
            continue;
        }
        let prev = i.checked_sub(1).and_then(|p| chars.get(p));
        let next = chars.get(i + 1);
        let boundary = c.is_uppercase()
            && prev.is_some_and(|p| {
                p.is_lowercase()
                    || p.is_ascii_digit()
                    || (p.is_uppercase() && next.is_some_and(|n| n.is_lowercase()))
            });
        if boundary && !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
        word.extend(c.to_lowercase());
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// Rejects values that are obviously not secrets: type names, empty-ish
/// literals and references to variables or other redactions.
pub(crate) fn not_placeholder(value: &str) -> bool {
    const LITERALS: &[&str] = &[
        "string",
        "str",
        "null",
        "none",
        "undefined",
        "true",
        "false",
        "required",
        "optional",
        "redacted",
        "xxx",
        "xxxx",
        "****",
        "changeme",
        "example",
    ];
    let lower = value.to_ascii_lowercase();
    let trimmed = lower.trim_end_matches([';', ')', '}', ']', '.']);
    !(LITERALS.contains(&trimmed)
        || value.starts_with('$')
        || value.starts_with('<')
        || value.starts_with('{')
        || value.starts_with('%')
        || value.starts_with("process.env")
        || value.starts_with("os.environ")
        || value.starts_with("env.")
        || value.chars().all(|c| c == '*' || c == 'x' || c == '.'))
}

/// A URL password, not a port followed by a path that happens to contain
/// `@` (`https://registry.npmjs.org:443/@types/node`).
fn url_password(value: &str) -> bool {
    let digits = value.bytes().take_while(u8::is_ascii_digit).count();
    !(digits > 0 && value.as_bytes().get(digits) == Some(&b'/'))
}

/// A command-line flag value: not a placeholder and not the next flag.
fn cli_value(value: &str) -> bool {
    !value.starts_with('-') && not_placeholder(value)
}

/// A value that looks random enough to be a credential: mixed letters and
/// digits with high per-byte entropy.
fn high_entropy(value: &str) -> bool {
    let has = |f: fn(&u8) -> bool| value.bytes().any(|b| f(&b));
    let threshold = if has(u8::is_ascii_digit) {
        MIN_ENTROPY
    } else if has(u8::is_ascii_uppercase) && has(u8::is_ascii_lowercase) {
        // Letters only: demand more randomness, so camelCase identifiers
        // (`getAuthentication`) are not taken for secrets.
        MIN_ENTROPY_LETTERS
    } else {
        return false;
    };
    not_placeholder(value) && entropy(value) >= threshold
}

/// Shannon entropy in bits per byte.
#[allow(clippy::cast_precision_loss)] // byte counts are far below 2^52
pub(crate) fn entropy(value: &str) -> f64 {
    let mut freq = [0_u32; 256];
    for b in value.bytes() {
        freq[usize::from(b)] += 1;
    }
    let len = value.len() as f64;
    freq.iter()
        .filter(|&&n| n > 0)
        .map(|&n| {
            let p = f64::from(n) / len;
            -p * p.log2()
        })
        .sum()
}

#[cfg(test)]
pub(crate) mod tests;
