//! The Settings groups, drawn by the extras module
//! ([`crate::extras::mount`]) below the page's header, from the page's
//! [`Snapshot`]. What the buttons do happens in the page, through the
//! host.

use core::cell::Cell;
use core::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::JsValue;
use web_sys::HtmlElement;

use super::model::Snapshot;
use super::prefs::Pref;
use crate::extras::Ask;
use crate::extras::mount::{self, Mounted};
use crate::live::Conn;
use crate::push::browser::PermissionPrompt;
use crate::push::state::{Command, Outcome, PushState};
use crate::routes::AppRoute;
use crate::session::{self, Credential};
use crate::ui::format;
use crate::ui::icons::{self, Icon};
use crate::version::{APP_BUILT, APP_SHA, Freshness, app_built, bridge_label, built_ago};

thread_local! {
    /// The push switch: kept while the page lives, so a change still in
    /// flight when Settings closes lands, and isn't checked over.
    static PUSH: ArcRwSignal<PushState> = ArcRwSignal::new(PushState::Checking);
    /// The newest push check.
    static PROBES: Cell<u32> = const { Cell::new(0) };
}

/// Draws the groups in `root`, fed `feed` (a [`Snapshot`] as JSON).
pub fn mount(root: HtmlElement, feed: &str) -> Mounted {
    let snapshot = ArcRwSignal::new(parse(feed));
    let take = snapshot.clone();
    Mounted::new(
        root,
        move || view! { <Groups snapshot=RwSignal::from(snapshot).read_only() /> }.into_any(),
        move |json| {
            take.set(parse(json));
            true
        },
    )
}

fn parse(json: &str) -> Snapshot {
    serde_json::from_str(json).unwrap_or_default()
}

/// Every group, in order.
#[component]
fn Groups(snapshot: ReadSignal<Snapshot>) -> impl IntoView {
    view! {
        <BridgeGroup snapshot=snapshot />
        <NotificationsGroup snapshot=snapshot />
        {move || snapshot.with(|s| s.pref(Pref::ReviewDictation).is_some()).then(|| view! {
            <h2 class="group-title">"Dictation"</h2>
            <div class="group">
                <PrefSwitch snapshot=snapshot pref=Pref::ReviewDictation />
            </div>
            <p class="group-note">{Pref::ReviewDictation.note()}</p>
        })}
        <h2 class="group-title">"Thread"</h2>
        <div class="group">
            <PrefSwitch snapshot=snapshot pref=Pref::FullTranscripts />
        </div>
        <p class="group-note">{Pref::FullTranscripts.note()}</p>
        <OfflineGroup snapshot=snapshot />
        <AboutGroup snapshot=snapshot />
    }
}

/// How the Bridge group describes this device's credential.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AuthView {
    /// How the device signs in, e.g. `Paired as "Pixel"`.
    mode: String,
    /// The link to the pairing screen.
    pair_label: &'static str,
    /// Forgetting the credential, when there is one.
    forget: Option<ForgetView>,
}

/// The forget button and its confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ForgetView {
    label: &'static str,
    question: &'static str,
    detail: &'static str,
    done: &'static str,
}

impl AuthView {
    /// The view of `credential`. `secure` is whether the page is a secure
    /// context: without one, pairing is out and the Pair screen only takes
    /// a pasted token.
    fn of(credential: &Credential, secure: bool) -> Self {
        match credential {
            Credential::None => Self {
                mode: "Not paired".to_owned(),
                pair_label: if secure {
                    "Pair this device"
                } else {
                    "Paste your token"
                },
                forget: None,
            },
            Credential::Device { name } => Self {
                mode: match name {
                    Some(name) => format!("Paired as \u{201c}{name}\u{201d}"),
                    None => "Paired device".to_owned(),
                },
                pair_label: "Pair again",
                forget: Some(ForgetView {
                    label: "Forget this device",
                    question: "Forget this device?",
                    detail: "Its key is deleted and this phone stops seeing your agents until it \
                             pairs again. The bridge lists the device until you run \
                             `passporter devices revoke`.",
                    done: "Device forgotten",
                }),
            },
            Credential::StaticToken => Self {
                mode: "Static token over WireGuard".to_owned(),
                pair_label: "Replace token",
                forget: Some(ForgetView {
                    label: "Forget token",
                    question: "Forget the token?",
                    detail: "This phone stops seeing your agents until you paste it again.",
                    done: "Token forgotten",
                }),
            },
        }
    }
}

/// Whether the forget confirmation is open. A change of what would be
/// forgotten (forgotten elsewhere, re-paired, token pasted) closes it, so
/// it never opens unasked; the device's name arriving with a sign-in does
/// not.
fn forget_confirmation(forget: Memo<Option<ForgetView>>) -> RwSignal<bool> {
    let open = RwSignal::new(false);
    Effect::watch(
        move || forget.track(),
        move |(), _, _| open.set(false),
        false,
    );
    open
}

/// The Bridge group: how this device signs in, the connection, pairing
/// and forgetting the credential.
#[component]
fn BridgeGroup(snapshot: ReadSignal<Snapshot>) -> impl IntoView {
    let secure = session::secure_context();
    let auth = Memo::new(move |_| snapshot.with(|s| AuthView::of(&s.credential, secure)));
    let forget_view = Memo::new(move |_| auth.with(|auth| auth.forget));
    let confirm_forget = forget_confirmation(forget_view);
    let forget = move |_| {
        confirm_forget.set(false);
        let done = forget_view
            .get_untracked()
            .map_or("Forgotten", |forget| forget.done);
        spawn_local(async move {
            match mount::ask_outcome(Ask::Forget).await {
                Ok(()) => mount::toast(done),
                Err(message) => mount::toast(&message),
            }
        });
    };
    let conn = move || snapshot.with(|s| s.paired.then_some(s.conn));
    let connection = move || match conn() {
        None | Some(Conn::Off) => "Not connected",
        Some(Conn::Live) => "Connected",
        Some(Conn::Connecting) => "Connecting…",
        Some(Conn::Retrying) => "Reconnecting…",
    };
    view! {
        <h2 class="group-title">"Bridge"</h2>
        <div class="group">
            <div class="row">
                <span class="row-icon"><Icon paths=icons::KEY /></span>
                <span class="row-label">{move || auth.with(|auth| auth.mode.clone())}</span>
                <span class="row-value" class:row-value-ok=move || conn() == Some(Conn::Live)>{connection}</span>
            </div>
            <a class="row row-link" href=AppRoute::Pair.href()>
                <span class="row-label">{move || auth.with(|auth| auth.pair_label)}</span>
                <span class="row-chevron"><Icon paths=icons::CHEVRON /></span>
            </a>
            {move || forget_view.get().map(|forget| view! {
                <button type="button" class="row row-button row-danger" on:click=move |_| confirm_forget.set(true)>
                    <span class="row-label">{forget.label}</span>
                </button>
            })}
        </div>
        {move || forget_view.get().filter(|_| confirm_forget.get()).map(|confirm| view! {
            <div class="confirm" role="alertdialog" aria-label=confirm.question>
                <p class="confirm-text">{confirm.question}" "{confirm.detail}</p>
                <div class="confirm-actions">
                    <button type="button" class="button button-secondary" on:click=move |_| confirm_forget.set(false)>"Cancel"</button>
                    <button type="button" class="button button-danger" on:click=forget>"Forget"</button>
                </div>
            </div>
        })}
    }
}

/// The notifications switch (push wake pings, see [`crate::push`]), with
/// a one-line note under it. The page talks to the push module; the
/// switch's state machine ([`PushState`]) runs here.
#[component]
fn NotificationsGroup(snapshot: ReadSignal<Snapshot>) -> impl IntoView {
    let push = PUSH.with(Clone::clone);
    let shown = RwSignal::from(push.clone()).read_only();
    // Checked once the stored credentials have loaded (after a reload
    // straight into Settings the device is still signing in), and again
    // whenever the device gains or loses a token; renewals don't count.
    let loaded = Memo::new(move |_| snapshot.with(|s| s.token_loaded));
    let paired = Memo::new(move |_| snapshot.with(|s| s.paired));
    {
        let push = push.clone();
        Effect::new(move |_| {
            if !loaded.get() {
                return;
            }
            paired.track();
            if matches!(
                push.get_untracked(),
                PushState::Enabling | PushState::Disabling
            ) {
                return;
            }
            push.set(PushState::Checking);
            // Only the newest check's answer counts: an older one may have
            // used a token that has since changed.
            let mine = PROBES.get().wrapping_add(1);
            PROBES.set(mine);
            let push = push.clone();
            spawn_local(async move {
                let probed =
                    mount::ask_later(Ask::PushProbe, &JsValue::UNDEFINED, &JsValue::UNDEFINED)
                        .await
                        .ok()
                        .and_then(|state| state.as_string())
                        .and_then(|state| serde_json::from_str(&state).ok())
                        .unwrap_or(PushState::Unknown);
                if PROBES.get() == mine {
                    push.set(probed);
                }
            });
        });
    }
    let toggle = move |_| {
        let Some((next, command)) = push.get_untracked().toggle() else {
            return;
        };
        // Synchronously, inside the tap's handler: the permission prompt
        // needs the user's gesture.
        let prompt = (command == Command::Enable).then(PermissionPrompt::start);
        push.set(next);
        let push = push.clone();
        spawn_local(async move {
            let result = match prompt {
                Some(prompt) => match prompt.refused().await {
                    Ok(Some(refused)) => Ok(refused),
                    Ok(None) => mount::ask_outcome(Ask::PushEnable)
                        .await
                        .map(|()| Outcome::Enabled),
                    Err(error) => Err(error.to_string()),
                },
                None => mount::ask_outcome(Ask::PushDisable)
                    .await
                    .map(|()| Outcome::Disabled),
            };
            let outcome = result.unwrap_or_else(|message| {
                mount::toast(&message);
                Outcome::Failed
            });
            // The switch itself shows success; only failures need words.
            push.update(|s| *s = s.finish(outcome));
        });
    };
    view! {
        <h2 class="group-title">"Notifications"</h2>
        <div class="group">
            <label class="row row-switch">
                <span class="row-label">"Blocked and done agents"</span>
                <input
                    type="checkbox"
                    role="switch"
                    class="switch"
                    prop:checked=move || shown.get().is_checked(paired.get())
                    disabled=move || !shown.get().can_toggle(paired.get())
                    on:change=toggle
                />
            </label>
        </div>
        <p class="group-note">{move || shown.get().note(paired.get())}</p>
    }
}

/// The switch for `pref`, in a group.
#[component]
fn PrefSwitch(snapshot: ReadSignal<Snapshot>, pref: Pref) -> impl IntoView {
    let toggle = move |ev: leptos::ev::Event| {
        let on = event_target_checked(&ev);
        mount::ask(Ask::SetPref, &pref.name().into(), &on.into());
    };
    view! {
        <label class="row row-switch">
            <span class="row-label">{pref.label()}</span>
            <input
                type="checkbox"
                role="switch"
                class="switch"
                prop:checked=move || snapshot.with(|s| s.pref(pref).unwrap_or(false))
                on:change=toggle
            />
        </label>
    }
}

/// Offline storage, and clearing the read cache.
#[component]
fn OfflineGroup(snapshot: ReadSignal<Snapshot>) -> impl IntoView {
    let clearing = RwSignal::new(false);
    let clear_snapshots = move |_| {
        clearing.set(true);
        spawn_local(async move {
            let _ = mount::ask_later(
                Ask::ClearSnapshots,
                &JsValue::UNDEFINED,
                &JsValue::UNDEFINED,
            )
            .await;
            let _ = clearing.try_set(false);
        });
    };
    let storage = move || match snapshot.with(|s| s.storage) {
        Some(true) => "Persistent",
        Some(false) => "Best effort",
        None => "Checking…",
    };
    view! {
        <h2 class="group-title">"Offline"</h2>
        <div class="group">
            <div class="row">
                <span class="row-label">"Storage"</span>
                <span class="row-value">{storage}</span>
            </div>
            <button type="button" class="row row-button" on:click=clear_snapshots disabled=move || clearing.get()>
                <span class="row-label">"Clear cached snapshots"</span>
            </button>
        </div>
        <p class="group-note">"Unsent replies are kept. Only the read cache is cleared."</p>
    }
}

/// The app's and the bridge's builds, and a note when they differ and
/// reloading can't help.
#[component]
fn AboutGroup(snapshot: ReadSignal<Snapshot>) -> impl IntoView {
    let built = app_built();
    // "built 3h ago" keeps ticking while Settings is open.
    let now = RwSignal::new(format::now());
    if let Ok(ticking) =
        set_interval_with_handle(move || now.set(format::now()), Duration::from_secs(1))
    {
        on_cleanup(move || ticking.clear());
    }
    let freshness = move || snapshot.with(|s| s.freshness);
    let differs = move || matches!(freshness(), Freshness::Behind | Freshness::Stuck);
    view! {
        <h2 class="group-title">"About"</h2>
        <div class="group">
            <div class="row">
                <span class="row-label">"App"</span>
                <span class="row-value version-app">
                    <span class="mono">{APP_SHA}</span>
                    {move || {
                        let ago = built_ago(built, now.get());
                        (!ago.is_empty()).then(|| format!(" · {ago}"))
                    }}
                </span>
            </div>
            <div class="row">
                <span class="row-label">"Bridge"</span>
                <span class="row-value mono version-bridge" class:row-value-warn=differs>
                    {move || snapshot.with(|s| bridge_label(&s.bridge))}
                </span>
            </div>
        </div>
        {move || (freshness() == Freshness::Stuck).then(|| view! {
            <p class="group-note">
                "Reloading didn't pick up the bridge's build: it may be serving an older app. \
                 Rebuild the app on the bridge host (just web-build), then reload."
            </p>
        })}
        <p class="version-footnote mono">{format!("{APP_SHA} · {APP_BUILT}")}</p>
    }
}

#[cfg(test)]
mod tests {
    use protocol::DeviceName;

    use super::*;

    #[test]
    fn a_paired_device_shows_its_name_and_can_be_forgotten() {
        let view = AuthView::of(
            &Credential::Device {
                name: Some(DeviceName::new("Pixel").expect("name")),
            },
            true,
        );
        assert_eq!(view.mode, "Paired as \u{201c}Pixel\u{201d}");
        assert_eq!(view.pair_label, "Pair again");
        let forget = view.forget.expect("forgettable");
        assert_eq!(forget.label, "Forget this device");
        assert!(forget.detail.contains("passporter devices revoke"));
        assert!(!format!("{view:?}").to_lowercase().contains("token"));
    }

    #[test]
    fn a_device_before_its_first_sign_in_is_still_a_paired_device() {
        let view = AuthView::of(&Credential::Device { name: None }, true);
        assert_eq!(view.mode, "Paired device");
        assert_eq!(
            view.forget.map(|forget| forget.label),
            Some("Forget this device")
        );
    }

    #[test]
    fn the_static_token_says_so() {
        let view = AuthView::of(&Credential::StaticToken, false);
        assert_eq!(view.mode, "Static token over WireGuard");
        assert_eq!(view.pair_label, "Replace token");
        assert_eq!(view.forget.map(|forget| forget.label), Some("Forget token"));
    }

    #[test]
    fn nothing_to_forget_without_a_credential() {
        let view = AuthView::of(&Credential::None, true);
        assert_eq!(view.mode, "Not paired");
        assert_eq!(view.pair_label, "Pair this device");
        assert_eq!(view.forget, None);
        // Over plain HTTP the Pair screen only takes a pasted token.
        let plain = AuthView::of(&Credential::None, false);
        assert_eq!(plain.pair_label, "Paste your token");
    }
}
