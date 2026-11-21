//! "New agent": start an agent from the phone (`POST /v1/agents`).
//!
//! A `+` beside the triage segments, in thumb reach, opens a bottom sheet:
//! pick a project from the bridge's allowlist (`GET /v1/projects`), the
//! agent kind, and optionally a first prompt, typed or dictated with the
//! same dictation as the composer. Start sends one request; once the
//! bridge answers with the new agent and the live list shows it, the app
//! opens it. The phone never sends a path or a flag: only a project name
//! and a kind the bridge offered.
//!
//! - [`sheet`]: what is picked, the request, retries with the same key,
//!   and what each answer means (host-tested).
//! - [`view`](mod@view): the sheet, drawn by the extras module ([`crate::extras`]).
//! - this module: the button, and the page's part of the sheet: opening
//!   the new agent and the prompt's dictation.

pub mod sheet;
pub mod view;

use core::cell::RefCell;

use leptos::mount::{UnmountHandle, mount_to};
use leptos::prelude::*;
use protocol::AgentId;
use web_sys::HtmlElement;

use crate::extras::Surface;
use crate::extras::page::{self, Extra};
use crate::live::Live;
use crate::nav::{History, Nav};
use crate::pair::scanner::sleep;
use crate::routes::AppRoute;
use crate::ui::AppState;
use crate::ui::icons::{self, Icon};
use crate::voice::phase::append_transcript;
use crate::voice::view::{Dictation, DictationStrip, MicButton};

/// What the dictation module calls the sheet's dictation. Its dictations
/// are keyed by agent id, and the agent does not exist yet; herdr pane ids
/// look like `w1:p2`, so this never names a real one.
const DICTATION_KEY: &str = "new-agent";

/// How long to wait for the live list to show the new agent before
/// opening it anyway, and how often to look.
const OPEN_WAIT_MS: i32 = 5_000;
const OPEN_POLL_MS: i32 = 100;

/// The `+` that opens the sheet, while `shown` (the device is connected).
#[component]
pub fn NewAgentButton(open: RwSignal<bool>, shown: Signal<bool>) -> impl IntoView {
    view! {
        <button
            type="button"
            class="new-agent-button"
            hidden=move || !shown.get()
            aria-haspopup="dialog"
            aria-label="New agent"
            on:click=move |_| open.set(true)
        >
            <Icon paths=icons::PLUS />
        </button>
    }
}

/// The sheet, while `open`.
#[component]
pub fn NewAgentSheet(open: RwSignal<bool>) -> impl IntoView {
    move || open.get().then(|| view! { <Sheet open=open /> })
}

#[component]
fn Sheet(open: RwSignal<bool>) -> impl IntoView {
    // The prompt's dictation lives as long as the sheet.
    on_cleanup(|| drop(DICTATION.take()));
    view! { <Extra surface=Surface::NewAgent on_close=Callback::new(move |()| open.set(false)) /> }
}

/// The sheet's dictation as mounted into the module's view.
struct SheetDictation {
    dictation: Dictation,
    _strip: UnmountHandle<leptos::tachys::view::any_view::AnyViewState>,
    _mic: UnmountHandle<leptos::tachys::view::any_view::AnyViewState>,
}

impl Drop for SheetDictation {
    /// The sheet closed: a recording stops (and is kept).
    fn drop(&mut self) {
        self.dictation.leave();
    }
}

thread_local! {
    static DICTATION: RefCell<Option<SheetDictation>> = const { RefCell::new(None) };
}

/// Keeps an unsent first prompt before opening the new agent, once the
/// live list shows it (never waiting long). Toasts its note and closes the sheet.
pub(crate) async fn open_agent(app: AppState, live: Live, nav: Nav, opened: sheet::Opened) {
    let sheet::Opened { agent, note, draft } = opened;
    if let Some(draft) = draft {
        live.set_draft(
            &agent,
            append_transcript(&live.draft(&agent), draft.as_str()),
        );
    }
    // The bridge waits for its own list first, so the stream usually has
    // the agent already.
    let mut waited = 0;
    while waited < OPEN_WAIT_MS && !live.agents.with(|all| all.iter().any(|a| a.id == agent)) {
        sleep(OPEN_POLL_MS).await;
        waited += OPEN_POLL_MS;
    }
    if let Some(note) = note {
        app.toast(note);
    }
    nav.go(&AppRoute::Agent(agent), History::Push);
    page::close(Surface::NewAgent);
}

/// Mounts the sheet's dictation into the module's view: picked up where
/// it was left, what was said appended to the prompt (never sent by
/// itself: here "send" means the user's own Start), and a recording
/// stopped (and kept) when the sheet closes.
pub(crate) fn mount_dictation(app: AppState, strip: HtmlElement, mic: HtmlElement) {
    // A remount replaces the old one, which leaves first.
    drop(DICTATION.take());
    let key: AgentId = DICTATION_KEY
        .parse()
        .unwrap_or_else(|_| unreachable!("a valid pane id"));
    let dictation = Dictation::for_agent(app, &key);
    dictation.on_take(Callback::new(move |(said, _send): (String, bool)| {
        page::send(Surface::NewAgent, &said)
    }));
    dictation.resume();
    let mounted = |root: HtmlElement, mic: bool| {
        let dictation = dictation.clone();
        mount_to(root, move || {
            provide_context(app);
            if mic {
                view! { <MicButton dictation=dictation /> }.into_any()
            } else {
                view! { <DictationStrip dictation=dictation /> }.into_any()
            }
        })
    };
    let sheet = SheetDictation {
        _strip: mounted(strip, false),
        _mic: mounted(mic, true),
        dictation,
    };
    DICTATION.set(Some(sheet));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dictation_key_is_an_agent_id_no_herdr_pane_has() {
        let key: AgentId = DICTATION_KEY.parse().expect("parses as an agent id");
        assert!(!key.pane().as_str().contains(':'), "herdr panes are wN:pM");
    }
}
