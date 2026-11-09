//! Photo attachments from the composer (design §5): pick a photo from the
//! phone's camera or gallery, see its thumbnail, and send it with the
//! prompt.
//!
//! - [`state`]: the photo's upload, the Send gate and what the prompt
//!   carries; pure, tested on the host.
//! - [`view`] (the app): the attach button, the preview's slot and the
//!   photo's state.
//! - [`preview`] and [`upload`] (the extras module): the preview (the
//!   thumbnail, progress, remove), decoding, shrinking to a 2048 px JPEG
//!   and `POST /v1/attachments`, all kept out of the app's WASM.
//!
//! The upload starts as soon as a photo is picked, while the user types;
//! Send waits for it. The prompt then goes through the outbox like any
//! other, naming the photo by its id, so a dead zone after the upload
//! costs nothing.

pub mod preview;
pub mod state;
pub mod upload;
pub mod view;
