//! QR scanning with the rear camera and the Shape Detection API's
//! `BarcodeDetector` (Chrome on Android). `web-sys` has no binding for it, so
//! the few calls go through `js_sys::Reflect`, contained in this module.

use js_sys::{Array, Function, Object, Promise, Reflect};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{HtmlVideoElement, MediaStream, MediaStreamConstraints};

use crate::platform::js_error_message;

/// Why scanning is not possible.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScanError {
    /// The browser has no camera API or no `BarcodeDetector`.
    #[error("this browser can't scan QR codes; enter the code instead")]
    Unsupported,
    /// The user (or policy) denied the camera, or it is in use.
    #[error("camera unavailable: {0}")]
    Camera(String),
}

/// Whether both the camera and QR detection are available.
#[must_use]
pub fn supported() -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };
    let has_detector =
        Reflect::has(&window, &JsValue::from_str("BarcodeDetector")).unwrap_or(false);
    has_detector && window.navigator().media_devices().is_ok()
}

/// Opens the rear camera and shows it in `video`.
pub async fn start_camera(video: &HtmlVideoElement) -> Result<MediaStream, ScanError> {
    let window = web_sys::window().ok_or(ScanError::Unsupported)?;
    let devices = window
        .navigator()
        .media_devices()
        .map_err(|_| ScanError::Unsupported)?;

    let video_constraints = Object::new();
    set(
        &video_constraints,
        "facingMode",
        &JsValue::from_str("environment"),
    );
    let constraints = MediaStreamConstraints::new();
    constraints.set_video(&video_constraints);
    constraints.set_audio(&JsValue::FALSE);

    let promise = devices
        .get_user_media_with_constraints(&constraints)
        .map_err(|e| ScanError::Camera(js_error_message(&e)))?;
    let stream: MediaStream = JsFuture::from(promise)
        .await
        .map_err(|e| ScanError::Camera(js_error_message(&e)))?
        .unchecked_into();

    video.set_src_object(Some(&stream));
    if let Ok(playing) = video.play() {
        // Autoplay of a muted, inline video is allowed; ignore a rejection
        // (the frame loop simply finds nothing until it plays).
        let _ = JsFuture::from(playing).await;
    }
    Ok(stream)
}

/// A `BarcodeDetector` restricted to QR codes.
#[derive(Debug, Clone)]
pub struct QrDetector {
    detector: JsValue,
    detect: Function,
}

impl QrDetector {
    /// Constructs `new BarcodeDetector({ formats: ["qr_code"] })`.
    pub fn new() -> Result<Self, ScanError> {
        let window = web_sys::window().ok_or(ScanError::Unsupported)?;
        let class: Function = Reflect::get(&window, &JsValue::from_str("BarcodeDetector"))
            .ok()
            .and_then(|c| c.dyn_into().ok())
            .ok_or(ScanError::Unsupported)?;
        let options = Object::new();
        set(
            &options,
            "formats",
            &Array::of1(&JsValue::from_str("qr_code")),
        );
        let detector = Reflect::construct(&class, &Array::of1(&options))
            .map_err(|_| ScanError::Unsupported)?;
        let detect = Reflect::get(&detector, &JsValue::from_str("detect"))
            .ok()
            .and_then(|f| f.dyn_into().ok())
            .ok_or(ScanError::Unsupported)?;
        Ok(Self { detector, detect })
    }

    /// The raw text of the first QR code in the current frame, if any.
    /// Detection errors (no frame yet) count as "nothing found".
    pub async fn detect(&self, video: &HtmlVideoElement) -> Option<String> {
        let promise: Promise = self
            .detect
            .call1(&self.detector, video)
            .ok()?
            .dyn_into()
            .ok()?;
        let found: Array = JsFuture::from(promise).await.ok()?.dyn_into().ok()?;
        Reflect::get(&found.get(0), &JsValue::from_str("rawValue"))
            .ok()?
            .as_string()
    }
}

/// Resolves after `millis` milliseconds.
pub async fn sleep(millis: i32) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let promise = Promise::new(&mut |resolve: Function, _reject: Function| {
        let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, millis);
    });
    let _ = JsFuture::from(promise).await;
}

fn set(target: &Object, key: &str, value: &JsValue) {
    // Setting a plain property on a fresh object cannot fail.
    let _ = Reflect::set(target, &JsValue::from_str(key), value);
}
