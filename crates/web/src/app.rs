//! The root component: app state, navigation and shell layout.

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::agent::AgentPage;
use crate::live::{self, Live};
use crate::nav::{self, Nav};
use crate::outbox::OutboxService;
use crate::pair::PairPage;
use crate::platform;
use crate::push;
use crate::routes::AppRoute;
use crate::runbooks::{self, RunbooksSheet};
use crate::session;
use crate::settings::{self, SettingsPage};
use crate::triage::TriagePage;
use crate::ui::{
    AppState, DictationTarget, Dock, NavigationTracker, NotFound, OfflineBanner, ToastHost, format,
};
use crate::version::{UpdateBanner, Versions};

/// The whole app.
#[component]
pub fn App() -> impl IntoView {
    let app = AppState::new();
    provide_context(app);
    provide_context(DictationTarget::new());
    let nav = nav::provide();
    push::open_from_notifications(nav);
    let live_state = Live::new(app);
    provide_context(live_state);
    provide_context(OutboxService::start(app));
    runbooks::provide();
    spawn_local(session::load(app));
    spawn_local(settings::prefs::load(app));
    live::start(live_state);
    app.now.set(format::now());
    // Ticks "time in state" on every card; app-lifetime, never cleared.
    let _ = set_interval_with_handle(
        move || app.now.set(format::now()),
        core::time::Duration::from_secs(1),
    );

    // App-lifetime listeners: the handles are intentionally never removed.
    let _ = window_event_listener(leptos::ev::online, move |_| app.online.set(true));
    let _ = window_event_listener(leptos::ev::offline, move |_| app.online.set(false));
    // Report whether storage is already persistent; asking for it waits
    // until the device has something to keep (see pairing).
    spawn_local(async move {
        let persistent = platform::storage_is_persistent().await;
        app.persistent_storage.set(Some(persistent));
    });
    spawn_local(platform::register_service_worker(move || {
        let _ = app.update_ready.try_set(true);
    }));
    Versions::start(app);

    view! {
        <NavigationTracker />
        <div class="app" class:app-composing=move || app.composing.get()>
            <OfflineBanner />
            <main class="main">
                <Screens nav=nav />
            </main>
            <UpdateBanner />
            <RunbooksSheet />
            <ToastHost />
            <Dock />
        </div>
    }
}

/// Which page a route shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Triage,
    Agent,
    Pair,
    Settings,
    NotFound,
}

/// The page for the current route. Keyed by page, not route, so moving
/// between agents keeps the agent page and only changes the agent it shows.
#[component]
fn Screens(nav: Nav) -> impl IntoView {
    let page = Memo::new(move |_| {
        nav.route().with(|route| match route {
            Some(AppRoute::Triage) => Page::Triage,
            Some(AppRoute::Agent(_)) => Page::Agent,
            Some(AppRoute::Pair) => Page::Pair,
            Some(AppRoute::Settings) => Page::Settings,
            None => Page::NotFound,
        })
    });
    move || match page.get() {
        Page::Triage => view! { <TriagePage /> }.into_any(),
        Page::Agent => view! { <AgentPage /> }.into_any(),
        Page::Pair => view! { <PairPage /> }.into_any(),
        Page::Settings => view! { <SettingsPage /> }.into_any(),
        Page::NotFound => view! { <NotFound /> }.into_any(),
    }
}
