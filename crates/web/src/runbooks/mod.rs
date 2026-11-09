//! Runbooks: named operations from the bridge config, run from the phone
//! by tap or by voice (`GET /v1/runbooks`, `POST /v1/runbooks/:name/run`).
//!
//! A terminal button in the triage header opens the Runbooks sheet:
//! every runbook with its exact command and latest run. A tap on one asks
//! first (unless its config says `confirm = false`), showing the exact
//! `argv` and folder; a dictated utterance on an agent screen that names a
//! runbook exactly ([`matching`]) opens the same confirm instead of being
//! sent, and speech always asks. The run itself happens in its own herdr
//! tab; the sheet follows it from the stream (`runbook_updated`): running,
//! then the exit code, with its last lines. The phone never sends anything
//! but the runbook's name.
//!
//! - [`matching`]: which runbook an utterance names (host-tested).
//! - [`sheet`]: the sheet's feed, requests, retries and answers
//!   (host-tested).
//! - [`view`](mod@view): the sheet, drawn by the extras module ([`crate::extras`]).
//! - this module: the context (the list dictation matches against, the
//!   offer), the button, and the page's part of the sheet.

pub mod matching;
pub mod sheet;
pub mod view;

use leptos::prelude::*;
use leptos::task::spawn_local;
use protocol::{Runbook, RunbookList, RunbookName, RunbookRun};

use crate::extras::Surface;
use crate::extras::page::Extra;
use crate::live::{Live, use_live};
use crate::session;
use crate::ui::AppState;
use crate::ui::icons::{self, Icon};
use sheet::{Feed, OfferView};

/// The runbook waiting for a confirm, and how it was offered.
#[derive(Clone)]
struct Offer {
    name: RunbookName,
    /// What was said, when speech offered it, and where those words go if
    /// the user does not run the runbook (the agent's composer).
    spoken: Option<(String, Callback<String>)>,
}

/// App-wide runbook state, provided as context.
#[derive(Debug, Clone, Copy)]
pub struct Runbooks {
    /// The bridge's runbooks once loaded: what dictation matches against.
    list: RwSignal<Option<Vec<Runbook>>>,
    /// Whether the sheet is open.
    open: RwSignal<bool>,
    /// The runbook the sheet asks about.
    offer: RwSignal<Option<Offer>>,
}

/// Makes the runbook state and provides it.
pub fn provide() -> Runbooks {
    let runbooks = Runbooks {
        list: RwSignal::new(None),
        open: RwSignal::new(false),
        offer: RwSignal::new(None),
    };
    provide_context(runbooks);
    runbooks
}

/// The runbook state.
///
/// # Panics
/// Outside the app (no [`provide`] above).
#[must_use]
pub fn use_runbooks() -> Runbooks {
    expect_context()
}

impl Runbooks {
    /// Fetches the list (fresh commands).
    fn refresh(self, app: AppState) {
        spawn_local(async move {
            if let Ok(list) = session::api(app).runbooks().await {
                let _ = self.list.try_set(Some(list.runbooks));
            }
        });
    }

    /// The sheet fetched the list (a `RunbookList` as JSON).
    pub(crate) fn listed(self, json: &str) {
        if let Ok(list) = serde_json::from_str::<RunbookList>(json) {
            self.list.set(Some(list.runbooks));
        }
    }

    /// The sheet started a run (a `RunbookRun` as JSON): the latest run
    /// of its runbook, and the offer it confirmed is settled.
    pub(crate) fn started(self, live: Live, json: &str) {
        if let Ok(run) = serde_json::from_str::<RunbookRun>(json) {
            live.note_runbook_run(run);
        }
        self.offer.set(None);
    }

    /// Drops the offer without running it: spoken words go back to where
    /// they were dictated.
    pub(crate) fn decline(self) {
        if let Some(Offer {
            spoken: Some((said, give_back)),
            ..
        }) = self.offer.get_untracked()
        {
            give_back.run(said);
        }
        self.offer.set(None);
    }

    /// Fetches the list unless it is loaded: before dictation needs it.
    pub fn ensure_loaded(self, app: AppState) {
        if self.list.with_untracked(Option::is_none) {
            self.refresh(app);
        }
    }

    /// If `said` names a runbook exactly ([`matching::find`]), opens its
    /// confirm and returns true: the words are taken, not sent; if the user
    /// does not run it, they go to `give_back` (the composer) instead.
    /// Otherwise false, and they are dictated text as usual.
    pub fn offer_spoken(self, said: &str, give_back: Callback<String>) -> bool {
        let found = self.list.with_untracked(|list| {
            list.as_deref()
                .and_then(|l| matching::find(said, l))
                .map(|r| r.name.clone())
        });
        let Some(name) = found else {
            return false;
        };
        self.offer.set(Some(Offer {
            name,
            spoken: Some((said.trim().to_owned(), give_back)),
        }));
        self.open.set(true);
        true
    }
}

/// The button that opens the sheet, while `shown` (the device is paired).
#[component]
pub fn RunbooksButton(shown: Signal<bool>) -> impl IntoView {
    let runbooks = use_runbooks();
    view! {
        <button
            type="button"
            class="icon-button runbooks-button"
            hidden=move || !shown.get()
            aria-haspopup="dialog"
            aria-label="Runbooks"
            on:click=move |_| runbooks.open.set(true)
        >
            <Icon paths=icons::TERMINAL />
        </button>
    }
}

/// The sheet, while open.
#[component]
pub fn RunbooksSheet() -> impl IntoView {
    let runbooks = use_runbooks();
    move || {
        runbooks
            .open
            .get()
            .then(|| view! { <Sheet runbooks=runbooks /> })
    }
}

/// The page's part of the sheet: its feed, and closing it (which declines
/// an offer).
#[component]
fn Sheet(runbooks: Runbooks) -> impl IntoView {
    let live = use_live();
    let feed = Memo::new(move |_| {
        let feed = Feed {
            offer: runbooks.offer.with(|offer| {
                offer.as_ref().map(|offer| OfferView {
                    name: offer.name.clone(),
                    said: offer.spoken.as_ref().map(|(said, _)| said.clone()),
                })
            }),
            runs: live.runbook_runs(),
            resyncs: live.resync_generation(),
        };
        serde_json::to_string(&feed).unwrap_or_default()
    });
    let close = Callback::new(move |()| {
        runbooks.decline();
        runbooks.open.set(false);
    });
    view! { <Extra surface=Surface::Runbooks feed=feed on_close=close /> }
}
