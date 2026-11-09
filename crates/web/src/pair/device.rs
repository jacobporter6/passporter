//! This device's key (design §8): a **non-extractable** `WebCrypto` P-256
//! key pair. Only the public half ever leaves the browser (exported raw,
//! for `POST /v1/auth/pair`); the private half signs challenges and is
//! kept, as a `CryptoKey`, in its own `IndexedDB` database.
//!
//! The key lives apart from the app's JSON store (`crate::store`): a
//! `CryptoKey` can only be stored by structured clone, never as JSON, and
//! this database has its own, simpler lifecycle (one record, written at
//! pairing, deleted on forget or revocation).
//!
//! Needs a secure context: `crypto.subtle` does not exist without one.
//! Works from the service worker too, which signs in for the push wake.

use js_sys::{Array, Function, Object, Promise, Reflect, Uint8Array};
use protocol::{DeviceId, PublicKey};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    CryptoKey, CryptoKeyPair, EcKeyGenParams, EcdsaParams, IdbDatabase, IdbObjectStore,
    IdbOpenDbRequest, IdbRequest, IdbTransactionMode, SubtleCrypto,
};

use crate::api::auth::{DeviceSigner, hex};
use crate::platform::{Global, js_error_message};

/// The key's database, object store and record key.
const DB_NAME: &str = "passporter-device";
const DB_VERSION: u32 = 1;
const STORE: &str = "device";
const RECORD: &str = "key";

/// Why the device key could not be made, stored or used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("device key: {0}")]
pub struct DeviceError(String);

impl DeviceError {
    fn js(error: &JsValue) -> Self {
        Self(js_error_message(error))
    }
}

/// A key pair generated for pairing, not yet bound to a device id.
#[derive(Debug, Clone)]
pub struct NewKey {
    private: CryptoKey,
    /// The public key, to register.
    pub public: PublicKey,
}

impl NewKey {
    /// Binds the key to the id the bridge assigned.
    #[must_use]
    pub fn paired(self, device_id: DeviceId) -> DeviceKey {
        DeviceKey {
            device_id,
            private: self.private,
        }
    }
}

/// A paired device's key.
#[derive(Debug, Clone)]
pub struct DeviceKey {
    device_id: DeviceId,
    private: CryptoKey,
}

impl DeviceKey {
    /// A key handed across from the pair WASM, which makes and registers
    /// it ([`crate::pair::screen`]); the page stores it.
    #[must_use]
    pub const fn from_parts(device_id: DeviceId, private: CryptoKey) -> Self {
        Self { device_id, private }
    }

    /// The private key (non-extractable: it can be handed across, never
    /// read).
    #[must_use]
    pub const fn private_key(&self) -> &CryptoKey {
        &self.private
    }
}

fn subtle() -> Result<SubtleCrypto, DeviceError> {
    let crypto = Global::current()
        .crypto()
        .map_err(|e| DeviceError::js(&e))?;
    // `subtle` is undefined outside a secure context.
    let subtle = Reflect::get(&crypto, &JsValue::from_str("subtle")).unwrap_or(JsValue::UNDEFINED);
    if subtle.is_undefined() {
        return Err(DeviceError("needs a secure (HTTPS) connection".into()));
    }
    Ok(crypto.subtle())
}

/// Generates a fresh, non-extractable P-256 signing key.
pub async fn generate() -> Result<NewKey, DeviceError> {
    let subtle = subtle()?;
    let params = EcKeyGenParams::new("ECDSA", "P-256");
    let usages = Array::of1(&JsValue::from_str("sign"));
    // `extractable: false` applies to the private key; the public key of a
    // pair is always exportable.
    let pair: CryptoKeyPair = JsFuture::from(
        subtle
            .generate_key_with_object(&params, false, &usages)
            .map_err(|e| DeviceError::js(&e))?,
    )
    .await
    .map_err(|e| DeviceError::js(&e))?
    .unchecked_into();
    let raw = JsFuture::from(
        subtle
            .export_key("raw", &pair.get_public_key())
            .map_err(|e| DeviceError::js(&e))?,
    )
    .await
    .map_err(|e| DeviceError::js(&e))?;
    let public = PublicKey::new(hex(&Uint8Array::new(&raw).to_vec()))
        .map_err(|e| DeviceError(e.to_string()))?;
    Ok(NewKey {
        private: pair.get_private_key(),
        public,
    })
}

impl DeviceSigner for DeviceKey {
    fn device_id(&self) -> &DeviceId {
        &self.device_id
    }

    async fn sign(&self, message: &[u8]) -> Result<Vec<u8>, String> {
        let subtle = subtle().map_err(|e| e.to_string())?;
        let params = EcdsaParams::new_with_str("ECDSA", "SHA-256");
        let signature = JsFuture::from(
            subtle
                .sign_with_object_and_u8_array(&params, &self.private, message)
                .map_err(|e| js_error_message(&e))?,
        )
        .await
        .map_err(|e| js_error_message(&e))?;
        Ok(Uint8Array::new(&signature).to_vec())
    }
}

/// Stores `key` as this device's key, replacing any other.
pub async fn save(key: &DeviceKey) -> Result<(), DeviceError> {
    let record = Object::new();
    set(
        &record,
        "deviceId",
        &JsValue::from_str(key.device_id.as_str()),
    );
    set(&record, "key", &key.private);
    let store = open_store(IdbTransactionMode::Readwrite).await?;
    let request = store
        .put_with_key(&record, &JsValue::from_str(RECORD))
        .map_err(|e| DeviceError::js(&e))?;
    settle(&request).await.map(drop)
}

/// This device's stored key, if it has paired. A record that no longer
/// reads back as one is ignored.
pub async fn load() -> Result<Option<DeviceKey>, DeviceError> {
    let store = open_store(IdbTransactionMode::Readonly).await?;
    let request = store
        .get(&JsValue::from_str(RECORD))
        .map_err(|e| DeviceError::js(&e))?;
    let record = settle(&request).await?;
    if record.is_undefined() {
        return Ok(None);
    }
    let device_id = Reflect::get(&record, &JsValue::from_str("deviceId"))
        .ok()
        .and_then(|v| v.as_string())
        .and_then(|id| DeviceId::new(id).ok());
    let private = Reflect::get(&record, &JsValue::from_str("key"))
        .ok()
        .and_then(|v| v.dyn_into::<CryptoKey>().ok());
    Ok(device_id
        .zip(private)
        .map(|(device_id, private)| DeviceKey { device_id, private }))
}

/// Deletes this device's key.
pub async fn forget() -> Result<(), DeviceError> {
    let store = open_store(IdbTransactionMode::Readwrite).await?;
    let request = store
        .delete(&JsValue::from_str(RECORD))
        .map_err(|e| DeviceError::js(&e))?;
    settle(&request).await.map(drop)
}

/// Opens the database (creating the store on first use) and starts a
/// transaction on the store. The connection closes itself once the
/// transaction is done.
async fn open_store(mode: IdbTransactionMode) -> Result<IdbObjectStore, DeviceError> {
    let factory = Global::current()
        .indexed_db()
        .map_err(|e| DeviceError::js(&e))?
        .ok_or_else(|| DeviceError("IndexedDB is not supported".into()))?;
    let open: IdbOpenDbRequest = factory
        .open_with_u32(DB_NAME, DB_VERSION)
        .map_err(|e| DeviceError::js(&e))?;
    let upgrading = open.clone();
    let on_upgrade = wasm_bindgen::closure::Closure::<dyn FnMut()>::new(move || {
        if let Ok(db) = upgrading.result().and_then(JsCast::dyn_into::<IdbDatabase>)
            && !db.object_store_names().contains(STORE)
        {
            let _ = db.create_object_store(STORE);
        }
    });
    open.set_onupgradeneeded(Some(on_upgrade.as_ref().unchecked_ref()));
    let db = settle(&open).await;
    open.set_onupgradeneeded(None);
    let db: IdbDatabase = db?.dyn_into().map_err(|e| DeviceError::js(&e))?;
    let tx = db
        .transaction_with_str_and_mode(STORE, mode)
        .map_err(|e| DeviceError::js(&e))?;
    // Close as soon as the transaction finishes, so a later schema upgrade
    // is never blocked by this connection.
    db.close();
    tx.object_store(STORE).map_err(|e| DeviceError::js(&e))
}

/// Resolves with `request`'s result once it succeeds, or its error.
async fn settle(request: &IdbRequest) -> Result<JsValue, DeviceError> {
    let promise = Promise::new(&mut |resolve: Function, reject: Function| {
        request.set_onsuccess(Some(&resolve));
        request.set_onerror(Some(&reject));
    });
    let outcome = JsFuture::from(promise).await;
    request.set_onsuccess(None);
    request.set_onerror(None);
    match outcome {
        Ok(_) => request.result().map_err(|e| DeviceError::js(&e)),
        Err(_) => Err(request.error().ok().flatten().map_or_else(
            || DeviceError("storage request failed".into()),
            |e| DeviceError(e.message()),
        )),
    }
}

fn set(target: &Object, key: &str, value: &JsValue) {
    // Setting a plain property on a fresh object cannot fail.
    let _ = Reflect::set(target, &JsValue::from_str(key), value);
}
