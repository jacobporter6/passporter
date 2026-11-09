# web: the Passporter PWA

A mobile-first, voice-first PWA in Rust: Leptos 0.8 (CSR) compiled to
WASM with trunk. It runs standalone in Chrome on the phone, and later inside
the Tauri shell (design §3, §7).

```sh
just web-build    # release build into crates/web/dist
just web-size     # build, then fail if the gzipped WASM is over 400 KiB
just web-serve    # serve on 127.0.0.1:8080 with the production CSP
```

The bridge serves `dist/` in production. trunk, wasm-bindgen and wasm-opt
versions are pinned in `mise.toml` and `Trunk.toml`.

## Layout

| Path | What |
|---|---|
| `src/lib.rs`, `src/main.rs` | Library with all logic, plus a two-line binary for trunk |
| `src/bin/notice.rs` | The notice WASM (`notice.js`, `notice_bg.wasm`, built by trunk as a worker): the service worker's push wake, and the Push API glue the page loads on demand |
| `src/bin/voice.rs`, `src/bin/pair.rs`, `src/bin/extras.rs` | On-demand WASM modules (ES modules loaded by `public/module-boot.js`, see `src/lazy.rs`): dictation, the Connect screen, and the rarely used surfaces (the New agent and Runbooks sheets, Settings, the link confirm; `src/extras/`) |
| `src/app.rs` | Root component: state, router, shell |
| `src/routes.rs` | Typed route table (`AppRoute`): every link and route param goes through it |
| `src/api/` | Typed bridge client over `crates/protocol`; token handling behind `TokenSource` |
| `src/session.rs` | MVP auth: the pasted bridge token (IndexedDB + signal), the `TokenSource` all calls use |
| `src/live/` | `WS /v1/stream` (StreamAuth first, backoff, foreground reconnect), agent list, segment rules |
| `src/idgen.rs` | v4 idempotency keys from `crypto.getRandomValues` (no `randomUUID` over plain HTTP) |
| `src/store/` | Typed key/value store over IndexedDB: snapshot cache and outbox |
| `src/push/` | Web Push: the Settings toggle's state machine and browser glue, the wake and the notification text |
| `src/triage/`, `src/agent/`, `src/pair/`, `src/settings/` | Screens |
| `src/ui/` | Shared state, dock, mic button, toasts, icons |
| `style/tokens.css` | Design tokens (colour, spacing, type, radii, motion) |
| `style/app.css` | Every style in the app |
| `public/` | `boot.js` loader, `sw.js` service worker, manifest, icons |
| `scripts/` | `stamp-sw.sh` (trunk hook), `check-size.sh` (size gate), `gen-icons.sh` |

Everything except browser glue is unit-tested on the host:
`cargo nextest run -p web`. `web-sys` compiles on every target and is only
*called* in the browser, so `cargo build --workspace` works on the host.

## Content Security Policy

The app must run under this policy, sent by the bridge as a response header
on every document (and mirrored in a `<meta>` tag in `index.html`):

```
default-src 'self';
script-src 'self' 'wasm-unsafe-eval';
style-src 'self';
connect-src 'self';
img-src 'self';
object-src 'none';
base-uri 'none';
form-action 'self';
frame-ancestors 'none'
```

`frame-ancestors` only works as a header, so it is left out of the `<meta>` tag.

What each part means for this crate:

- **No inline scripts.** trunk's default loader is an inline
  `<script type="module">`. `Trunk.toml` swaps it (via `pattern_script` and
  `pattern_preload`) for `<script src="/boot.js">` plus preload links for the
  hashed JS and WASM. `boot.js` reads those links and calls the wasm-bindgen
  initializer. It is the only hand-written JS apart from the service worker.
  The service worker loads the push wake's glue (`notice.js`) with
  `importScripts`, also from `'self'`, and compiles `notice_bg.wasm`, so if
  the bridge sends a CSP on `/sw.js` it needs `'wasm-unsafe-eval'` too (the
  policy above has it).
- **`'wasm-unsafe-eval'`** lets the page compile WebAssembly. It does not
  allow `eval` or `new Function`. The generated JS uses neither (check:
  `grep -E 'eval|new Function' dist/*.js`).
- **No inline styles.** Nothing sets a `style` attribute. All styling is in
  `style/*.css`, served from `'self'` with SRI hashes. SVG icons are styled by
  class.
- **`connect-src 'self'`**: the API client only uses relative paths (`/v1/...`),
  and the bridge serves both the app and the API. API fetches also use
  `redirect: "error"` (a redirect could carry the token elsewhere),
  `cache: "no-store"` and `credentials: "omit"`.
- **`img-src 'self'`**: no `data:` URLs; every image is a file in `public/icons`.
- **Untrusted text**: agent output, QR contents and pairing input are only
  ever rendered as text nodes. Leptos templates use `innerHTML` for the app's
  own static markup only (compiled from `view!`), never for data.
- `trunk serve`'s live-reload client is inline, so `Trunk.toml` turns
  autoreload off and serves with the same CSP header. Dev matches prod; reload
  by hand.

## PWA

- `manifest.webmanifest`: standalone, portrait, dark theme. Icons are 192/512
  PNG, a 512 maskable PNG and an SVG (`scripts/gen-icons.sh` regenerates the
  PNGs from the SVGs).
- `sw.js` is a thin service worker. It **caches the app shell** and shows
  **push notifications**; the push logic is Rust:
  - On install it precaches the build's files with `cache: "reload"`, so
    nothing comes from a stale HTTP cache. The list and the cache version
    are stamped in by the `stamp-sw.sh` trunk hook, and the version is a hash
    of every staged file.
  - A new version does **not** take over by itself. It waits, and the app
    shows "New version available: Reload". Reload posts `skip-waiting`; the
    new worker activates, deletes older `passporter-shell-*` caches, and the
    page reloads on `controllerchange`. A page never loses the cache it was
    built against while it runs.
  - Browsers look for a new worker only on navigation, which an installed
    PWA resumed from the background never makes. So the app also asks
    (`registration.update()`) every time its stream reconnects, which a
    bridge restart after a deploy and a return to the foreground both
    cause, and compares its build with the bridge's (`GET /v1/version`,
    see `src/version.rs`). A different commit shows the same banner even
    before the new worker has installed; Reload then fetches it, waits for
    it to install, and hands over, falling back to a plain reload. If the
    reload still leaves the app behind (the bridge serves an older PWA
    than it runs), the banner stays away for that launch and Settings says
    to rebuild the PWA.
  - Both builds are stamped at compile time with the git commit (`-dirty`
    for a modified tree, `unknown` without git) and the build time, by
    `build.rs` (`build/version.rs`, shared with the bridge). Settings shows
    them under About.
  - Precached files are served cache-first. Navigations to app routes get
    `/index.html`, so the app opens with no signal. `/sw.js`, `/boot.js`,
    `/manifest.webmanifest` are never answered with the shell. `/v1/*` and
    everything else go to the network untouched.

  - On a `push` it never reads the payload (an opaque ping id). It loads
    the push wake (`notice.js` + `notice_bg.wasm` from the shell cache,
    built from `src/bin/notice.rs`), which gets a token the way the app
    does (signs in with the device key, keeping that session for later
    pings until shortly before it expires, or reads the pasted token), fetches
    `GET /v1/agents` and returns the notification: which agents are blocked
    or have an unread finished turn, by name only, under one tag, so a new
    ping replaces the last notification. Offline, unpaired, a refused token,
    a WASM that won't load, or no answer within 8 s all give a generic
    notification instead: every push shows exactly one.
  - `notificationclick` focuses an open window and posts it
    `{"open": "/agent/…"}`, which the app routes in place
    (`push::open_from_notifications`, known routes only), or opens a window at
    that path.

  Data caching and the outbox are in Rust, in IndexedDB.
- **Notifications toggle** (Settings): only in a secure context with the
  Push and Notification APIs; elsewhere it is disabled with a one-line
  reason. Turning it on asks for permission, subscribes with the bridge's
  VAPID key (`GET /v1/push/key`) and registers the subscription
  (`POST /v1/push/subscriptions`); turning it off unsubscribes, then
  deletes it on the bridge (best effort: the push service tells the bridge
  it is gone on the next send anyway). Opening Settings checks a browser
  subscription against `GET /v1/push/subscriptions`; one the bridge has
  dropped (usually because the push service said it is gone) is let go,
  and the switch reads off. Only an unreachable bridge leaves the browser
  trusted; a 404 reads "push is turned off on the bridge", and any other
  error (or a notice WASM that won't load) reads "couldn't check", which
  turning the switch on retries. Every change (on, off, the check) holds
  the `passporter-push` Web Lock, so two tabs can't interleave them, and
  the notice WASM's bridge requests time out after 8 s, so a hung tunnel
  can't hold that lock or the wake's sign-in. Forgetting the credentials
  (in the background, after they are cleared), a revoked device, a refused
  pasted token or a stored one that no longer validates turns push off.
  A 401 from the push calls (including the check, and the bridge delete
  when turning off) reaches the app's token source like any other API
  call. The states are in `push::state`.
  The Push API glue runs in the notice WASM, which the page loads on
  demand (`push::module`), so it stays out of the app's size budget; only
  the permission prompt runs in the app, because it must start inside the
  tap.
- **The bridge must serve `/index.html` for any unknown non-`/v1` path**
  (SPA fallback), so deep links like `/agent/<id>` work online before the
  service worker is installed. It must also serve `/sw.js` with
  `Cache-Control: no-cache`.
- The Rust side registers the worker only where `navigator.serviceWorker`
  exists in a secure context. It asks for persistent storage only once the
  device has something to keep (after pairing; pairing should also ask on the
  first outbox write), not on launch.
- **Out of scope here:** wiping local data (IndexedDB, caches) when the
  device is revoked or unpaired belongs with device auth.

## Smoothness budget

- WASM under **400 KiB gzipped** and JS glue under **20 KiB gzipped**
  (the push wake's `notice_bg.wasm` under 80 KiB, the on-demand
  `voice_bg.wasm` under 100 KiB, `pair_bg.wasm` under 120 KiB,
  `extras_bg.wasm` under 160 KiB and `terminal_bg.wasm` under 104 KiB),
  enforced in `just gate` by `just web-size` (`scripts/check-size.sh`). The
  same script fails if `Trunk.toml`'s `wasm_bindgen` version differs from
  the `wasm-bindgen` crate in `Cargo.lock`.
  The `web-release` cargo profile (root `Cargo.toml`) uses `opt-level = "z"`,
  fat LTO and `panic = "abort"`, then trunk runs `wasm-opt -Oz`.
  `.cargo/config.toml` (picked up because trunk runs cargo from this
  directory) builds the WASM with `--cfg erase_components`. Each component
  then returns Leptos's type-erased `AnyView` instead of one deeply nested
  view type per screen. The DOM is the same and the WASM is about 40 KB
  smaller gzipped. Serde's `flatten` and internal tags buffer the whole
  object and generate a lot of code, so the tagged types the app reads
  (`ThreadItem`, `PendingInput`, `AgentEvent`, `ActionResult`) decode in
  one pass by hand instead (`protocol::loose`).
- Code a screen needs once per device stays out of the app's WASM. The
  Connect screen's body (camera, key generation, the code and token
  forms) is `src/bin/pair.rs`, an ES module with its own Leptos runtime
  that `public/module-boot.js` loads when the screen opens and mounts below
  the page's header (`src/pair/module.rs`). It stores the device key or
  token and asks the page to adopt it, so the session stays in the app.
  Dictation's engine is split out the same way (`src/bin/voice.rs`).
  Both load through `src/lazy.rs`: one load per page, shared by every
  caller, and a failed one retried with a fresh URL on the next call.
- Measure before cutting. Build without stripping names, then run
  `twiggy top` / `twiggy dominators` on the bindgen output passed through
  `wasm-opt -Oz --debuginfo`.
- First paint doesn't wait for the WASM. `index.html` paints a CSS boot mark
  straight away, and JS and WASM are preloaded in parallel.
- Motion uses only transform and opacity. Every duration is a token, and
  under `prefers-reduced-motion` the durations drop to 0 and looping
  animations stop.

## UI principles in code

- One thumb: a fixed bottom dock holds Agents, the mic button (bottom
  centre) and Settings. The triage segments are pinned just above it.
- Every interactive element is at least 48 px (`--tap`).
- Dark by default. The UI is greys plus one hue per agent state
  (`--c-blocked`, `--c-working`, `--c-done`). Blocked is the only loud
  one, the only state ever shown as a fill; the others stay quiet.
- No horizontal page scroll at 360 px. Flex children have `min-width: 0`,
  long titles wrap, and `.app` clips overflow-x as a backstop.

## Playwright smoke test and Lighthouse (deferred)

There is no Node toolchain in the repo yet. Once there is, a mobile-emulated
(Pixel 7, 360–412 px) smoke test against `just web-serve` should check:

1. `/` renders the Agents header, the three segments (Blocked selected) and
   the dock, with no console errors and no CSP violations
   (`securitypolicyviolation` listener).
2. `document.scrollingElement.scrollWidth <= innerWidth` on every route at
   360 px.
3. Every `button, a, [role=tab]` has a bounding box of at least 48×48.
4. Client routing works: dock links, back links, `/agent/<id>` with a valid
   id and a garbage id (shows "Not found"), and deep links on reload.
5. `/pair`: manual entry rejects `ABC` with an inline error and accepts
   `abcd-ef12` (shown as `ABCD-EF12`). With a fake `BarcodeDetector` and a
   fake camera stream, a scanned URL with `#code=` is accepted.
6. The service worker installs. After `context.setOffline(true)` a reload
   still renders the shell and shows the offline banner.
7. The manifest is valid and installable (Lighthouse PWA audit, or
   `page.evaluate` against the manifest plus a `beforeinstallprompt`
   listener).
8. With `prefers-reduced-motion: reduce` emulated, computed
   `animation-duration` on `.page` is about 0.
