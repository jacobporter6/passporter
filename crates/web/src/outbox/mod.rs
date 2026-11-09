//! The offline outbox (design §6.5): every reply, key, choice and interrupt
//! is stored in `IndexedDB` with its idempotency key before it is sent,
//! retried with backoff until the bridge answers, and never sent again
//! after a definitive answer.
//!
//! - [`machine`]: the pure state machine, tested on the host.
//! - [`record`]: the compact stored form of an entry.
//! - [`service`]: the browser side: storage, flushing, retry triggers.
//! - [`view`]: per-message status rows and the decision cards.

pub mod machine;
pub mod record;
pub mod service;
pub mod view;

pub use machine::{Delivery, Entry, Outbox};
pub use service::{OutboxService, use_outbox};
pub use view::OutboxTray;
