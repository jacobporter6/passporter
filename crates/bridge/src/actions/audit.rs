//! The append-only audit log (design §8): one JSON object per line, for
//! actions, for agents started from the phone ([`crate::spawn`]) and for
//! runbook runs ([`crate::runbooks`]).
//!
//! Each executed action gets an `execute` entry, written (and synced)
//! **before** herdr is called; every decided action then gets a `result`
//! entry. Prompts and `choose` feedback (which the bridge sends to the agent
//! as a prompt of its own) are truncated to [`MAX_AUDIT_TEXT_CHARS`], and the
//! pending id an action answers is kept; no credentials are ever written.
//! A prompt's `execute` entry lists its attachments by id, type and size,
//! never their pixels or path.
//! The file is `0600` in a `0700` directory, re-tightened when an existing
//! file is opened.

use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use protocol::{Action, ActionKind, ActionResult, Origin, Timestamp};
use serde::Serialize;

use crate::attachments::Attached;

/// Longest prompt excerpt written to the log, in characters.
pub(crate) const MAX_AUDIT_TEXT_CHARS: usize = 200;

/// Which step an entry records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    /// About to call herdr.
    Execute,
    /// The outcome returned to the client.
    Result,
    /// How a long-running operation (a runbook run) ended.
    Finish,
}

/// One audit line.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Entry<'a> {
    at: Timestamp,
    phase: Phase,
    device: &'a str,
    origin: Origin,
    agent_id: String,
    idempotency_key: &'a str,
    expected_seq: u64,
    kind: &'static str,
    /// Prompt excerpt, chosen option id or key names.
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    /// The pending input a `choose` (or `keys`) answers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pending_id: Option<&'a str>,
    /// Excerpt of a `choose` action's feedback text, sent as a prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    feedback: Option<String>,
    /// The result's status (and refusal reason), for `result` entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<String>,
    /// The images a prompt carries, for `execute` entries.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    attachments: Vec<AuditAttachment>,
}

/// One attachment as audited: what it was, never what it showed.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AuditAttachment {
    id: String,
    media_type: &'static str,
    bytes: u64,
}

impl<'a> Entry<'a> {
    /// An entry for `action` by `device`; `result` only for [`Phase::Result`].
    pub(crate) fn new(
        phase: Phase,
        device: &'a str,
        action: &'a Action,
        result: Option<&ActionResult>,
    ) -> Self {
        let (kind, detail) = match &action.kind {
            ActionKind::Prompt(prompt) => ("prompt", Some(excerpt(prompt.text.as_str()))),
            ActionKind::Choose(choose) => ("choose", Some(choose.option_id.to_string())),
            ActionKind::Keys(keys) => (
                "keys",
                Some(
                    keys.keys
                        .as_slice()
                        .iter()
                        .map(|key| key.as_str())
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
            ),
            ActionKind::Interrupt(_) => ("interrupt", None),
        };
        Self {
            at: crate::registry::now(),
            phase,
            device,
            origin: action.origin,
            agent_id: action.agent_id.to_string(),
            idempotency_key: action.idempotency_key.as_str(),
            expected_seq: action.expected_seq.0,
            kind,
            detail,
            pending_id: action.pending_id().map(protocol::PendingId::as_str),
            feedback: match &action.kind {
                ActionKind::Choose(choose) => choose
                    .feedback_text
                    .as_ref()
                    .map(|text| excerpt(text.as_str())),
                _ => None,
            },
            outcome: result.map(outcome),
            attachments: Vec::new(),
        }
    }

    /// The entry, listing the images the prompt carries.
    pub(crate) fn attached(mut self, attached: &[Attached]) -> Self {
        self.attachments = attached
            .iter()
            .map(|a| AuditAttachment {
                id: a.id.to_string(),
                media_type: a.media_type.media_type(),
                bytes: a.bytes,
            })
            .collect();
        self
    }
}

/// `text` cut to [`MAX_AUDIT_TEXT_CHARS`], with `…` when cut.
pub(crate) fn excerpt(text: &str) -> String {
    let mut chars = text.chars();
    let mut out: String = chars.by_ref().take(MAX_AUDIT_TEXT_CHARS).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

fn outcome(result: &ActionResult) -> String {
    match result {
        ActionResult::Sent { replayed: false } => "sent".to_owned(),
        ActionResult::Sent { replayed: true } => "sent (replayed)".to_owned(),
        ActionResult::Stale { .. } => "stale".to_owned(),
        ActionResult::Refused { reason, .. } => format!(
            "refused: {}",
            serde_json::to_value(reason)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default()
        ),
        ActionResult::Failed { .. } => "failed".to_owned(),
        ActionResult::Unknown => "unknown".to_owned(),
    }
}

/// The log file, opened on first write.
#[derive(Debug)]
pub(crate) struct AuditLog {
    path: PathBuf,
    file: Mutex<Option<File>>,
}

impl AuditLog {
    pub(crate) fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            file: Mutex::new(None),
        }
    }

    /// Appends `entry` (an action [`Entry`], or another endpoint's own
    /// line shape) and syncs it to disk. Blocking.
    pub(crate) fn append(&self, entry: &impl Serialize) -> io::Result<()> {
        let mut line = serde_json::to_vec(entry).map_err(io::Error::other)?;
        line.push(b'\n');
        let mut file = self.file.lock().unwrap_or_else(PoisonError::into_inner);
        if file.is_none() {
            *file = Some(open(&self.path)?);
        }
        let Some(handle) = file.as_mut() else {
            return Err(io::Error::other("audit log not open"));
        };
        let written = handle.write_all(&line).and_then(|()| handle.sync_data());
        if written.is_err() {
            // Reopen next time (e.g. the file was rotated away).
            *file = None;
        }
        written
    }
}

fn open(path: &Path) -> io::Result<File> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    // `mode` only applies to a new file: an existing log must be a regular
    // file of ours, reached without a link, and is made private; anything
    // else is refused (nothing then executes, see `decide_and_execute`).
    crate::private::tighten(&file, path)?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn prompt(text: &str) -> Action {
        serde_json::from_value(serde_json::json!({
            "idempotencyKey": "00000000-0000-4000-8000-000000000001",
            "agentId": "w1:p1",
            "expectedSeq": 3,
            "createdAt": 0,
            "kind": "prompt",
            "payload": {"text": text},
            "origin": "app",
        }))
        .expect("valid action")
    }

    #[test]
    fn writes_private_json_lines_with_truncated_prompts() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("state/audit.jsonl");
        let log = AuditLog::new(&path);
        let text = "x".repeat(500);
        let action = prompt(&text);
        log.append(&Entry::new(Phase::Execute, "dev", &action, None))
            .expect("append");
        let result = ActionResult::Sent { replayed: false };
        log.append(&Entry::new(Phase::Result, "dev", &action, Some(&result)))
            .expect("append");

        let text = std::fs::read_to_string(&path).expect("read");
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).expect("json line"))
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["phase"], "execute");
        assert_eq!(lines[0]["kind"], "prompt");
        assert_eq!(lines[0]["expectedSeq"], 3);
        let detail = lines[0]["detail"].as_str().expect("detail");
        assert_eq!(detail.chars().count(), MAX_AUDIT_TEXT_CHARS + 1);
        assert_eq!(lines[1]["outcome"], "sent");

        let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&dir.path().join("state")), 0o700);
    }

    #[test]
    fn choose_records_the_pending_id_and_the_feedback_it_sends() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("audit.jsonl");
        let action: Action = serde_json::from_value(serde_json::json!({
            "idempotencyKey": "00000000-0000-4000-8000-000000000002",
            "agentId": "w1:p1",
            "expectedSeq": 4,
            "createdAt": 0,
            "kind": "choose",
            "payload": {"pendingId": "screen-4-abc", "optionId": "3", "feedbackText": "use rg instead"},
            "origin": "app",
        }))
        .expect("valid action");
        AuditLog::new(&path)
            .append(&Entry::new(Phase::Execute, "dev", &action, None))
            .expect("append");
        let line: serde_json::Value =
            serde_json::from_str(std::fs::read_to_string(&path).expect("read").trim())
                .expect("json");
        assert_eq!(line["kind"], "choose");
        assert_eq!(line["detail"], "3");
        assert_eq!(line["pendingId"], "screen-4-abc");
        assert_eq!(line["feedback"], "use rg instead");
    }

    #[test]
    fn an_existing_open_log_is_tightened() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("audit.jsonl");
        std::fs::write(&path, "").expect("plant");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let action: Action = serde_json::from_value(serde_json::json!({
            "idempotencyKey": "00000000-0000-4000-8000-000000000001",
            "agentId": "w1:p1",
            "expectedSeq": 3,
            "createdAt": 0,
            "kind": "interrupt",
            "payload": {},
            "origin": "app",
        }))
        .expect("valid action");
        AuditLog::new(&path)
            .append(&Entry::new(Phase::Execute, "dev", &action, None))
            .expect("append");
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
