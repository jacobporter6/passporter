//! The agent registry: herdr's agents as protocol [`Agent`](protocol::Agent)s,
//! kept live from herdr events.
//!
//! - [`Hub`] holds the current list and broadcasts changes to subscribers.
//! - [`spawn`] starts the driver that fills the hub from herdr.

mod backoff;
mod driver;
mod hub;
mod mapping;
mod quiet;
mod repo;

pub(crate) use driver::now;
pub use driver::{RegistrySettings, spawn};
pub use hub::{Feed, Hub, Placement};
pub(crate) use mapping::{
    agent_id, ask, kind as agent_kind, sanitize_label, status as protocol_status,
};
pub use quiet::{QuietDone, Verdict};
