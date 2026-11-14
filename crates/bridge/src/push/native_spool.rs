//! The built-in transport's spool ([`super::native`]): sealed envelopes,
//! ciphertext only, kept in a private file until the companion takes
//! them, so a companion that is down, or a bridge restart, delays a
//! notice instead of losing it. Bounded in entries, per registration and
//! in bytes; the oldest go first.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};

use super::StoreError;
use super::native::{MAX_ENVELOPE, now};
use crate::private::write_atomically;

/// The spool's file, beside the subscription store.
pub(super) const SPOOL_FILE: &str = "push-native-spool.json";

/// Most envelopes kept.
const MAX_ITEMS: usize = 256;

/// Most envelopes kept for one registration (one phone).
const MAX_PER_REGISTRATION: usize = 16;

/// Largest spool file read back.
const MAX_BYTES: u64 = 2 * 1024 * 1024;

/// One sealed notice waiting for the companion.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Pending {
    /// Whose topic it goes to.
    pub registration: protocol::IdempotencyKey,
    /// Unix seconds; dropped unsent after.
    pub expires: u64,
    /// The signed envelope's JSON.
    pub envelope: String,
}

/// The spool: the file and its contents in memory, by message id.
#[derive(Debug)]
pub(super) struct Spool {
    path: PathBuf,
    items: Mutex<BTreeMap<String, Pending>>,
}

impl Spool {
    /// Reads the spool at `path` (none yet is an empty one).
    pub(super) fn open(path: &Path) -> Result<Self, StoreError> {
        let corrupt = |message: &str| StoreError::Corrupt {
            path: path.to_owned(),
            message: message.into(),
        };
        let items = if path.exists() {
            let bytes = crate::config::read_private(path, MAX_BYTES)
                .map_err(|_| corrupt("cannot read the built-in notice spool privately"))?;
            let items: BTreeMap<String, Pending> = serde_json::from_slice(&bytes)
                .map_err(|_| corrupt("invalid built-in notice spool"))?;
            if items.len() > MAX_ITEMS || items.values().any(|i| i.envelope.len() > MAX_ENVELOPE) {
                return Err(corrupt("built-in notice spool exceeds its limits"));
            }
            items
        } else {
            BTreeMap::new()
        };
        Ok(Self {
            path: path.to_owned(),
            items: Mutex::new(items),
        })
    }

    /// Adds `item` as `id`, dropping expired envelopes and, past a bound,
    /// the oldest (of its registration first, then of all).
    pub(super) fn put(&self, id: &str, item: &Pending) -> io::Result<()> {
        let mut items = self.lock();
        let mut next = items.clone();
        next.insert(id.to_owned(), item.clone());
        let now = now();
        next.retain(|_, entry| entry.expires > now);
        while next
            .values()
            .filter(|entry| entry.registration == item.registration)
            .count()
            > MAX_PER_REGISTRATION
        {
            let Some(oldest) = oldest(&next, |entry| entry.registration == item.registration)
            else {
                break;
            };
            next.remove(&oldest);
        }
        while next.len() > MAX_ITEMS {
            let Some(oldest) = oldest(&next, |_| true) else {
                break;
            };
            next.remove(&oldest);
        }
        write_atomically(&self.path, &serde_json::to_vec(&next)?)?;
        *items = next;
        Ok(())
    }

    /// Everything spooled now.
    pub(super) fn snapshot(&self) -> BTreeMap<String, Pending> {
        self.lock().clone()
    }

    /// Drops `id` (sent, expired or orphaned).
    pub(super) fn remove(&self, id: &str) -> io::Result<()> {
        let mut items = self.lock();
        if !items.contains_key(id) {
            return Ok(());
        }
        let mut next = items.clone();
        next.remove(id);
        write_atomically(&self.path, &serde_json::to_vec(&next)?)?;
        *items = next;
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, Pending>> {
        self.items.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The id of the entry of `items` that expires first among those `which`
/// selects. Every entry lives the same time, so that is the oldest.
fn oldest(items: &BTreeMap<String, Pending>, which: impl Fn(&Pending) -> bool) -> Option<String> {
    items
        .iter()
        .filter(|(_, entry)| which(entry))
        .min_by_key(|(_, entry)| entry.expires)
        .map(|(id, _)| id.clone())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn registration(n: u8) -> protocol::IdempotencyKey {
        protocol::IdempotencyKey::new(format!("00000000-0000-4000-8000-0000000000{n:02}"))
            .expect("uuid")
    }

    fn pending(registration: protocol::IdempotencyKey, expires: u64) -> Pending {
        Pending {
            registration,
            expires,
            envelope: "ciphertext".into(),
        }
    }

    #[test]
    fn envelopes_survive_a_restart_privately_and_are_bounded_per_registration() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("spool.json");
        let spool = Spool::open(&path).expect("open");
        for i in 0..30_u64 {
            spool
                .put(&i.to_string(), &pending(registration(1), now() + 60 + i))
                .expect("put");
        }
        spool
            .put("other", &pending(registration(2), now() + 60))
            .expect("put");
        let mode = std::fs::metadata(&path).expect("file").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let restarted = Spool::open(&path).expect("reopen");
        let items = restarted.snapshot();
        assert_eq!(items.len(), MAX_PER_REGISTRATION + 1);
        assert!(!items.contains_key("0"), "the oldest went first");
        assert!(items.contains_key("29") && items.contains_key("other"));
        restarted.remove("29").expect("remove");
        restarted.remove("missing").expect("nothing to remove");
        assert_eq!(
            Spool::open(&path).expect("reopen").snapshot().len(),
            MAX_PER_REGISTRATION
        );
    }

    #[test]
    fn expired_envelopes_are_dropped_and_the_whole_spool_is_capped() {
        let dir = tempfile::tempdir().expect("tmp");
        let spool = Spool::open(&dir.path().join("spool.json")).expect("open");
        spool
            .put("stale", &pending(registration(1), now().saturating_sub(1)))
            .expect("put");
        spool
            .put("fresh", &pending(registration(1), now() + 60))
            .expect("put");
        assert_eq!(
            spool.snapshot().keys().collect::<Vec<_>>(),
            ["fresh"],
            "a put sweeps what expired"
        );
        for n in 0..=(MAX_ITEMS / MAX_PER_REGISTRATION) {
            let n = u8::try_from(n).expect("small");
            for i in 0..MAX_PER_REGISTRATION as u64 {
                spool
                    .put(
                        &format!("{n}-{i}"),
                        &pending(registration(n), now() + 100 + i),
                    )
                    .expect("put");
            }
        }
        assert_eq!(spool.snapshot().len(), MAX_ITEMS);
    }

    #[test]
    fn an_oversized_or_invalid_spool_is_refused() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("spool.json");
        write_atomically(&path, b"not json").expect("write");
        assert!(matches!(
            Spool::open(&path),
            Err(StoreError::Corrupt { .. })
        ));
        let big = serde_json::json!({
            "x": {"registration": registration(1), "expires": 1, "envelope": "a".repeat(MAX_ENVELOPE + 1)},
        });
        write_atomically(&path, big.to_string().as_bytes()).expect("write");
        assert!(matches!(
            Spool::open(&path),
            Err(StoreError::Corrupt { .. })
        ));
    }
}
