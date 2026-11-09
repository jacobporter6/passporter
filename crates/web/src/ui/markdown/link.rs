//! The "Open this link?" confirm, drawn by the extras module ([`crate::extras::mount`]):
//! the link's host as the browser parses it, the full URL, and an anchor
//! that opens it in a new tab without an opener or referrer. Its feed is
//! the URL.

use leptos::html;
use leptos::prelude::*;
use web_sys::HtmlElement;

use super::url_host;
use crate::extras::Surface;
use crate::extras::mount::{self, Mounted};

/// Draws the confirm for `url` in `root`; its feed is the URL to confirm.
pub fn mount(root: HtmlElement, url: &str) -> Mounted {
    let url = ArcRwSignal::new(url.to_owned());
    let fed = url.clone();
    Mounted::new(
        root,
        move || view! { <Confirm url=RwSignal::from(url).read_only() /> }.into_any(),
        move |next| {
            fed.set(next.to_owned());
            true
        },
    )
}

/// The host that will open `url`, as the browser parses it (punycode for
/// any IDN), when it agrees with ours; `None` (open nothing) otherwise.
fn opening_host(url: &str) -> Option<String> {
    let ours = url_host(url)?.to_ascii_lowercase();
    let host = web_sys::Url::new(url).ok()?.hostname();
    let ours = ours.trim_start_matches('[').trim_end_matches(']');
    let theirs = host.trim_start_matches('[').trim_end_matches(']');
    (ours == theirs || (ours.contains(':') && theirs.contains(':'))).then_some(host)
}

#[component]
fn Confirm(url: ReadSignal<String>) -> impl IntoView {
    let cancel_ref = NodeRef::<html::Button>::new();
    Effect::new(move |_| {
        if let Some(button) = cancel_ref.get() {
            let _ = button.focus();
        }
    });
    let close = || mount::close(Surface::Link);
    move || {
        let url = url.get();
        opening_host(&url).map(|host| {
            view! {
                <div class="link-confirm-backdrop" on:click=move |_| close()>
                    <div
                        class="confirm link-confirm"
                        role="alertdialog"
                        aria-modal="true"
                        aria-labelledby="link-confirm-text"
                        on:click=|ev| ev.stop_propagation()
                        on:keydown=move |ev: leptos::ev::KeyboardEvent| {
                            if ev.key() == "Escape" {
                                close();
                            }
                        }
                    >
                        <p class="link-confirm-host-label">"Opens a page on"</p>
                        <p class="link-confirm-host mono">{host}</p>
                        <p id="link-confirm-text" class="confirm-text">
                            "Open "<span class="link-confirm-url mono">{url.clone()}</span>"?"
                        </p>
                        <p class="field-help">"It opens in a new tab. Check the address: agent text is not trusted."</p>
                        <div class="confirm-actions">
                            <button type="button" class="button button-secondary" node_ref=cancel_ref on:click=move |_| close()>"Cancel"</button>
                            <a
                                class="button button-primary"
                                href=url
                                target="_blank"
                                rel="noopener noreferrer"
                                referrerpolicy="no-referrer"
                                on:click=move |_| close()
                            >"Open"</a>
                        </div>
                    </div>
                </div>
            }
        })
    }
}
