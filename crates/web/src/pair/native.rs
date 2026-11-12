//! The Android app's device identity, reached over Tauri's IPC (design §8).
//!
//! In the APK the device key is a non-exportable Android Keystore key owned
//! by native code (the `device` plugin, `apps/android/plugins/device`), so
//! notification replies and the car can sign in with the app closed. The
//! PWA asks that plugin to pair and for app-scope session tokens instead of
//! using `WebCrypto` ([`super::device`]).
//!
//! The plugin is there only when the page runs in the app and the shell
//! granted it (`window.__TAURI__` exists, and the `WebView` passed the shell's
//! frame-isolation check). [`probe`] says whether it answers; everywhere else
//! the `WebCrypto` path is used, unchanged.

use js_sys::{Function, JSON, Object, Promise, Reflect};
use protocol::{DeviceId, PairingCode, SessionGrant};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

use crate::api::ApiError;
use crate::api::auth::DeviceAuthError;
use crate::platform::js_error_message;

/// What the plugin's `status` answers.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeStatus {
    /// Whether the app is paired.
    pub paired: bool,
    /// The bridge's id for this device, once paired.
    #[serde(default)]
    pub device_id: Option<DeviceId>,
}

/// What the plugin's `pair` answers.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Paired {
    device_id: DeviceId,
}

/// A rejected plugin call: `{message, code}` from the plugin, or a bare
/// message from Tauri (an ACL refusal, say).
#[derive(Debug, Deserialize)]
struct Failure {
    message: String,
    #[serde(default)]
    code: Option<String>,
}

/// What asking the native layer for its status came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// No native layer here: Chrome, or an app whose `WebView` was too old
    /// for the shell to grant it (Tauri refuses the call, with no code).
    Absent,
    /// The plugin answered.
    Status(NativeStatus),
    /// The plugin is there but failed to answer (a busy Keystore, say).
    Unknown,
}

impl Probe {
    /// Whether a native layer is here, answering or not.
    #[must_use]
    pub fn present(&self) -> bool {
        !matches!(self, Self::Absent)
    }

    /// Whether it may be paired: `false` only when it said it is not.
    #[must_use]
    pub fn maybe_paired(&self) -> bool {
        match self {
            Self::Status(status) => status.paired,
            Self::Unknown => true,
            Self::Absent => false,
        }
    }
}

/// Asks the plugin for its status.
pub async fn probe() -> Probe {
    if invoke_fn().is_none() {
        return Probe::Absent;
    }
    match invoke::<NativeStatus>("status", &Object::new()).await {
        Ok(status) => Probe::Status(status),
        Err(failure) => {
            leptos::logging::warn!("native device identity: {}", failure.message);
            // The plugin always sends a code; Tauri's own refusals do not.
            if failure.code.is_some() {
                Probe::Unknown
            } else {
                Probe::Absent
            }
        }
    }
}

/// Pairs the app with `code`: the plugin makes a Keystore key, registers it
/// and keeps the id. An earlier native pairing is replaced only on success.
pub async fn pair(code: &PairingCode) -> Result<DeviceId, DeviceAuthError> {
    let args = Object::new();
    // Setting a plain property on a fresh object cannot fail.
    let _ = Reflect::set(&args, &"code".into(), &code.as_str().into());
    call::<Paired>("pair", &args)
        .await
        .map(|paired| paired.device_id)
}

/// A fresh app-scope session. The plugin never hands the webview a car token.
pub async fn session() -> Result<SessionGrant, DeviceAuthError> {
    call("session", &Object::new()).await
}

/// Forgets the native pairing (key and id).
pub async fn forget() -> Result<(), DeviceAuthError> {
    call::<()>("forget", &Object::new()).await
}

/// Whether this page runs in the Android app (Tauri's global is there).
#[must_use]
pub fn in_app() -> bool {
    invoke_fn().is_some()
}

/// `window.__TAURI__.core.invoke`, if this page runs in the app.
fn invoke_fn() -> Option<Function> {
    let tauri = Reflect::get(&js_sys::global(), &"__TAURI__".into()).ok()?;
    let core = Reflect::get(&tauri, &"core".into()).ok()?;
    Reflect::get(&core, &"invoke".into()).ok()?.dyn_into().ok()
}

/// Calls the plugin's `command` with `args` and reads its answer as `T`.
async fn call<T: DeserializeOwned>(command: &str, args: &Object) -> Result<T, DeviceAuthError> {
    invoke(command, args)
        .await
        .map_err(|failure| auth_error(failure.code.as_deref(), failure.message))
}

/// [`call`], with the rejection as it came.
async fn invoke<T: DeserializeOwned>(command: &str, args: &Object) -> Result<T, Failure> {
    let invoke = invoke_fn().ok_or_else(|| Failure {
        message: "the app's native layer is gone".to_owned(),
        code: None,
    })?;
    let promise: Promise = invoke
        .call2(
            &JsValue::NULL,
            &format!("plugin:device|{command}").into(),
            args,
        )
        .and_then(JsCast::dyn_into)
        .map_err(|e| failure(&e))?;
    let value = JsFuture::from(promise).await.map_err(|e| failure(&e))?;
    decode(&value).map_err(|message| Failure {
        message,
        code: Some("badResponse".to_owned()),
    })
}

/// `value` (a plugin answer) as `T`, through its JSON text.
fn decode<T: DeserializeOwned>(value: &JsValue) -> Result<T, String> {
    let text = if value.is_undefined() {
        "null".to_owned()
    } else {
        JSON::stringify(value)
            .map(String::from)
            .map_err(|e| js_error_message(&e))?
    };
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

/// A rejection as it came: the plugin's `{message, code}`, or Tauri's bare
/// message.
fn failure(error: &JsValue) -> Failure {
    decode::<Failure>(error).unwrap_or_else(|_| Failure {
        message: js_error_message(error),
        code: None,
    })
}

/// Maps the plugin's error codes (`DeviceAuthException.code` in Kotlin) to
/// [`DeviceAuthError`], keeping what matters to the session: whether the
/// pairing is gone, and whether to retry.
fn auth_error(code: Option<&str>, message: String) -> DeviceAuthError {
    match code {
        // The plugin has already forgotten the pairing on a 410. Only a 410
        // means that: `notPaired` (say, the saved bridge address could not
        // be read) must not wipe a pairing the bridge still accepts.
        Some("forgotten") => DeviceAuthError::Forgotten,
        Some("refused") => DeviceAuthError::Refused,
        Some("rateLimited") => DeviceAuthError::RateLimited,
        Some("tooManyDevices") => DeviceAuthError::TooManyDevices,
        // A busy Keystore (the plugin already retried once) or a crash in
        // the plugin may pass, so the session tries again, slowly, while
        // the plugin still says it is paired. `notPaired` is not one: the
        // plugin's status then says unpaired too, so the session cannot tell
        // an unreadable bridge address from a lost key.
        Some("keystore" | "internal") => DeviceAuthError::NativeUnavailable(message),
        Some("unreachable" | "serverError") => DeviceAuthError::Api(ApiError::Transport(message)),
        _ => DeviceAuthError::Api(ApiError::Decode(message)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_codes_keep_their_meaning() {
        let cases = [
            (Some("forgotten"), DeviceAuthError::Forgotten),
            (Some("refused"), DeviceAuthError::Refused),
            (Some("rateLimited"), DeviceAuthError::RateLimited),
            (Some("tooManyDevices"), DeviceAuthError::TooManyDevices),
        ];
        for (code, want) in cases {
            assert_eq!(auth_error(code, "m".to_owned()), want, "{code:?}");
        }
        for passing in ["keystore", "internal"] {
            assert_eq!(
                auth_error(Some(passing), "m".to_owned()),
                DeviceAuthError::NativeUnavailable("m".to_owned()),
                "{passing}"
            );
        }
        for retryable in ["unreachable", "serverError", "keystore", "internal"] {
            assert!(auth_error(Some(retryable), "m".to_owned()).is_retryable());
        }
        for final_ in [
            Some("badResponse"),
            Some("storage"),
            Some("notPaired"),
            None,
        ] {
            let error = auth_error(final_, "m".to_owned());
            assert!(!error.is_retryable());
            assert_ne!(
                error,
                DeviceAuthError::Forgotten,
                "{final_:?} keeps the pairing"
            );
        }
    }

    #[test]
    fn a_failed_probe_is_not_an_absent_layer() {
        let paired = NativeStatus {
            paired: true,
            device_id: None,
        };
        assert!(Probe::Status(paired.clone()).maybe_paired());
        assert!(
            !Probe::Status(NativeStatus {
                paired: false,
                ..paired
            })
            .maybe_paired()
        );
        assert!(Probe::Unknown.present() && Probe::Unknown.maybe_paired());
        assert!(!Probe::Absent.present() && !Probe::Absent.maybe_paired());
    }

    #[test]
    fn status_is_read_strictly() {
        let status: NativeStatus =
            serde_json::from_str(r#"{"paired":true,"deviceId":"dev-0123456789abcdef"}"#).unwrap();
        assert!(status.paired);
        assert_eq!(status.device_id.unwrap().as_str(), "dev-0123456789abcdef");
        let unpaired: NativeStatus = serde_json::from_str(r#"{"paired":false}"#).unwrap();
        assert_eq!(unpaired.device_id, None);
        assert!(serde_json::from_str::<NativeStatus>(r#"{"paired":true,"extra":1}"#).is_err());
        assert!(serde_json::from_str::<NativeStatus>(r#"{"paired":true,"deviceId":"x"}"#).is_err());
    }

    #[test]
    fn a_native_session_is_a_session_grant() {
        let grant: SessionGrant =
            serde_json::from_str(r#"{"token":"abc","expiresInSecs":899,"deviceName":"Phone"}"#)
                .unwrap();
        assert_eq!(grant.expires_in_secs, 899);
        assert_eq!(grant.scope, protocol::Scope::App);
    }
}
