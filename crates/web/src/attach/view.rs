//! The page's side of a photo: the attach button, the slot its preview
//! is drawn into, and the state Send depends on. The preview itself (the
//! thumbnail, the upload's progress, the remove button) and the work
//! behind it run in the extras module ([`super::preview`],
//! [`super::upload`]), keeping them out of the app's WASM.

use leptos::html::Div;
use leptos::prelude::*;
use leptos::task::spawn_local;
use protocol::AttachmentId;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;
use web_sys::{File, HtmlInputElement};

use super::state::Photo;
use crate::extras::page::Exports;
use crate::idgen::random_key;
use crate::ui::AppState;
use crate::ui::icons::{Icon, Paths};

/// A camera.
const CAMERA: Paths = &[
    "M4 8.5h3l1.8-2.5h6.4L17 8.5h3V19H4z",
    "M12 16.5a3.2 3.2 0 1 0 0-6.4 3.2 3.2 0 0 0 0 6.4z",
];

/// One reply bar's photo: its state, the slot its preview is drawn in,
/// and the module that draws and sends it.
#[derive(Debug, Clone, Copy)]
pub struct Attach {
    /// The attached photo, if any.
    pub photo: RwSignal<Option<Photo>>,
    /// Where the extras module draws the preview.
    pub slot: NodeRef<Div>,
    /// What the preview's remove button calls.
    removed: StoredValue<JsValue, LocalStorage>,
    exports: Exports,
    app: AppState,
}

impl Attach {
    /// No photo yet. Made in a component, for the app's handles; the
    /// preview is dropped with it.
    #[must_use]
    pub fn new(app: AppState) -> Self {
        let photo = RwSignal::new(None);
        let removed = Closure::<dyn Fn()>::new(move || {
            let _ = photo.try_set(None);
        })
        .into_js_value();
        let attach = Self {
            photo,
            slot: NodeRef::new(),
            removed: StoredValue::new_local(removed),
            exports: Exports::current(),
            app,
        };
        on_cleanup(move || attach.clear());
        attach
    }

    /// The user picked `file`: it replaces any photo, and its preview and
    /// upload start in the extras module.
    fn pick(self, file: &File) {
        let Some(id) = random_key().and_then(|key| AttachmentId::new(key.as_str()).ok()) else {
            self.app
                .toast("This browser can't generate secure random ids.");
            return;
        };
        let Some(slot) = self.slot.get_untracked() else {
            return;
        };
        self.photo.set(Some(Photo::new(id.clone())));
        let args = js_sys::Array::of4(
            &JsValue::from_str(id.as_str()),
            file,
            &slot,
            &self.removed.get_value(),
        );
        spawn_local(async move {
            let outcome = match self.exports.call("extras_attach", &args).await {
                Ok(answer) => answer.as_string().map_or(Ok(()), Err),
                Err(error) => {
                    crate::platform::log_error("could not attach the photo", &error);
                    Err("Couldn't start the upload. Check the connection and try again.".to_owned())
                }
            };
            self.photo.try_update(|photo| {
                if let Some(photo) = photo {
                    photo.settle(&id, outcome);
                }
            });
        });
    }

    /// Photo `id` is safe in the outbox: off the composer, unless another
    /// one replaced it meanwhile.
    pub fn sent(self, id: &AttachmentId) {
        let same = self
            .photo
            .try_with_untracked(|photo| photo.as_ref().is_some_and(|p| &p.id == id));
        if same == Some(true) {
            let _ = self.photo.try_set(None);
            self.clear();
        }
    }

    /// Removes the preview, if the extras module drew one.
    fn clear(self) {
        self.exports.call_loaded("extras_attach_clear");
    }
}

/// The composer's attach button: the phone offers its camera or gallery.
#[component]
pub fn AttachButton(attach: Attach) -> impl IntoView {
    let on_change = move |ev: leptos::ev::Event| {
        let input: HtmlInputElement = event_target(&ev);
        if let Some(file) = input.files().and_then(|files| files.get(0)) {
            attach.pick(&file);
        }
        // The same file can be picked again after a remove.
        input.set_value("");
    };
    view! {
        <label class="icon-button attach-button" title="Attach a photo">
            <input
                type="file"
                accept="image/*"
                class="visually-hidden"
                aria-label="Attach a photo"
                on:change=on_change
            />
            <Icon paths=CAMERA />
        </label>
    }
}
