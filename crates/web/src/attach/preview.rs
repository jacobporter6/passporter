//! The extras module's preview of the composer's photo: its thumbnail,
//! the upload's progress and a remove button, drawn into the slot the
//! page hands over ([`super::view::Attach`]). One photo at a time.

use core::cell::RefCell;

use js_sys::Function;
use leptos::prelude::*;
use protocol::AttachmentId;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{File, HtmlCanvasElement, HtmlElement};

use super::state::Phase;
use super::upload;
use crate::extras::mount::Mounted;
use crate::ui::icons::{self, Icon};

/// The preview on screen.
struct Preview {
    id: AttachmentId,
    phase: RwSignal<Phase>,
    _view: Mounted,
}

thread_local! {
    static PREVIEW: RefCell<Option<Preview>> = const { RefCell::new(None) };
}

/// Draws photo `id`'s preview in `slot` (replacing any other), then
/// shrinks and uploads `file`. `removed` is called if the user removes it.
///
/// # Errors
///
/// What to tell the user if the photo could not be sent.
pub async fn attach(
    id: AttachmentId,
    file: File,
    slot: HtmlElement,
    removed: Function,
) -> Result<(), String> {
    clear();
    let phase = RwSignal::new(Phase::Uploading);
    let remove = move |_| {
        let _ = removed.call0(&JsValue::NULL);
        clear();
    };
    let view = Mounted::new(
        slot.clone(),
        move || {
            view! {
                <div class=move || phase.with(Phase::class)>
                    <div class="attach-thumb">
                        <canvas aria-hidden="true"></canvas>
                        <span class="spinner" aria-hidden="true"></span>
                    </div>
                    <p class="attach-status" role="status">{move || phase.with(|p| p.status().to_owned())}</p>
                    <button type="button" class="icon-button" aria-label="Remove photo" on:click=remove>
                        <Icon paths=icons::CLOSE />
                    </button>
                </div>
            }
            .into_any()
        },
        |_| false,
    );
    let thumb: Option<HtmlCanvasElement> = slot
        .query_selector("canvas")
        .ok()
        .flatten()
        .and_then(|canvas| canvas.dyn_into().ok());
    PREVIEW.set(Some(Preview {
        id: id.clone(),
        phase,
        _view: view,
    }));
    let outcome = upload::attach(&id, &file, thumb.as_ref()).await;
    PREVIEW.with_borrow(|preview| {
        if let Some(preview) = preview.as_ref().filter(|p| p.id == id) {
            preview.phase.set(
                outcome
                    .clone()
                    .map_or_else(Phase::Failed, |()| Phase::Ready),
            );
        }
    });
    outcome
}

/// Removes the preview, if one is on screen.
pub fn clear() {
    // Dropped outside the borrow: unmounting may run arbitrary code.
    let preview = PREVIEW.take();
    drop(preview);
}
