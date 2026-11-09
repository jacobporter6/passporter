//! Shared UI: app-wide state, the shell chrome (dock and its mic, offline
//! banner, toasts) and small building blocks used by every screen.

pub mod follow;
pub mod format;
pub mod icons;
pub mod markdown;
pub mod swipe;

use core::time::Duration;

use leptos::prelude::*;

use protocol::{AgentId, Timestamp};

use crate::api::BearerToken;
use crate::nav::use_nav;
use crate::platform;
use crate::routes::AppRoute;
use crate::store::{Store, StoreError};
use crate::voice::view::{Dictation, MicButton};
use icons::Icon;

/// How long a toast stays up.
const TOAST_TTL: Duration = Duration::from_millis(2600);

/// App-wide reactive state, provided as context by [`crate::app::App`].
#[derive(Debug, Clone, Copy)]
pub struct AppState {
    /// Browser connectivity, kept current by `online`/`offline` events.
    pub online: RwSignal<bool>,
    /// Whether the browser granted persistent storage (`None` = not asked yet).
    pub persistent_storage: RwSignal<Option<bool>>,
    /// Whether this device holds a bridge token.
    pub paired: Signal<bool>,
    /// The bridge token (MVP auth; see [`crate::session`]).
    pub token: RwSignal<Option<BearerToken>>,
    /// The stored token has been read (so "not connected" is real).
    pub token_loaded: RwSignal<bool>,
    /// The bridge rejected the last token.
    pub token_rejected: RwSignal<bool>,
    /// The device clock, ticking once a second, for "time in state".
    pub now: RwSignal<Timestamp>,
    /// The composer has focus: the dock hides so the composer stays above
    /// the on-screen keyboard.
    pub composing: RwSignal<bool>,
    /// The screen shown before the current one, for back links.
    pub previous_route: RwSignal<Option<AppRoute>>,
    /// A new app version has installed and waits for a reload.
    pub update_ready: RwSignal<bool>,
    /// Show dictation for review before it is sent (a per-device setting,
    /// see [`crate::settings::prefs::Pref::ReviewDictation`]); off sends it on stop.
    pub review_dictation: RwSignal<bool>,
    /// The agent screen shows whole transcripts, not turn summaries (a
    /// per-device setting, see [`crate::settings::prefs::Pref::FullTranscripts`]).
    pub full_transcripts: RwSignal<bool>,
    toast: RwSignal<Option<Toast>>,
    toast_seq: StoredValue<u32>,
    store: StoredValue<Option<Store>, LocalStorage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Toast {
    id: u32,
    text: String,
}

impl AppState {
    /// Fresh state, with connectivity read from the browser.
    #[must_use]
    pub fn new() -> Self {
        let token = RwSignal::new(None::<BearerToken>);
        Self {
            online: RwSignal::new(platform::is_online()),
            persistent_storage: RwSignal::new(None),
            paired: Signal::derive(move || token.with(Option::is_some)),
            token,
            token_loaded: RwSignal::new(false),
            token_rejected: RwSignal::new(false),
            now: RwSignal::new(Timestamp::default()),
            composing: RwSignal::new(false),
            previous_route: RwSignal::new(None),
            update_ready: RwSignal::new(false),
            review_dictation: RwSignal::new(false),
            full_transcripts: RwSignal::new(false),
            toast: RwSignal::new(None),
            toast_seq: StoredValue::new(0),
            store: StoredValue::new_local(None),
        }
    }

    /// The app's shared database connection, opened on first use and
    /// reopened if another tab's schema upgrade closed it.
    pub async fn store(&self) -> Result<Store, StoreError> {
        if let Some(store) = self.store.get_value().filter(Store::is_open) {
            return Ok(store);
        }
        let store = Store::open().await?;
        self.store.set_value(Some(store.clone()));
        Ok(store)
    }

    /// Shows a short, polite status message above the dock.
    pub fn toast(&self, text: impl Into<String>) {
        let id = self.toast_seq.get_value().wrapping_add(1);
        self.toast_seq.set_value(id);
        self.toast.set(Some(Toast {
            id,
            text: text.into(),
        }));
        let toast = self.toast;
        set_timeout(
            move || {
                // Only dismiss our own toast, not a newer one.
                toast.update(|t| {
                    if t.as_ref().is_some_and(|t| t.id == id) {
                        *t = None;
                    }
                });
            },
            TOAST_TTL,
        );
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

/// The [`AppState`] from context.
///
/// # Panics
///
/// Outside [`crate::app::App`], which always provides it.
#[must_use]
pub fn use_app() -> AppState {
    expect_context()
}

/// The agent the dock's mic may dictate to: the one whose composer is up
/// (see [`MicFab`]). Provided as context by [`crate::app::App`].
#[derive(Debug, Clone, Copy)]
pub struct DictationTarget(RwSignal<Option<AgentId>>);

impl DictationTarget {
    /// No target yet.
    #[must_use]
    pub fn new() -> Self {
        Self(RwSignal::new(None))
    }

    /// The dock's mic may dictate to `agent` until the current reactive
    /// owner (its composer) is cleaned up.
    pub fn set(self, agent: AgentId) {
        self.0.set(Some(agent.clone()));
        on_cleanup(move || {
            // The next agent's composer may already have taken over.
            let _ = self.0.try_update(|shown| {
                shown.take_if(|shown| *shown == agent);
            });
        });
    }
}

impl Default for DictationTarget {
    fn default() -> Self {
        Self::new()
    }
}

/// The [`DictationTarget`] from context.
///
/// # Panics
///
/// Outside [`crate::app::App`], which always provides it.
#[must_use]
pub fn use_dictation_target() -> DictationTarget {
    expect_context()
}

/// Records the previous screen in [`AppState::previous_route`]. Renders
/// nothing.
#[component]
pub fn NavigationTracker() -> impl IntoView {
    let app = use_app();
    let route = use_nav().route();
    Effect::new(move |previous: Option<Option<AppRoute>>| {
        let current = route.get();
        if let Some(previous) = previous
            && previous != current
        {
            app.previous_route.set(previous);
        }
        current
    });
}

/// The bottom dock: Agents · mic · Settings, within thumb reach.
#[component]
pub fn Dock() -> impl IntoView {
    let route = use_nav().route();
    let on_agents = move || matches!(route.get(), Some(AppRoute::Triage | AppRoute::Agent(_)));
    let on_settings = move || route.get() == Some(AppRoute::Settings);
    view! {
        <nav class="dock" aria-label="Primary">
            <DockLink href=AppRoute::Triage.href() label="Agents" icon=icons::INBOX active=Signal::derive(on_agents) />
            <MicFab />
            <DockLink href=AppRoute::Settings.href() label="Settings" icon=icons::SLIDERS active=Signal::derive(on_settings) />
        </nav>
    }
}

#[component]
fn DockLink(
    href: String,
    label: &'static str,
    icon: icons::Paths,
    active: Signal<bool>,
) -> impl IntoView {
    view! {
        <a class="dock-link" href=href aria-current=move || active.get().then_some("page")>
            <Icon paths=icon />
            <span class="dock-label">{label}</span>
        </a>
    }
}

/// The dictation button, bottom-centre in the dock (design §6.3, §7): the
/// app's only mic. On an agent's screen it dictates to that agent while
/// the composer is up. Elsewhere there is no one to dictate to; with the
/// keypad or choices up, dictation would have nowhere to land; and
/// outside a secure context the browser can't record.
#[component]
pub fn MicFab() -> impl IntoView {
    let app = use_app();
    let target = use_dictation_target().0;
    let route = use_nav().route();
    let target = Memo::new(move |_| target.with(|target| mic_target(route.get(), target.as_ref())));
    move || {
        target
            .get()
            .map(|id| view! { <MicButton dictation=Dictation::for_agent(app, &id) fab=true /> })
    }
}

/// The agent the dock's mic dictates to: the one on screen, while its
/// composer is up (the [`DictationTarget`]).
fn mic_target(route: Option<AppRoute>, target: Option<&AgentId>) -> Option<AgentId> {
    match route {
        Some(AppRoute::Agent(id)) if target == Some(&id) => Some(id),
        _ => None,
    }
}

/// A slim banner while the browser is offline.
#[component]
pub fn OfflineBanner() -> impl IntoView {
    let app = use_app();
    view! {
        <Show when=move || !app.online.get()>
            <div class="offline-banner" role="status">
                <Icon paths=icons::OFFLINE />
                <span>"Offline. Anything you send waits until you're back."</span>
            </div>
        </Show>
    }
}

/// Renders the current toast, if any, in a live region.
#[component]
pub fn ToastHost() -> impl IntoView {
    let app = use_app();
    view! {
        <div class="toast-host" aria-live="polite">
            {move || app.toast.get().map(|t| view! { <div class="toast">{t.text}</div> })}
        </div>
    }
}

/// A screen's title block. `back` adds a back link (48dp hit area);
/// `children`, if any, sit at the trailing edge (the screen's rarer actions).
#[component]
pub fn PageHeader(
    /// The title.
    #[prop(into)]
    title: Signal<String>,
    /// A secondary line under the title.
    #[prop(optional, into)]
    subtitle: Option<Signal<String>>,
    /// Where the back link goes, if the screen has one.
    #[prop(optional, into)]
    back: Option<Signal<AppRoute>>,
    /// Trailing actions.
    #[prop(optional)]
    children: Option<Children>,
) -> impl IntoView {
    view! {
        <header class="page-header">
            {back.map(|route| view! {
                <a class="back-link" href=move || route.get().href() aria-label="Back">
                    <Icon paths=icons::BACK />
                </a>
            })}
            <div class="page-titles">
                <h1 class="page-title">{title}</h1>
                {subtitle.map(|s| view! { <p class="page-subtitle">{s}</p> })}
            </div>
            {children.map(|actions| view! { <div class="page-actions">{actions()}</div> })}
        </header>
    }
}

/// A centred, quiet "nothing here" block.
#[component]
pub fn EmptyState(
    icon: icons::Paths,
    title: &'static str,
    text: &'static str,
    /// Optional call to action below the text.
    #[prop(optional)]
    children: Option<Children>,
) -> impl IntoView {
    view! {
        <div class="empty">
            <div class="empty-icon"><Icon paths=icon /></div>
            <h2 class="empty-title">{title}</h2>
            <p class="empty-text">{text}</p>
            {children.map(|c| c())}
        </div>
    }
}

/// The router's fallback.
#[component]
pub fn NotFound() -> impl IntoView {
    view! {
        <section class="page">
            <PageHeader title="Not found" back=AppRoute::Triage />
            <EmptyState icon=icons::INBOX title="Nothing here" text="That link doesn't point at a screen or an agent this app knows.">
                <a class="button button-secondary" href=AppRoute::Triage.href()>"Back to agents"</a>
            </EmptyState>
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: &str) -> AgentId {
        id.parse().expect("agent id")
    }

    #[test]
    fn the_dock_mic_dictates_to_the_agent_on_screen() {
        let id = agent("w1:p1");
        assert_eq!(
            mic_target(Some(AppRoute::Agent(id.clone())), Some(&id)),
            Some(id)
        );
    }

    #[test]
    fn the_dock_mic_hides_without_an_agent_on_screen() {
        for route in [AppRoute::Triage, AppRoute::Settings, AppRoute::Pair] {
            assert_eq!(mic_target(Some(route), Some(&agent("w1:p1"))), None);
        }
        assert_eq!(mic_target(None, Some(&agent("w1:p1"))), None);
    }

    #[test]
    fn the_dock_mic_hides_while_the_composer_is_down() {
        let id = agent("w1:p1");
        assert_eq!(mic_target(Some(AppRoute::Agent(id)), None), None);
    }

    #[test]
    fn the_dock_mic_never_dictates_to_an_agent_left_behind() {
        // Swiping on: the last agent's composer may linger a moment.
        assert_eq!(
            mic_target(Some(AppRoute::Agent(agent("w1:p2"))), Some(&agent("w1:p1"))),
            None
        );
    }
}
