//! "Paste your token": the MVP's way to connect this phone to the bridge.
//!
//! The token is checked against the bridge (`GET /v1/agents`) before it is
//! stored, so a typo is caught here rather than as a silent reconnect loop.
//! Part of the pair WASM, like the rest of the screen ([`super::screen`]):
//! the page stores the token.

use leptos::ev::SubmitEvent;
use leptos::prelude::*;
use leptos::task::spawn_local;

use super::screen::{self, ConnectedCard};
use crate::api::{
    ApiClient, ApiError, BearerToken, FetchTransport, MemoryToken, parse_pasted_token,
};
use crate::platform;

/// Checks `token` against the bridge and has the page store it, even if
/// the user left the screen meanwhile. `Ok(false)` when the bridge was
/// unreachable (stored anyway, so the phone connects once it can); `Err`
/// when the bridge said no.
async fn verify_and_save(token: BearerToken) -> Result<bool, String> {
    let host = screen::host();
    let probe = ApiClient::new(FetchTransport, MemoryToken::new(token.clone()));
    let reachable = match probe.agents().await {
        Ok(_) => true,
        Err(ApiError::Unauthorized) => {
            return Err(
                "The bridge rejected this token. Check you copied the latest one.".to_owned(),
            );
        }
        Err(e) if e.is_retryable() => false,
        Err(e) => return Err(e.to_string()),
    };
    screen::save_token(host, token.expose()).await?;
    Ok(reachable)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Check {
    Idle,
    Checking,
    Error(String),
}

/// The token form. `paired` is whether the device holds a credential.
#[component]
pub fn TokenForm(paired: ReadSignal<bool>) -> impl IntoView {
    let input = RwSignal::new(String::new());
    let check = RwSignal::new(Check::Idle);

    let on_submit = move |ev: SubmitEvent| {
        ev.prevent_default();
        if check.get_untracked() == Check::Checking {
            return;
        }
        let Ok(token) = parse_pasted_token(&input.get_untracked()) else {
            check.set(Check::Error(
                "That doesn't look like a token. Copy the whole value printed by setup.sh --insecure-mvp."
                    .to_owned(),
            ));
            return;
        };
        check.set(Check::Checking);
        spawn_local(async move {
            let reachable = match verify_and_save(token).await {
                Ok(reachable) => reachable,
                Err(message) => {
                    let _ = check.try_set(Check::Error(message));
                    return;
                }
            };
            platform::haptic_tick();
            let _ = input.try_set(String::new());
            let _ = check.try_set(Check::Idle);
            screen::toast(if reachable {
                "Connected"
            } else {
                "Saved. The bridge isn't reachable right now"
            });
            screen::done();
        });
    };

    let checking = move || check.get() == Check::Checking;
    let error = move || match check.get() {
        Check::Error(message) => Some(message),
        Check::Idle | Check::Checking => None,
    };

    view! {
        <ConnectedCard paired=paired hint="Paste a new token below to replace it." />
        <form class="token-form" on:submit=on_submit novalidate=true>
            <label class="field-label" for="bridge-token">"Bridge token"</label>
            <input
                id="bridge-token"
                class="field token-field"
                type="password"
                autocomplete="off"
                autocapitalize="off"
                spellcheck="false"
                inputmode="text"
                placeholder="Paste your token"
                aria-invalid=move || error().is_some().to_string()
                aria-describedby="bridge-token-help bridge-token-error"
                prop:value=move || input.get()
                on:input=move |ev| {
                    input.set(event_target_value(&ev));
                    if matches!(check.get_untracked(), Check::Error(_)) {
                        check.set(Check::Idle);
                    }
                }
            />
            <p id="bridge-token-help" class="field-help">
                "Printed at the end of " <code>"deploy/bridge/setup.sh --insecure-mvp"</code> ". It stays on this phone."
            </p>
            <p id="bridge-token-error" class="field-error" role="alert">{error}</p>
            <button type="submit" class="button button-primary button-block" disabled=checking>
                {move || if checking() { "Checking…" } else { "Connect" }}
            </button>
        </form>
    }
}
