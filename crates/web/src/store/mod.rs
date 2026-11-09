//! A small typed key/value store over `IndexedDB`: the snapshot cache and the
//! outbox (design §6.5). Outbox *logic* (flush, retry, stale cards) lives in
//! [`crate::outbox`]; this is only the durable storage it builds on.
//!
//! Values are stored as JSON strings of the [`StoreKey::Value`] type: one
//! serialization path (serde) for the wire and the disk, and no structured
//! clone surprises.
//!
//! # Connection lifecycle
//!
//! The app shares one connection (see `ui::AppState::store`). When another
//! tab opens a newer schema version, this connection closes itself
//! (`versionchange`) so the upgrade isn't blocked, and [`Store::is_open`]
//! turns false so the next use reopens. An open that is itself blocked by an
//! older tab fails with [`StoreError::Blocked`] instead of hanging.

pub mod keys;

use std::cell::Cell;
use std::rc::Rc;

use js_sys::{Array, Function, Promise};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    IdbDatabase, IdbObjectStore, IdbOpenDbRequest, IdbRequest, IdbTransaction, IdbTransactionMode,
};

pub use keys::{AgentsKey, OutboxKey, Snapshot, StoreKey, StoreName, TokenKey};

use crate::platform::{Global, js_error_message};

/// Database name.
pub const DB_NAME: &str = "passporter";
/// Schema version. Bump it, and extend [`create_stores`], to change stores.
pub const DB_VERSION: u32 = 1;

/// Why a store operation failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// `IndexedDB` is missing (private mode on some browsers) or threw.
    #[error("local storage unavailable: {0}")]
    Unavailable(String),
    /// Another tab holds an older version of the database open.
    #[error("close other Passporter tabs to finish updating local storage")]
    Blocked,
    /// A stored value no longer matches its type (schema drift, corruption).
    #[error("stored value is unreadable: {0}")]
    Corrupt(String),
}

impl StoreError {
    fn js(error: &JsValue) -> Self {
        Self::Unavailable(js_error_message(error))
    }
}

/// An open handle to the app's database. Cheap to clone; clones share the
/// connection.
#[derive(Debug, Clone)]
pub struct Store {
    db: IdbDatabase,
    closed: Rc<Cell<bool>>,
    /// Keeps the `versionchange` handler alive as long as the connection.
    _on_version_change: Rc<Closure<dyn FnMut()>>,
}

impl Store {
    /// Opens (creating or upgrading) the database.
    pub async fn open() -> Result<Self, StoreError> {
        let factory = Global::current()
            .indexed_db()
            .map_err(|e| StoreError::js(&e))?
            .ok_or_else(|| StoreError::Unavailable("IndexedDB is not supported".into()))?;
        let open = factory
            .open_with_u32(DB_NAME, DB_VERSION)
            .map_err(|e| StoreError::js(&e))?;

        let upgrading = open.clone();
        let on_upgrade = Closure::<dyn FnMut()>::new(move || create_stores(&upgrading));
        open.set_onupgradeneeded(Some(on_upgrade.as_ref().unchecked_ref()));
        let blocked = Rc::new(Cell::new(false));
        let promise = {
            let blocked = blocked.clone();
            Promise::new(&mut |resolve: Function, reject: Function| {
                open.set_onsuccess(Some(&resolve));
                open.set_onerror(Some(&reject));
                let blocked = blocked.clone();
                let on_blocked = Closure::once_into_js(move || {
                    blocked.set(true);
                    let _ = reject.call0(&JsValue::NULL);
                });
                open.set_onblocked(Some(on_blocked.unchecked_ref()));
            })
        };
        let outcome = JsFuture::from(promise).await;
        open.set_onsuccess(None);
        open.set_onerror(None);
        open.set_onblocked(None);

        if blocked.get() {
            // The request is still pending: if the old tab lets go later,
            // the upgrade runs and the open completes. Keep the upgrade
            // handler alive for that (once per blocked open), and close the
            // late connection straight away so it doesn't linger.
            on_upgrade.forget();
            let late = open.clone();
            let close_late = Closure::once_into_js(move || {
                if let Ok(db) = late.result().and_then(JsCast::dyn_into::<IdbDatabase>) {
                    db.close();
                }
            });
            open.set_onsuccess(Some(close_late.unchecked_ref()));
            return Err(StoreError::Blocked);
        }
        open.set_onupgradeneeded(None);
        drop(on_upgrade);
        if outcome.is_err() {
            return Err(open.error().ok().flatten().map_or_else(
                || StoreError::Unavailable("open failed".into()),
                |e| StoreError::Unavailable(e.message()),
            ));
        }

        let db: IdbDatabase = open
            .result()
            .and_then(JsCast::dyn_into)
            .map_err(|e| StoreError::js(&e))?;
        let closed = Rc::new(Cell::new(false));
        let on_version_change = {
            let db = db.clone();
            let closed = closed.clone();
            Closure::<dyn FnMut()>::new(move || {
                db.close();
                closed.set(true);
            })
        };
        db.set_onversionchange(Some(on_version_change.as_ref().unchecked_ref()));
        Ok(Self {
            db,
            closed,
            _on_version_change: Rc::new(on_version_change),
        })
    }

    /// Closes the connection now, rather than when the last clone is
    /// collected. For short-lived users (the service worker's push wake),
    /// so they never hold up another tab's schema upgrade.
    pub fn close(self) {
        self.db.close();
        self.closed.set(true);
    }

    /// Whether the connection is still usable; false after another tab
    /// upgraded the schema. Reopen when false.
    #[must_use]
    pub fn is_open(&self) -> bool {
        !self.closed.get()
    }

    /// Reads the value under `key`, or `None` if absent.
    pub async fn get<K: StoreKey>(&self, key: &K) -> Result<Option<K::Value>, StoreError> {
        let (_, store) = self.begin(K::STORE, IdbTransactionMode::Readonly)?;
        let request = store
            .get(&JsValue::from_str(&key.encode()))
            .map_err(|e| StoreError::js(&e))?;
        let value = settle(&request).await?;
        if value.is_undefined() {
            return Ok(None);
        }
        let json = value
            .as_string()
            .ok_or_else(|| StoreError::Corrupt("not a JSON string".into()))?;
        serde_json::from_str(&json)
            .map(Some)
            .map_err(|e| StoreError::Corrupt(e.to_string()))
    }

    /// Writes `value` under `key` and waits until the write is committed.
    pub async fn put<K: StoreKey>(&self, key: &K, value: &K::Value) -> Result<(), StoreError> {
        let json = serde_json::to_string(value).map_err(|e| StoreError::Corrupt(e.to_string()))?;
        let (tx, store) = self.begin(K::STORE, IdbTransactionMode::Readwrite)?;
        let done = committed(&tx);
        store
            .put_with_key(&JsValue::from_str(&json), &JsValue::from_str(&key.encode()))
            .map_err(|e| StoreError::js(&e))?;
        done.await
    }

    /// Removes `key`, if present, and waits until committed.
    pub async fn delete<K: StoreKey>(&self, key: &K) -> Result<(), StoreError> {
        let (tx, store) = self.begin(K::STORE, IdbTransactionMode::Readwrite)?;
        let done = committed(&tx);
        store
            .delete(&JsValue::from_str(&key.encode()))
            .map_err(|e| StoreError::js(&e))?;
        done.await
    }

    /// Every outbox record, oldest first, read in one transaction (one
    /// consistent snapshot, one round trip). Keys this app did not write
    /// are skipped; an unreadable value is reported as `Err` beside its key
    /// so the caller can leave it alone.
    pub async fn outbox_entries(
        &self,
    ) -> Result<Vec<(OutboxKey, Result<String, StoreError>)>, StoreError> {
        let (_, store) = self.begin(StoreName::Outbox, IdbTransactionMode::Readonly)?;
        let keys = store.get_all_keys().map_err(|e| StoreError::js(&e))?;
        let values = store.get_all().map_err(|e| StoreError::js(&e))?;
        // Both requests run in the same transaction, in key order, so the
        // arrays line up.
        let keys: Array = settle(&keys).await?.unchecked_into();
        let values: Array = settle(&values).await?.unchecked_into();
        Ok(keys
            .iter()
            .zip(values.iter())
            .filter_map(|(key, value)| {
                let key = OutboxKey::decode(&key.as_string()?)?;
                let entry = value
                    .as_string()
                    .ok_or_else(|| StoreError::Corrupt("not a JSON string".into()))
                    .and_then(|json| {
                        serde_json::from_str(&json).map_err(|e| StoreError::Corrupt(e.to_string()))
                    });
                Some((key, entry))
            })
            .collect())
    }

    /// Empties the snapshot cache. The outbox is deliberately not clearable
    /// from here: it holds unsent user decisions.
    pub async fn clear_snapshots(&self) -> Result<(), StoreError> {
        let (tx, store) = self.begin(StoreName::Snapshots, IdbTransactionMode::Readwrite)?;
        let done = committed(&tx);
        store.clear().map_err(|e| StoreError::js(&e))?;
        done.await
    }

    fn begin(
        &self,
        store: StoreName,
        mode: IdbTransactionMode,
    ) -> Result<(IdbTransaction, IdbObjectStore), StoreError> {
        if self.closed.get() {
            return Err(StoreError::Unavailable(
                "connection closed by an upgrade".into(),
            ));
        }
        let tx = self
            .db
            .transaction_with_str_and_mode(store.as_str(), mode)
            .map_err(|e| StoreError::js(&e))?;
        let object_store = tx
            .object_store(store.as_str())
            .map_err(|e| StoreError::js(&e))?;
        Ok((tx, object_store))
    }
}

/// `onupgradeneeded`: creates any missing object store. Runs inside the
/// version-change transaction, so a failure here aborts the open.
fn create_stores(open: &IdbOpenDbRequest) {
    let Ok(db) = open.result().and_then(JsCast::dyn_into::<IdbDatabase>) else {
        return;
    };
    let existing = db.object_store_names();
    for store in StoreName::ALL {
        if !existing.contains(store.as_str()) && db.create_object_store(store.as_str()).is_err() {
            if let Some(tx) = open.transaction() {
                // Best effort: the open then fails with the abort error.
                let _ = tx.abort();
            }
            return;
        }
    }
}

/// Resolves when `request` succeeds (with its result) or fails. The
/// handlers are attached at once, not on the first poll, so several
/// requests can be issued and then awaited in turn without missing an
/// event.
pub(crate) fn settle(
    request: &IdbRequest,
) -> impl Future<Output = Result<JsValue, StoreError>> + use<> {
    let promise = Promise::new(&mut |resolve: Function, reject: Function| {
        request.set_onsuccess(Some(&resolve));
        request.set_onerror(Some(&reject));
    });
    let request = request.clone();
    async move {
        let outcome = JsFuture::from(promise).await;
        request.set_onsuccess(None);
        request.set_onerror(None);
        match outcome {
            Ok(_) => request.result().map_err(|e| StoreError::js(&e)),
            Err(_) => Err(request.error().ok().flatten().map_or_else(
                || StoreError::Unavailable("request failed".into()),
                |e| StoreError::Unavailable(e.message()),
            )),
        }
    }
}

/// Resolves when `tx` commits; errors if it fails or aborts. Create it
/// before issuing requests, so no completion event can be missed.
pub(crate) fn committed(
    tx: &IdbTransaction,
) -> impl Future<Output = Result<(), StoreError>> + use<> {
    let promise = Promise::new(&mut |resolve: Function, reject: Function| {
        tx.set_oncomplete(Some(&resolve));
        tx.set_onerror(Some(&reject));
        tx.set_onabort(Some(&reject));
    });
    let tx = tx.clone();
    async move {
        let outcome = JsFuture::from(promise).await;
        tx.set_oncomplete(None);
        tx.set_onerror(None);
        tx.set_onabort(None);
        outcome.map(drop).map_err(|_| {
            tx.error().map_or_else(
                || StoreError::Unavailable("transaction aborted".into()),
                |e| StoreError::Unavailable(e.message()),
            )
        })
    }
}
