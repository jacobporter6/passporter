//! The phone against agents as herdr reports them on a busy day
//! ([`World::reality`]): unnamed agents known by their terminal title,
//! agents with no session id, the swarm's quiet workspace, a Codex agent.
//! Each test reproduces a way a fake that was tidier than reality once
//! let a bug reach the phone; each runs on its own bridge and browser.

use e2e::app::{
    RECORDING, Rig, RigSpec, card_titles, click, click_text, comes_true, connect_secure, expect,
    fits_narrow, js, open_card, prompt_reaches_bridge, prompts_executed, segment_lists,
    segment_shows, set_visible, tap_mic, type_draft, until, visible,
};
use e2e::herdr::{Keyed, Status};
use e2e::{
    BLOCKED, BLOCKED_PANE, CODEX, DICTATED, DONE, DONE_PANE, DONE_REPLY, NO_SESSION,
    NO_SESSION_REPLY, STATUS_LINE, SWARM, USER_DONE, WORKING, WORKING_PANE,
};
use serde_json::{Value, json};

/// The dialog the blocked agent stops at after its first is answered.
const NEXT_DIALOG: &str = "\
⏺ Bash(cargo build)

────────────────────────────────────────────────────────────────────────────────
 Bash command

   cargo build
   Build the workspace

 Do you want to proceed?
 ❯ 1. Yes
   2. Yes, and don't ask again for cargo build commands in
      /home/dev/git/passporter
   3. No, and tell Claude what to do differently (esc)
";

/// Starts a rig for `spec`; `None` (skipped) without Chromium.
async fn rig(spec: RigSpec) -> Option<Rig> {
    Rig::start(spec).await
}

/// Agents herdr knows only by their terminal title, with no session id,
/// are listed under that title and have a thread (matched by folder); the
/// swarm's finished agent is listed too. The working agents' cards carry
/// the summarizer's status line, and a status change in herdr moves a card
/// to its new segment while the list is open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Chromium and a web build: run `just e2e`"]
async fn agents_as_herdr_reports_them() {
    let Some(rig) = rig(RigSpec {
        reality: true,
        ..RigSpec::default()
    })
    .await
    else {
        return;
    };
    let page = rig.page().await;
    let bridge = &rig.bridge;
    connect_secure(&page, &rig).await;

    segment_lists(&page, bridge, "done", SWARM).await;
    let mut done = card_titles(&page).await;
    done.sort();
    let mut want = [DONE, SWARM, USER_DONE].map(str::to_owned);
    want.sort();
    assert_eq!(done, want, "every finished agent, the swarm's too");

    segment_lists(&page, bridge, "working", NO_SESSION).await;
    fits_narrow(&page, bridge, "triage on a busy day").await;
    // A tap on the status-line bar asks for a line now.
    open_card(&page, bridge, WORKING).await;
    expect(
        &page,
        bridge,
        "!!document.querySelector('.status-line-bar')",
        "a working agent's screen has a status-line bar",
    )
    .await;
    click(&page, ".status-line-bar").await;
    expect(
        &page,
        bridge,
        &format!(
            "document.querySelector('.status-line-bar .status-line-text')?.textContent === {}",
            js(STATUS_LINE)
        ),
        "the tapped status line",
    )
    .await;
    page.eval("history.back()").await.expect("back");
    segment_lists(&page, bridge, "working", WORKING).await;
    expect(
        &page,
        bridge,
        &format!(
            "[...document.querySelectorAll('.agent-card')].some(c => \
               c.querySelector('.agent-title')?.textContent === {} \
               && c.querySelector('.status-line-text')?.textContent === {})",
            js(WORKING),
            js(STATUS_LINE)
        ),
        "the working agent's card carries its status line",
    )
    .await;

    // Live: herdr says the working agent finished; its card moves.
    rig.herdr.set_status(WORKING_PANE, Status::Done);
    expect(
        &page,
        bridge,
        &format!(
            "![...document.querySelectorAll('.agent-card .agent-title')].some(e => e.textContent === {})",
            js(WORKING)
        ),
        "the finished agent leaves the working segment without a reload",
    )
    .await;
    segment_lists(&page, bridge, "done", WORKING).await;

    // No session id, no name: its thread is the newest transcript in its
    // folder.
    segment_lists(&page, bridge, "working", NO_SESSION).await;
    open_card(&page, bridge, NO_SESSION).await;
    expect(
        &page,
        bridge,
        &format!(
            "!!document.querySelector('.thread.thread-inferred') \
             && [...document.querySelectorAll('.thread .msg-user')].some(e => e.textContent.includes('Wire the fan controller.')) \
             && [...document.querySelectorAll('.thread .msg-assistant')].some(e => e.textContent.includes({}))",
            js(NO_SESSION_REPLY)
        ),
        "a session-less, unnamed agent's thread, matched by folder",
    )
    .await;
    rig.assert_no_problems();
}

/// A Codex agent at its approval dialog: its thread comes from its
/// rollout, its dialog's options are chips with Codex's risk rules (approve
/// once is low and tappable; "don't ask again" is elevated and locked to
/// the desktop), and a tap on the low one sends its key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Chromium and a web build: run `just e2e`"]
async fn a_codex_agent() {
    let Some(rig) = rig(RigSpec {
        reality: true,
        ..RigSpec::default()
    })
    .await
    else {
        return;
    };
    let page = rig.page().await;
    let bridge = &rig.bridge;
    connect_secure(&page, &rig).await;
    segment_lists(&page, bridge, "blocked", CODEX).await;
    open_card(&page, bridge, CODEX).await;
    // A blocked agent opens on its screen; the thread is a tab away.
    click_text(&page, ".panel-tab", "Thread").await;
    expect(
        &page,
        bridge,
        "[...document.querySelectorAll('.thread .msg-user')].some(e => e.textContent.includes('Create a file hello.txt')) \
         && [...document.querySelectorAll('.chips .chip:not(.chip-elevated)')].some(c => c.textContent.trim() === 'Yes, proceed' && !c.disabled) \
         && [...document.querySelectorAll('.chips .chip-elevated')].some(c => c.textContent.includes(\"don't ask again\"))",
        "the Codex agent's thread and its approval chips",
    )
    .await;
    fits_narrow(&page, bridge, "Codex agent").await;
    click_text(&page, ".chips .chip:not(.chip-elevated)", "Yes, proceed").await;
    until(bridge, "the approval's key reaches herdr", || {
        rig.herdr.keyed().iter().any(|k| k.pane == e2e::CODEX_PANE)
    })
    .await;
    let keyed: Vec<Keyed> = rig.herdr.keyed();
    assert_eq!(keyed.len(), 1, "one answer: {keyed:?}");
    assert_eq!(keyed[0].keys, ["1"], "approve once is the first option");
    rig.assert_no_problems();
}

/// Option chips on Claude's permission dialog: the elevated one is locked
/// (a tap explains why) and sends nothing; "Yes" sends its key. Then the
/// keypad, for what the chips can't say: on the screen view, a digit goes
/// to the agent as a key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Chromium and a web build: run `just e2e`"]
async fn chips_and_keypad_answer_a_dialog() {
    let Some(rig) = rig(RigSpec::default()).await else {
        return;
    };
    let page = rig.page().await;
    let bridge = &rig.bridge;
    connect_secure(&page, &rig).await;
    segment_shows(&page, bridge, "blocked", BLOCKED).await;
    open_card(&page, bridge, BLOCKED).await;
    expect(
        &page,
        bridge,
        "[...document.querySelectorAll('.chips .chip')].some(c => c.textContent.trim() === 'Yes' && !c.disabled) \
         && document.querySelectorAll('.chips .chip-elevated').length === 1",
        "the dialog's chips, one elevated",
    )
    .await;
    click(&page, ".chips .chip-elevated").await;
    expect(
        &page,
        bridge,
        "!!document.querySelector('.chips-why') \
         && document.querySelector('.chips .chip-elevated').getAttribute('aria-disabled') === 'true'",
        "a tap on the elevated chip explains it is for the desktop",
    )
    .await;
    assert!(
        rig.herdr.keyed().is_empty(),
        "the elevated chip sent nothing"
    );

    click_text(&page, ".chips .chip", "Yes").await;
    until(bridge, "the Yes key reaches herdr", || {
        !rig.herdr.keyed().is_empty()
    })
    .await;
    assert_eq!(
        rig.herdr.keyed(),
        [Keyed {
            pane: BLOCKED_PANE.to_owned(),
            keys: vec!["1".to_owned()],
        }]
    );

    // Claude takes the answer, runs, and stops at the next dialog.
    rig.herdr.set_status(BLOCKED_PANE, Status::Working);
    rig.herdr.set_screen(BLOCKED_PANE, NEXT_DIALOG);
    rig.herdr.set_status(BLOCKED_PANE, Status::Blocked);
    expect(
        &page,
        bridge,
        "!!document.querySelector('.choices-prompt') && document.querySelector('.choices-prompt').textContent.includes('cargo build')",
        "the next dialog",
    )
    .await;

    // The keypad, on the screen view.
    click_text(&page, ".choices-foot button", "Use keys").await;
    expect(
        &page,
        bridge,
        "!!document.querySelector('.keypad')",
        "the keypad",
    )
    .await;
    if page
        .eval("[...document.querySelectorAll('.keypad-hint button')].some(b => b.textContent === 'Show screen')")
        .await
        .expect("evaluates")
        == Value::Bool(true)
    {
        click_text(&page, ".keypad-hint button", "Show screen").await;
    }
    expect(
        &page,
        bridge,
        "[...document.querySelectorAll('.keypad .key-digit')].some(k => k.textContent === '3' && !k.disabled)",
        "the keypad's keys are ready on the screen view",
    )
    .await;
    fits_narrow(&page, bridge, "keypad").await;
    // A tap within the double-tap guard after the last answer (400 ms) is
    // swallowed by design, so tap until one lands: never more than one.
    for _ in 0..10 {
        click_text(&page, ".keypad .key-digit", "3").await;
        if comes_true(&page, "new Promise(ok => setTimeout(() => ok(true), 250))").await
            && rig.herdr.keyed().len() > 1
        {
            break;
        }
    }
    until(bridge, "the keypad's key reaches herdr", || {
        rig.herdr.keyed().len() == 2
    })
    .await;
    assert_eq!(rig.herdr.keyed()[1].keys, ["3"]);
    rig.assert_no_problems();
}

/// A prompt sent with no connection waits in the outbox, saying so, and
/// goes once the connection is back. One queued while the agent moves on
/// is not sent silently when the connection returns: the bridge refuses
/// it as stale and the outbox asks, and "send anyway" sends it once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Chromium and a web build: run `just e2e`"]
async fn the_outbox_offline_and_stale() {
    const OFFLINE: &str = "Ship it once you're back online.";
    const STALE: &str = "Also bump the version.";
    let Some(rig) = rig(RigSpec::default()).await else {
        return;
    };
    let page = rig.page().await;
    let bridge = &rig.bridge;
    connect_secure(&page, &rig).await;
    segment_shows(&page, bridge, "done", DONE).await;
    open_card(&page, bridge, DONE).await;
    expect(
        &page,
        bridge,
        "!!document.querySelector('#reply-text')",
        "the composer",
    )
    .await;

    page.eval("actionsNet = 'fail'").await.expect("offline");
    type_draft(&page, OFFLINE).await;
    click(&page, ".send-button").await;
    expect(
        &page,
        bridge,
        &format!(
            "[...document.querySelectorAll('.outbox .outbox-item')].some(i => \
               i.querySelector('.outbox-text')?.textContent === {} && !i.classList.contains('outbox-sent'))",
            js(OFFLINE)
        ),
        "the prompt waits in the outbox",
    )
    .await;
    assert_eq!(prompts_executed(bridge, OFFLINE), 0, "nothing sent offline");
    page.eval("actionsNet = 'pass'").await.expect("online");
    prompt_reaches_bridge(bridge, OFFLINE, "the queued prompt").await;
    expect(
        &page,
        bridge,
        "!document.querySelector('.outbox-queued, .outbox-sending')",
        "the outbox settles",
    )
    .await;
    assert_eq!(prompts_executed(bridge, OFFLINE), 1, "sent once");

    // Offline again; the agent moves on before the prompt can go.
    rig.herdr.set_status(DONE_PANE, Status::Done);
    page.eval("actionsNet = 'fail'").await.expect("offline");
    type_draft(&page, STALE).await;
    click(&page, ".send-button").await;
    expect(
        &page,
        bridge,
        &format!(
            "[...document.querySelectorAll('.outbox .outbox-text')].some(t => t.textContent === {})",
            js(STALE)
        ),
        "the second prompt waits",
    )
    .await;
    rig.herdr.set_status(DONE_PANE, Status::Working);
    rig.herdr.set_status(DONE_PANE, Status::Done);
    page.eval("actionsNet = 'pass'").await.expect("online");
    expect(
        &page,
        bridge,
        "[...document.querySelectorAll('.outbox-card button')].some(b => b.textContent.startsWith('Send anyway'))",
        "the stale prompt becomes a decision",
    )
    .await;
    assert_eq!(prompts_executed(bridge, STALE), 0, "never sent silently");
    fits_narrow(&page, bridge, "stale decision").await;
    page.eval(
        "[...document.querySelectorAll('.outbox-card button')].find(b => b.textContent.startsWith('Send anyway')).click()",
    )
    .await
    .expect("sends anyway");
    prompt_reaches_bridge(bridge, STALE, "the prompt sent anyway").await;
    expect(
        &page,
        bridge,
        "!document.querySelector('.outbox-card, .outbox-queued, .outbox-sending')",
        "the decision is gone",
    )
    .await;
    assert_eq!(prompts_executed(bridge, STALE), 1, "sent once");
    // The bridge's 409 for the stale prompt is logged by the browser.
    rig.assert_no_problems_but(&["status of 409 (Conflict)"]);
}

/// Settings switches: "Show full transcripts by default" opens every turn
/// and "Review dictation before sending" brings back the transcript card;
/// both survive a reload. With review on, the dock mic records, and the
/// card's Use puts the words in the composer instead of sending them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Chromium and a web build: run `just e2e`"]
async fn settings_and_dictation_review() {
    let Some(rig) = rig(RigSpec::default()).await else {
        return;
    };
    let page = rig.page().await;
    let bridge = &rig.bridge;
    connect_secure(&page, &rig).await;
    click(&page, ".dock a[href='/settings']").await;
    let switch = |label: &str| {
        format!(
            "[...document.querySelectorAll('.row-switch')].find(r => r.querySelector('.row-label')?.textContent === {})?.querySelector('input')",
            js(label)
        )
    };
    let review = switch("Review dictation before sending");
    let full = switch("Show full transcripts by default");
    expect(
        &page,
        bridge,
        &format!("{review}?.checked === false && {full}?.checked === false"),
        "both switches, off",
    )
    .await;
    page.eval(&format!("{review}.click(); {full}.click()"))
        .await
        .expect("turns both on");
    expect(
        &page,
        bridge,
        &format!("{review}?.checked === true && {full}?.checked === true"),
        "both switches on",
    )
    .await;
    page.goto(&format!("{}settings", rig.loopback()))
        .await
        .expect("reloads");
    expect(
        &page,
        bridge,
        &format!("{review}?.checked === true && {full}?.checked === true"),
        "both switches stay on after a reload",
    )
    .await;

    click(&page, ".dock-link[href='/']").await;
    segment_shows(&page, bridge, "done", DONE).await;
    open_card(&page, bridge, DONE).await;
    expect(
        &page,
        bridge,
        &visible(".thread .msg-assistant", DONE_REPLY, true),
        "full transcripts: the reply shows without opening the turn",
    )
    .await;

    tap_mic(&page).await;
    expect(
        &page,
        bridge,
        "!!document.querySelector('.dictation-live:not(.dictation-opening)') \
         && !['', '0:00'].includes(document.querySelector('.dictation-live .rec-time')?.textContent ?? '')",
        "recording",
    )
    .await;
    tap_mic(&page).await;
    expect(
        &page,
        bridge,
        &format!(
            "document.querySelector('.dictation-card #dictation-text')?.value === {}",
            js(DICTATED)
        ),
        "review: the transcript card, editable",
    )
    .await;
    assert_eq!(
        prompts_executed(bridge, DICTATED),
        0,
        "review sends nothing"
    );
    fits_narrow(&page, bridge, "transcript card").await;
    click_text(&page, ".dictation-card button", "Use").await;
    expect(
        &page,
        bridge,
        &format!(
            "document.querySelector('#reply-text')?.value.includes({}) && !document.querySelector('.dictation-card')",
            js(DICTATED)
        ),
        "Use puts the words in the composer",
    )
    .await;
    assert_eq!(prompts_executed(bridge, DICTATED), 0, "still nothing sent");
    click(&page, ".send-button").await;
    prompt_reaches_bridge(bridge, DICTATED, "the reviewed dictation").await;
    // Settings asks for the push key; this bridge has no [push] (404).
    rig.assert_no_problems_but(&["404 (Not Found) http://127.0.0.1"]);
}

/// The note on the transcript card of a take the screen turning off cut
/// short.
const SCREEN_OFF_NOTE: &str = "Recording stopped when the screen turned off or you switched apps.";

/// A long dictation keeps the phone's screen on (a user's report: the
/// screen timed out mid-sentence and cut the dictation short). The take
/// holds a screen wake lock from the tap until its transcript is sent,
/// and lets go after. When the screen goes off anyway (the power button),
/// what was said so far is kept, uploaded and transcribed, and waits on
/// its card with a note saying why it stopped; while that clip is still
/// on its way, the lock is taken again when the screen comes back on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Chromium and a web build: run `just e2e`"]
async fn a_long_dictation_keeps_the_screen_on() {
    let Some(rig) = rig(RigSpec::default()).await else {
        return;
    };
    let page = rig.page().await;
    let bridge = &rig.bridge;
    connect_secure(&page, &rig).await;
    segment_shows(&page, bridge, "done", DONE).await;
    open_card(&page, bridge, DONE).await;

    tap_mic(&page).await;
    expect(
        &page,
        bridge,
        &format!("{RECORDING} && wake.held === 1 && wake.requests === 1"),
        "the screen is kept on while recording",
    )
    .await;
    tap_mic(&page).await;
    prompt_reaches_bridge(bridge, DICTATED, "the dictated prompt").await;
    expect(
        &page,
        bridge,
        "wake.held === 0 && wake.requests === 1 \
         && document.querySelector('.mic-fab')?.getAttribute('aria-pressed') === 'false'",
        "the screen may sleep once the take is sent",
    )
    .await;

    // The power button, mid-take; the clip's upload is slow.
    page.eval("audioNet = 'hold'").await.expect("holds uploads");
    tap_mic(&page).await;
    expect(
        &page,
        bridge,
        &format!("{RECORDING} && wake.held === 1 && wake.requests === 2"),
        "the next take keeps the screen on too",
    )
    .await;
    set_visible(&page, false).await;
    expect(
        &page,
        bridge,
        "wake.held === 0 && audioHeld.length > 0 && !document.querySelector('.dictation-live')",
        "screen off: the lock is gone, the recording stopped and its upload started",
    )
    .await;
    set_visible(&page, true).await;
    expect(
        &page,
        bridge,
        "wake.held === 1 && wake.requests === 3 && !document.querySelector('.dictation-card')",
        "screen on again: the lock is taken again while the clip is on its way",
    )
    .await;
    page.eval("releaseAudio()").await.expect("sends uploads on");
    expect(
        &page,
        bridge,
        &format!(
            "document.querySelector('.dictation-card #dictation-text')?.value === {} \
             && document.querySelector('.dictation-card')?.textContent.includes({}) \
             && wake.held === 0",
            js(DICTATED),
            js(SCREEN_OFF_NOTE)
        ),
        "what was said before the screen went off waits on its card, with why it stopped",
    )
    .await;
    assert_eq!(rig.whisper.served(), 2, "both takes were transcribed");
    assert_eq!(
        prompts_executed(bridge, DICTATED),
        1,
        "the cut-short take waits for review"
    );
    rig.assert_no_problems();
}

/// A bridge deployed while the app runs reports another build: the app
/// offers a reload.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Chromium and a web build: run `just e2e`"]
async fn a_newer_bridge_offers_a_reload() {
    let Some(rig) = rig(RigSpec::default()).await else {
        return;
    };
    let page = rig.page().await;
    let bridge = &rig.bridge;
    connect_secure(&page, &rig).await;
    assert_eq!(
        page.eval("!!document.querySelector('.update-prompt')")
            .await
            .expect("evaluates"),
        Value::Bool(false),
        "no banner while the builds match"
    );
    // The bridge restarts on a new build: the stream drops and comes back.
    page.eval("bridgeBuild = 'fedcba9'; sockets.forEach(s => s.close())")
        .await
        .expect("a new bridge build");
    expect(
        &page,
        bridge,
        "!!document.querySelector('.update-prompt button')",
        "the update banner offers a reload",
    )
    .await;
    fits_narrow(&page, bridge, "update banner").await;
    rig.assert_no_problems();
}

/// What the service worker shows for a push: the agents that need the
/// user, by name, and never the swarm's finished agents (their turns are
/// not news). Delivered to the real service worker, which runs the real
/// push wake (`notice.js`) against the bridge.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Chromium and a web build: run `just e2e`"]
async fn the_notification_names_only_the_users_agents() {
    let Some(rig) = rig(RigSpec {
        reality: true,
        push: true,
    })
    .await
    else {
        return;
    };
    let page = rig.page().await;
    let bridge = &rig.bridge;
    connect_secure(&page, &rig).await;
    let push = Pushes::ready(&page, &rig).await;
    push.deliver(&page).await;
    expect(
        &page,
        bridge,
        "navigator.serviceWorker.ready.then(r => r.getNotifications()).then(n => n.length > 0)",
        "the push shows a notification",
    )
    .await;
    let shown = page
        .eval(
            "navigator.serviceWorker.ready.then(r => r.getNotifications())\
               .then(n => n.map(x => x.title + '\\n' + x.body))",
        )
        .await
        .expect("evaluates");
    let shown: Vec<String> = serde_json::from_value(shown).expect("strings");
    let [text] = shown.as_slice() else {
        panic!("one notification: {shown:?}");
    };
    for named in [BLOCKED, CODEX, DONE, USER_DONE] {
        assert!(
            text.contains(named),
            "the notification names {named}: {text:?}"
        );
    }
    assert!(
        !text.contains(SWARM),
        "the swarm's finished agent is not news: {text:?}"
    );
    assert!(
        !text.contains("news. Open"),
        "not the generic text: {text:?}"
    );
}

/// A user-reported bug: "all caught up, no agent needs you" notifications.
/// The bridge pinged when an agent became blocked, but the user answered it
/// (and read the finished one at the desk) before the push woke the phone.
/// The ping's reason is gone, so the push must not alert: the browser
/// requires a notification per push, so it shows a silent one that closes
/// itself. And when an alert is already showing, a push with nothing new
/// leaves it as it is, without alerting again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Chromium and a web build: run `just e2e`"]
async fn a_push_whose_reason_is_gone_does_not_alert() {
    let Some(rig) = rig(RigSpec {
        push: true,
        ..RigSpec::default()
    })
    .await
    else {
        return;
    };
    let page = rig.page().await;
    let bridge = &rig.bridge;
    connect_secure(&page, &rig).await;
    let push = Pushes::ready(&page, &rig).await;

    // The finished turn is read at the desk (herdr: done → idle), and the
    // blocked agent is answered from the phone and gets back to work.
    rig.herdr.set_status(DONE_PANE, Status::Idle);
    segment_shows(&page, bridge, "blocked", BLOCKED).await;
    open_card(&page, bridge, BLOCKED).await;
    expect(
        &page,
        bridge,
        "[...document.querySelectorAll('.chips .chip')].some(c => c.textContent.trim() === 'Yes' && !c.disabled)",
        "the dialog's chips",
    )
    .await;
    click_text(&page, ".chips .chip", "Yes").await;
    until(bridge, "the Yes key reaches herdr", || {
        !rig.herdr.keyed().is_empty()
    })
    .await;
    rig.herdr.set_status(BLOCKED_PANE, Status::Working);
    page.eval("history.back()").await.expect("back");
    segment_lists(&page, bridge, "working", BLOCKED).await;

    // The ping the block caused arrives now.
    push.deliver(&page).await;
    expect(
        &page,
        bridge,
        &notifications(""),
        "the push shows a notification",
    )
    .await;
    let showing = shown(&page).await;
    let [quiet] = showing.as_slice() else {
        panic!("one notification: {showing:?}");
    };
    assert_eq!(quiet["silent"], true, "no sound or buzz: {quiet}");
    assert_eq!(quiet["renotify"], false, "{quiet}");
    let text = quiet["text"].as_str().unwrap_or_default();
    assert!(!text.contains("caught up"), "{text:?}");
    assert!(!text.contains("needs you"), "{text:?}");
    expect(
        &page,
        bridge,
        &format!("{NOTIFICATIONS}.then(n => n.length === 0)"),
        "the silent notification closes itself",
    )
    .await;

    // A real block alerts; a later push with nothing new leaves that alert
    // as it is, silently.
    rig.herdr.set_status(WORKING_PANE, Status::Blocked);
    segment_lists(&page, bridge, "blocked", WORKING).await;
    push.deliver(&page).await;
    expect(
        &page,
        bridge,
        &notifications(&format!("{WORKING} is blocked")),
        "the block alerts",
    )
    .await;
    let alert = shown(&page).await;
    assert_eq!(alert.len(), 1, "{alert:?}");
    assert_ne!(alert[0]["silent"], true, "a real alert: {alert:?}");
    rig.herdr.set_status(WORKING_PANE, Status::Working);
    segment_lists(&page, bridge, "working", WORKING).await;
    push.deliver(&page).await;
    expect(
        &page,
        bridge,
        &format!(
            "{NOTIFICATIONS}.then(n => n.length === 1 && n[0].silent && n[0].title === {})",
            js(&format!("{WORKING} is blocked"))
        ),
        "the alert stays as it was, updated silently",
    )
    .await;
    rig.assert_no_problems();
}

/// The service worker's notifications, as a promise.
const NOTIFICATIONS: &str = "navigator.serviceWorker.ready.then(r => r.getNotifications())";

/// An expression true once a notification whose title and body contain
/// `text` is showing.
fn notifications(text: &str) -> String {
    format!(
        "{NOTIFICATIONS}.then(n => n.some(x => (x.title + '\\n' + x.body).includes({})))",
        js(text)
    )
}

/// The notifications showing: their text and how they alert.
async fn shown(page: &e2e::cdp::Page<'_>) -> Vec<Value> {
    let shown = page
        .eval(&format!(
            "{NOTIFICATIONS}.then(n => n.map(x => ({{ \
               text: x.title + '\\n' + x.body, silent: x.silent, renotify: x.renotify }})))"
        ))
        .await
        .expect("evaluates");
    serde_json::from_value(shown).expect("notifications")
}

/// Push messages to the app's real service worker, as the push service
/// delivers them (`ServiceWorker.deliverPushMessage`).
struct Pushes {
    origin: String,
    registration: String,
}

impl Pushes {
    /// Grants notifications, waits for the service worker and finds its
    /// registration.
    async fn ready(page: &e2e::cdp::Page<'_>, rig: &Rig) -> Self {
        let bridge = &rig.bridge;
        let origin = rig.loopback().trim_end_matches('/').to_owned();
        rig.browser
            .browser_call(
                "Browser.grantPermissions",
                json!({"origin": origin, "permissions": ["notifications"]}),
            )
            .await
            .expect("grants notifications");
        page.call("ServiceWorker.enable", json!({}))
            .await
            .expect("watches service workers");
        expect(
            page,
            bridge,
            "navigator.serviceWorker.ready.then(r => !!r.active)",
            "the service worker is active",
        )
        .await;
        let registration = || {
            rig.browser
                .service_worker_events()
                .iter()
                .filter(|e| e["method"] == "ServiceWorker.workerRegistrationUpdated")
                .flat_map(|e| {
                    e["params"]["registrations"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                })
                .find(|r| {
                    r["scopeURL"]
                        .as_str()
                        .is_some_and(|s| s.starts_with(&origin))
                })
                .and_then(|r| r["registrationId"].as_str().map(str::to_owned))
        };
        until(bridge, "the service worker's registration", || {
            registration().is_some()
        })
        .await;
        let registration = registration().expect("registration");
        Self {
            origin,
            registration,
        }
    }

    /// Delivers one wake ping.
    async fn deliver(&self, page: &e2e::cdp::Page<'_>) {
        page.call(
            "ServiceWorker.deliverPushMessage",
            json!({"origin": self.origin, "registrationId": self.registration, "data": "wake"}),
        )
        .await
        .expect("delivers a push");
    }
}
