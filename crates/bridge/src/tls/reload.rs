//! Keeping the served leaf current: [`Poller`] decides when to reload,
//! [`spawn_reloader`] runs it on a timer and on SIGHUP, logs outcomes
//! without repeating itself, and warns as the served leaf nears expiry.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use tokio::signal::unix::Signal;
use tokio::task::JoinHandle;

use super::{CertStore, FileDigests, Leaf, TlsError};
use crate::shutdown::Shutdown;

/// How often [`spawn_reloader`] looks at the files for a renewed leaf.
pub const RELOAD_POLL: Duration = Duration::from_secs(60);

/// A served leaf closer to expiry than this is warned about (and
/// `passporter doctor` warns too).
pub const EXPIRY_WARNING: Duration = Duration::from_hours(14 * 24);

/// How often the expiry warning repeats while it applies.
const EXPIRY_NAG: Duration = Duration::from_hours(24);

/// Decides, poll by poll, when to reload a [`CertStore`].
///
/// A change is loaded once the files have held still since the previous
/// poll, so a renewal caught between writing the chain and the key waits a
/// poll instead of failing. A failure that the content explains (a key
/// that does not match) is not retried until the files change again; one
/// it does not (a key others can read, a leaf not valid yet) is retried at
/// every poll, so fixing the mode or waiting out clock skew is enough.
#[derive(Debug)]
pub struct Poller {
    store: Arc<CertStore>,
    /// The files' content at the previous poll, while a change settles.
    settling: Option<FileDigests>,
}

impl Poller {
    /// A poller for `store`, with nothing settling.
    #[must_use]
    pub fn new(store: Arc<CertStore>) -> Self {
        Self {
            store,
            settling: None,
        }
    }

    /// One poll: `Ok(Some)` with the new leaf when one was loaded,
    /// `Ok(None)` when nothing was due.
    pub fn poll(&mut self) -> Result<Option<Leaf>, TlsError> {
        let Some(now) = self.store.pending_change() else {
            self.settling = None;
            return Ok(None);
        };
        if self.settling.replace(now) != Some(now) {
            return Ok(None);
        }
        let reloaded = self.store.reload();
        // After a transient failure the files are still settled: retry at
        // the next poll without waiting another one.
        if !matches!(&reloaded, Err(error) if error.is_transient()) {
            self.settling = None;
        }
        reloaded.map(Some)
    }

    /// Reloads at once (SIGHUP), whatever the files did.
    pub fn force(&mut self) -> Result<Leaf, TlsError> {
        self.settling = None;
        self.store.reload()
    }
}

/// What to say about `leaf`'s expiry at `now`, if it is within
/// [`EXPIRY_WARNING`] or past.
fn expiry_notice(leaf: &Leaf, now: SystemTime) -> Option<String> {
    let not_after = leaf.validity()?.not_after;
    match not_after.remaining(now) {
        None => Some(format!(
            "the served TLS leaf expired at {not_after}: the edge refuses it; install a renewed leaf"
        )),
        Some(left) if left < EXPIRY_WARNING => Some(format!(
            "the served TLS leaf expires at {not_after}, in {} h: install a renewed leaf",
            left.as_secs() / 3600
        )),
        Some(_) => None,
    }
}

/// Logs reload outcomes and expiry notices, each at most once in a row
/// (expiry notices again after [`EXPIRY_NAG`]).
#[derive(Debug, Default)]
struct Reporter {
    last_error: Option<String>,
    last_notice: Option<Instant>,
}

impl Reporter {
    fn outcome(
        &mut self,
        outcome: &Result<Option<Leaf>, TlsError>,
        forced: bool,
        store: &CertStore,
    ) {
        match outcome {
            Ok(Some(leaf)) => {
                self.last_error = None;
                self.last_notice = None;
                tracing::info!(
                    cert = %store.config().cert.display(),
                    expires = %leaf.expiry_text(),
                    reason = if forced { "SIGHUP" } else { "files changed" },
                    "TLS leaf reloaded"
                );
            }
            Ok(None) => {}
            Err(error) => {
                let text = error.to_string();
                if self.last_error.as_ref() == Some(&text) {
                    tracing::debug!(%error, "TLS reload still failing");
                } else {
                    tracing::error!(%error, "TLS reload failed; still serving the previous leaf");
                    self.last_error = Some(text);
                }
            }
        }
    }

    fn expiry(&mut self, leaf: &Leaf, now: SystemTime, at: Instant) {
        let Some(notice) = expiry_notice(leaf, now) else {
            self.last_notice = None;
            return;
        };
        if self
            .last_notice
            .is_some_and(|last| at.duration_since(last) < EXPIRY_NAG)
        {
            return;
        }
        tracing::warn!("{notice}");
        self.last_notice = Some(at);
    }
}

/// Reloads `store` on each signal from `hangups` (the binary passes
/// SIGHUP, installed at startup), and every `poll` as its [`Poller`]
/// decides, until `shutdown`. A reload that fails is logged and the
/// current leaf kept. Each poll also checks the served leaf's expiry.
pub fn spawn_reloader(
    store: Arc<CertStore>,
    poll: Duration,
    mut hangups: Signal,
    mut shutdown: Shutdown,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(poll);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut poller = Some(Poller::new(Arc::clone(&store)));
        let mut reporter = Reporter::default();
        reporter.expiry(&store.current(), SystemTime::now(), Instant::now());
        ticks.tick().await;
        loop {
            let forced = tokio::select! {
                () = shutdown.wait() => return,
                _ = ticks.tick() => false,
                Some(()) = hangups.recv() => true,
            };
            let mut task_poller = poller
                .take()
                .unwrap_or_else(|| Poller::new(Arc::clone(&store)));
            let joined = tokio::task::spawn_blocking(move || {
                let outcome = if forced {
                    task_poller.force().map(Some)
                } else {
                    task_poller.poll()
                };
                (task_poller, outcome)
            })
            .await;
            match joined {
                Ok((task_poller, outcome)) => {
                    poller = Some(task_poller);
                    reporter.outcome(&outcome, forced, &store);
                }
                Err(error) => tracing::error!(%error, "TLS reload task failed"),
            }
            reporter.expiry(&store.current(), SystemTime::now(), Instant::now());
        }
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use rustls::pki_types::DnsName;

    use super::*;
    use crate::config::{DEFAULT_TLS_NAME, TlsConfig};

    /// A self-signed leaf for the default name, valid from `from` days
    /// ago until `until` days from now: (chain PEM, key PEM).
    fn self_signed(from: i64, until: i64) -> (String, String) {
        let now = time::OffsetDateTime::now_utc();
        let key = rcgen::KeyPair::generate().expect("key");
        let mut params =
            rcgen::CertificateParams::new(vec![DEFAULT_TLS_NAME.to_owned()]).expect("params");
        params.not_before = now - time::Duration::days(from);
        params.not_after = now + time::Duration::days(until);
        let cert = params.self_signed(&key).expect("cert");
        (cert.pem(), key.serialize_pem())
    }

    fn config(dir: &Path) -> TlsConfig {
        TlsConfig {
            cert: dir.join("leaf.pem"),
            key: dir.join("leaf.key"),
            name: DnsName::try_from(DEFAULT_TLS_NAME)
                .expect("name")
                .to_owned(),
        }
    }

    fn install(config: &TlsConfig, (chain, key): &(String, String), key_mode: u32) {
        fs::write(&config.cert, chain).expect("write chain");
        fs::write(&config.key, key).expect("write key");
        fs::set_permissions(&config.key, fs::Permissions::from_mode(key_mode)).expect("chmod");
    }

    fn poller(dir: &Path) -> Poller {
        let config = config(dir);
        install(&config, &self_signed(30, 90), 0o600);
        Poller::new(Arc::new(CertStore::load(&config).expect("loads")))
    }

    #[test]
    fn a_change_is_reloaded_only_once_it_holds_still_for_a_poll() {
        let dir = tempfile::tempdir().expect("tmp");
        let mut poller = poller(dir.path());
        assert!(matches!(poller.poll(), Ok(None)), "nothing changed");

        // A renewal lands the chain first; this poll sees the old key.
        let (chain, key) = self_signed(30, 90);
        fs::write(&poller.store.config().cert, chain).expect("write chain");
        assert!(matches!(poller.poll(), Ok(None)), "settling");
        // The key lands before the next poll: still settling, no attempt,
        // so no false "key mismatch" alarm.
        fs::write(&poller.store.config().key, key).expect("write key");
        assert!(matches!(poller.poll(), Ok(None)), "still settling");
        // Unchanged for a whole poll: now the pair is loaded.
        assert!(matches!(poller.poll(), Ok(Some(_))));
        assert!(matches!(poller.poll(), Ok(None)));
    }

    #[test]
    fn a_content_failure_waits_for_the_files_to_change() {
        let dir = tempfile::tempdir().expect("tmp");
        let mut poller = poller(dir.path());
        let config = poller.store.config().clone();
        let other = self_signed(30, 90);
        fs::write(&config.key, &other.1).expect("write key");
        assert!(matches!(poller.poll(), Ok(None)), "settling");
        assert!(matches!(poller.poll(), Err(TlsError::KeyMismatch { .. })));
        assert!(matches!(poller.poll(), Ok(None)), "not retried");
        assert!(matches!(poller.poll(), Ok(None)), "not retried");
    }

    #[test]
    fn a_chmod_alone_lets_the_next_poll_load_the_renewal() {
        let dir = tempfile::tempdir().expect("tmp");
        let mut poller = poller(dir.path());
        let config = poller.store.config().clone();
        install(&config, &self_signed(30, 90), 0o644);
        assert!(matches!(poller.poll(), Ok(None)), "settling");
        assert!(matches!(poller.poll(), Err(TlsError::TooOpen { .. })));
        assert!(
            matches!(poller.poll(), Err(TlsError::TooOpen { .. })),
            "retried while it is open"
        );
        // The operator runs the chmod the error asks for; nothing else.
        fs::set_permissions(&config.key, fs::Permissions::from_mode(0o600)).expect("chmod");
        assert!(matches!(poller.poll(), Ok(Some(_))));
        assert!(matches!(poller.poll(), Ok(None)));
    }

    #[test]
    fn a_leaf_not_valid_yet_is_retried_until_it_is() {
        let dir = tempfile::tempdir().expect("tmp");
        let mut poller = poller(dir.path());
        let config = poller.store.config().clone();
        install(&config, &self_signed(-1, 90), 0o600);
        assert!(matches!(poller.poll(), Ok(None)), "settling");
        assert!(matches!(poller.poll(), Err(TlsError::NotYetValid { .. })));
        // Not marked as tried: every poll checks the clock again.
        assert!(matches!(poller.poll(), Err(TlsError::NotYetValid { .. })));
        assert!(poller.store.pending_change().is_some());
    }

    #[test]
    fn an_in_place_rewrite_of_the_same_size_is_a_change() {
        let dir = tempfile::tempdir().expect("tmp");
        let poller = poller(dir.path());
        assert!(poller.store.pending_change().is_none());
        let path = &poller.store.config().cert;
        let mut chain = fs::read(path).expect("read");
        // Same length, same inode, likely the same timestamp tick.
        let byte = chain.len() - 30;
        chain[byte] ^= 1;
        fs::write(path, chain).expect("write");
        assert!(poller.store.pending_change().is_some());
    }

    #[test]
    fn the_served_leaf_is_warned_about_near_and_past_expiry() {
        let dir = tempfile::tempdir().expect("tmp");
        let poller = poller(dir.path());
        let leaf = poller.store.current();
        let now = SystemTime::now();
        assert!(expiry_notice(&leaf, now).is_none(), "90 days left");
        let near = now + Duration::from_hours(80 * 24);
        assert!(
            expiry_notice(&leaf, near).is_some_and(|n| n.contains("expires at")),
            "10 days left"
        );
        let past = now + Duration::from_hours(91 * 24);
        assert!(expiry_notice(&leaf, past).is_some_and(|n| n.contains("expired at")));

        let mut reporter = Reporter::default();
        let start = Instant::now();
        reporter.expiry(&leaf, near, start);
        assert_eq!(reporter.last_notice, Some(start));
        reporter.expiry(&leaf, near, start + Duration::from_hours(1));
        assert_eq!(
            reporter.last_notice,
            Some(start),
            "not repeated within a day"
        );
        let later = start + EXPIRY_NAG;
        reporter.expiry(&leaf, near, later);
        assert_eq!(reporter.last_notice, Some(later), "repeated daily");
    }
}
