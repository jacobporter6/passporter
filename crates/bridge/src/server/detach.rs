//! Work that must outlive the request asking for it.

use std::future::Future;

use tokio::task::JoinError;

/// Runs `work` in its own task and waits for it. Dropping the waiting
/// request (client gone, timeout) does not cancel `work`, so bookkeeping
/// it does is never left half done. `Err` only if the task panicked or
/// the runtime is shutting down.
pub(crate) async fn detached<T: Send + 'static>(
    work: impl Future<Output = T> + Send + 'static,
) -> Result<T, JoinError> {
    tokio::spawn(work).await
}
