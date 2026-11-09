//! The outbox under the reply bar: one row per message with its status
//! icon (queued, sending, sent), and a card for anything that needs the
//! user: a stale result ("the agent moved on"), a refusal, a failure.

use leptos::prelude::*;
use protocol::Seq;

use super::machine::{Delivery, Entry};
use super::service::use_outbox;
use crate::agent::reply::{Draft, refusal_message};
use crate::store::OutboxKey;
use crate::ui::icons::{self, Icon, Paths};
use crate::ui::{AppState, use_app};

/// Queued: a clock.
const QUEUED: Paths = &["M21 12a9 9 0 1 1-18 0 9 9 0 0 1 18 0z", "M12 7v5l3 2"];
/// Sending: an arrow on its way.
const SENDING: Paths = &["M12 19V5", "M6 11l6-6 6 6"];

/// One agent's outbox, oldest first.
#[component]
pub fn OutboxTray(
    /// The agent's entries, oldest first.
    entries: Memo<Vec<(OutboxKey, Entry)>>,
    /// The newest known seq: "send anyway" uses it when a 409 came without
    /// the agent's state.
    seq: Signal<Seq>,
    /// Puts a stale prompt's text back in the composer.
    on_edit: Callback<String>,
) -> impl IntoView {
    // `entries` leaves retry times out (see `OutboxService::entries`), so
    // rows are rebuilt only when what the tray shows changes: rows aren't
    // re-announced and buttons stay put. A plain list: a keyed `<For>`
    // costs far more WASM for the handful of rows there are.
    move || {
        let rows = entries.get();
        (!rows.is_empty()).then(|| {
            view! {
                <ul class="outbox" aria-label="Outbox">
                    {rows
                        .into_iter()
                        .map(|(key, entry)| item(&key, entry, seq, on_edit))
                        .collect_view()}
                </ul>
            }
        })
    }
}

/// One entry's row or card.
fn item(key: &OutboxKey, entry: Entry, seq: Signal<Seq>, on_edit: Callback<String>) -> AnyView {
    let draft = Draft::from_action(&entry.action, entry.label.as_deref());
    match entry.delivery {
        Delivery::Queued { attempts, .. } => queued_row(key.clone(), summary(&draft), attempts),
        Delivery::Sending { .. } => row(
            "outbox-item outbox-sending",
            SENDING,
            summary(&draft),
            RowState::Fixed("Sending…"),
            None,
        ),
        Delivery::Sent => row(
            "outbox-item outbox-sent",
            icons::CHECK,
            summary(&draft),
            RowState::Fixed("Sent"),
            None,
        ),
        Delivery::Stale { .. } => stale_card(key, &draft, seq, on_edit),
        Delivery::Refused { reason, message } => {
            let text = refusal_message(&draft, reason, &message);
            let title = format!("{} not sent", draft.describe());
            card(key, title, text, vec![dismiss(key, "Dismiss")])
        }
        Delivery::Failed { message } => card(
            key,
            draft.describe(),
            message,
            vec![dismiss(key, "Dismiss")],
        ),
    }
}

/// The row's text: a prompt's first line, else what the decision was.
fn summary(draft: &Draft) -> String {
    match draft {
        Draft::Prompt(text) => text.as_str().lines().next().unwrap_or_default().to_owned(),
        _ => draft.describe(),
    }
}

/// A queued row's status: why it isn't going, if it can't.
const fn queued_state(online: bool, rejected: bool, paired: bool, attempts: u32) -> &'static str {
    if !online {
        "Waiting for a connection"
    } else if rejected {
        "Not sent: signed out. Connect again to send it"
    } else if !paired {
        "Waiting to sign in"
    } else if attempts > 0 {
        "Retrying"
    } else {
        "Queued"
    }
}

/// What a status row says.
#[derive(Debug, Clone, Copy)]
enum RowState {
    Fixed(&'static str),
    /// Queued: why it isn't going, if it can't ([`queued_state`]).
    Queued {
        attempts: u32,
    },
}

impl RowState {
    /// The text now (reactive for a queued row).
    fn text(self, app: AppState) -> &'static str {
        match self {
            Self::Fixed(text) => text,
            Self::Queued { attempts } => queued_state(
                app.online.get(),
                app.token_rejected.get(),
                app.paired.get(),
                attempts,
            ),
        }
    }
}

/// A cancel button on a status row: the entry, its label and its
/// accessible name.
type Cancel = (OutboxKey, &'static str, &'static str);

/// One status row (every state that isn't a card shares it).
fn row(
    class: &'static str,
    icon: Paths,
    summary: String,
    state: RowState,
    cancel: Option<Cancel>,
) -> AnyView {
    let app = use_app();
    let outbox = use_outbox();
    let button = cancel.map(|(key, label, aria)| {
        // A request that timed out may still be out and may still arrive:
        // cancelling is only offered once it is back.
        move || {
            (!outbox.in_flight(&key)).then(|| {
                let key = key.clone();
                view! {
                <button
                    type="button"
                    class="outbox-cancel"
                    aria-label=aria
                    title=aria
                    on:click=move |_| {
                        outbox.discard(&key);
                    }
                >
                    {label}
                </button>
                }
            })
        }
    });
    view! {
        <li class=class>
            <Icon paths=icon />
            <span class="outbox-text">{summary}</span>
            <span class="outbox-state" role="status">{move || state.text(app)}</span>
            {button}
        </li>
    }
    .into_any()
}

fn queued_row(key: OutboxKey, summary: String, attempts: u32) -> AnyView {
    // Before the first attempt it is a true cancel. After one, an answer
    // may have been lost on the way back, so it only stops the retries, and
    // says so.
    let (label, aria) = if attempts > 0 {
        (
            "Stop retrying",
            "Stop retrying (it may already have arrived)",
        )
    } else {
        ("Cancel", "Cancel this message")
    };
    row(
        "outbox-item outbox-queued",
        QUEUED,
        summary,
        RowState::Queued { attempts },
        Some((key, label, aria)),
    )
}

/// A button on a card.
struct CardButton {
    label: &'static str,
    primary: bool,
    run: Callback<()>,
}

/// A button that drops the card's entry.
fn dismiss(key: &OutboxKey, label: &'static str) -> CardButton {
    let outbox = use_outbox();
    let key = key.clone();
    CardButton {
        label,
        primary: false,
        run: Callback::new(move |()| {
            outbox.discard(&key);
        }),
    }
}

/// A card for an entry that needs the user. While a "send anyway"
/// replacement for it is being stored, its buttons wait: an Edit or a
/// second send then would duplicate the message.
fn card(key: &OutboxKey, title: String, text: String, buttons: Vec<CardButton>) -> AnyView {
    let outbox = use_outbox();
    let busy = {
        let key = key.clone();
        Signal::derive(move || outbox.superseding(&key))
    };
    view! {
        <li class="outbox-card">
            <div class="outcome-card" role="alert">
                <p class="outcome-title"><Icon paths=icons::ALERT />{title}</p>
                <p class="outcome-text">{text}</p>
                <div class="outcome-actions">
                    {buttons.into_iter().map(|button| {
                        let class = if button.primary {
                            "button button-primary button-small"
                        } else {
                            "button button-secondary button-small"
                        };
                        view! {
                            <button type="button" class=class disabled=busy on:click=move |_| {
                                if !busy.get_untracked() {
                                    button.run.run(());
                                }
                            }>{button.label}</button>
                        }
                    }).collect_view()}
                </div>
            </div>
        </li>
    }
    .into_any()
}

/// "The agent moved on". A prompt may be sent anyway as a *new* prompt (new
/// key, the agent's current seq), edited, or discarded. Choices, keys and
/// interrupts are never resent: what they answered is gone, so the user
/// looks again.
fn stale_card(
    key: &OutboxKey,
    draft: &Draft,
    seq: Signal<Seq>,
    on_edit: Callback<String>,
) -> AnyView {
    let app = use_app();
    let outbox = use_outbox();
    let described = draft.describe();
    let (title, text) = match draft {
        Draft::Prompt(_) => (
            "The agent moved on",
            "It changed before your message arrived. Nothing was sent.".to_owned(),
        ),
        Draft::Choose { .. } => (
            "The dialog changed — check it again",
            described + " was not sent. Look at what it asks now and choose again.",
        ),
        Draft::Keys(_) => (
            "The dialog changed — check it again",
            described + " was not sent. Check the screen and press again.",
        ),
        Draft::Interrupt => (
            "The agent moved on",
            "The screen changed before your interrupt arrived. Nothing was sent; check the screen and press again."
                .to_owned(),
        ),
    };
    let buttons = match draft.editable_text() {
        Some(body) => {
            let (anyway, edited) = (key.clone(), key.clone());
            vec![
                CardButton {
                    label: "Send anyway as a new prompt",
                    primary: true,
                    run: Callback::new(move |()| {
                        if let Err(error) = outbox.send_anyway(&anyway, seq.get_untracked()) {
                            app.toast(error.to_string());
                        }
                    }),
                },
                CardButton {
                    label: "Edit",
                    primary: false,
                    // Only once it is gone, so a tap that can't take it (or
                    // a second tap) never pastes the text twice.
                    run: Callback::new(move |()| {
                        if outbox.discard(&edited) {
                            on_edit.run(body.clone());
                        }
                    }),
                },
                dismiss(key, "Discard"),
            ]
        }
        None => vec![dismiss(key, "OK")],
    };
    card(key, title.to_owned(), text, buttons)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_queued_row_says_why_it_is_not_going() {
        assert_eq!(
            queued_state(false, true, false, 1),
            "Waiting for a connection"
        );
        assert!(queued_state(true, true, false, 0).contains("Connect again"));
        assert_eq!(queued_state(true, false, false, 0), "Waiting to sign in");
        assert_eq!(queued_state(true, false, true, 2), "Retrying");
        assert_eq!(queued_state(true, false, true, 0), "Queued");
    }
}
