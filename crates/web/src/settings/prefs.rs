//! Device preferences: the switches in Settings, kept per device in the
//! app's store and read into [`AppState`] at startup.
//!
//! Adding one: a [`Pref`] variant with its arms below (name, store key,
//! label, note, the [`AppState`] signal it sets), then its
//! `<PrefSwitch pref=… />` where it goes in [`super::view`](mod@super::view).

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::{Deserialize, Serialize};

use crate::store::{StoreKey, StoreName};
use crate::ui::AppState;
use crate::voice::browser;

/// A device preference with a switch in Settings. Absent from the store
/// means off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Pref {
    /// "Review dictation before sending". Off, a dictation is sent to the
    /// agent as soon as the user stops; on, its transcript waits on a card
    /// to edit and "Use" (see [`crate::voice`]).
    ReviewDictation,
    /// "Show full transcripts by default". Off, the agent screen's thread
    /// is summary-first (each turn's prompt and headline, the rest behind
    /// "Show full turn"); on, it shows whole transcripts (see
    /// [`crate::agent::thread`]).
    FullTranscripts,
}

impl Pref {
    /// Every preference.
    pub const ALL: [Self; 2] = [Self::ReviewDictation, Self::FullTranscripts];

    /// The name it crosses the module boundary by.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ReviewDictation => "review-dictation",
            Self::FullTranscripts => "full-transcripts",
        }
    }

    /// The preference called `name`, if any.
    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.name() == name)
    }

    /// Its key in the store's meta object store.
    const fn store_key(self) -> &'static str {
        match self {
            Self::ReviewDictation => "reviewDictation",
            Self::FullTranscripts => "fullTranscripts",
        }
    }

    /// The switch's label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ReviewDictation => "Review dictation before sending",
            Self::FullTranscripts => "Show full transcripts by default",
        }
    }

    /// The note under the switch.
    #[must_use]
    pub const fn note(self) -> &'static str {
        match self {
            Self::ReviewDictation => {
                "Off, what you dictate is sent as soon as you stop. On, you check and edit it first."
            }
            Self::FullTranscripts => {
                "Off, each turn shows its prompt and headline, with the rest a tap away."
            }
        }
    }

    /// Whether Settings offers it on this device: the dictation switch
    /// only where the browser can record (there is no mic button either).
    #[must_use]
    pub fn offered(self) -> bool {
        match self {
            Self::ReviewDictation => browser::available(),
            Self::FullTranscripts => true,
        }
    }

    /// The app's signal for it.
    #[must_use]
    pub const fn signal(self, app: AppState) -> RwSignal<bool> {
        match self {
            Self::ReviewDictation => app.review_dictation,
            Self::FullTranscripts => app.full_transcripts,
        }
    }
}

/// Where a [`Pref`] is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrefKey(pub Pref);

impl StoreKey for PrefKey {
    type Value = bool;
    const STORE: StoreName = StoreName::Meta;
    fn encode(&self) -> String {
        self.0.store_key().to_owned()
    }
}

/// Reads the device's preferences into their signals. One never set (or
/// a store that can't be read) stays off.
pub async fn load(app: AppState) {
    let Ok(store) = app.store().await else {
        return;
    };
    for pref in Pref::ALL {
        if let Ok(Some(on)) = store.get(&PrefKey(pref)).await {
            pref.signal(app).set(on);
        }
    }
}

/// Sets the preference called `name` (from its switch) and stores it.
pub fn set(app: AppState, name: &str, on: bool) {
    let Some(pref) = Pref::named(name) else {
        return;
    };
    pref.signal(app).set(on);
    spawn_local(async move {
        let saved = match app.store().await {
            Ok(store) => store.put(&PrefKey(pref), &on).await,
            Err(error) => Err(error),
        };
        if let Err(error) = saved {
            app.toast(error.to_string());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pref_crosses_by_its_own_name() {
        for pref in Pref::ALL {
            assert_eq!(Pref::named(pref.name()), Some(pref));
        }
        assert_eq!(Pref::named("nope"), None);
    }

    #[test]
    fn stored_keys_never_change() {
        // Devices already hold the value under this key.
        assert_eq!(PrefKey(Pref::ReviewDictation).encode(), "reviewDictation");
        assert_eq!(PrefKey(Pref::FullTranscripts).encode(), "fullTranscripts");
    }
}
