//! Wire types shared by the Passporter bridge (host) and the web app (WASM).
//!
//! Both sides compile against this crate, so an API mismatch is a build error
//! rather than a runtime surprise (design §3, §5).
//!
//! # Wire conventions
//!
//! - JSON field names are `camelCase`; enum values are `snake_case`.
//! - **Inbound** types (client → bridge, e.g. [`Action`]) reject unknown fields
//!   and validate every field during deserialization, through the same
//!   constructors Rust callers use. A value of an inbound type is therefore
//!   always valid: size limits, the key allowlist and id formats are enforced
//!   by the type, not by the handler.
//! - **Outbound** types (bridge → client, e.g. [`Agent`], [`AgentEvent`]) are
//!   tolerant: unknown fields are ignored and open enums have an `unknown`
//!   fallback, so an older app keeps working against a newer bridge.
//! - Timestamps are unix milliseconds ([`Timestamp`]); see its docs for why.
//! - Error types implement `Display`/`Error` by hand rather than with
//!   `thiserror`: a deliberate exception to the library convention, to keep
//!   this crate's dependencies to `serde` only.
//!
//! # Modules
//!
//! - [`ids`], [`time`], [`text`], [`keys`]: validated scalar newtypes.
//! - [`agent`], [`thread`], [`pending`], [`tldr`], [`headline`], [`status_line`],
//!   [`transcript`]: read models.
//! - [`action`], [`result`]: the single mutating endpoint, `POST /v1/actions`.
//! - [`attachment`]: photo uploads, `POST /v1/attachments`.
//! - [`audio`]: dictation uploads, `POST`/`PATCH /v1/audio`.
//! - [`spawn`]: starting a new agent, `GET /v1/projects` and `POST /v1/agents`.
//! - [`runbook`]: preconfigured operations, `GET /v1/runbooks` and
//!   `POST /v1/runbooks/:name/run`.
//! - [`command`]: commands the agent asks the user to run (terminal mode),
//!   `GET /v1/agents/:id/commands` and `POST /v1/agents/:id/commands/run`.
//! - [`event`]: the `WS /v1/stream` event stream.
//! - [`auth`]: device pairing, session tokens and their scope (design §8).
//! - [`car`]: Android Auto replies, `POST /v1/agents/:id/car-reply`.
//! - [`policy`]: step-up and Android Auto rules (design §6.6, §8).
//! - [`suggestion`]: the rule for reply suggestions (design §6.2).
//! - [`tolerant`]: lenient parsing for nested outbound data.
//! - [`version`]: the bridge's build, `GET /v1/version`.

#![warn(missing_docs)]

mod macros;

pub mod action;
pub mod agent;
pub mod attachment;
pub mod audio;
pub mod auth;
pub mod car;
pub mod command;
pub mod error;
pub mod event;
pub mod headline;
pub mod ids;
pub mod keys;
mod loose;
pub mod pending;
pub mod policy;
pub mod push;
pub mod result;
pub mod runbook;
pub mod screen;
pub mod spawn;
pub mod status_line;
pub mod suggestion;
pub mod text;
pub mod thread;
pub mod time;
pub mod tldr;
pub mod tolerant;
pub mod transcript;
pub mod version;

pub use action::{
    Action, ActionKind, ChoosePayload, InterruptPayload, KeysPayload, Origin, PromptPayload,
};
pub use agent::{Agent, AgentKind, AgentStatus, MAX_ASK_CHARS};
pub use attachment::{
    AttachmentReceipt, Attachments, IDEMPOTENCY_KEY_HEADER, ImageType, MAX_ATTACHMENTS,
};
pub use audio::{AudioFailure, ClipState, NewUpload, UploadStatus};
pub use auth::{
    Challenge, ChallengeRequest, ChallengeResponse, DEVICE_UNKNOWN_STATUS, DeviceId, DeviceName,
    PairRequest, PairResponse, PairingCode, PublicKey, Scope, SessionGrant, SessionRequest,
    Signature, signed_message,
};
pub use car::{
    CarIntent, CarReply, CarReplyRequest, MAX_UTTERANCE_BYTES, Utterance, UtteranceRefusal,
};
pub use command::{
    COMMAND_HASH_LEN, CommandCandidate, CommandHash, CommandIntent, CommandList, CommandRefusal,
    CommandResult, CommandRunRequest, CommandSource, MAX_CANDIDATES, MAX_COMMAND_CHARS,
    MAX_COMMAND_LINES, RunTarget, Terminal, TerminalKeysRequest, TerminalScreen,
};
pub use error::ValidationError;
pub use event::AgentEvent;
pub use headline::{MAX_HEADLINE_CHARS, TurnHeadline};
pub use ids::{
    AgentId, AttachmentId, AudioId, IdempotencyKey, OptionId, PaneId, PendingId, Seq, SessionId,
};
pub use keys::{AllowedKey, KeySequence};
pub use pending::{
    Confidence, PendingInput, PendingKind, PendingOption, PendingOptions, PendingSource, Risk,
    ScreenTail,
};
pub use policy::{
    CarRefusal, StepUpReason, Unanswerable, check_answerable, check_car_policy, requires_step_up,
    step_up_reason,
};
pub use push::{
    MAX_NOTICES, NativeRegister, NativeRegistration, Notice, NoticeState, PushEndpoint, PushKey,
    PushMessage, PushPayload, PushSubscriptions,
};
pub use result::{ActionResult, RefusalReason};
pub use runbook::{
    MAX_RUN_LINES, MAX_RUNBOOK_NAME_CHARS, RunState, Runbook, RunbookList, RunbookName,
    RunbookRefusal, RunbookResult, RunbookRun, RunbookRunRequest, command_line, normalise_phrase,
};
pub use screen::{AgentScreen, MAX_SCREEN_LINES, StreamAuth};
pub use spawn::{
    MAX_PROJECT_NAME_LEN, ProjectName, SpawnKind, SpawnOptions, SpawnRefusal, SpawnRequest,
    SpawnResult,
};
pub use status_line::{MAX_STATUS_LINE_CHARS, StatusLine};
pub use suggestion::{MAX_SUGGESTION_CHARS, MAX_SUGGESTIONS, SuggestionRejected, suggestion};
pub use text::{FeedbackText, PromptText};
pub use thread::{
    ThreadContent, ThreadCursor, ThreadEpoch, ThreadItem, ThreadPage, ToolStatus, TurnIndex,
};
pub use time::Timestamp;
pub use tldr::Tldr;
pub use tolerant::Lenient;
pub use transcript::Transcript;
pub use version::BridgeVersion;
