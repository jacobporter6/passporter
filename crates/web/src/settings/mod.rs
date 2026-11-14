//! Settings: the bridge connection (and how this device signs in),
//! notifications, device preferences, offline storage and app information.
//!
//! - [`model`]: what the page feeds the groups ([`model::Snapshot`]).
//! - [`prefs`]: the device preferences' switches.
//! - [`app_notifications`]: the Android app's notification status and setup.
//! - [`view`](mod@view): the groups, drawn by the extras module ([`crate::extras`]).
//! - this module: the page (its header and the feed), and what the
//!   groups' buttons do that only the app can: forget the credential,
//!   clear the cache (push goes through [`crate::push::module`]).

pub mod app_notifications;
pub mod model;
pub mod prefs;
pub mod view;

use leptos::prelude::*;

use crate::extras::Surface;
use crate::extras::page::Extra;
use crate::live::{Live, use_live};
use crate::session;
use crate::ui::{AppState, PageHeader, use_app};
use crate::version::use_versions;
use model::Snapshot;
use prefs::Pref;

/// The settings screen.
#[component]
pub fn SettingsPage() -> impl IntoView {
    let app = use_app();
    let live = use_live();
    let versions = use_versions();
    let credential = session::credential();
    let feed = Memo::new(move |_| {
        let snapshot = Snapshot {
            credential: credential.get(),
            paired: app.paired.get(),
            conn: live.conn.get(),
            storage: app.persistent_storage.get(),
            token_loaded: app.token_loaded.get(),
            prefs: Pref::ALL
                .into_iter()
                .filter(|pref| pref.offered())
                .map(|pref| (pref, pref.signal(app).get()))
                .collect(),
            bridge: versions.bridge.get(),
            freshness: versions.freshness(),
        };
        serde_json::to_string(&snapshot).unwrap_or_default()
    });
    view! {
        <section class="page">
            <PageHeader title="Settings" />
            <Extra surface=Surface::Settings feed=feed />
        </section>
    }
}

/// Forgets this device's credential, and the agents it showed.
pub(crate) async fn forget(app: AppState, live: Live) -> Result<(), String> {
    session::forget(app).await?;
    live.agents.set(Vec::new());
    live.fresh_at.set(None);
    Ok(())
}

/// Clears the cached snapshots (unsent replies are kept), and says how
/// that went.
pub(crate) async fn clear_snapshots(app: AppState) {
    let result = match app.store().await {
        Ok(store) => store.clear_snapshots().await,
        Err(e) => Err(e),
    };
    match result {
        Ok(()) => app.toast("Cached snapshots cleared"),
        Err(e) => app.toast(e.to_string()),
    }
}
