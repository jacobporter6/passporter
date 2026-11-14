//! Notifications in the Android app: its own status light and setup
//! instead of the browser's push switch (the `WebView` has no Push API).
//! The app brings news through the bridge's built-in transport or the
//! ntfy app, and says which ([`native::notification_status`]); *Set up*
//! opens its native dialog ([`native::notification_setup`]).

use core::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::extras::mount;
use crate::pair::native::{self, NotificationStatus};

/// How often the status is read while Settings shows it (a local IPC call).
const POLL: Duration = Duration::from_secs(3);

/// The status row (its light, and the app's sentence under the group) and
/// the *Set up* button.
#[component]
pub fn AppNotifications() -> impl IntoView {
    let status = RwSignal::new(None::<NotificationStatus>);
    let read = move || {
        spawn_local(async move {
            let now = native::notification_status().await;
            if status.try_get_untracked().flatten() != now {
                let _ = status.try_set(now);
            }
        });
    };
    read();
    let poll = set_interval_with_handle(read, POLL);
    on_cleanup(move || {
        if let Ok(handle) = poll {
            handle.clear();
        }
    });
    let setup = move |_| {
        spawn_local(async move {
            if !native::notification_setup().await {
                mount::toast("Couldn't open the app's notification setup. Update the app.");
            }
        });
    };
    let connected = move || status.with(|s| s.as_ref().map(|s| s.connected));
    view! {
        <div class="group">
            <div class="row" role="status">
                <span class="row-label">"Blocked and done agents"</span>
                <span
                    class="row-value status-light"
                    class:row-value-ok=move || connected() == Some(true)
                    class:row-value-warn=move || connected() == Some(false)
                >
                    {move || match connected() {
                        Some(true) => "On",
                        Some(false) => "Needs attention",
                        None => "Checking…",
                    }}
                </span>
            </div>
            <button type="button" class="row row-button" on:click=setup>
                <span class="row-label">"Set up"</span>
            </button>
        </div>
        <p class="group-note">
            {move || status.with(|s| s.as_ref().map_or_else(
                || "Asking the app…".to_owned(),
                |s| s.message.clone(),
            ))}
        </p>
    }
}
