//! The rig every scenario runs on, and the steps they share.
//!
//! A [`Rig`] is one fixture [`World`] behind the fake herdr, a fake
//! whisper, the real bridge and a headless Chromium. Steps drive the app
//! only the way a person does (taps, typing, switching apps) and wait on
//! what the page shows, never on time: every wait is a condition with a
//! deadline ([`WAIT`]), and a failed one prints the page and the bridge's
//! log.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::World;
use crate::bridge::{Bridge, BridgeSpec};
use crate::cdp::{Browser, CHROMIUM_ENV, Page, find_chromium};
use crate::herdr::FakeHerdr;
use crate::whisper::FakeWhisper;

/// Environment variable naming the `passporter-bridge` binary.
const BRIDGE_ENV: &str = "PASSPORTER_BRIDGE_BIN";
/// Environment variable naming the PWA build directory.
const DIST_ENV: &str = "PASSPORTER_WEB_DIST";

/// The phone the app is designed for (design §7).
pub const PHONE: (u32, u32) = (390, 844);
/// The narrowest phone the layout must fit without sideways scrolling.
pub const NARROW: (u32, u32) = (360, 780);

/// A non-loopback name for the bridge, mapped to 127.0.0.1 inside Chromium.
/// Loopback origins are secure contexts, which would show the device-pairing
/// screen; the phone MVP reaches the bridge over plain HTTP, so the app is
/// exercised the same way here and shows the token form.
pub const HOST: &str = "passporter.test";

/// Set to `1` to run Chromium without its own sandbox, for containers and
/// sandboxed shells where it cannot start one. The page is the local PWA only.
const NO_SANDBOX_ENV: &str = "PASSPORTER_E2E_NO_SANDBOX";

/// Browser errors the test setup causes by design, not the app: the bridge
/// always sends COOP, which browsers ignore off a trustworthy origin (and
/// say so at error level); the dictation scenarios drive the mic from
/// script, not a real tap, so its haptic ticks are blocked.
const EXPECTED: &[&str] = &[
    "Cross-Origin-Opener-Policy header has been ignored",
    "Blocked call to navigator.vibrate because user hasn't tapped",
];

/// How long the app may take to reach any one expected state.
pub const WAIT: Duration = Duration::from_secs(15);

/// The harmless runbook the bridge is configured with.
pub const RUNBOOK: &str = "Say hello";
/// [`RUNBOOK`]'s argv.
pub const RUNBOOK_ARGV: [&str; 2] = ["echo", "hello-from-a-runbook"];

/// Installed before the app loads: keeps every `WebSocket` the app opens
/// (so a test can kill the stream, as a phone does to a background tab),
/// counts `GET …/pending` reads in `window.pendingReads` and routes them
/// through `window.pendingNet`: `"pass"`, `"fail"` (rejects, as Chrome
/// fails requests while the phone suspends a tab's network) or `"hang"`
/// (never answers, as over a dead tunnel, unless the request is aborted).
/// `window.actionsNet = "fail"` fails `POST /v1/actions` the same way (no
/// signal), `window.audioNet = "hold"` holds every `/v1/audio` request
/// until `window.releaseAudio()` sends them on (a clip on its way over a
/// slow link), and `window.bridgeBuild`, when set, replaces the build the
/// bridge reports at `/v1/version` (a bridge deployed since the app
/// loaded).
///
/// On a secure origin, `navigator.wakeLock` behaves as Chromium's does on
/// a phone (a headless one has no screen to keep on) and is observed in
/// `window.wake`: `requests` made and locks `held`. A request from a
/// hidden page is refused, and hiding the page releases every lock.
const PAGE_HOOKS: &str = "
    window.sockets = [];
    window.pendingNet = 'pass';
    window.pendingReads = 0;
    window.actionsNet = 'pass';
    window.audioNet = 'pass';
    window.audioHeld = [];
    window.releaseAudio = () => { audioNet = 'pass'; audioHeld.splice(0).forEach((go) => go()); };
    window.bridgeBuild = null;
    window.wake = { requests: 0, held: 0 };
    if (window.isSecureContext) {
        const locks = new Set();
        const wakeLock = {
            request(type) {
                wake.requests += 1;
                if (type !== 'screen') return Promise.reject(new TypeError('unknown wake lock type'));
                if (document.visibilityState !== 'visible') {
                    return Promise.reject(new DOMException('The requesting page is not visible', 'NotAllowedError'));
                }
                const sentinel = new EventTarget();
                let released = false;
                Object.defineProperty(sentinel, 'type', { value: 'screen' });
                Object.defineProperty(sentinel, 'released', { get: () => released });
                sentinel.release = () => {
                    if (!released) {
                        released = true;
                        locks.delete(sentinel);
                        wake.held -= 1;
                        sentinel.dispatchEvent(new Event('release'));
                    }
                    return Promise.resolve();
                };
                locks.add(sentinel);
                wake.held += 1;
                return Promise.resolve(sentinel);
            },
        };
        Object.defineProperty(Navigator.prototype, 'wakeLock', { configurable: true, get: () => wakeLock });
        document.addEventListener('visibilitychange', () => {
            if (document.visibilityState === 'hidden') [...locks].forEach((lock) => lock.release());
        });
    }
    const NativeSocket = window.WebSocket;
    window.WebSocket = class extends NativeSocket {
        constructor(...args) { super(...args); sockets.push(this); }
    };
    const nativeFetch = window.fetch.bind(window);
    window.fetch = (input, init) => {
        const url = typeof input === 'string' ? input : input.url;
        if (url.endsWith('/v1/actions') && actionsNet === 'fail') {
            return Promise.reject(new TypeError('Failed to fetch'));
        }
        if (url.includes('/v1/audio') && audioNet === 'hold') {
            return new Promise((ok) => audioHeld.push(() => ok(nativeFetch(input, init))));
        }
        if (url.endsWith('/v1/version') && bridgeBuild) {
            return nativeFetch(input, init).then(async (r) => {
                const body = await r.json();
                body.bridge = bridgeBuild;
                return new Response(JSON.stringify(body), { status: r.status, headers: r.headers });
            });
        }
        if (!url.endsWith('/pending')) return nativeFetch(input, init);
        pendingReads += 1;
        if (pendingNet === 'pass') return nativeFetch(input, init);
        if (pendingNet === 'fail') return Promise.reject(new TypeError('Failed to fetch'));
        const signal = input.signal ?? init?.signal;
        return new Promise((_, fail) => signal?.addEventListener('abort', () => fail(signal.reason)));
    };
";

/// Which world a [`Rig`] serves and how its bridge is configured.
#[derive(Debug, Clone, Copy, Default)]
pub struct RigSpec {
    /// [`World::reality`] instead of [`World::create`].
    pub reality: bool,
    /// Configure Web Push (`[push]`).
    pub push: bool,
}

/// One scenario's world, fakes, bridge and browser; all stopped on drop.
#[derive(Debug)]
pub struct Rig {
    /// The fixture agents and their files.
    pub world: World,
    /// The fake herdr the bridge follows.
    pub herdr: FakeHerdr,
    /// The fake whisper-server.
    pub whisper: FakeWhisper,
    /// The real bridge.
    pub bridge: Bridge,
    /// Headless Chromium.
    pub browser: Browser,
    /// The bridge's state dir.
    pub state_dir: PathBuf,
    // Last: removed after everything using it has stopped.
    _scratch: tempfile::TempDir,
}

#[expect(clippy::print_stderr, reason = "the skip notice is the output")]
fn skip(reason: &str) {
    eprintln!("SKIPPED e2e test: {reason}");
}

fn env_path(var: &str, default: &str) -> PathBuf {
    std::env::var_os(var).filter(|v| !v.is_empty()).map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(default),
        PathBuf::from,
    )
}

/// Chromium flags for this run.
fn chromium_args(bridge: &Bridge) -> Vec<String> {
    let mut args = vec![
        format!("--host-resolver-rules=MAP {HOST} {}", bridge.addr().ip()),
        // Never send the fake host to a proxy from the environment.
        "--no-proxy-server".to_owned(),
        "--disable-features=HttpsUpgrades,HttpsFirstBalancedModeAutoEnable".to_owned(),
        // Dictation: a synthetic microphone, granted without a prompt, and
        // audio that runs without a real gesture.
        "--use-fake-device-for-media-stream".to_owned(),
        "--use-fake-ui-for-media-stream".to_owned(),
        "--autoplay-policy=no-user-gesture-required".to_owned(),
    ];
    if std::env::var_os(NO_SANDBOX_ENV).is_some_and(|v| v == "1") {
        args.push("--no-sandbox".to_owned());
    }
    args
}

impl Rig {
    /// Starts everything for `spec`, or returns `None` (having said why)
    /// when there is no Chromium to run.
    ///
    /// # Panics
    /// When the bridge binary or PWA build is missing, or anything fails to
    /// start.
    pub async fn start(spec: RigSpec) -> Option<Self> {
        Self::start_with(spec, |root| {
            if spec.reality {
                World::reality(root)
            } else {
                World::create(root)
            }
        })
        .await
    }

    /// Like [`Self::start`], around the world `make` builds in the rig's
    /// scratch directory instead of a fixture world (`spec.reality` is not
    /// consulted).
    ///
    /// # Panics
    /// When the bridge binary or PWA build is missing, `make` fails, or
    /// anything fails to start.
    pub async fn start_with(
        spec: RigSpec,
        make: impl FnOnce(&Path) -> std::io::Result<World>,
    ) -> Option<Self> {
        let Some(chromium) = find_chromium() else {
            skip(&format!(
                "no Chromium found (install chromium, or set {CHROMIUM_ENV} to a Chrome/Chromium binary)"
            ));
            return None;
        };
        let bridge_bin = env_path(BRIDGE_ENV, "../target/debug/passporter-bridge");
        let dist = env_path(DIST_ENV, "../crates/web/dist");
        assert!(
            bridge_bin.is_file(),
            "no bridge binary at {} (run `just e2e`, or set {BRIDGE_ENV})",
            bridge_bin.display()
        );
        assert!(
            dist.join("index.html").is_file(),
            "no PWA build in {} (run `just e2e`, or set {DIST_ENV})",
            dist.display()
        );

        let scratch = tempfile::tempdir().expect("tempdir");
        let world = make(scratch.path()).expect("fixture world");
        let herdr = FakeHerdr::serve(
            &scratch.path().join("herdr.sock"),
            world.agents.clone(),
            &world.workspace_labels(),
        )
        .expect("fake herdr");
        let whisper = FakeWhisper::serve(&world.heard)
            .await
            .expect("fake whisper");
        let work_dir = scratch.path().join("bridge");
        std::fs::create_dir_all(&work_dir).expect("mkdir");
        let spawn_projects: Vec<(&str, &Path)> = world
            .spawn_projects
            .iter()
            .map(|(name, folder)| (name.as_str(), folder.as_path()))
            .collect();
        let runbooks: Vec<(&str, &Path, &[&str])> = spawn_projects
            .first()
            .map(|(_, folder)| (RUNBOOK, *folder, RUNBOOK_ARGV.as_slice()))
            .into_iter()
            .collect();
        let bridge = Bridge::start(&BridgeSpec {
            binary: &bridge_bin,
            static_dir: &dist,
            herdr_socket: herdr.socket(),
            projects_dir: &world.projects_dir,
            claude: &world.claude,
            work_dir: &work_dir,
            whisper: Some(whisper.addr()),
            codex_dir: Some(&world.codex_dir),
            push: spec.push,
            spawn_projects: &spawn_projects,
            runbooks: &runbooks,
        })
        .await
        .expect("bridge starts");
        let browser = Browser::launch(
            &chromium,
            &scratch.path().join("profile"),
            &chromium_args(&bridge),
        )
        .await
        .expect("chromium starts");
        Some(Self {
            world,
            herdr,
            whisper,
            bridge,
            browser,
            state_dir: work_dir.join("state"),
            _scratch: scratch,
        })
    }

    /// A new tab at phone size, with [`PAGE_HOOKS`] installed.
    ///
    /// # Panics
    /// If Chromium refuses.
    pub async fn page(&self) -> Page<'_> {
        let page = self.browser.new_page().await.expect("page");
        page.call(
            "Page.addScriptToEvaluateOnNewDocument",
            serde_json::json!({"source": PAGE_HOOKS}),
        )
        .await
        .expect("installs the page hooks");
        page.set_viewport(PHONE.0, PHONE.1).await.expect("viewport");
        page
    }

    /// Fails with every console error, uncaught exception and browser log
    /// error seen, except the ones the setup causes ([`EXPECTED`]).
    ///
    /// # Panics
    /// If there were any.
    pub fn assert_no_problems(&self) {
        self.assert_no_problems_but(&[]);
    }

    /// [`Self::assert_no_problems`], also allowing errors containing one
    /// of `expected` (a scenario that provokes them on purpose).
    ///
    /// # Panics
    /// If there were others.
    pub fn assert_no_problems_but(&self, expected: &[&str]) {
        let problems: Vec<String> = self
            .browser
            .problems()
            .into_iter()
            .filter(|p| {
                !EXPECTED
                    .iter()
                    .chain(expected)
                    .any(|known| p.contains(known))
            })
            .collect();
        assert!(
            problems.is_empty(),
            "console errors:\n{}\n--- bridge log ---\n{}",
            problems.join("\n"),
            self.bridge.log()
        );
    }

    /// The loopback origin: a secure context, where the mic, the service
    /// worker and push exist.
    #[must_use]
    pub fn loopback(&self) -> String {
        format!("http://{}/", self.bridge.addr())
    }
}

/// A JavaScript string literal.
#[must_use]
pub fn js(text: &str) -> String {
    Value::from(text).to_string()
}

/// Waits for `expression` to be truthy, failing with `what`, the page and
/// the bridge log otherwise.
///
/// # Panics
/// On timeout, or if the page can't be evaluated.
pub async fn expect(page: &Page<'_>, bridge: &Bridge, expression: &str, what: &str) {
    let seen = page.wait_for(expression, WAIT).await.expect("evaluates");
    if matches!(seen, Value::Null | Value::Bool(false)) {
        let body = page
            .eval("location.href + '\\n' + document.body.innerText")
            .await
            .unwrap_or_default();
        panic!(
            "{what}: timed out\n--- page ---\n{}\n--- bridge log ---\n{}",
            body.as_str().unwrap_or_default(),
            bridge.log()
        );
    }
}

/// Like [`expect`], but returns whether it came true instead of failing.
///
/// # Panics
/// If the page can't be evaluated.
pub async fn comes_true(page: &Page<'_>, expression: &str) -> bool {
    let seen = page.wait_for(expression, WAIT).await.expect("evaluates");
    !matches!(seen, Value::Null | Value::Bool(false))
}

/// Waits on the host side until `done()` holds, failing with `what` and
/// the bridge's audit and log otherwise.
///
/// # Panics
/// On timeout.
pub async fn until(bridge: &Bridge, what: &str, mut done: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !done() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{what}: timed out\n--- audit ---\n{}\n--- bridge log ---\n{}",
            bridge.audit(),
            bridge.log()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Taps the first element matching `selector`.
///
/// # Panics
/// If there is none.
pub async fn click(page: &Page<'_>, selector: &str) {
    click_nth(page, selector, 0).await;
}

/// Taps the last element matching `selector` (e.g. the latest turn's).
///
/// # Panics
/// If there is none.
pub async fn click_last(page: &Page<'_>, selector: &str) {
    click_nth(page, selector, -1).await;
}

async fn click_nth(page: &Page<'_>, selector: &str, at: i32) {
    let clicked = page
        .eval(&format!(
            "(() => {{ const el = [...document.querySelectorAll({})].at({at}); if (!el) return false; el.click(); return true; }})()",
            js(selector)
        ))
        .await
        .expect("evaluates");
    assert_eq!(clicked, Value::Bool(true), "no {selector} to click");
}

/// Taps the element matching `selector` whose text is exactly `text`.
///
/// # Panics
/// If there is none.
pub async fn click_text(page: &Page<'_>, selector: &str, text: &str) {
    let clicked = page
        .eval(&format!(
            "(() => {{ const el = [...document.querySelectorAll({})].find(e => e.textContent.trim() === {}); if (!el) return false; el.click(); return true; }})()",
            js(selector),
            js(text)
        ))
        .await
        .expect("evaluates");
    assert_eq!(
        clicked,
        Value::Bool(true),
        "no {selector} reading {text:?} to click"
    );
}

/// The titles of the cards on the triage list, in order.
///
/// # Panics
/// If the page can't be evaluated.
pub async fn card_titles(page: &Page<'_>) -> Vec<String> {
    let titles = page
        .eval("[...document.querySelectorAll('.agent-card .agent-title')].map(e => e.textContent)")
        .await
        .expect("evaluates");
    serde_json::from_value(titles).expect("strings")
}

/// Shows triage's `segment` and waits until it lists `title`.
///
/// # Panics
/// If it never does.
pub async fn segment_lists(page: &Page<'_>, bridge: &Bridge, segment: &str, title: &str) {
    click(page, &format!("#tab-{segment}")).await;
    expect(
        page,
        bridge,
        &format!(
            "document.querySelector('#tab-{segment}')?.getAttribute('aria-selected') === 'true' \
             && [...document.querySelectorAll('.agent-card .agent-title')].some(e => e.textContent === {})",
            js(title)
        ),
        &format!("{segment} segment lists {title}"),
    )
    .await;
}

/// Shows `segment` and checks it lists exactly `title`.
///
/// # Panics
/// If it lists anything else.
pub async fn segment_shows(page: &Page<'_>, bridge: &Bridge, segment: &str, title: &str) {
    segment_lists(page, bridge, segment, title).await;
    assert_eq!(card_titles(page).await, [title], "{segment} segment");
}

/// Opens the card titled `title` on the current segment.
///
/// # Panics
/// If there is no such card, or it does not open.
pub async fn open_card(page: &Page<'_>, bridge: &Bridge, title: &str) {
    let clicked = page
        .eval(&format!(
            "(() => {{ const t = [...document.querySelectorAll('.agent-card .agent-title')].find(e => e.textContent === {}); \
             const card = t?.closest('.agent-card'); if (!card) return false; card.click(); return true; }})()",
            js(title)
        ))
        .await
        .expect("evaluates");
    assert_eq!(clicked, Value::Bool(true), "no card titled {title}");
    expect(
        page,
        bridge,
        "location.pathname.startsWith('/agent/')",
        &format!("{title} opens"),
    )
    .await;
}

/// Whether some element matching `selector` that contains `text` is shown
/// (`true`) or hidden, e.g. behind its turn (`false`).
#[must_use]
pub fn visible(selector: &str, text: &str, visible: bool) -> String {
    format!(
        "[...document.querySelectorAll({})] \
           .filter(e => e.textContent.includes({})) \
           .some(e => e.checkVisibility() === {visible})",
        js(selector),
        js(text)
    )
}

/// At [`NARROW`] width the page must not scroll sideways; restores [`PHONE`].
///
/// # Panics
/// If it does.
pub async fn fits_narrow(page: &Page<'_>, bridge: &Bridge, screen: &str) {
    page.set_viewport(NARROW.0, NARROW.1)
        .await
        .expect("viewport");
    expect(
        page,
        bridge,
        &format!("window.innerWidth === {}", NARROW.0),
        "viewport resized",
    )
    .await;
    let overflow = page
        .eval(
            "(() => { const d = document.documentElement; \
             const wide = [...document.querySelectorAll('body *')] \
               .filter(e => e.getBoundingClientRect().right > d.clientWidth + 0.5) \
               .slice(0, 5).map(e => e.tagName.toLowerCase() + '.' + [...e.classList].join('.')); \
             return { scroll: d.scrollWidth, client: d.clientWidth, wide }; })()",
        )
        .await
        .expect("evaluates");
    let width = |key: &str| {
        overflow[key]
            .as_u64()
            .unwrap_or_else(|| panic!("{key} width is not a number: {overflow}"))
    };
    assert!(
        width("scroll") <= width("client"),
        "{screen} scrolls sideways at {}px: {overflow}",
        NARROW.0
    );
    page.set_viewport(PHONE.0, PHONE.1).await.expect("viewport");
}

/// Opens the app over plain HTTP (as the MVP phone reaches it over
/// `WireGuard`) and connects it with the static token, through the form.
///
/// # Panics
/// If any step does not happen.
pub async fn connect(page: &Page<'_>, bridge: &Bridge) {
    page.goto(&bridge.url_for_host(HOST))
        .await
        .expect("navigates");
    expect(
        page,
        bridge,
        "!!document.querySelector('.pair-prompt')",
        "triage offers to connect",
    )
    .await;
    assert_eq!(
        page.eval("window.isSecureContext")
            .await
            .expect("evaluates"),
        Value::Bool(false),
        "the app must see a plain-HTTP origin, as over WireGuard"
    );
    click(page, ".pair-prompt").await;
    expect(
        page,
        bridge,
        "!!document.querySelector('#bridge-token')",
        "token form",
    )
    .await;
    page.eval("document.querySelector('#bridge-token').focus()")
        .await
        .expect("focus");
    page.insert_text(bridge.token()).await.expect("types");
    click(page, ".token-form button[type=submit]").await;
    expect(
        page,
        bridge,
        "location.pathname === '/' && !!document.querySelector('#tab-blocked') \
         && document.querySelectorAll('.agent-card').length > 0",
        "connected triage",
    )
    .await;
}

/// Opens the app on the loopback origin (a secure context: the mic, the
/// service worker and push exist there) already connected: the token is
/// stored where the token form stores it, then the app is reloaded.
///
/// # Panics
/// If any step does not happen.
pub async fn connect_secure(page: &Page<'_>, rig: &Rig) {
    let bridge = &rig.bridge;
    page.goto(&rig.loopback()).await.expect("navigates");
    expect(
        page,
        bridge,
        "window.isSecureContext && !!document.querySelector('.pair-prompt, #tab-blocked')",
        "the app on the loopback origin",
    )
    .await;
    page.eval(&format!(
        "new Promise((ok, no) => {{
            const open = indexedDB.open('passporter');
            open.onerror = no;
            open.onsuccess = () => {{
                const tx = open.result.transaction('meta', 'readwrite');
                tx.objectStore('meta').put(JSON.stringify({}), 'bridgeToken');
                tx.oncomplete = () => {{ open.result.close(); ok(true); }};
                tx.onerror = no;
            }};
        }})",
        js(bridge.token())
    ))
    .await
    .expect("stores the token");
    page.goto(&rig.loopback()).await.expect("reloads");
    expect(
        page,
        bridge,
        "!!document.querySelector('#tab-blocked') && document.querySelectorAll('.agent-card').length > 0",
        "connected triage on the loopback origin",
    )
    .await;
}

/// Types `text` into the composer, as the keyboard would.
///
/// # Panics
/// If there is no composer.
pub async fn type_draft(page: &Page<'_>, text: &str) {
    page.eval(&format!(
        "{{
            const box = document.querySelector('#reply-text');
            box.value = {};
            box.dispatchEvent(new Event('input', {{ bubbles: true }}));
        }}",
        js(text)
    ))
    .await
    .expect("types a draft");
}

/// The dock mic is recording (the strip shows it live, and a second has
/// passed on its clock, so there is audio to send).
pub const RECORDING: &str = "document.querySelector('.mic-fab')?.getAttribute('aria-pressed') === 'true' \
     && !!document.querySelector('.dictation-live:not(.dictation-opening)') \
     && !['', '0:00'].includes(document.querySelector('.dictation-live .rec-time')?.textContent ?? '')";

/// Taps the dock's mic as a finger does (pointer down, pointer up).
///
/// # Panics
/// If there is no dock mic.
pub async fn tap_mic(page: &Page<'_>) {
    page.eval(
        "{
            const fab = document.querySelector('.mic-fab');
            const at = { bubbles: true, button: 0, pointerId: 1, isPrimary: true };
            fab.dispatchEvent(new PointerEvent('pointerdown', at));
            fab.dispatchEvent(new PointerEvent('pointerup', at));
        }",
    )
    .await
    .expect("taps the dock mic");
}

/// Sets the page's visibility as the browser would when the phone switches
/// apps, firing `visibilitychange`.
///
/// # Panics
/// If the page can't be evaluated.
pub async fn set_visible(page: &Page<'_>, visible: bool) {
    let state = if visible { "visible" } else { "hidden" };
    page.eval(&format!(
        "Object.defineProperty(document, 'visibilityState', {{ configurable: true, get: () => '{state}' }}); \
         Object.defineProperty(document, 'hidden', {{ configurable: true, get: () => {} }}); \
         document.dispatchEvent(new Event('visibilitychange'))",
        !visible
    ))
    .await
    .expect("sets visibility");
}

/// How many prompts with exactly `text` the bridge has executed (its audit
/// log).
#[must_use]
pub fn prompts_executed(bridge: &Bridge, text: &str) -> usize {
    bridge
        .audit()
        .lines()
        .filter(|line| {
            serde_json::from_str::<Value>(line).is_ok_and(|entry| {
                entry["phase"] == "execute" && entry["kind"] == "prompt" && entry["detail"] == text
            })
        })
        .count()
}

/// Waits until the bridge has executed a prompt with exactly `text`.
///
/// # Panics
/// If it never does.
pub async fn prompt_reaches_bridge(bridge: &Bridge, text: &str, what: &str) {
    until(bridge, &format!("{what} reaches the bridge"), || {
        prompts_executed(bridge, text) > 0
    })
    .await;
}
