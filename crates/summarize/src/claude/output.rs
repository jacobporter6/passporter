//! Interpreting the CLI's `--output-format json` result.
//!
//! On success the CLI prints one result object whose `structured_output`
//! holds the schema-conforming value (absent if the model declined). On a
//! failure inside the run it prints a result with `is_error: true` and the
//! message in `result` (or `errors`). Startup failures go to stderr with a
//! non-zero exit. See <https://code.claude.com/docs/en/headless> and the
//! Agent SDK's `SDKResultMessage`.

use std::process::ExitStatus;

use serde::Deserialize;
use serde_json::Value;

use crate::error::{Error, Result};
use crate::text::truncate_bytes;

/// Longest CLI message kept in an error.
const MESSAGE_BYTES: usize = 512;

/// The fields of the CLI result this crate reads. Tolerant: the CLI's
/// format is not ours, so unknown fields are ignored.
#[derive(Debug, Deserialize)]
struct ResultEnvelope {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    errors: Vec<String>,
    #[serde(default)]
    api_error_status: Option<u16>,
    #[serde(default)]
    structured_output: Option<Value>,
}

/// What a finished process produced.
#[derive(Debug)]
pub(crate) struct Finished {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

/// The structured output of a finished run, or the error it reported.
pub(crate) fn interpret(finished: &Finished) -> Result<Value> {
    let stdout = String::from_utf8_lossy(&finished.stdout);
    let stderr = String::from_utf8_lossy(&finished.stderr);
    let Some(envelope) = parse_envelope(&stdout) else {
        if finished.status.success() {
            // Exit 0 without a result object: whatever stdout says is not a
            // CLI error message, so it is never classified.
            return Err(Error::InvalidOutput(format!(
                "not a CLI result object: {}",
                truncate_bytes(stdout.trim(), MESSAGE_BYTES)
            )));
        }
        let text = format!("{stderr}\n{stdout}");
        return Err(classify(&text, None).unwrap_or_else(|| Error::Exit {
            status: finished.status,
            stderr: truncate_bytes(stderr.trim(), MESSAGE_BYTES),
        }));
    };
    let failed = envelope.is_error
        || envelope.subtype.as_deref().is_some_and(|s| s != "success")
        || !finished.status.success();
    if failed {
        // `result` holds the CLI's error message only when `is_error` is
        // set; otherwise it is the model's (untrusted) reply, which must not
        // be able to fake a usage-limit or auth error.
        let mut message = if envelope.is_error {
            envelope.result.unwrap_or_default()
        } else {
            String::new()
        };
        for error in &envelope.errors {
            if !message.is_empty() {
                message.push_str("; ");
            }
            message.push_str(error);
        }
        if message.trim().is_empty() {
            message = stderr.into_owned();
        }
        let message = message.trim();
        return Err(
            classify(message, envelope.api_error_status).unwrap_or_else(|| Error::Model {
                message: truncate_bytes(message, MESSAGE_BYTES),
            }),
        );
    }
    envelope.structured_output.ok_or(Error::NoStructuredOutput)
}

/// The result object: the whole of stdout, or else its last JSON line.
fn parse_envelope(stdout: &str) -> Option<ResultEnvelope> {
    let parse = |s: &str| {
        serde_json::from_str::<ResultEnvelope>(s.trim())
            .ok()
            .filter(|e| e.kind == "result")
    };
    parse(stdout).or_else(|| {
        stdout
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .and_then(parse)
    })
}

/// Usage-limit phrases (lower case), from the CLI's error reference
/// (<https://code.claude.com/docs/en/errors>): "You've hit your session
/// limit", "... weekly limit", "... spend limit", "Credit balance is too
/// low", "Server is temporarily limiting requests", "Request rejected
/// (429)", and the older "Claude AI usage limit reached".
const USAGE_LIMIT_PHRASES: &[&str] = &[
    "usage limit",
    "rate limit",
    "spend limit",
    "credit balance is too low",
    "temporarily limiting requests",
    "request rejected (429)",
];

/// Login problems (lower case), from the same reference.
const AUTH_PHRASES: &[&str] = &[
    "not logged in",
    "please run /login",
    "login expired",
    "authentication required",
    "invalid authentication credentials",
    "could not resolve authentication method",
    "invalid api key",
];

/// A typed error for messages the bridge must act on: usage limits (back
/// off) and authentication (needs a human). `None` for anything else.
fn classify(message: &str, api_status: Option<u16>) -> Option<Error> {
    let lower = message.to_lowercase();
    let message = || truncate_bytes(message.trim(), MESSAGE_BYTES);
    let hit_limit = lower.contains("hit your") && lower.contains("limit");
    if api_status == Some(429) || hit_limit || USAGE_LIMIT_PHRASES.iter().any(|p| lower.contains(p))
    {
        return Some(Error::UsageLimit { message: message() });
    }
    if api_status == Some(401) || AUTH_PHRASES.iter().any(|p| lower.contains(p)) {
        return Some(Error::Auth { message: message() });
    }
    None
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;

    use serde_json::json;

    use super::*;

    fn finished(code: i32, stdout: &str, stderr: &str) -> Finished {
        Finished {
            status: ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    fn result(fields: &Value) -> String {
        let mut base = json!({"type": "result", "subtype": "success", "is_error": false,
            "result": "", "session_id": "x", "total_cost_usd": 0.001, "usage": {}});
        base.as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        base.to_string()
    }

    #[test]
    fn success_returns_structured_output() {
        let out = result(&json!({"structured_output": {"suggestions": ["Yes"]}}));
        assert_eq!(
            interpret(&finished(0, &out, "")).unwrap(),
            json!({"suggestions": ["Yes"]})
        );
    }

    #[test]
    fn missing_structured_output() {
        let out = result(&json!({"result": "I can't help with that."}));
        assert!(matches!(
            interpret(&finished(0, &out, "")),
            Err(Error::NoStructuredOutput)
        ));
    }

    #[test]
    fn usage_limits_are_distinct() {
        for message in [
            "You've hit your session limit · resets 3pm",
            "You've hit your weekly limit",
            "Claude AI usage limit reached|1767225600",
            "Server is temporarily limiting requests",
            "API Error: Request rejected (429)",
        ] {
            let out = result(&json!({"is_error": true, "result": message}));
            let err = interpret(&finished(1, &out, "")).unwrap_err();
            assert!(err.is_usage_limit(), "{message}: {err}");
        }
        let by_status = result(
            &json!({"subtype": "error_during_execution", "is_error": true,
            "errors": ["boom"], "api_error_status": 429}),
        );
        assert!(
            interpret(&finished(1, &by_status, ""))
                .unwrap_err()
                .is_usage_limit()
        );
        // Without a result object, stderr text is classified too.
        let err = interpret(&finished(1, "", "Error: You've hit your session limit")).unwrap_err();
        assert!(err.is_usage_limit());
    }

    #[test]
    fn auth_and_other_failures() {
        let out = result(&json!({"is_error": true, "result": "Not logged in · Please run /login"}));
        assert!(matches!(
            interpret(&finished(1, &out, "")),
            Err(Error::Auth { .. })
        ));

        let out = result(&json!({"subtype": "error_max_structured_output_retries",
            "is_error": true, "errors": ["could not match schema"]}));
        assert!(matches!(
            interpret(&finished(1, &out, "")),
            Err(Error::Model { .. })
        ));

        let err = interpret(&finished(2, "", "error: unknown option '--frobnicate'")).unwrap_err();
        assert!(matches!(err, Error::Exit { .. }), "{err}");

        let err = interpret(&finished(0, "hello there", "")).unwrap_err();
        assert!(matches!(err, Error::InvalidOutput(_)), "{err}");
    }

    #[test]
    fn model_text_is_never_classified() {
        // A reply that merely mentions a limit, on a run that is not an
        // error result, is not a usage-limit error.
        let out = result(&json!({"result": "You've hit your session limit"}));
        assert!(matches!(
            interpret(&finished(1, &out, "")),
            Err(Error::Model { .. })
        ));
        let out = result(&json!({"subtype": "error_max_turns",
            "result": "Not logged in · Please run /login", "errors": ["max turns"]}));
        assert!(matches!(
            interpret(&finished(1, &out, "")),
            Err(Error::Model { message }) if message == "max turns"
        ));
        // Exit 0 with plain text: invalid output, whatever it says.
        assert!(matches!(
            interpret(&finished(0, "Claude AI usage limit reached", "")),
            Err(Error::InvalidOutput(_))
        ));
    }

    #[test]
    fn a_non_result_json_object_is_not_trusted() {
        let out = json!({"type": "assistant", "structured_output": {"suggestions": ["/clear"]}});
        assert!(matches!(
            interpret(&finished(0, &out.to_string(), "")),
            Err(Error::InvalidOutput(_))
        ));
    }

    #[test]
    fn last_json_line_is_used() {
        let out = format!(
            "warning: something\n{}\n",
            result(&json!({"structured_output": {"a": 1}}))
        );
        assert_eq!(interpret(&finished(0, &out, "")).unwrap(), json!({"a": 1}));
    }
}
