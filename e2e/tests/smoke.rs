//! Phone-size journeys through the PWA against the real bridge.
//!
//! Run with `just e2e`, which builds the web app and the bridge first.
//! Without Chromium each test skips with a message. Each test is one
//! browser on its own bridge; within a test the scenarios run in order on
//! the same page, as a person would use the app.
//!
//! - [`phone_smoke`]: the MVP phone over plain HTTP (the token form):
//!   triage, the summary-first thread, Summarise, option chips through
//!   leaving and returning, versions, a new agent and a runbook.
//! - [`phone_secure`]: the same app on a secure origin, where the mic
//!   exists: a suggested reply, dictation through the dock mic, a photo and
//!   terminal mode.

use std::path::{Path, PathBuf};

use e2e::app::{
    RECORDING, RUNBOOK, RUNBOOK_ARGV, Rig, RigSpec, card_titles, click, click_last, connect,
    connect_secure, expect, fits_narrow, js, open_card, prompt_reaches_bridge, prompts_executed,
    segment_shows, set_visible, tap_mic, type_draft, until, visible,
};
use e2e::bridge::Bridge;
use e2e::cdp::Page;
use e2e::herdr::{FakeHerdr, Started, Status};
use e2e::{
    BLOCKED, DICTATED, DONE, DONE_COMMAND, DONE_DECOY, DONE_PANE, DONE_PROMPT, DONE_QUEUED,
    DONE_REPLY, DONE_REPORT, DONE_REPORT_LABEL, DONE_SHELL_LABEL, DONE_TASK_LABEL, HEADLINE,
    INJECTED, SPAWN_PROJECT, SUGGESTED, TLDR_OUTCOME, WORKING, World,
};

/// Triage shows each agent in its segment; blocked is selected first.
async fn triage(page: &Page<'_>, bridge: &Bridge) {
    expect(
        page,
        bridge,
        &format!(
            "location.pathname === '/' && document.querySelector('#tab-blocked')?.getAttribute('aria-selected') === 'true' \
             && [...document.querySelectorAll('.agent-card .agent-title')].some(e => e.textContent === {})",
            js(BLOCKED)
        ),
        "connected triage on the blocked segment",
    )
    .await;
    assert_eq!(card_titles(page).await, [BLOCKED], "blocked segment");
    // Its card says what it asks: the command, parsed off the screen.
    expect(
        page,
        bridge,
        &format!(
            "document.querySelector('.agent-card-blocked .agent-ask')?.textContent === {} \
             && document.querySelector('.agent-card-blocked').getAttribute('aria-label').includes({0})",
            js("Run `cargo nextest run -p bridge`?")
        ),
        "the blocked card shows its ask",
    )
    .await;
    fits_narrow(page, bridge, "triage").await;
    segment_shows(page, bridge, "working", WORKING).await;
    segment_shows(page, bridge, "done", DONE).await;
}

/// Opens the working agent, which herdr names no session for (hooks off):
/// its thread is the transcript matched by its folder, and says so. Back
/// on triage's done segment after.
async fn folder_matched_thread(page: &Page<'_>, bridge: &Bridge) {
    segment_shows(page, bridge, "working", WORKING).await;
    open_card(page, bridge, WORKING).await;
    expect(
        page,
        bridge,
        "[...document.querySelectorAll('.thread .msg-user')].some(e => e.textContent.includes('Carry on.')) \
         && !!document.querySelector('.thread.thread-inferred') \
         && getComputedStyle(document.querySelector('.thread.thread-inferred'), '::before').content.includes('matched by folder')",
        "the working agent's thread, matched by folder",
    )
    .await;
    page.eval("history.back()").await.expect("back");
    segment_shows(page, bridge, "done", DONE).await;
}

/// Opens the finished agent: its thread renders from the realistic
/// transcript, summary-first: the last prompt and the bridge's headline
/// (from the fake summarizer), and the reply folded behind "Show full
/// turn" with everything Claude Code recorded as user input that the user
/// did not type: a subagent's report, task notifications and the `!`
/// command, each an aside, never the user's message. The prompt the user
/// queued is theirs. Injected context (the compact summary, caveats,
/// reminders) never shows. Over plain HTTP the browser can't record, so
/// there is no mic to tap.
async fn thread(page: &Page<'_>, bridge: &Bridge) {
    open_card(page, bridge, DONE).await;
    expect(
        page,
        bridge,
        &format!(
            "!document.querySelector('.mic-fab, .mic-button') \
             && [...document.querySelectorAll('.thread .turn-start .msg-user')].some(e => e.textContent.includes({})) \
             && [...document.querySelectorAll('.thread .turn-headline h3')].at(-1)?.textContent === {} \
             && {} && {}",
            js(DONE_PROMPT),
            js(HEADLINE),
            visible(".thread .msg-assistant", DONE_REPLY, false),
            visible(".thread .aside", DONE_REPORT_LABEL, false),
        ),
        "detail shows the turn's headline, its reply collapsed",
    )
    .await;
    fits_narrow(page, bridge, "thread").await;
    click_last(page, ".thread .turn-toggle").await;
    expect(
        page,
        bridge,
        &format!(
            "[...document.querySelectorAll('.thread .turn-toggle')].at(-1)?.getAttribute('aria-expanded') === 'true' \
             && {} && {} && {} && {} && {}",
            visible(".thread .msg-assistant", DONE_REPLY, true),
            visible(".thread .aside", DONE_REPORT_LABEL, true),
            visible(".thread .aside", DONE_TASK_LABEL, true),
            visible(".thread .aside", DONE_SHELL_LABEL, true),
            visible(".thread .msg-user", DONE_QUEUED, true),
        ),
        "show full turn reveals the reply, the asides and the queued prompt",
    )
    .await;
    let users = page
        .eval("[...document.querySelectorAll('.thread .msg-user')].map(e => e.textContent)")
        .await
        .expect("evaluates");
    let users: Vec<String> = serde_json::from_value(users).expect("strings");
    for text in &users {
        for not_theirs in [DONE_REPORT, DONE_TASK_LABEL, "git status --short"] {
            assert!(
                !text.contains(not_theirs),
                "shown as the user's: {text:?} (contains {not_theirs:?})"
            );
        }
    }
    let body = page
        .eval("document.querySelector('.thread').textContent")
        .await
        .expect("evaluates");
    for injected in INJECTED {
        assert!(
            !body.as_str().unwrap_or_default().contains(injected),
            "injected text shown in the thread: {injected:?}"
        );
    }
}

/// Whether the latest turn's head is followed by a TL;DR card.
const LAST_TURN_HAS_TLDR: &str = "!![...document.querySelectorAll('.thread .turn-headline')].at(-1)\
     ?.nextElementSibling?.querySelector('.tldr')";

/// "Summarise" on the finished agent's last turn: a short turn, so it has
/// no TL;DR card of its own; asking makes one (from the fake summarizer)
/// inline under the turn's head, and asking again hides it.
async fn summarise(page: &Page<'_>, bridge: &Bridge) {
    assert_eq!(
        page.eval(LAST_TURN_HAS_TLDR).await.expect("evaluates"),
        serde_json::json!(false),
        "a short turn has no TL;DR card until asked"
    );
    click_last(page, ".thread .turn-summarise").await;
    expect(
        page,
        bridge,
        &format!(
            "[...document.querySelectorAll('.thread .turn-summarise')].at(-1)?.getAttribute('aria-expanded') === 'true' \
             && {LAST_TURN_HAS_TLDR} \
             && [...document.querySelectorAll('.thread .turn-headline')].at(-1).nextElementSibling\
                  .querySelector('.tldr-lede')?.textContent === {}",
            js(TLDR_OUTCOME)
        ),
        "summarise shows the turn's TL;DR under its head",
    )
    .await;
    fits_narrow(page, bridge, "summarised thread").await;
    click_last(page, ".thread .turn-summarise").await;
    expect(
        page,
        bridge,
        &format!(
            "!({LAST_TURN_HAS_TLDR}) \
             && [...document.querySelectorAll('.thread .turn-summarise')].at(-1)?.getAttribute('aria-expanded') === 'false'"
        ),
        "summarise again hides the card",
    )
    .await;
}

/// Opens the blocked agent: its dialog's options show as chips.
async fn blocked_chips(page: &Page<'_>, bridge: &Bridge) {
    page.eval("history.back()").await.expect("back");
    expect(
        page,
        bridge,
        "location.pathname === '/' && !!document.querySelector('#tab-blocked')",
        "back on triage",
    )
    .await;
    segment_shows(page, bridge, "blocked", BLOCKED).await;
    click(page, ".agent-card").await;
    expect(
        page,
        bridge,
        "location.pathname.startsWith('/agent/') && document.querySelectorAll('.chips .chip').length >= 2",
        "blocked agent shows option chips",
    )
    .await;
    let chips = page
        .eval("[...document.querySelectorAll('.chips .chip')].map(e => e.textContent)")
        .await
        .expect("evaluates");
    assert!(
        chips.to_string().contains("Yes"),
        "the permission prompt's options: {chips}"
    );
    fits_narrow(page, bridge, "blocked detail").await;
}

/// The blocked agent's "Yes" chip is shown and may be tapped.
const YES_READY: &str = "[...document.querySelectorAll('.chips .chip')].some(c => c.textContent.trim() === 'Yes' && !c.disabled)";

/// Watches the chips from now on: `window.chipsLost` turns true if the
/// dialog's chips ever disappear, even for a moment.
const WATCH_CHIPS: &str = "
    window.chipsLost = false;
    window.chipsWatch?.disconnect();
    window.chipsWatch = new MutationObserver(() => {
        if (document.querySelectorAll('.chips .chip').length < 2) chipsLost = true;
    });
    chipsWatch.observe(document.body, { childList: true, subtree: true });
";

/// The phone puts the tab away with `pending` reads going as `net`
/// (see `PAGE_HOOKS` in `e2e::app`): hidden, the stream killed, frozen,
/// then resumed and shown. Returns once the app has re-read the dialog at
/// least twice since (the first read on return, and the one after it
/// failed or was given up on). The blocked agent's screen ticks, so every
/// read differs from the last.
async fn background_and_return(page: &Page<'_>, bridge: &Bridge, net: &str) {
    let before = page.eval("pendingReads").await.expect("evaluates");
    set_visible(page, false).await;
    page.eval(&format!(
        "pendingNet = {}; sockets.forEach(s => s.close())",
        js(net)
    ))
    .await
    .expect("kills the stream");
    page.call(
        "Page.setWebLifecycleState",
        serde_json::json!({"state": "frozen"}),
    )
    .await
    .expect("freezes");
    page.call(
        "Page.setWebLifecycleState",
        serde_json::json!({"state": "active"}),
    )
    .await
    .expect("resumes");
    set_visible(page, true).await;
    expect(
        page,
        bridge,
        &format!("pendingReads >= {} + 2", before.as_u64().unwrap_or(0)),
        &format!("the dialog is re-read after returning ({net})"),
    )
    .await;
}

/// Leaving the app with a dialog unanswered and coming back keeps its
/// chips: a re-read that fails on the way keeps the dialog shown (waiting
/// for a good read before the chips may be tapped again), and one that
/// never answers is given up on and retried rather than holding the chips
/// forever. The chips never vanish, not even for a moment.
async fn chips_survive_background(page: &Page<'_>, bridge: &Bridge) {
    expect(page, bridge, YES_READY, "chips ready before leaving").await;
    page.eval(WATCH_CHIPS).await.expect("watches the chips");

    background_and_return(page, bridge, "fail").await;
    page.eval("pendingNet = 'pass'")
        .await
        .expect("network back");
    expect(
        page,
        bridge,
        YES_READY,
        "chips ready again after a failed re-read",
    )
    .await;

    background_and_return(page, bridge, "hang").await;
    page.eval("pendingNet = 'pass'")
        .await
        .expect("network back");
    expect(
        page,
        bridge,
        YES_READY,
        "chips ready again after a hung re-read",
    )
    .await;
    assert_eq!(
        page.eval("chipsLost").await.expect("evaluates"),
        serde_json::Value::Bool(false),
        "the chips disappeared while the app came back"
    );
    page.eval("chipsWatch.disconnect()")
        .await
        .expect("stops watching");
}

/// Settings (drawn by the extras WASM, loaded on demand through
/// module-boot.js) shows the app's and the bridge's builds, the same
/// commit here (both built from this tree), so no update banner.
async fn versions(page: &Page<'_>, bridge: &Bridge) {
    click(page, ".dock a[href='/settings']").await;
    expect(
        page,
        bridge,
        "location.pathname === '/settings' \
         && !!globalThis.passporterModules?.extras \
         && /^[0-9a-f]{7,}(-dirty)?$/.test(document.querySelector('.version-app .mono')?.textContent) \
         && /^[0-9a-f]{7,}(-dirty)?$/.test(document.querySelector('.version-bridge')?.textContent)",
        "settings shows both builds",
    )
    .await;
    let shown = page
        .eval(
            "[document.querySelector('.version-app').textContent, \
              document.querySelector('.version-bridge').textContent, \
              !!document.querySelector('.update-prompt')]",
        )
        .await
        .expect("evaluates");
    let (app, bridge_sha, banner): (String, String, bool) =
        serde_json::from_value(shown).expect("reads the builds");
    let commit = |sha: &str| sha.trim_end_matches("-dirty").to_owned();
    assert!(app.contains(" · built "), "the build time is shown: {app}");
    assert!(
        app.starts_with(&commit(&bridge_sha)),
        "app {app} and bridge {bridge_sha} are built from one commit"
    );
    assert!(!banner, "no update banner when the builds match");
}

/// What the new agent is asked first.
const NEW_AGENT_PROMPT: &str = "Add a changelog entry for the gizmo fix.";

/// Starts a new agent from triage: the `+` beside the segments opens the
/// sheet with the bridge's lone project picked, a typed first prompt goes
/// with it, and the app opens the new agent. The fake herdr saw one start
/// in the project folder, with no native arguments, and the prompt.
async fn new_agent(page: &Page<'_>, bridge: &Bridge, herdr: &FakeHerdr, world: &World) {
    click(page, ".dock-link[href='/']").await;
    expect(
        page,
        bridge,
        "location.pathname === '/' && !!document.querySelector('.triage-dock .new-agent-button')",
        "triage offers a new agent",
    )
    .await;
    click(page, ".new-agent-button").await;
    expect(
        page,
        bridge,
        &format!(
            "[...document.querySelectorAll('.new-agent-sheet .option-chip')] \
               .some(c => c.textContent === {} && c.getAttribute('aria-checked') === 'true') \
             && !document.querySelector('.new-agent-start').disabled",
            js(SPAWN_PROJECT)
        ),
        "the sheet with the lone project picked",
    )
    .await;
    fits_narrow(page, bridge, "new agent sheet").await;
    page.eval("document.querySelector('#new-agent-prompt').focus()")
        .await
        .expect("focus");
    page.insert_text(NEW_AGENT_PROMPT).await.expect("types");
    click(page, ".new-agent-start").await;
    expect(
        page,
        bridge,
        "location.pathname.startsWith('/agent/w') && !document.querySelector('.new-agent-sheet')",
        "the new agent opens",
    )
    .await;
    let started = herdr.started();
    let [
        Started {
            pane,
            kind,
            args,
            cwd,
        },
    ] = started.as_slice()
    else {
        panic!("one agent started: {started:?}");
    };
    assert_eq!(kind, "claude");
    assert!(args.is_empty(), "no native arguments: {args:?}");
    assert_eq!(cwd, &world.spawn_project);
    let path = page.eval("location.pathname").await.expect("evaluates");
    assert!(
        path.as_str()
            .is_some_and(|p| p.starts_with(&format!("/agent/{pane}"))),
        "opened {path}, started in {pane}"
    );
    until(bridge, "the first prompt reaches the new agent", || {
        herdr
            .prompted()
            .iter()
            .any(|p| &p.pane == pane && p.text == NEW_AGENT_PROMPT)
    })
    .await;
}

/// Runs a runbook from triage: the terminal button in the triage header
/// opens the Runbooks sheet, a tap on the runbook asks first, showing its
/// exact argv, and Run starts it in its own herdr tab. The fake herdr's
/// shell runs the one line the bridge typed (the bridge's runner on a
/// spec file), and the sheet follows the run to exit 0 with its output.
async fn runbook(page: &Page<'_>, bridge: &Bridge, herdr: &FakeHerdr) {
    click(page, ".dock-link[href='/']").await;
    expect(
        page,
        bridge,
        "location.pathname === '/' && !!document.querySelector('.page-header .runbooks-button')",
        "triage offers runbooks",
    )
    .await;
    click(page, ".runbooks-button").await;
    expect(
        page,
        bridge,
        &format!(
            "[...document.querySelectorAll('.runbooks-sheet .runbook-name')].some(n => n.textContent === {})",
            js(RUNBOOK)
        ),
        "the sheet lists the runbook",
    )
    .await;
    fits_narrow(page, bridge, "runbooks sheet").await;
    click(page, ".runbook-row").await;
    let argv = RUNBOOK_ARGV.join(" ");
    expect(
        page,
        bridge,
        &format!(
            "document.querySelector('.runbook-argv')?.textContent === {}",
            js(&argv)
        ),
        "the confirm shows the exact argv",
    )
    .await;
    assert!(herdr.typed().is_empty(), "nothing runs before the confirm");
    click(page, ".runbook-run").await;
    expect(
        page,
        bridge,
        "(document.querySelector('.runbook-run-status')?.textContent ?? '').includes('Done · exit 0') \
         && (document.querySelector('.runbook-lines')?.textContent ?? '').includes('hello-from-a-runbook')",
        "the run ends with exit 0 and its output",
    )
    .await;
    let typed = herdr.typed();
    let [(_, line)] = typed.as_slice() else {
        panic!("one line typed: {typed:?}");
    };
    assert!(line.contains(" runbook "), "the runner on a spec: {line}");
    assert!(
        !line.contains("echo"),
        "argv never reaches the shell: {line}"
    );
    page.eval("document.querySelector('.runbooks-sheet .sheet-header .icon-button').click()")
        .await
        .expect("closes the sheet");
}

/// The finished agent ended on a question, so its composer offers the
/// fake summarizer's reply: a double tap on it sends it once, at once, as a
/// prompt (the bridge's audit log), and leaves a typed draft alone.
async fn suggested_reply(page: &Page<'_>, bridge: &Bridge) {
    expect(
        page,
        bridge,
        &visible(".suggestions .chip-suggestion", SUGGESTED, true),
        "the suggested reply",
    )
    .await;
    type_draft(page, "A draft").await;
    page.eval(
        "{
            const chip = document.querySelector('.chip-suggestion');
            chip.click();
            chip.click();
        }",
    )
    .await
    .expect("double-taps the suggestion");
    prompt_reaches_bridge(bridge, SUGGESTED, "the suggested reply").await;
    // Settled: nothing left to send (a second send would still be queued
    // or on its way).
    expect(
        page,
        bridge,
        "document.querySelector('#reply-text')?.value === 'A draft' \
         && !document.querySelector('.outbox-queued, .outbox-sending')",
        "the draft is left as typed and the outbox is settled",
    )
    .await;
    assert_eq!(
        prompts_executed(bridge, SUGGESTED),
        1,
        "a double tap sends once\n--- audit ---\n{}",
        bridge.audit()
    );
    type_draft(page, "").await;
}

/// Dictation as the app does it, on a secure origin: the agent's screen
/// loads the dictation WASM through the app's own loader, and a tap on the
/// dock's mic reaches `getUserMedia` (counted by a wrapper) and shows it;
/// a second tap sends what was heard to the agent: the prompt reaches the
/// bridge's actions endpoint (its audit log). The dock's is the only mic:
/// the composer has none, and the dock has none on triage, where there is
/// no one to dictate to. Before dictating, a suggested reply
/// ([`suggested_reply`]).
async fn app_dictation(page: &Page<'_>, bridge: &Bridge) {
    segment_shows(page, bridge, "done", DONE).await;
    expect(
        page,
        bridge,
        "!document.querySelector('.mic-fab')",
        "no dock mic on triage",
    )
    .await;
    open_card(page, bridge, DONE).await;
    expect(
        page,
        bridge,
        "document.querySelector('.mic-fab:not([disabled])')?.getAttribute('aria-pressed') === 'false' \
         && !!document.querySelector('.composer') \
         && !document.querySelector('.composer [class*=\"mic\"]') \
         && document.querySelectorAll('.mic-fab, .mic-button').length === 1",
        "the agent's screen shows the dock mic and no composer mic",
    )
    .await;
    suggested_reply(page, bridge).await;
    page.eval(
        "window.gum = 0;
        const media = navigator.mediaDevices;
        const open = media.getUserMedia.bind(media);
        media.getUserMedia = (constraints) => { gum += 1; return open(constraints); };",
    )
    .await
    .expect("counts microphone opens");
    tap_mic(page).await;
    expect(
        page,
        bridge,
        &format!("gum === 1 && {RECORDING}"),
        "the dock mic opens the microphone and records",
    )
    .await;
    tap_mic(page).await;
    prompt_reaches_bridge(bridge, DICTATED, "the dictated prompt").await;
    expect(
        page,
        bridge,
        "document.querySelector('.mic-fab')?.getAttribute('aria-pressed') === 'false' \
         && document.querySelector('#reply-text')?.value === ''",
        "the mic is free and the composer empty after sending",
    )
    .await;
}

/// What the photo scenario types with its picture.
const PHOTO_PROMPT: &str = "What is on this board?";

/// A photo from the composer: a picture drawn in the page is handed to
/// the attach button's file input (as the phone's picker would), its
/// preview shows it uploading and then attached while Send waits, and
/// Send delivers the typed words to the agent with a line naming the
/// stored file, which is a JPEG in the bridge's private attachments dir.
async fn photo(page: &Page<'_>, bridge: &Bridge, herdr: &FakeHerdr, state_dir: &Path) {
    type_draft(page, PHOTO_PROMPT).await;
    page.eval(
        "new Promise((ok) => {
            const canvas = document.createElement('canvas');
            canvas.width = 3000;
            canvas.height = 2000;
            const g = canvas.getContext('2d');
            g.fillStyle = '#1b5e20';
            g.fillRect(0, 0, 3000, 2000);
            g.fillStyle = '#ffd54f';
            g.fillRect(400, 300, 900, 600);
            canvas.toBlob((blob) => {
                const input = document.querySelector('.attach-button input[type=file]');
                const files = new DataTransfer();
                files.items.add(new File([blob], 'board.png', { type: 'image/png' }));
                input.files = files.files;
                input.dispatchEvent(new Event('change', { bubbles: true }));
                ok(true);
            }, 'image/png');
        })",
    )
    .await
    .expect("picks a photo");
    expect(
        page,
        bridge,
        "!!document.querySelector('.attach-slot .attach-preview canvas')",
        "the photo's preview",
    )
    .await;
    expect(
        page,
        bridge,
        &visible(".attach-status", "Photo attached", true),
        "the photo uploads",
    )
    .await;
    expect(
        page,
        bridge,
        "!document.querySelector('.send-button').disabled",
        "Send is ready once the photo is on the bridge",
    )
    .await;
    click(page, ".send-button").await;
    until(bridge, "the photo prompt reaches the agent", || {
        herdr
            .prompted()
            .iter()
            .any(|p| p.text.starts_with(PHOTO_PROMPT))
    })
    .await;
    let sent = herdr
        .prompted()
        .into_iter()
        .find(|p| p.text.starts_with(PHOTO_PROMPT))
        .map(|p| p.text)
        .unwrap_or_default();
    let dir = std::fs::canonicalize(state_dir.join("attachments")).expect("attachments dir");
    let (words, line) = sent
        .split_once("\n\nAttached image: ")
        .expect("an attachment line");
    assert_eq!(words, PHOTO_PROMPT);
    let path = PathBuf::from(line.trim_start_matches('@'));
    assert!(
        path.starts_with(&dir) && path.extension().is_some_and(|e| e == "jpg"),
        "names a stored JPEG: {sent}"
    );
    let stored = std::fs::read(&path).expect("the file the agent is pointed at");
    assert!(stored.starts_with(&[0xFF, 0xD8, 0xFF]), "a JPEG");
    assert!(
        bridge.audit().contains("\"attachments\":[{\"id\""),
        "the audit lists the attachment\n{}",
        bridge.audit()
    );
    expect(
        page,
        bridge,
        "!document.querySelector('.attach-preview') \
         && document.querySelector('#reply-text')?.value === ''",
        "the composer is empty again",
    )
    .await;
}

/// Terminal mode on the finished agent, whose reply asks for a `!`
/// command in its session and offers a decoy for another terminal: both
/// show as chips, in order; tapping the `!` one and confirming "Run here
/// (!)" types exactly `!<command>` into the agent (the fake herdr's
/// `agent.prompt`), and nothing of the decoy.
async fn command_from_message(page: &Page<'_>, bridge: &Bridge, herdr: &FakeHerdr) {
    // The prompts before left the agent working; back at its prompt.
    herdr.set_status(DONE_PANE, Status::Done);
    expect(
        page,
        bridge,
        &format!(
            "(() => {{ const chips = [...document.querySelectorAll('.command-strip .command-chip')]; \
               return chips.length === 2 \
                 && chips[0].querySelector('.command-chip-text')?.textContent === {} \
                 && chips[0].querySelector('.command-badge')?.textContent === '!' \
                 && chips[1].querySelector('.command-chip-text')?.textContent === {}; }})()",
            js(DONE_COMMAND),
            js(DONE_DECOY)
        ),
        "the message's command and its decoy as chips",
    )
    .await;
    fits_narrow(page, bridge, "agent with commands").await;
    page.eval(&format!(
        "[...document.querySelectorAll('.command-chip')] \
           .find(c => c.querySelector('.command-chip-text').textContent === {}).click()",
        js(DONE_COMMAND)
    ))
    .await
    .expect("taps the command's chip");
    expect(
        page,
        bridge,
        &format!(
            "document.querySelector('.command-confirm .command-text')?.textContent === {} \
             && [...document.querySelectorAll('.command-confirm button')] \
                  .some(b => b.textContent === 'Run here (!)' && !b.disabled)",
            js(DONE_COMMAND)
        ),
        "the confirm shows the exact command",
    )
    .await;
    assert!(
        herdr.prompted().iter().all(|p| !p.text.starts_with('!')),
        "nothing runs before the confirm"
    );
    page.eval(
        "[...document.querySelectorAll('.command-confirm button')] \
           .find(b => b.textContent === 'Run here (!)').click()",
    )
    .await
    .expect("confirms");
    let want = format!("!{DONE_COMMAND}");
    until(bridge, "the command reaches herdr", || {
        herdr
            .prompted()
            .iter()
            .any(|p| p.pane == DONE_PANE && p.text == want)
    })
    .await;
    expect(
        page,
        bridge,
        "!document.querySelector('.command-confirm')",
        "the confirm closes once sent",
    )
    .await;
    let bangs: Vec<_> = herdr
        .prompted()
        .into_iter()
        .filter(|p| p.text.starts_with('!'))
        .collect();
    assert_eq!(bangs.len(), 1, "exactly one command: {bangs:?}");
    assert!(
        herdr
            .typed()
            .iter()
            .all(|(_, line)| !line.contains(DONE_DECOY)),
        "the decoy never ran"
    );
}

/// The MVP phone over plain HTTP.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Chromium and a web build: run `just e2e`"]
async fn phone_smoke() {
    let Some(rig) = Rig::start(RigSpec::default()).await else {
        return;
    };
    let page = rig.page().await;
    let bridge = &rig.bridge;
    connect(&page, bridge).await;
    triage(&page, bridge).await;
    folder_matched_thread(&page, bridge).await;
    thread(&page, bridge).await;
    summarise(&page, bridge).await;
    blocked_chips(&page, bridge).await;
    chips_survive_background(&page, bridge).await;
    versions(&page, bridge).await;
    new_agent(&page, bridge, &rig.herdr, &rig.world).await;
    runbook(&page, bridge, &rig.herdr).await;
    rig.assert_no_problems();
}

/// The app on a secure origin, where the mic exists.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Chromium and a web build: run `just e2e`"]
async fn phone_secure() {
    let Some(rig) = Rig::start(RigSpec::default()).await else {
        return;
    };
    let page = rig.page().await;
    let bridge = &rig.bridge;
    connect_secure(&page, &rig).await;
    app_dictation(&page, bridge).await;
    photo(&page, bridge, &rig.herdr, &rig.state_dir).await;
    command_from_message(&page, bridge, &rig.herdr).await;
    assert_eq!(rig.whisper.served(), 1, "the take was transcribed once");
    rig.assert_no_problems();
}
