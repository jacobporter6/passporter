---
description: Building, testing and installing Passporter's Android app, a Tauri 2 shell around the PWA. One script installs the toolchain under your user at pinned versions; two just recipes build the APK and run its checks.
sidebar:
  order: 2
sources:
  - apps/android/
  - deploy/android/setup.sh
  - mise.android.toml
  - justfile
---

# Android app

The Android app ([design §3](../design.md), decision D8) is a Tauri 2
shell in `apps/android`. It does not bundle the PWA: on first launch it
asks for your bridge's HTTPS address (the one you open in Chrome), then
loads the PWA from there, so the app shows exactly what the browser shows
and picks up PWA updates without a new APK. You pair the phone in the PWA
as usual; in the app the device key is made and kept by native code (see
[Device identity](#device-identity)). Other native plugins (notifications,
UnifiedPush, the recorder, Android Auto) are later additions.

Building it needs a JDK, the Android SDK and NDK, the Rust Android
targets and the Tauri CLI. One script installs all of them under your
user, with no sudo and no Android Studio.

## Set up from scratch

You need `git`, `mise`, `rustup` (1.28 or later), `curl`, `unzip` and
`sha256sum`, and about 3.5 GB of disk. From the repo checkout:

```sh
mise trust          # once, so mise reads the repo's config files
just android-setup  # or: deploy/android/setup.sh
```

The script accepts the licences of the packages it installs on your
behalf, without asking, so read Google's
[Android SDK licence](https://developer.android.com/studio/terms) before
you run it. It is safe to re-run: each step
checks what is already there, and a second run changes nothing.

The Android tools live in `mise.android.toml`, which mise reads only when
`MISE_ENV=android`, so working on the bridge or the PWA never fetches a JDK.
In the shell you build the app from:

```sh
export MISE_ENV=android   # with mise activated, cd into the repo sets the rest
eval "$(mise env --shell bash)"   # or load them once, without activation
cargo tauri --version
```

That sets `ANDROID_HOME`, `NDK_HOME` and `JAVA_HOME` and puts `java` and
`cargo-tauri` on `PATH`. It replaces any `ANDROID_HOME` you already have
(for example Android Studio's `~/Android/Sdk`) inside the repo, so the
script installs its own SDK rather than changing yours.

## Build

```sh
just android-build            # debug APK, arm64: target/android/passporter-debug.apk
just android-build debug x86_64   # for an x86_64 emulator
just android-build release    # signed release APK (see Release builds)
```

The recipe loads the Android environment itself (`MISE_ENV=android`), so
it works from any shell. It builds one ABI (`aarch64` by default, which
fits current phones; also `armv7`, `i686`, `x86_64`), checks the result
with `apksigner verify`, and prints the APK's path and size. A debug build
is signed with Gradle's automatic debug key (`~/.android/debug.keystore`)
and is about 40 MB, most of it the unstripped Rust library.

## Test

```sh
just android-test
```

It runs clippy on the shell and the plugins for the Android target, their
Rust tests (also part of `just verify`), and Gradle's JVM unit tests (the
app's `testUniversalDebugUnitTest`, in `gen/android/app/src/test/`, and each
plugin's `testDebugUnitTest`, in `plugins/<name>/android/src/test/`). The
JVM tests need the Kotlin files a build generates, so run `just android-build`
once in a fresh checkout first. Nothing
here needs a device. `just verify` and CI never touch the Android
toolchain: on the host the crate builds without Tauri.

## Install on a phone

The APK is sideloaded (decision D3). Either:

- **adb**: turn on *Developer options → USB debugging* on the phone, plug
  it in, and run
  `$ANDROID_HOME/platform-tools/adb install -r target/android/passporter-debug.apk`.
- **File transfer**: copy the APK to the phone (USB, a file share), open it
  in the Files app, and allow *Install unknown apps* for that app when
  Android asks.

Open Passporter, enter the bridge's address (for example
`https://bridge.example.ts.net`), then pair on the next screen as in the
browser. The app remembers the address. To use a different bridge,
long-press the app's icon, choose *Bridge*, then *Change address*: the app
forgets the address and this phone's device key, and restarts on the
Connect page, where you connect and pair again
([The Bridge page](#the-bridge-page)). The old bridge still lists the
device until you revoke it there (`passporter devices revoke`).

A debug and a release build use different keys, so Android refuses to
install one over the other: uninstall first.

A debug build is for development: it is debuggable, and its webview can be
inspected over USB, so anyone with your unlocked phone and a cable can run
script as your paired device or copy its data. For everyday use install a
release build, and turn *USB debugging* off again after installing.

## Release builds

A release APK is signed with your own key, which only you hold. Create it
once, outside the repo (`keytool` comes with the JDK; run it in the
Android environment, and choose the passwords when it asks):

```sh
mkdir -p ~/.local/share/passporter
keytool -genkeypair -v -storetype PKCS12 \
    -keystore ~/.local/share/passporter/android-release.jks \
    -alias passporter -keyalg EC -groupname secp256r1 -validity 10000 \
    -dname "CN=Passporter"
```

Back it up: an update must be signed with the same key, or Android makes
you uninstall (and lose the app's data) first. Then build with the four
variables set, for example from a password manager:

```sh
export PASSPORTER_ANDROID_KEYSTORE=~/.local/share/passporter/android-release.jks
export PASSPORTER_ANDROID_KEY_ALIAS=passporter
export PASSPORTER_ANDROID_KEYSTORE_PASSWORD=...   # from your password manager
export PASSPORTER_ANDROID_KEY_PASSWORD=...        # the same as the keystore's, for PKCS12
just android-build release   # target/android/passporter-release.apk
```

The recipe refuses to start if any of them is unset or the keystore is
missing. Gradle reads them from the environment only; nothing is written
to the repo or to Gradle's files.

## Network security

The app only talks HTTPS, and trusts only the certificate authorities that
ship with Android (`res/xml/network_security_config.xml`): plain HTTP is
off in every build, and CAs you install yourself in Android's settings are
ignored. Every recipe in [Deploying](../deploy.md) serves the phone a
publicly trusted certificate (Tailscale's `*.ts.net`, Let's Encrypt, or a
public wildcard on your own edge), so this needs nothing on the phone. The
insecure MVP mode (plain HTTP over WireGuard) does not work in the app;
use the browser for it.

If your bridge's proxy really serves a certificate from a private CA, build
an APK that trusts that CA as well: put its certificate (PEM) at
`apps/android/gen/android/app/src/main/res/raw/bridge_ca.pem` and add
`<certificates src="@raw/bridge_ca" />` next to the `system` entry in the
config. That build then trusts the CA for every site, so keep the CA's key
safe and the change out of commits (`SecurityConfigTest` rejects it on purpose).

## How the shell works

| Path in `apps/android` | What |
|---|---|
| `src/lib.rs`, `src/shell.rs` | The Tauri app: the main window, the app commands (`connect`, `bridge`, `forget_bridge`), the navigation guard, the bridge capability. `shell.rs` builds for Android only |
| `src/native.rs` | The Tauri plugin `passporter-shell`: registers the app's own Kotlin side (`ShellPlugin`, below) and talks to it. Android only |
| `src/origin.rs`, `src/navigation.rs`, `src/store.rs`, `src/destination.rs` | The rules, in plain Rust tested on any host: what counts as a bridge origin, which pages the webview may show, where the origin is saved, which page the webview opens on, where a notification tap, the shortcut, a failed load or a rebuilt webview sends it |
| `connect/` | The bundled pages (`frontendDist`): the Connect page (`index.html`) and the Bridge page (`bridge.html`) |
| `gen/android/app/src/main/java/dev/passporter/app/` | `MainActivity` (wraps each new webview's client); `ShellPlugin` (the shell's Kotlin side: intents, restart), `ShellRequest` (what it asks the shell to show), `ShellWebViewClient` (wraps wry's client: refused links to the browser, failed loads, history), `WebViewBuilds` (tells the shell when wry rebuilds the webview), `BridgeShortcutActivity` (the shortcut's trampoline) |
| `plugins/device/` | The device identity plugin: Keystore key, native bridge client, the PWA's `device` commands ([Device identity](#device-identity)) |
| `tauri.conf.json` | App id `dev.passporter.app`, name, the Connect page's CSP |
| `gen/android/` | The Gradle project from `cargo tauri android init`, trimmed: manifest, theme, icons, network security config, signing, JVM tests. Build output and the files Tauri regenerates on each build are gitignored |

Only Passporter's own pages reach native IPC, by three fences:

1. **Navigation.** The webview shows the bundled Connect page until you
   connect, then only the paired origin, and refuses every other
   navigation (`navigation.rs`). Back is a history navigation the guard
   does not see, so the bundled pages remove themselves from history when
   they open the bridge. Once paired, only the shell itself opens its
   Bridge page (`destination.rs`): wry checks the shell's own loads against
   the guard too, so the shell names the page's exact URL first and the
   guard lets that one load through once (`navigation::Guard`). No page
   can navigate there itself (`ShellWebViewClient` refuses it before the
   guard), and the history never holds a bundled page beside another one,
   so Back and Forward cannot reach it either. A bridge URL that fails to
   load does bring it up ([The Bridge page](#the-bridge-page)).
   The PWA's CSP (`default-src 'self'`) keeps other origins out of frames
   and scripts. A refused link you tapped opens in the browser instead
   ([Links](#links-taps-and-the-bridge-page)); the webview itself still
   never leaves the bridge.
2. **Capability.** At launch, or when you connect, the shell grants a
   remote capability to exactly `https://<bridge>/*` in the main window.
   The address must be a plain DNS name or IPv4 address, so the pattern
   holds no wildcard syntax; `localhost` names are refused.
   It lists the plugin permissions the PWA may use: `device:default`.
   It is granted only on a WebView with `WEB_MESSAGE_LISTENER` (below).
3. **App commands.** The app has no ACL manifest, so Tauri refuses its own
   commands (`connect`, `bridge`, `forget_bridge`) to every remote page;
   only the bundled pages can call them. The `passporter-shell` plugin has no
   permissions at all, so no page reaches its Kotlin commands (`watch`,
   `restart`); only Rust calls them.

Everything on the bridge's origin counts as Passporter, so serve nothing
else on that host name: another app behind the same proxy name would get
the same access.

The address is saved in the app's private storage (`bridge-origin` in the
data directory); it is not a secret. Native code reads the same file, so
the webview and native sign-ins always use the same bridge. The device key
is in the Android Keystore and its id in `noBackupFilesDir`; session tokens
stay in memory. Neither backups nor a
device-to-device transfer copy the app's data (`allowBackup="false"` plus
backup and data-extraction rules that exclude everything), so the key
stays on the phone; a new phone pairs afresh.

Fence 1 does not see POST navigations (Android does not report them); the
pages' CSP `form-action` (`'self'` on the bridge, `'none'` on the bundled
pages) covers form submissions instead.

Fence 2 tells frames apart only on a current WebView: Tauri learns each
frame's own origin through `WEB_MESSAGE_LISTENER` (Android System WebView
86 and later). On an older WebView every message is labelled with the URL
the main page last started loading, so only the PWA's CSP would keep a
foreign frame from borrowing the bridge capability, and a message sent
while the Connect page is being replaced could even count as local. So the
shell asks the device plugin about the feature before granting anything,
and on an old WebView grants nothing: logcat says why, and the PWA keeps
its device key in WebCrypto as in Chrome. The app never adds a local
capability. The app commands still answer local pages on an old WebView:
a message the bridge's page sends while the shell replaces it with the
Bridge page can count as local. The bridge's own PWA sends none, but a
bridge page can bring up the Bridge page by loading a URL that fails; the
worst would be `forget_bridge`.

## Notifications

Agent news arrives as an encrypted notice message
([design §6.6](../design.md#66-notifications-and-android-auto)). The Kotlin
package `dev.passporter.notify` (in `gen/android/app/src/main/java/`)
turns a decrypted message into notifications:

| File | What |
|---|---|
| `Notice.kt`, `NoticeParser.kt`, `PlainText.kt` | The message as typed Kotlin, parsed strictly where the bridge is strict (types, ids) and tolerantly where it is tolerant (unknown fields, unknown states); texts cleaned and bounded again |
| `NotificationPlanner.kt` | Pure: which notices of a message to post (one per agent, unknown states skipped) |
| `NoticeNotification.kt` | One `MessagingStyle` conversation per agent, with its shortcut and `LocusId`; channels *Needs you* (high, blocked agents) and *Finished* (default) |
| `NoticeNotifier.kt`, `RecentPings.kt` | The entry point, `NoticeNotifier(context).show(payload)`, and the pings of recent messages |
| `NotificationPermission.kt` | Asks for `POST_NOTIFICATIONS` (Android 13+) when you return to the app after connecting to a bridge |

**Only news disrupts.** The bridge sends a notice only for a new event,
so every notice of a new message alerts, replacing the agent's last
notification. A message delivered twice (a push retry) carries the same
`ping`, and the app remembers the last 32, so a retry never buzzes. A
message lists only the agents with news since the last one, so an agent it
leaves out keeps its notification. The text is the agent's name, its one
line and the *Decide* line, never more (the car reads the same). A tap
opens the app with the intent `passporter://agent/<id>` (the id is one
percent-encoded path segment: read it with `NoticeNotification.agentToOpen`).

A tap opens that agent's screen ([Links, taps and the Bridge
page](#links-taps-and-the-bridge-page)); old news is cleared when you open
the app ([Answering from the shade](#answering-from-the-shade)). Known gap:
each agent's conversation shortcut stays; Android drops the oldest once
the app reaches its shortcut limit.

A debug build can post sample messages through the same path, from the dev
machine with the phone on adb (`-n` targets the receiver, which only adb's
shell may call):

```sh
adb shell am broadcast -n dev.passporter.app/dev.passporter.notify.SampleNoticeReceiver --es sample blocked
```

The samples are `blocked` (alerts on *Needs you*), `blocked-again` (the
same agent at a new dialog: alerts again, in place), `finished` (on
*Finished*, quieter), `decide` (that turn's *Decide* line: alerts once
more) and `many` (four agents, one conversation each; one name and line
cut at their bounds). Each broadcast is a new message; add `--es ping
1234` to two broadcasts to send the same message twice, and the second
must not buzz. Release builds leave the receiver out. The sample agents are
not on your bridge, so their actions end as *That agent is gone*, and
opening the app clears them ([old news](#answering-from-the-shade)).

### Answering from the shade

A notice can be answered without opening the app (package
`dev.passporter.notify.actions`). What it offers:

| Notice | Action | Sends | Semantic action | `RemoteInput` key |
|---|---|---|---|---|
| Blocked, with a pending id | *Allow once* | `choose` of the dialog's low-risk `yes` | none | — |
| | *Deny* | `choose` of its low-risk `no` (else `no_feedback`) | none | — |
| | *Deny with note* | `choose` of `no_feedback`, the text as `feedbackText` | none | `dev.passporter.notify.TEXT` |
| Finished | *Reply* | `prompt` with the text | `SEMANTIC_ACTION_REPLY` | `dev.passporter.notify.TEXT` |

*Allow once* appears only when the notice shows the whole ask: a line cut
by the bridge or the app (ending in `…`) could hide the rest of a command,
so it gets *Deny* and *Deny with note* only. A notice does not say what
kind of dialog it is, so a question or a plan approval offers the same
actions: a Yes/No question takes *Allow once* as its Yes, others end as
*Answer this one in the app*. A dialog without a pending id (the bridge could not parse it), or a state
this app predates, has no actions: the tap opens the app. *Always allow* and
other elevated options are never offered. The dialog answers carry no
semantic action, so nothing in the system triggers them by guessing; none
shows a user interface. Nothing is answered while the phone is locked:
Android 12 and later ask you to unlock first, and on every version the
receiver refuses while the keyguard shows (*Unlock your phone to answer*),
also when a watch or another app fires the action, and also while Smart
Lock keeps the phone unlockable without a credential. Mark-as-read is
not offered: the bridge has no API for it (a turn is unread until herdr
sees the pane).

Each tap is a broadcast to `NoticeActionReceiver` (explicit, not exported;
pending intents immutable except the two with a `RemoteInput`). It makes
an `OutboxJob` with a fresh idempotency key, appends the answer to the
conversation as your message with *Sending…* (and no actions, so it cannot
be answered twice), and queues it:

| File | What |
|---|---|
| `NoticeAction.kt` | Which actions a notice offers (pure), and the `ActionRequest` in the intent's data, `passporter://action/<action>/<agent>[/<pending>]` |
| `NoticeActions.kt`, `NoticeActionReceiver.kt` | The notification actions, and their receiver |
| `OutboxJob.kt` | The job, and the text rules (`ReplyText`: at most 4 KiB, a note on one line) |
| `Outbox.kt`, `OutboxData.kt`, `OutboxWorker.kt` | The native outbox: WorkManager jobs, network-bound, with exponential backoff from 30 s and at most 8 attempts (roughly an hour online); a `Retry-After` longer than the backoff is followed for a dialog answer, up to 15 min (a reply keeps the backoff, so a later reply behind it never overtakes it). The unique work is per question: a dialog takes one answer (a second tap queues nothing), replies to an agent go one after the other |
| `OutboxSender.kt`, `SendOutcome.kt`, `ActionBody.kt`, `BridgeState.kt`, `Bridge.kt`, `FrozenBodies.kt` | One attempt: reads the agent (and the dialog) with `DeviceAuth`, posts `POST /v1/actions` with `origin: "notification"`, classifies the answer |
| `ActionFeedback.kt` | The notification while sending, after (removed when sent), or with the reason it was not sent |
| `Reconciler.kt` | Clears old news when the app comes to the foreground and after a sent answer |

A notice has no `expectedSeq` or option ids, so the first attempt reads
them, as the bridge's car replies do: the agent from `GET /v1/agents`, and
for a dialog answer `GET /v1/agents/:id/pending`, which must still show the
notice's dialog. A reply goes only while the agent is ready (`done`
since before you replied, or `idle`: herdr marks a turn seen when you
look at the pane). The body is then frozen, and a retry
resends it unchanged, so the bridge replays a lost answer instead of
deciding it again. The bridge's own rules still apply on top
(`origin: "notification"`: no prompt into a dialog, no `!`/`/`/`#` prompt).

| Bridge's answer | Shown |
|---|---|
| sent | the notification is removed |
| 409, or the dialog changed before sending | *Already answered* (a reply: *The agent moved on: open the app*) |
| 422 `no_pending_input` or `unknown_option` (the dialog went meanwhile) | *Already answered* |
| 403, 400, another 422, or no low-risk option to choose | *Answer this one in the app* |
| `failed` (herdr failed after sending) | *Not sure it arrived: open the app* (never resent) |
| no answer, 5xx, 429, a sign-in the bridge refused | retried; after the last attempt *Couldn't reach the bridge: open the app*, or *Not sure it arrived* once the action itself was sent |
| not paired or revoked | *Not paired: open the app* |

If news replaced the notification meanwhile (a new dialog, a new turn), the
news stays and the older answer's outcome is not shown.

**Old news is cleared.** The bridge sends no "resolved" message, so when
the app comes to the foreground, and after an answer was sent, a worker
reads `GET /v1/agents` (only if a notification is showing) and removes a
dialog's notification once the agent is no longer blocked, a finished
turn's once it is no longer unread, and any whose agent is gone (an
agent herdr cannot tell about keeps its notification). An answer's
notification is cleared once its job is no longer queued (a job lost
while *Sending…*), and so are the frozen bodies of jobs that are gone. A
final outcome (*Already answered*, *Not sure it arrived*, ...) stays until
you dismiss or tap it, or news replaces it; if the notification was gone
by then (swiped, or tapped into the app), only an outcome you must act on
(*Not sure it arrived*, *Not sent*, *Not paired*, ...) comes back. A tap
on a notice that news replaced meanwhile queues nothing: each notification
names its question (`EXTRA_QUESTION`: the pending id, or `turn`).

Answers are ordinary WorkManager jobs, not expedited ones: an expedited job
needs a foreground-service notification on Android 11 and older. A reply
from the shade wakes the app, so the job usually runs at once; under Doze
it can wait for the next maintenance window.

## Links, taps and the Bridge page

The shell's Kotlin side (`ShellPlugin`) tells the shell what to show; the
shell (`destination.rs`) builds every URL from the saved bridge origin, and
ignores every request until a bridge is paired.

- **A notification tap** opens `MainActivity` with the agent in the intent.
  Cold (the app was closed), the window starts on `https://<bridge>/agent/<id>`;
  warm, the shell sends the webview there. An intent Android delivers
  again when it restores the app from Recents is ignored. The id is
  checked against the bridge's id rules on both sides (`AgentId`), and the URL against the
  navigation guard.
- **A rebuilt webview.** Android re-creates `MainActivity` for a
  configuration change the manifest does not list (a new font or display
  size, or a switch between left-to-right and right-to-left languages;
  rotation and dark mode are listed, so they keep the webview). wry then builds a new webview and loads the first page
  it was given at launch again (`FirstPage` in `destination.rs`). That page
  is the bridge's, except on the launch where you paired: there it is the
  Connect page, which the guard refuses once paired. So `WebViewBuilds`
  tells the shell about every webview after the process's first, and the
  shell then opens the bridge's start page (the Bridge page, if it does not
  load). The guard is unchanged: the bridge needs no allowance. A process
  Android ended in the background starts afresh from Recents, on the saved
  bridge.
- **A link to another site** that you tap in the PWA (the link confirm's
  *Open*) opens in your browser. The guard still refuses it in the webview;
  `ShellWebViewClient` then opens it with an Android `VIEW` intent only for
  a top-level navigation you started (a tap, not a redirect, a frame or a
  script on its own) to an `https://` or `http://` address. Every other
  scheme (`intent:`, `file:`, `content:`, `javascript:`, ...) is refused and
  goes nowhere. wry does not tell the shell whether a navigation was a tap,
  which is why this part is Kotlin, wrapping wry's `WebViewClient`
  (`ShellWebViewClientTest` fails if wry starts overriding a method the
  wrapper does not pass on).

### The Bridge page

The bundled Bridge page (`connect/bridge.html`) shows the bridge's
address, a button back to it, and *Change address*. The shell opens it

- when a bridge page fails to load in the main frame: no network, the
  bridge down, or a 5xx from its proxy. This is the first-launch case,
  before the PWA's service worker has cached the app (once it has, the PWA
  opens offline and shows its own offline state). The button reads
  *Retry*;
- from the launcher: long-press the icon and choose *Bridge* (a static
  shortcut, `res/xml/shortcuts.xml`). Android starts a static shortcut
  with `FLAG_ACTIVITY_CLEAR_TASK`, which would destroy a running
  `MainActivity` and its webview, so the shortcut opens
  `BridgeShortcutActivity`, in a task of its own, which passes the request
  on to `MainActivity`.

The page has a fresh history (no bundled page shares one with another
page): Back from it leaves the app rather than reloading a bridge page
that failed, and *Retry* or *Open Passporter* replaces it with the
bridge. Both go to the bridge's start page, not to the agent a failed tap
asked for.

*Change address* asks once more, then calls `forget_bridge`: the shell
deletes the saved address and asks `ShellPlugin`, which forgets the
native pairing (`DeviceAuth.forget`: the Keystore key, the device id and
cached tokens) and restarts the app in a new process, on the Connect
page. The restart is deliberate:
Tauri cannot revoke the capability it granted to the old origin, so only a
new process starts without it. The old bridge's data in the webview
(its service worker cache, IndexedDB) stays under its own origin, which
the app never opens again; clearing the app's storage removes it. The old
bridge still lists the device until you revoke it there.

Any app can send `MainActivity` the tap or shortcut intent, because a
launcher activity is exported. The worst that does is open an agent's
screen or the Bridge page; changing the address always needs your two
taps on the page.

## Device identity

Inside the APK native code owns the device, so notification replies, push
registration and Android Auto can sign in with the app closed
([design §8](../design.md#8-security)). The plugin in `plugins/device`
(Kotlin package `dev.passporter.device`):

- makes a non-exportable P-256 key in the Android Keystore at pairing
  (StrongBox when the phone has one, the TEE otherwise), with no
  user-authentication requirement so it works with the screen locked
  (biometric step-up is a later phase);
- pairs and signs in itself (`/v1/auth/pair`, then challenge → session
  with a chosen scope, `app` or `car`), over the platform's
  `HttpsURLConnection` (no HTTP library in the APK), and caches each
  scope's token in memory until two minutes before it expires;
- forgets the pairing when the bridge answers `410 Gone` (revoked).

The PWA reaches it through `plugin:device|status`, `pair`, `session` (a
fresh **app**-scope token; the webview never gets a car token) and
`forget` (`crates/web/src/pair/native.rs`). Without the plugin (Chrome, an
old WebView) the PWA uses WebCrypto, unchanged.

Native code calls `DeviceAuth` directly, from a worker thread (every call
may do network or Keystore work):

```kotlin
import dev.passporter.device.DeviceAuth
import dev.passporter.device.DeviceAuthException
import dev.passporter.device.Method
import dev.passporter.device.Scope

val auth = DeviceAuth.get(context)
// An authenticated request; a 401 is retried once with a fresh token.
val response = auth.send(Scope.APP, Method.POST, "/v1/agents/$id/reply", json)
// Or just a token, e.g. for a header you set yourself:
val token = auth.token(Scope.CAR)       // token.authorization() == "Bearer …"
// If the bridge refuses it: auth.invalidate(token), then ask again.
```

Failures are `DeviceAuthException` subclasses: `NotPaired`, `Forgotten`
(revoked; the pairing is gone), `Unreachable` and `ServerError` (retry
later), `RateLimited`, `Refused`, `Keystore`, and so on. Another plugin uses
it by depending on the library in its `android/build.gradle.kts`:
`implementation(project(":tauri-plugin-device"))`.

## What it installs, and where

| What | Version | Where | Pinned in |
| --- | --- | --- | --- |
| JDK (Temurin) | 21 | `~/.local/share/mise/installs/java/` (`JAVA_HOME`) | `mise.android.toml` |
| Tauri CLI (`cargo-tauri`) | 2.12.4 | `~/.local/share/mise/installs/` | `mise.android.toml` |
| Android command-line tools | 21.0, SHA-256 checked | `~/.local/share/android-sdk/cmdline-tools/latest` | `setup.sh` |
| `platform-tools` (adb) | latest at install (unpinned) | `~/.local/share/android-sdk/platform-tools` | `setup.sh` |
| SDK platform | `android-37.0` | `~/.local/share/android-sdk/platforms` | `setup.sh` |
| Build tools | 37.0.0 | `~/.local/share/android-sdk/build-tools` | `setup.sh` |
| NDK | r29 (29.0.14206865) | `~/.local/share/android-sdk/ndk/` (`NDK_HOME`) | `setup.sh`, `mise.android.toml` |
| Rust targets | `aarch64`, `armv7`, `i686`, `x86_64` Android | rustup, for the toolchain in `rust-toolchain.toml` | `setup.sh` |

`ANDROID_HOME` is `~/.local/share/android-sdk`, set in
`mise.android.toml`'s `[env]`. The NDK version appears in both `setup.sh`
and `mise.android.toml`; the script stops if they disagree. `platform-tools`
is the one package not pinned, because sdkmanager only offers the latest;
builds don't use it, only talking to a device does. The command-line tools
are held at 21.0: from 22.0 their `sdkmanager` downloads a separate,
unpinned "Android CLI" that collects usage metrics. The Rust targets go
through rustup rather than `rust-toolchain.toml`, so people who never build
the app don't download them.

Gradle and the Android Gradle plugin are not installed here: the app's
Gradle wrapper fetches them into `~/.gradle` on its first build (Gradle
9.6.1, checked against its pinned SHA-256; AGP 9.3.1 and the AndroidX
libraries are pinned by version only, without Gradle dependency
verification). A debug
build also creates the debug signing key `~/.android/debug.keystore`.

## Upgrading

Change the pins and re-run the script. It doesn't remove the old
versions: delete their directories under `~/.local/share/android-sdk`. For a new NDK, change `NDK_VERSION`
in `deploy/android/setup.sh` and `NDK_HOME` in `mise.android.toml` together. For
new command-line tools, take the build number and SHA-1 from Google's
[repository manifest](https://dl.google.com/android/repository/repository2-3.xml),
check the download against the SHA-1, and pin its SHA-256.

## Uninstall

```sh
rm -rf ~/.local/share/android-sdk
MISE_ENV=android mise uninstall java@temurin-21.0.13+9.0.LTS github:tauri-apps/tauri@tauri-cli-v2.12.4
# from the repo checkout, so rustup picks the toolchain in rust-toolchain.toml
rustup target remove \
    aarch64-linux-android armv7-linux-androideabi i686-linux-android x86_64-linux-android
```

Builds may also have left `~/.gradle` (Gradle caches) and `~/.android`
(sdkmanager settings and the debug keystore). Remove them too if nothing
else on the machine uses them.
