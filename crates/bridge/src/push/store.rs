//! Push subscriptions per device, persisted as one private JSON file.
//!
//! A device (the authenticated [`Principal`](crate::server::Principal)) may
//! hold a few subscriptions (one per browser profile it pushes to). An
//! endpoint belongs to one device: subscribing it again from another device
//! moves it. Every change rewrites the file atomically (write a fresh
//! `0600` temporary file, sync, rename, sync the directory), so a crash
//! leaves the old or the new list, never half of one. All methods block on
//! the file system.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};

use crate::private::write_atomically;

use super::subscription::{Endpoint, Subscription};

/// Most subscriptions one device may hold.
pub const MAX_PER_DEVICE: usize = 8;

/// The store's file name inside the state dir.
pub const STORE_FILE: &str = "push-subscriptions.json";

/// Why the store could not be read or changed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A built-in registration cannot be reassigned to another device.
    #[error("native registration belongs to another device")]
    Owned,
    /// The file could not be read or written.
    #[error("push subscription store {}: {source}", path.display())]
    Io {
        /// The store file.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// The file is not a store this bridge understands.
    #[error("push subscription store {} is unreadable: {message}", path.display())]
    Corrupt {
        /// The store file.
        path: PathBuf,
        /// What was wrong (never the file's contents).
        message: String,
    },
    /// The device already holds [`MAX_PER_DEVICE`] subscriptions.
    #[error("this device already has {MAX_PER_DEVICE} push subscriptions")]
    Full,
}

/// What [`SubscriptionStore::put_unless`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Put {
    /// A new subscription.
    Created,
    /// The endpoint was known; its keys and owner are now these.
    Replaced,
}

/// The file's shape.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct File {
    version: u32,
    /// Device id → its subscriptions.
    devices: BTreeMap<String, Vec<Subscription>>,
}

const VERSION: u32 = 1;

/// Every device's subscriptions.
#[derive(Debug)]
pub struct SubscriptionStore {
    path: PathBuf,
    devices: Mutex<BTreeMap<String, Vec<Subscription>>>,
}

impl SubscriptionStore {
    /// Opens the store at `path`; a missing file is an empty store.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let path = path.into();
        let devices = match std::fs::read(&path) {
            Ok(bytes) => {
                let file: File =
                    serde_json::from_slice(&bytes).map_err(|e| StoreError::Corrupt {
                        path: path.clone(),
                        // Line and column only: the message could quote a key.
                        message: format!("invalid JSON at line {} column {}", e.line(), e.column()),
                    })?;
                if file.version != VERSION {
                    return Err(StoreError::Corrupt {
                        path,
                        message: format!("unknown version {}", file.version),
                    });
                }
                file.devices
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(source) => return Err(StoreError::Io { path, source }),
        };
        Ok(Self {
            path,
            devices: Mutex::new(devices),
        })
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, Vec<Subscription>>> {
        self.devices.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// [`Self::put_unless`] for a device that is never revoked (tests).
    #[cfg(test)]
    pub(crate) fn put(&self, device: &str, subscription: Subscription) -> Result<Put, StoreError> {
        self.put_unless(device, subscription, || false)
            .map(|put| put.expect("never revoked"))
    }

    /// Adds `subscription` for `device`, or replaces the one with its
    /// endpoint (whichever device held it), unless `revoked` (asked under the store's lock) says
    /// `device` is revoked by now: then nothing is stored and the answer is
    /// `None`. A revocation marks the device before its subscriptions are
    /// forgotten (under this lock too), so a subscribe racing a revoke is
    /// either refused here or forgotten after: never kept.
    pub fn put_unless(
        &self,
        device: &str,
        subscription: Subscription,
        revoked: impl FnOnce() -> bool,
    ) -> Result<Option<Put>, StoreError> {
        let mut devices = self.lock();
        if revoked() {
            return Ok(None);
        }
        if super::native::registration(&subscription.endpoint).is_some()
            && devices.iter().any(|(owner, subscriptions)| {
                owner != device
                    && subscriptions
                        .iter()
                        .any(|s| s.endpoint == subscription.endpoint)
            })
        {
            return Err(StoreError::Owned);
        }
        let mut next = devices.clone();
        let existed = remove_endpoint(&mut next, &subscription.endpoint);
        let list = next.entry(device.to_owned()).or_default();
        if list.len() >= MAX_PER_DEVICE {
            return Err(StoreError::Full);
        }
        list.push(subscription);
        self.save(&next)?;
        *devices = next;
        Ok(Some(if existed { Put::Replaced } else { Put::Created }))
    }

    /// Removes `device`'s subscription at `endpoint`. False if it had none.
    pub fn remove(&self, device: &str, endpoint: &Endpoint) -> Result<bool, StoreError> {
        let mut devices = self.lock();
        let owned = devices
            .get(device)
            .is_some_and(|list| list.iter().any(|s| s.endpoint == *endpoint));
        if !owned {
            return Ok(false);
        }
        let mut next = devices.clone();
        remove_endpoint(&mut next, endpoint);
        self.save(&next)?;
        *devices = next;
        Ok(true)
    }

    /// Forgets `endpoint` whoever holds it (the push service said it is
    /// gone). False if it was not stored.
    pub fn forget(&self, endpoint: &Endpoint) -> Result<bool, StoreError> {
        let mut devices = self.lock();
        let mut next = devices.clone();
        if !remove_endpoint(&mut next, endpoint) {
            return Ok(false);
        }
        self.save(&next)?;
        *devices = next;
        Ok(true)
    }

    /// Forgets every subscription `device` holds (the device was revoked).
    /// Returns how many there were.
    pub fn forget_device(&self, device: &str) -> Result<usize, StoreError> {
        let mut devices = self.lock();
        let Some(count) = devices.get(device).map(Vec::len) else {
            return Ok(0);
        };
        let mut next = devices.clone();
        next.remove(device);
        self.save(&next)?;
        *devices = next;
        Ok(count)
    }

    /// Forgets the subscriptions of every device not in the set `known`
    /// returns (devices revoked while the bridge was down). `known` is
    /// asked under the store's lock, so a device that subscribes meanwhile
    /// (it must be known to authenticate) is never swept. Returns how many
    /// subscriptions were dropped; none if `known` is `None` (not known).
    pub fn retain_devices(
        &self,
        known: impl FnOnce() -> Option<BTreeSet<String>>,
    ) -> Result<usize, StoreError> {
        let mut devices = self.lock();
        let Some(known) = known() else {
            return Ok(0);
        };
        let count: usize = devices
            .iter()
            .filter(|(device, _)| !known.contains(*device))
            .map(|(_, list)| list.len())
            .sum();
        if count == 0 {
            return Ok(0);
        }
        let mut next = devices.clone();
        next.retain(|device, _| known.contains(device));
        self.save(&next)?;
        *devices = next;
        Ok(count)
    }

    /// `device`'s subscriptions, oldest first.
    #[must_use]
    pub fn of(&self, device: &str) -> Vec<Subscription> {
        self.lock().get(device).cloned().unwrap_or_default()
    }

    /// Every device that has at least one subscription.
    #[must_use]
    pub fn devices(&self) -> Vec<String> {
        self.lock().keys().cloned().collect()
    }

    fn save(&self, devices: &BTreeMap<String, Vec<Subscription>>) -> Result<(), StoreError> {
        let io_err = |source| StoreError::Io {
            path: self.path.clone(),
            source,
        };
        let file = File {
            version: VERSION,
            devices: devices.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&file).map_err(|e| io_err(io::Error::other(e)))?;
        write_atomically(&self.path, &bytes).map_err(io_err)
    }
}

/// Drops `endpoint` from whichever device holds it, and devices left with
/// none. True if it was there.
fn remove_endpoint(devices: &mut BTreeMap<String, Vec<Subscription>>, endpoint: &Endpoint) -> bool {
    let mut found = false;
    devices.retain(|_, list| {
        let before = list.len();
        list.retain(|s| s.endpoint != *endpoint);
        found |= list.len() != before;
        !list.is_empty()
    });
    found
}

#[cfg(test)]
pub(crate) mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use super::*;
    use crate::push::ece::tests::Receiver;

    /// A valid subscription at `https://push.example.com/<name>`.
    pub(crate) fn subscription(name: &str) -> Subscription {
        let receiver = Receiver::new();
        serde_json::from_value(serde_json::json!({
            "endpoint": format!("https://push.example.com/{name}"),
            "keys": {"p256dh": receiver.p256dh(), "auth": receiver.auth()},
        }))
        .expect("valid")
    }

    fn endpoints(store: &SubscriptionStore, device: &str) -> Vec<String> {
        store
            .of(device)
            .iter()
            .map(|s| s.endpoint.as_str().to_owned())
            .collect()
    }

    #[test]
    fn a_device_revoked_before_its_subscribe_lands_keeps_nothing() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = SubscriptionStore::open(dir.path().join(STORE_FILE)).expect("empty");
        let put = store
            .put_unless("phone", subscription("a"), || true)
            .expect("put");
        assert_eq!(put, None);
        assert_eq!(store.of("phone"), Vec::new());
        let put = store
            .put_unless("phone", subscription("a"), || false)
            .expect("put");
        assert_eq!(put, Some(Put::Created));
        assert_eq!(store.of("phone").len(), 1);
    }

    #[test]
    fn crud_round_trip_persists_privately() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("state").join(STORE_FILE);
        let store = SubscriptionStore::open(&path).expect("empty");
        assert_eq!(store.devices(), Vec::<String>::new());

        assert_eq!(
            store.put("phone", subscription("a")).expect("put"),
            Put::Created
        );
        assert_eq!(
            store.put("phone", subscription("b")).expect("put"),
            Put::Created
        );
        // Same endpoint, new keys: replaced, not duplicated.
        assert_eq!(
            store.put("phone", subscription("a")).expect("put"),
            Put::Replaced
        );
        assert_eq!(
            endpoints(&store, "phone"),
            ["https://push.example.com/b", "https://push.example.com/a"]
        );
        let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().expect("dir")), 0o700);

        let reopened = SubscriptionStore::open(&path).expect("reopens");
        assert_eq!(reopened.of("phone"), store.of("phone"));

        let a: Endpoint = "https://push.example.com/a".parse().expect("endpoint");
        assert!(!store.remove("tablet", &a).expect("not theirs"));
        assert!(store.remove("phone", &a).expect("removed"));
        assert!(!store.remove("phone", &a).expect("already gone"));
        assert_eq!(endpoints(&store, "phone"), ["https://push.example.com/b"]);

        let b: Endpoint = "https://push.example.com/b".parse().expect("endpoint");
        assert!(store.forget(&b).expect("forgotten"));
        assert_eq!(store.devices(), Vec::<String>::new());
        assert_eq!(
            SubscriptionStore::open(&path).expect("reopens").devices(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn forgetting_a_device_drops_only_its_subscriptions() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join(STORE_FILE);
        let store = SubscriptionStore::open(&path).expect("empty");
        store.put("phone", subscription("a")).expect("put");
        store.put("phone", subscription("b")).expect("put");
        store.put("tablet", subscription("c")).expect("put");

        assert_eq!(store.forget_device("phone").expect("forgotten"), 2);
        assert_eq!(store.forget_device("phone").expect("already gone"), 0);
        assert_eq!(store.devices(), ["tablet"]);
        assert_eq!(
            SubscriptionStore::open(&path).expect("reopens").devices(),
            ["tablet"]
        );
    }

    #[test]
    fn unknown_devices_are_swept() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join(STORE_FILE);
        let store = SubscriptionStore::open(&path).expect("empty");
        store.put("phone", subscription("a")).expect("put");
        store.put("gone", subscription("b")).expect("put");
        store.put("gone", subscription("c")).expect("put");
        let known = BTreeSet::from(["phone".to_owned(), "never-subscribed".to_owned()]);

        let known = || Some(known.clone());
        assert_eq!(store.retain_devices(known).expect("swept"), 2);
        assert_eq!(store.retain_devices(known).expect("nothing left"), 0);
        assert_eq!(store.retain_devices(|| None).expect("unknown"), 0);
        assert_eq!(store.devices(), ["phone"]);
        assert_eq!(
            SubscriptionStore::open(&path).expect("reopens").devices(),
            ["phone"]
        );
    }

    #[test]
    fn a_stale_world_readable_tmp_file_is_not_reused() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join(STORE_FILE);
        let tmp = dir.path().join(format!("{STORE_FILE}.tmp"));
        std::fs::write(&tmp, "left over from a crash").expect("write");
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).expect("chmod");

        let store = SubscriptionStore::open(&path).expect("empty");
        store.put("phone", subscription("a")).expect("put");

        let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(!tmp.exists(), "renamed into place");
        assert_eq!(
            SubscriptionStore::open(&path).expect("reopens").of("phone"),
            store.of("phone")
        );
    }

    #[test]
    fn an_endpoint_moves_to_the_device_that_subscribes_it_last() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = SubscriptionStore::open(dir.path().join(STORE_FILE)).expect("empty");
        store.put("old-pairing", subscription("a")).expect("put");
        assert_eq!(
            store.put("new-pairing", subscription("a")).expect("put"),
            Put::Replaced
        );
        assert_eq!(store.devices(), ["new-pairing"]);
    }

    #[test]
    fn a_device_is_capped() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = SubscriptionStore::open(dir.path().join(STORE_FILE)).expect("empty");
        for i in 0..MAX_PER_DEVICE {
            store
                .put("phone", subscription(&i.to_string()))
                .expect("put");
        }
        assert!(matches!(
            store.put("phone", subscription("one-more")),
            Err(StoreError::Full)
        ));
        // Re-subscribing a known endpoint still works at the cap.
        assert_eq!(
            store.put("phone", subscription("0")).expect("put"),
            Put::Replaced
        );
        assert_eq!(store.of("phone").len(), MAX_PER_DEVICE);
    }

    #[test]
    fn a_corrupt_file_is_refused_without_quoting_it() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join(STORE_FILE);
        std::fs::write(&path, "{\"version\":1,\"devices\":SECRET").expect("write");
        let err = SubscriptionStore::open(&path).expect_err("corrupt");
        assert!(matches!(err, StoreError::Corrupt { .. }), "{err}");
        assert!(!err.to_string().contains("SECRET"), "{err}");
        std::fs::write(&path, "{\"version\":9,\"devices\":{}}").expect("write");
        assert!(SubscriptionStore::open(&path).is_err());
    }
}
