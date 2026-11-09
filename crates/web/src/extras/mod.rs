//! The extras WASM (`src/bin/extras.rs`): the app's rarely used surfaces,
//! loaded the first time one opens.
//!
//! The "New agent" sheet, the Runbooks sheet (with the voice-runbook
//! confirm), the Settings groups and the link confirm are compiled into
//! one on-demand module, with its own Leptos runtime, rather than the app,
//! whose WASM has no room for views opened a few times a day
//! ([`crate::lazy`]). One module for all four costs one runtime, not four.
//! `extras.js` and `extras_bg.wasm` are in the shell cache, like the rest
//! of the app.
//!
//! The two sides have separate memories and talk through plain values:
//!
//! - [`page`] (the app): an [`page::Extra`] element loads the module,
//!   mounts a [`Surface`] into itself with its *feed* (a string: JSON or
//!   text, per surface), sends the feed again whenever it changes, and
//!   shows a spinner while loading or a message if the module won't load.
//!   It hands the module one host function for everything only the app
//!   can do ([`Ask`]): the session's token, toasts, navigation, the
//!   stream's state.
//! - [`mount`] (the module): mounts and updates each surface's view and
//!   asks the page through that host.
//!
//! Each surface's code lives with its feature: [`crate::spawn`],
//! [`crate::runbooks`], [`crate::settings`] and [`crate::ui::markdown`].

pub mod mount;
pub mod page;

/// A view the extras module draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// The "New agent" sheet ([`crate::spawn`]).
    NewAgent,
    /// The Runbooks sheet and its confirm ([`crate::runbooks`]).
    Runbooks,
    /// The Settings page's groups ([`crate::settings`]).
    Settings,
    /// The "Open this link?" confirm for a link in agent text ([`crate::ui::markdown`]).
    Link,
    /// The confirm for a command in the agent's message, and the terminal
    /// it opens ([`crate::commands`]). Drawn by the terminal WASM, not the
    /// extras one ([`Surface::module`]).
    Command,
}

impl Surface {
    /// Every surface.
    pub const ALL: [Self; 5] = [
        Self::NewAgent,
        Self::Runbooks,
        Self::Settings,
        Self::Link,
        Self::Command,
    ];

    /// The name the surface crosses the module boundary by.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::NewAgent => "new-agent",
            Self::Runbooks => "runbooks",
            Self::Settings => "settings",
            Self::Link => "link",
            Self::Command => "command",
        }
    }

    /// The on-demand WASM that draws it (`src/bin/<module>.rs`): `terminal`
    /// for [`Surface::Command`], which would not fit in the extras' budget,
    /// `extras` for the rest. Both export the same `extras_*` functions and
    /// take the same host.
    #[must_use]
    pub const fn module(self) -> &'static str {
        match self {
            Self::Command => "terminal",
            Self::NewAgent | Self::Runbooks | Self::Settings | Self::Link => "extras",
        }
    }

    /// The surface called `name`, if any.
    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }

    /// Its index in [`Self::ALL`], for per-surface tables.
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }

    /// Whether it floats over the page (a sheet or a dialog) rather than
    /// filling it: where its loading spinner goes.
    #[must_use]
    pub const fn floats(self) -> bool {
        !matches!(self, Self::Settings)
    }

    /// What the user calls it, for "… didn't load".
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::NewAgent => "New agent",
            Self::Runbooks => "Runbooks",
            Self::Settings => "Settings",
            Self::Link => "The link",
            Self::Command => "The command",
        }
    }
}

/// What the module asks of the page, through the host function
/// `host(ask, a, b)`: [`Ask::name`], then up to two arguments. Each says
/// what it takes and returns; one that waits returns a promise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ask {
    /// The session's bearer token (a string), or `undefined` without one.
    Token,
    /// The bridge refused the token `a`: the session drops or renews it.
    Unauthorized,
    /// Shows `a` as a toast.
    Toast,
    /// The user closed the surface named `a`.
    Close,
    /// The bridge started agent `a`: wait (briefly) for the agent list to
    /// show it, toast `b` (if a string), open it and close the "New agent"
    /// sheet. A promise of when that is done.
    OpenAgent,
    /// Mounts the "New agent" prompt's dictation (the page's): its strip
    /// in element `a`, its mic button in element `b`.
    MountDictation,
    /// The bridge's runbooks, freshly fetched (`a`: a `RunbookList` as
    /// JSON): what dictation matches against.
    RunbooksListed,
    /// A runbook run started (`a`: a `RunbookRun` as JSON): the latest of
    /// its runbook, and the offer it confirmed is settled.
    RunbookStarted,
    /// The offered runbook was not run: spoken words go back to the
    /// composer they were dictated in.
    Declined,
    /// Forgets this device's credential. A promise of `undefined`, or the
    /// failure's message.
    Forget,
    /// Clears the cached snapshots, and toasts how that went. A promise of
    /// when done.
    ClearSnapshots,
    /// Checks push. A promise of the switch's state
    /// ([`crate::push::state::PushState`] as JSON).
    PushProbe,
    /// Subscribes to push, once permission is granted. A promise of
    /// `undefined`, or the failure's message.
    PushEnable,
    /// Unsubscribes from push. Resolves like [`Ask::PushEnable`].
    PushDisable,
    /// Sets the device preference named `a` to `b` (a boolean).
    SetPref,
}

impl Ask {
    /// Every ask.
    pub const ALL: [Self; 15] = [
        Self::Token,
        Self::Unauthorized,
        Self::Toast,
        Self::Close,
        Self::OpenAgent,
        Self::MountDictation,
        Self::RunbooksListed,
        Self::RunbookStarted,
        Self::Declined,
        Self::Forget,
        Self::ClearSnapshots,
        Self::PushProbe,
        Self::PushEnable,
        Self::PushDisable,
        Self::SetPref,
    ];

    /// The name it crosses the module boundary by.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Token => "token",
            Self::Unauthorized => "unauthorized",
            Self::Toast => "toast",
            Self::Close => "close",
            Self::OpenAgent => "open-agent",
            Self::MountDictation => "mount-dictation",
            Self::RunbooksListed => "runbooks-listed",
            Self::RunbookStarted => "runbook-started",
            Self::Declined => "declined",
            Self::Forget => "forget",
            Self::ClearSnapshots => "clear-snapshots",
            Self::PushProbe => "push-probe",
            Self::PushEnable => "push-enable",
            Self::PushDisable => "push-disable",
            Self::SetPref => "set-pref",
        }
    }

    /// The ask called `name`, if any.
    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|ask| ask.name() == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_ask_crosses_by_its_own_name() {
        for ask in Ask::ALL {
            assert_eq!(Ask::named(ask.name()), Some(ask));
        }
        assert_eq!(Ask::named("nope"), None);
    }

    #[test]
    fn every_surface_crosses_by_its_own_name() {
        for (i, surface) in Surface::ALL.into_iter().enumerate() {
            assert_eq!(Surface::named(surface.name()), Some(surface));
            assert_eq!(surface.index(), i);
        }
        assert_eq!(Surface::named("nope"), None);
        assert_eq!(Surface::Command.module(), "terminal");
        assert_eq!(Surface::Link.module(), "extras");
    }
}
