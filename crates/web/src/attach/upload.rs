//! The extras module's side of a photo (`extras_attach`): decode the
//! picked file, draw its thumbnail into the composer, shrink it to
//! [`MAX_EDGE`] as a JPEG and upload it to `POST /v1/attachments`.
//!
//! The browser decodes the file (`createImageBitmap` honours its EXIF
//! orientation) and the canvas re-encodes the pixels, so the upload
//! already carries no camera metadata; the bridge strips it again anyway,
//! since it never trusts what a client sends.

use js_sys::{Promise, Reflect};
use protocol::{AttachmentId, IDEMPOTENCY_KEY_HEADER};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    Blob, CanvasRenderingContext2d, File, HtmlCanvasElement, ImageBitmap, ImageEncodeOptions,
    OffscreenCanvas, OffscreenCanvasRenderingContext2d,
};

use super::state::{JPEG_QUALITY, MAX_EDGE, THUMB_EDGE, failure_text, fit};
use crate::api::{RawRequest, TokenSource, fetch_raw};
use crate::extras::mount::HostToken;

/// How long one upload may take, body included.
const UPLOAD_TIMEOUT_MS: u32 = 120_000;
/// Waits before retrying an upload the network dropped.
const RETRY_DELAYS_MS: [i32; 2] = [1_000, 4_000];

/// Attaches `file` as photo `id`, drawing its thumbnail into `thumb`.
/// Runs in the extras module, for [`super::preview::attach`].
///
/// # Errors
///
/// What to tell the user: the file is not an image the browser can
/// decode, or the upload failed.
pub async fn attach(
    id: &AttachmentId,
    file: &File,
    thumb: Option<&HtmlCanvasElement>,
) -> Result<(), String> {
    let unreadable = |error: JsValue| {
        crate::platform::log_error("could not read the photo", &error);
        "That file isn't a photo this browser can read.".to_owned()
    };
    let bitmap = decode(file).await.map_err(unreadable)?;
    // The thumbnail is a nicety: a failure there never stops the upload.
    if let Some(Err(error)) = thumb.map(|thumb| draw_thumb(&bitmap, thumb)) {
        crate::platform::log_error("could not draw the thumbnail", &error);
    }
    let jpeg = shrink(&bitmap).await;
    bitmap.close();
    upload(id, &jpeg.map_err(unreadable)?).await
}

async fn decode(file: &File) -> Result<ImageBitmap, JsValue> {
    let window = web_sys::window().ok_or_else(|| JsValue::from_str("no window"))?;
    JsFuture::from(window.create_image_bitmap_with_blob(file)?)
        .await?
        .dyn_into()
}

/// Draws `bitmap` into `thumb`, centre-cropped to a square of
/// [`THUMB_EDGE`] CSS pixels at the screen's density.
fn draw_thumb(bitmap: &ImageBitmap, thumb: &HtmlCanvasElement) -> Result<(), JsValue> {
    let density = web_sys::window().map_or(1.0, |w| w.device_pixel_ratio().clamp(1.0, 3.0));
    let edge = f64::from(THUMB_EDGE) * density;
    // At most 168 device pixels, so the cast cannot truncate.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let pixels = edge.round() as u32;
    thumb.set_width(pixels);
    thumb.set_height(pixels);
    let context: CanvasRenderingContext2d = thumb
        .get_context("2d")?
        .ok_or_else(|| JsValue::from_str("no 2d context"))?
        .dyn_into()?;
    let (width, height) = (f64::from(bitmap.width()), f64::from(bitmap.height()));
    let side = width.min(height);
    smooth(&context);
    context.draw_image_with_image_bitmap_and_sw_and_sh_and_dx_and_dy_and_dw_and_dh(
        bitmap,
        (width - side) / 2.0,
        (height - side) / 2.0,
        side,
        side,
        0.0,
        0.0,
        edge,
        edge,
    )
}

/// Asks a 2D context for its best scaling (the default, `low`, aliases
/// a big photo shrunk to a fraction). web-sys has no binding for it.
fn smooth(context: &JsValue) {
    let _ = Reflect::set(context, &"imageSmoothingQuality".into(), &"high".into());
}

/// `bitmap` within [`MAX_EDGE`], as a JPEG.
async fn shrink(bitmap: &ImageBitmap) -> Result<Blob, JsValue> {
    let (width, height) = fit(bitmap.width(), bitmap.height(), MAX_EDGE);
    let canvas = OffscreenCanvas::new(width, height)?;
    let context: OffscreenCanvasRenderingContext2d = canvas
        .get_context("2d")?
        .ok_or_else(|| JsValue::from_str("no 2d context"))?
        .dyn_into()?;
    smooth(&context);
    context.draw_image_with_image_bitmap_and_dw_and_dh(
        bitmap,
        0.0,
        0.0,
        f64::from(width),
        f64::from(height),
    )?;
    let options = ImageEncodeOptions::new();
    options.set_type("image/jpeg");
    options.set_quality(JPEG_QUALITY);
    let blob: Promise = canvas.convert_to_blob_with_options(&options)?;
    JsFuture::from(blob).await?.dyn_into()
}

/// Sends `jpeg` as photo `id`, retrying a dropped connection: the id is
/// the idempotency key, so a retry after a lost reply is harmless.
async fn upload(id: &AttachmentId, jpeg: &Blob) -> Result<(), String> {
    let body: JsValue = jpeg.into();
    let headers = [
        ("Content-Type", "image/jpeg"),
        ("Accept", "application/json"),
        (IDEMPOTENCY_KEY_HEADER, id.as_str()),
    ];
    let mut delays = RETRY_DELAYS_MS.into_iter();
    loop {
        let token = HostToken.current();
        let sent = fetch_raw(RawRequest {
            method: "POST",
            path: "/v1/attachments",
            headers: &headers,
            auth: token.as_ref(),
            body: Some(&body),
            timeout_ms: Some(UPLOAD_TIMEOUT_MS),
        })
        .await;
        match sent {
            Ok(response) if matches!(response.status, 200 | 201) => return Ok(()),
            Ok(response) => {
                if response.status == 401
                    && let Some(token) = &token
                {
                    HostToken.unauthorized(token);
                }
                return Err(failure_text(Some(response.status)).to_owned());
            }
            Err(error) => {
                if let Some(delay) = delays.next() {
                    crate::pair::scanner::sleep(delay).await;
                } else {
                    crate::platform::log_error("could not upload the photo", &error);
                    return Err(failure_text(None).to_owned());
                }
            }
        }
    }
}
