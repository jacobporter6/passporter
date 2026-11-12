//! The Settings toggle's state machine: what the browser allows, what the
//! user asked for, and what is in flight. Pure, so every transition is
//! tested on the host; [`super::browser`] runs the [`Command`]s.

/// Why push can't be turned on here at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Unavailable {
    /// Not a secure context (plain `http://` other than localhost): the
    /// browser hides the push APIs.
    Insecure,
    /// No service worker, Push API or Notification API (an old browser,
    /// a private window, iOS outside an installed web app).
    Unsupported,
    /// The user blocked notifications for this site.
    Denied,
    /// The bridge has push turned off (404 from the push endpoints).
    BridgeOff,
    /// The page runs in the Android app, whose `WebView` has no Push API:
    /// the app registers itself through `UnifiedPush` instead.
    NativeApp,
    /// In the Android app without its native layer (a `WebView` too old for
    /// it): no `UnifiedPush` either, since that needs the native identity.
    NativeOff,
    /// In the Android app, signed in with a browser key paired before the
    /// app's native identity could be used: `UnifiedPush` needs the latter.
    PairInApp,
}

impl Unavailable {
    /// One line for under the disabled toggle.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Insecure => "Needs a secure (https) connection to the bridge.",
            Self::Unsupported => "This browser can't receive push notifications.",
            Self::Denied => "Notifications are blocked for this site in your browser settings.",
            Self::BridgeOff => "Push is turned off on the bridge.",
            Self::NativeApp => {
                "In the app, notifications come through UnifiedPush (ntfy), not this switch."
            }
            Self::NativeOff => {
                "Notifications need the app's native features, which this WebView can't run: \
                 update Android System WebView."
            }
            Self::PairInApp => {
                "Notifications need the app's own pairing: forget this phone, then pair it again."
            }
        }
    }
}

/// What the notice WASM found about this browser's subscription, once the
/// app has checked that push can work here at all
/// ([`super::browser::availability`]). Crosses the module boundary by
/// name ([`Probe::as_str`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// Subscribed, and the bridge has the subscription (or couldn't be
    /// reached, so the browser is trusted).
    Subscribed,
    /// Push would work but nothing is subscribed.
    NotSubscribed,
    /// The bridge has push turned off.
    BridgeOff,
    /// Couldn't tell: the bridge answered with an error, or a browser step
    /// failed. Worth trying again.
    Unknown,
}

impl Probe {
    /// Every probe result, for tests.
    #[cfg(test)]
    const ALL: [Self; 4] = [
        Self::Subscribed,
        Self::NotSubscribed,
        Self::BridgeOff,
        Self::Unknown,
    ];

    /// The name the notice WASM returns it as (see `super::module`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Subscribed => "subscribed",
            Self::NotSubscribed => "not-subscribed",
            Self::BridgeOff => "bridge-off",
            Self::Unknown => "unknown",
        }
    }

    /// Reads [`Probe::as_str`] back; anything else is unknown.
    #[must_use]
    pub fn parse(name: &str) -> Self {
        match name {
            "subscribed" => Self::Subscribed,
            "not-subscribed" => Self::NotSubscribed,
            "bridge-off" => Self::BridgeOff,
            _ => Self::Unknown,
        }
    }
}

/// The toggle's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PushState {
    /// Asking the browser (Settings just opened).
    Checking,
    /// Push can't be turned on here.
    Unavailable(Unavailable),
    /// Off: no subscription.
    Off,
    /// Couldn't check (see [`Probe::Unknown`]): shown off, and turning it
    /// on tries again.
    Unknown,
    /// Asking for permission, subscribing and registering with the bridge.
    Enabling,
    /// Subscribed and registered.
    On,
    /// Unsubscribing and telling the bridge.
    Disabling,
}

/// Work the toggle hands to the browser glue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Ask for permission, subscribe with the bridge's key, register.
    Enable,
    /// Unsubscribe and delete the bridge's copy.
    Disable,
}

/// How a [`Command`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Subscribed and registered.
    Enabled,
    /// No subscription left in the browser.
    Disabled,
    /// The user refused the permission prompt.
    Denied,
    /// The user dismissed the permission prompt without choosing.
    Dismissed,
    /// Something failed; the browser's subscription is as it was before.
    Failed,
}

impl PushState {
    /// The state for what the browser reported.
    #[must_use]
    pub const fn from_probe(probe: Probe) -> Self {
        match probe {
            Probe::Subscribed => Self::On,
            Probe::NotSubscribed => Self::Off,
            Probe::BridgeOff => Self::Unavailable(Unavailable::BridgeOff),
            Probe::Unknown => Self::Unknown,
        }
    }

    /// The user flipped the toggle: the next state and the work to start,
    /// or `None` when the toggle can't move (checking, unavailable, busy).
    #[must_use]
    pub const fn toggle(self) -> Option<(Self, Command)> {
        match self {
            Self::Off | Self::Unknown => Some((Self::Enabling, Command::Enable)),
            Self::On => Some((Self::Disabling, Command::Disable)),
            Self::Checking | Self::Unavailable(_) | Self::Enabling | Self::Disabling => None,
        }
    }

    /// A [`Command`] finished. Outcomes that don't belong to the current
    /// state (a stale result after a re-probe) leave it unchanged.
    #[must_use]
    pub const fn finish(self, outcome: Outcome) -> Self {
        match (self, outcome) {
            // A failed disable leaves the browser subscribed: still on.
            (Self::Enabling | Self::Disabling, Outcome::Enabled)
            | (Self::Disabling, Outcome::Failed) => Self::On,
            (Self::Enabling | Self::Disabling, Outcome::Disabled)
            | (Self::Enabling, Outcome::Dismissed | Outcome::Failed) => Self::Off,
            (Self::Enabling, Outcome::Denied) => Self::Unavailable(Unavailable::Denied),
            (state, _) => state,
        }
    }

    /// Whether the switch shows as on: on, or on its way there. A busy
    /// switch shows what the user asked for and snaps back if it fails. An
    /// unpaired device is always off: losing the credentials turns push off.
    #[must_use]
    pub const fn is_checked(self, paired: bool) -> bool {
        paired && matches!(self, Self::On | Self::Enabling)
    }

    /// Whether the switch can be flipped: settled, on a paired device (the
    /// bridge only takes subscriptions from one, and an unpaired device has
    /// push off already).
    #[must_use]
    pub const fn can_toggle(self, paired: bool) -> bool {
        paired && matches!(self, Self::Off | Self::On | Self::Unknown)
    }

    /// The one-line note under the switch.
    #[must_use]
    pub const fn note(self, paired: bool) -> &'static str {
        match self {
            Self::Unavailable(why) => why.reason(),
            Self::Off | Self::Unknown if !paired => "Pair this device first.",
            Self::Unknown => "Couldn't check push notifications. Turn them on to try again.",
            _ => {
                "Only a wake-up ping goes through the push service. Details come from your bridge."
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_sets_the_starting_state() {
        assert_eq!(PushState::from_probe(Probe::Subscribed), PushState::On);
        assert_eq!(PushState::from_probe(Probe::NotSubscribed), PushState::Off);
        assert_eq!(
            PushState::from_probe(Probe::BridgeOff),
            PushState::Unavailable(Unavailable::BridgeOff)
        );
        assert_eq!(PushState::from_probe(Probe::Unknown), PushState::Unknown);
    }

    #[test]
    fn an_unknown_state_can_be_turned_on_to_retry() {
        let unknown = PushState::Unknown;
        assert!(!unknown.is_checked(true));
        assert!(unknown.can_toggle(true));
        assert!(!unknown.can_toggle(false));
        assert_eq!(
            unknown.toggle(),
            Some((PushState::Enabling, Command::Enable))
        );
        assert_ne!(unknown.note(true), PushState::Off.note(true));
        assert_eq!(unknown.note(false), "Pair this device first.");
    }

    #[test]
    fn probes_cross_the_module_boundary_by_name() {
        for probe in Probe::ALL {
            assert_eq!(Probe::parse(probe.as_str()), probe);
        }
        assert_eq!(Probe::parse("garbage"), Probe::Unknown);
    }

    #[test]
    fn toggling_starts_work_only_from_a_settled_state() {
        assert_eq!(
            PushState::Off.toggle(),
            Some((PushState::Enabling, Command::Enable))
        );
        assert_eq!(
            PushState::On.toggle(),
            Some((PushState::Disabling, Command::Disable))
        );
        for state in [
            PushState::Checking,
            PushState::Enabling,
            PushState::Disabling,
            PushState::Unavailable(Unavailable::Insecure),
            PushState::Unavailable(Unavailable::Unsupported),
            PushState::Unavailable(Unavailable::Denied),
            PushState::Unavailable(Unavailable::NativeApp),
            PushState::Unavailable(Unavailable::NativeOff),
            PushState::Unavailable(Unavailable::PairInApp),
        ] {
            assert_eq!(state.toggle(), None, "{state:?}");
        }
    }

    #[test]
    fn enabling_outcomes() {
        let enabling = PushState::Enabling;
        assert_eq!(enabling.finish(Outcome::Enabled), PushState::On);
        assert_eq!(
            enabling.finish(Outcome::Denied),
            PushState::Unavailable(Unavailable::Denied)
        );
        assert_eq!(enabling.finish(Outcome::Dismissed), PushState::Off);
        assert_eq!(enabling.finish(Outcome::Failed), PushState::Off);
    }

    #[test]
    fn disabling_outcomes() {
        let disabling = PushState::Disabling;
        assert_eq!(disabling.finish(Outcome::Disabled), PushState::Off);
        // The browser still holds the subscription: still on.
        assert_eq!(disabling.finish(Outcome::Failed), PushState::On);
    }

    #[test]
    fn stray_outcomes_change_nothing() {
        for outcome in [
            Outcome::Enabled,
            Outcome::Disabled,
            Outcome::Denied,
            Outcome::Failed,
        ] {
            assert_eq!(PushState::Off.finish(outcome), PushState::Off);
            assert_eq!(PushState::On.finish(outcome), PushState::On);
            assert_eq!(PushState::Checking.finish(outcome), PushState::Checking);
        }
    }

    #[test]
    fn a_full_round_trip() {
        let (state, command) = PushState::Off.toggle().unwrap();
        assert_eq!(command, Command::Enable);
        assert!(state.is_checked(true) && !state.can_toggle(true));
        let state = state.finish(Outcome::Enabled);
        assert!(state.is_checked(true) && state.can_toggle(true));
        let (state, command) = state.toggle().unwrap();
        assert_eq!(command, Command::Disable);
        assert!(!state.is_checked(true) && !state.can_toggle(true));
        assert_eq!(state.finish(Outcome::Disabled), PushState::Off);
    }

    #[test]
    fn the_switch_needs_a_paired_device() {
        assert!(PushState::Off.can_toggle(true));
        assert!(!PushState::Off.can_toggle(false));
        assert_eq!(PushState::Off.note(false), "Pair this device first.");
        // Losing the credentials turns push off.
        assert!(!PushState::On.can_toggle(false));
        assert!(!PushState::On.is_checked(false));
    }

    #[test]
    fn unavailable_states_explain_themselves() {
        for why in [
            Unavailable::Insecure,
            Unavailable::Unsupported,
            Unavailable::Denied,
            Unavailable::BridgeOff,
            Unavailable::NativeApp,
            Unavailable::NativeOff,
            Unavailable::PairInApp,
        ] {
            let state = PushState::Unavailable(why);
            assert!(!state.can_toggle(true));
            assert!(!state.is_checked(true));
            assert_eq!(state.note(true), why.reason());
            assert!(!why.reason().contains('\n'));
        }
    }
}
