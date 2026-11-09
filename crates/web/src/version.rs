//! Which build is running, and whether the bridge runs a newer one.
//!
//! The app and the bridge are both stamped at compile time with the commit
//! they were built from (see `build/version.rs`). When the bridge's commit
//! differs from the app's, the phone runs a stale app (a service worker
//! still serving the old shell, or a PWA that slept through the deploy), so
//! the update banner offers a reload that gets past the service worker
//! ([`platform::reload_into_latest`]). The banner also shows when a new
//! service worker is already waiting ([`AppState::update_ready`]).
//!
//! The bridge is asked (`GET /v1/version`, no token) at launch and every
//! time the stream (re)connects, which is what a bridge restart after a
//! deploy and a phone back from the background both cause. Each reconnect
//! also asks the browser to look for a new service worker, so a deploy of
//! the PWA alone is found too.

use leptos::prelude::*;
use leptos::task::spawn_local;
use protocol::Timestamp;

use crate::live::{Conn, use_live};
use crate::platform;
use crate::session;
use crate::ui::{AppState, format, use_app};

/// The commit this app was built from: short, `-dirty` when the tree had
/// changes, or `unknown`.
pub const APP_SHA: &str = env!("PASSPORTER_GIT_SHA");
/// When this app was built, RFC 3339 in UTC.
pub const APP_BUILT: &str = env!("PASSPORTER_BUILT");
/// The same, in unix seconds.
const APP_BUILT_UNIX: &str = env!("PASSPORTER_BUILT_UNIX");

/// The `sessionStorage` key holding the bridge commit this tab last
/// reloaded for, so a reload that can't help (the bridge serves an older
/// app than it runs itself) is offered once per launch, not forever.
const RELOADED_FOR: &str = "passporter.reloaded-for";

/// When this app was built.
#[must_use]
pub fn app_built() -> Option<Timestamp> {
    let secs: u64 = APP_BUILT_UNIX.parse().ok()?;
    Some(Timestamp::from_unix_millis(secs.checked_mul(1_000)?))
}

/// The commit a build id names, without `-dirty`; `None` when unknown.
/// Dirty builds compare by commit: a work tree's bridge and app are both
/// `-dirty` (or not) depending on when each was last rebuilt, which is no
/// reason to nag.
fn commit(sha: &str) -> Option<&str> {
    let commit = sha.strip_suffix("-dirty").unwrap_or(sha);
    (!commit.is_empty() && commit != "unknown").then_some(commit)
}

/// What the app knows of the bridge's build.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BridgeBuild {
    /// Not answered yet.
    Checking,
    /// The bridge's build id.
    Known(String),
    /// The bridge could not be asked.
    Unreachable,
}

/// How the running app compares with the bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Freshness {
    /// Can't tell: the bridge hasn't answered, or a build is unknown.
    Unknown,
    /// Built from the same commit.
    Current,
    /// The bridge runs another build: reloading should fetch it.
    Behind,
    /// The bridge runs another build, and this tab already reloaded for it
    /// without getting it: the bridge serves an older app than it runs.
    Stuck,
}

/// How an app built as `app` compares with `bridge`. `reloaded_for` is
/// the bridge build this tab last reloaded for.
#[must_use]
pub fn freshness(app: &str, bridge: &BridgeBuild, reloaded_for: Option<&str>) -> Freshness {
    let BridgeBuild::Known(bridge) = bridge else {
        return Freshness::Unknown;
    };
    match (commit(app), commit(bridge)) {
        (Some(app), Some(theirs)) if app == theirs => Freshness::Current,
        (Some(_), Some(_)) if reloaded_for == Some(bridge.as_str()) => Freshness::Stuck,
        (Some(_), Some(_)) => Freshness::Behind,
        _ => Freshness::Unknown,
    }
}

/// Whether to show the "new version" banner: a new service worker is
/// waiting, or the bridge runs a build this app isn't.
#[must_use]
pub fn offer_reload(update_ready: bool, freshness: Freshness) -> bool {
    update_ready || freshness == Freshness::Behind
}

/// "built 3h ago" (or "built just now"); empty when the time is unknown.
#[must_use]
pub fn built_ago(built: Option<Timestamp>, now: Timestamp) -> String {
    match built.map(|built| format::time_in_state(built, now)) {
        Some(ago) if ago == "now" => "built just now".to_owned(),
        Some(ago) => format!("built {ago} ago"),
        None => String::new(),
    }
}

/// The bridge's build as the Settings row shows it.
#[must_use]
pub fn bridge_label(bridge: &BridgeBuild) -> String {
    match bridge {
        BridgeBuild::Checking => "Checking…".to_owned(),
        BridgeBuild::Known(sha) => sha.clone(),
        BridgeBuild::Unreachable => "Unreachable".to_owned(),
    }
}

/// The bridge's build and how this app compares, provided as context.
#[derive(Debug, Clone, Copy)]
pub struct Versions {
    /// What the bridge last said.
    pub bridge: RwSignal<BridgeBuild>,
    /// The bridge build this tab last reloaded for.
    reloaded_for: StoredValue<Option<String>>,
    /// A reload into the new build is under way.
    pub reloading: RwSignal<bool>,
}

impl Versions {
    /// Starts asking the bridge for its build, now and on every stream
    /// (re)connect, and provides the result as context. Call once, inside
    /// the app (it needs [`AppState`] and the live state from context).
    pub fn start(app: AppState) -> Self {
        let live = use_live();
        let bridge = RwSignal::new(BridgeBuild::Checking);
        let versions = Self {
            bridge,
            reloaded_for: StoredValue::new(platform::session_value(RELOADED_FOR)),
            reloading: RwSignal::new(false),
        };
        spawn_local(versions.ask(app, false));
        Effect::new(move |_| {
            if live.conn.get() == Conn::Live {
                spawn_local(versions.ask(app, true));
            }
        });
        provide_context(versions);
        versions
    }

    /// How this app compares with the bridge (tracked).
    #[must_use]
    pub fn freshness(self) -> Freshness {
        self.reloaded_for.with_value(|reloaded_for| {
            self.bridge
                .with(|bridge| freshness(APP_SHA, bridge, reloaded_for.as_deref()))
        })
    }

    /// Asks the bridge for its build (a failed ask keeps a known answer),
    /// then, if `and_worker`, the browser for a new service worker.
    async fn ask(self, app: AppState, and_worker: bool) {
        let answer = session::api(app).version().await;
        self.bridge.update(|bridge| match answer {
            Ok(version) => *bridge = BridgeBuild::Known(version.bridge),
            Err(_) if *bridge == BridgeBuild::Checking => *bridge = BridgeBuild::Unreachable,
            Err(_) => {}
        });
        if and_worker {
            platform::check_for_update().await;
        }
    }

    /// Reloads into the bridge's build, remembering which one, so a reload
    /// that doesn't get it isn't offered again in this launch.
    pub fn reload(self) {
        if self.reloading.get_untracked() {
            return;
        }
        self.reloading.set(true);
        if let BridgeBuild::Known(sha) = self.bridge.get_untracked()
            && untrack(|| self.freshness()) == Freshness::Behind
        {
            platform::set_session_value(RELOADED_FOR, &sha);
        }
        spawn_local(platform::reload_into_latest());
    }
}

/// The [`Versions`] from context.
///
/// # Panics
///
/// Outside [`crate::app::App`], which always provides it.
#[must_use]
pub fn use_versions() -> Versions {
    expect_context()
}

/// "New version available", with a reload that gets past the service
/// worker.
#[component]
pub fn UpdateBanner() -> impl IntoView {
    let app = use_app();
    let versions = use_versions();
    let shown = move || offer_reload(app.update_ready.get(), versions.freshness());
    view! {
        <Show when=shown>
            <div class="update-prompt" role="status">
                <span>"New version available"</span>
                <button
                    type="button"
                    class="button button-primary button-small"
                    disabled=move || versions.reloading.get()
                    on:click=move |_| versions.reload()
                >
                    {move || if versions.reloading.get() { "Reloading…" } else { "Reload" }}
                </button>
            </div>
        </Show>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(sha: &str) -> BridgeBuild {
        BridgeBuild::Known(sha.to_owned())
    }

    #[test]
    fn the_same_commit_is_current() {
        assert_eq!(
            freshness("0123abcd", &known("0123abcd"), None),
            Freshness::Current
        );
        // Dirty builds compare by commit.
        assert_eq!(
            freshness("0123abcd-dirty", &known("0123abcd"), None),
            Freshness::Current
        );
        assert_eq!(
            freshness("0123abcd", &known("0123abcd-dirty"), None),
            Freshness::Current
        );
    }

    #[test]
    fn another_commit_is_behind_until_a_reload_did_not_help() {
        assert_eq!(
            freshness("0123abcd", &known("4567ef01"), None),
            Freshness::Behind
        );
        // Reloaded for an older bridge build: the new one is offered again.
        assert_eq!(
            freshness("0123abcd", &known("4567ef01"), Some("89abcdef")),
            Freshness::Behind
        );
        assert_eq!(
            freshness("0123abcd", &known("4567ef01"), Some("4567ef01")),
            Freshness::Stuck
        );
    }

    #[test]
    fn unknown_builds_never_nag() {
        for bridge in [
            BridgeBuild::Checking,
            BridgeBuild::Unreachable,
            known("unknown"),
            known(""),
        ] {
            assert_eq!(
                freshness("0123abcd", &bridge, None),
                Freshness::Unknown,
                "{bridge:?}"
            );
        }
        assert_eq!(
            freshness("unknown", &known("0123abcd"), None),
            Freshness::Unknown
        );
        assert_eq!(
            freshness("unknown-dirty", &known("0123abcd"), None),
            Freshness::Unknown
        );
    }

    #[test]
    fn the_banner_shows_for_a_waiting_worker_or_a_newer_bridge() {
        assert!(offer_reload(false, Freshness::Behind));
        assert!(offer_reload(true, Freshness::Current));
        assert!(offer_reload(true, Freshness::Unknown));
        assert!(
            offer_reload(true, Freshness::Stuck),
            "a waiting worker helps"
        );
        for quiet in [Freshness::Current, Freshness::Unknown, Freshness::Stuck] {
            assert!(!offer_reload(false, quiet), "{quiet:?}");
        }
    }

    #[test]
    fn build_times_read_as_ages() {
        let built = Timestamp::from_unix_millis(1_000_000);
        let at = |millis: u64| Timestamp::from_unix_millis(1_000_000 + millis);
        assert_eq!(built_ago(Some(built), at(2_000)), "built just now");
        assert_eq!(built_ago(Some(built), at(3 * 3_600_000)), "built 3h ago");
        // A phone clock behind the build machine's.
        assert_eq!(
            built_ago(Some(built), Timestamp::from_unix_millis(0)),
            "built just now"
        );
        assert_eq!(built_ago(None, at(0)), "");
    }

    #[test]
    fn the_bridge_row_says_what_is_known() {
        assert_eq!(bridge_label(&BridgeBuild::Checking), "Checking…");
        assert_eq!(bridge_label(&known("0123abcd")), "0123abcd");
        assert_eq!(bridge_label(&BridgeBuild::Unreachable), "Unreachable");
    }

    #[test]
    fn this_build_is_stamped() {
        assert_ne!(APP_SHA, "");
        assert!(app_built().is_some_and(|t| t.unix_millis() > 0));
        assert!(APP_BUILT.ends_with('Z'), "{APP_BUILT}");
    }
}
