//! A cloneable "stop now" signal shared by the server, the registry and every
//! WebSocket connection.

use tokio::sync::watch;

/// Fires [`Shutdown`] for every clone. Dropping it without calling
/// [`trigger`](Self::trigger) also counts as shutdown, so no task can outlive
/// its owner by accident.
#[derive(Debug)]
pub struct ShutdownTrigger(watch::Sender<bool>);

/// Resolves once shutdown has been triggered. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Shutdown(watch::Receiver<bool>);

/// A connected trigger and signal.
#[must_use]
pub fn channel() -> (ShutdownTrigger, Shutdown) {
    let (tx, rx) = watch::channel(false);
    (ShutdownTrigger(tx), Shutdown(rx))
}

impl ShutdownTrigger {
    /// Tells every [`Shutdown`] to stop.
    pub fn trigger(&self) {
        self.0.send_replace(true);
    }
}

impl Shutdown {
    /// Waits until shutdown is triggered (or the trigger is dropped).
    pub async fn wait(&mut self) {
        // An error means the trigger is gone, which is shutdown too.
        let _ = self.0.wait_for(|stop| *stop).await;
    }

    /// [`wait`](Self::wait), taking ownership: for APIs that want a `'static` future.
    pub async fn wait_owned(mut self) {
        self.wait().await;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn trigger_reaches_every_clone() {
        let (trigger, mut a) = channel();
        let mut b = a.clone();
        trigger.trigger();
        a.wait().await;
        b.wait().await;
    }

    #[tokio::test]
    async fn dropping_the_trigger_is_shutdown() {
        let (trigger, signal) = channel();
        drop(trigger);
        tokio::time::timeout(Duration::from_secs(1), signal.wait_owned())
            .await
            .expect("resolves");
    }

    #[tokio::test]
    async fn pending_until_triggered() {
        let (_trigger, mut signal) = channel();
        let waited = tokio::time::timeout(Duration::from_millis(20), signal.wait()).await;
        assert!(waited.is_err());
    }
}
