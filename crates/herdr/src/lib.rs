//! Typed client for the [herdr](https://herdr.dev) socket API.
//!
//! Passporter's bridge drives herdr through the [`Herdr`] trait. [`SocketClient`] speaks herdr's newline-delimited JSON
//! protocol over its unix socket; `FakeHerdr` (feature `fake`) is an
//! in-memory stand-in with scripted state transitions for tests.
//!
//! Two herdr semantics callers must respect:
//! - a timeout or stalled prompt does **not** prove non-delivery (see
//!   [`Error::may_have_taken_effect`]): inspect state before retrying;
//! - `agent.prompt` refuses agents waiting at a dialog (`agent_blocked`);
//!   dialogs are answered with keys.

mod client;
mod error;
mod events;
mod ids;
mod model;

#[cfg(feature = "fake")]
pub mod fake;
#[cfg(feature = "fake")]
pub use fake::{Call, FakeHerdr, Fault, Method, TICK};
#[cfg(feature = "fake")]
pub mod replay;
#[cfg(feature = "fake")]
pub use replay::ReplaySocket;

pub use client::{EventStream, Herdr, ShellWait, SocketClient, SocketEvents, StartAgent};
pub use error::{Error, ErrorCode, Result};
pub use events::{Event, Subscription};
pub use ids::{AgentName, InvalidAgentName, PaneId, TabId, Target, TerminalId, WorkspaceId};
pub use model::{
    Agent, AgentSession, AgentStatus, CreatedTab, CreatedWorkspace, Pane, ReadSource, Screen,
    ServerInfo, SplitDirection, Tab, Workspace,
};
