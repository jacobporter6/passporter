//! The connect screen (design §8).
//!
//! In a secure context: scan the QR from `passporter pair`, or type its
//! code; the app then makes a device key, registers it and signs in. Device
//! keys and the camera need a secure context, so over the MVP's plain-HTTP
//! tunnel the screen offers the pasted bridge token instead ([`token`]).
//!
//! The page here is only the header and a mount point: the body
//! ([`screen`]) runs in the pair WASM, loaded when the screen opens
//! ([`module`]). [`code`], [`scanner`] and [`device`] are shared with the
//! rest of the app (the device key signs every sign-in).

pub mod code;
pub mod device;
pub mod module;
pub mod scanner;
pub mod screen;
pub mod token;

use leptos::html::Div;
use leptos::prelude::*;

pub use code::PairingCode;

use crate::nav::use_nav;
use crate::routes::AppRoute;
use crate::ui::{PageHeader, use_app};

/// The connect screen.
#[component]
pub fn PairPage() -> impl IntoView {
    let app = use_app();
    // Back to wherever the user came from (triage prompt or settings).
    let back = Signal::derive(move || {
        app.previous_route
            .get()
            .filter(|r| *r != AppRoute::Pair)
            .unwrap_or(AppRoute::Settings)
    });
    let root = NodeRef::<Div>::new();
    let failed = RwSignal::new(false);
    module::show(app, use_nav(), root, failed);
    view! {
        <section class="page page-pair">
            <PageHeader title="Connect" subtitle="Link this phone to your bridge" back=back />
            <div class="pair-body" node_ref=root></div>
            <Show when=move || failed.get()>
                <p class="field-error" role="alert">
                    "Pairing didn't load. Check the connection and open this screen again."
                </p>
            </Show>
        </section>
    }
}
