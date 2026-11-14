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
[Device identity](#device-identity)). Agent news arrives as native
notifications, from the bridge's own transport or through UnifiedPush
([Push](#push)); [Android
Auto](#android-auto) and the dictation recorder ([Dictation](#dictation))
are native too.

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
JVM tests need the Kotlin files a build generates, so in a fresh checkout it
runs `just android-build` once first. Nothing here needs a device.

`just gate` runs it too (`just android-gate`) when the Android toolchain is
installed (`cargo-tauri` and `$ANDROID_HOME/build-tools/37.0.0` once the
`MISE_ENV=android` env is loaded), so the gate compiles the Tauri wiring and
the Kotlin. Without the toolchain, as on CI, it prints
`android: SKIPPED: no Android toolchain` and passes. `just verify` never
touches the Android toolchain: on the host the crate builds without Tauri.

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
| `src/origin.rs`, `src/navigation.rs`, `src/store.rs`, `src/connect.rs`, `src/destination.rs` | The rules, in plain Rust tested on any host: what counts as a bridge origin (`plugins/device/android/src/test/resources/bridge-origins.json` holds the cases the Kotlin reader checks too), how connecting chooses the bridge, which pages the webview may show, where the origin is saved, which page the webview opens on, where a notification tap, the shortcut, a failed load or a rebuilt webview sends it |
| `connect/` | The bundled pages (`frontendDist`): the Connect page (`index.html`) and the Bridge page (`bridge.html`) |
| `gen/android/app/src/main/java/dev/passporter/app/` | `MainActivity` (wraps each new webview's client); `ShellPlugin` (the shell's Kotlin side: intents, restart), `ShellRequest` (what it asks the shell to show), `ShellWebViewClient` (wraps wry's client: refused links to the browser, failed loads, history), `WebViewBuilds` (tells the shell when wry rebuilds the webview), `BridgeShortcutActivity` (the shortcut's trampoline) |
| `gen/android/app/src/main/java/dev/passporter/push/` | The built-in receiver, UnifiedPush registration, incoming notices and the notification setup ([Push](#push)) |
| `plugins/device/` | The device identity plugin: Keystore key, native bridge client, the PWA's `device` commands ([Device identity](#device-identity)) |
| `plugins/recorder/` | The dictation recorder plugin: a foreground service that records takes to app-private files, the PWA's `recorder` commands ([Dictation](#dictation)) |
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
   It lists the plugin permissions the PWA may use: `device:default` and
   `recorder:default`.
   It is granted only on a WebView with `WEB_MESSAGE_LISTENER` (below).
3. **App commands.** The app has no ACL manifest, so Tauri refuses its own
   commands (`connect`, `bridge`, `forget_bridge`) to every message it
   labels remote; only the bundled pages can call them (on an old WebView,
   see below). The `passporter-shell` plugin has no
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
capability. The app commands still answer messages labelled local, and on
an old WebView a message a bridge page sends while the shell replaces it
with the Bridge page can be one (a bridge page can bring up the Bridge page
by loading a URL that fails). An ACL manifest would not help: the label is
what is wrong. So on an old WebView `forget_bridge` refuses, and *Change
address* says to update Android System WebView or to clear the app's
storage instead; `connect` refuses anyway once a bridge is chosen, and
`bridge` only reads the address.

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
| `NotificationPermission.kt` | Asks for `POST_NOTIFICATIONS` (Android 13+) when you return to the app after connecting to a bridge, or as soon as the app pairs (pairing by typed code involves no return to the app) |

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
the app ([Answering from the shade](#answering-from-the-shade)). Each
agent's conversation shortcut stays until the pairing changes
(`PairingReset` removes them all); within one pairing, Android drops the
oldest once the app reaches its shortcut limit.

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
| Finished | *Reply* | `prompt` with the text | none | `dev.passporter.notify.TEXT` |

*Allow once* appears only when the notice shows the whole ask: a line cut
by the bridge or the app (ending in `…`) could hide the rest of a command,
so it gets *Deny* and *Deny with note* only. A notice does not say what
kind of dialog it is, so a question or a plan approval offers the same
actions: a Yes/No question takes *Allow once* as its Yes, others end as
*Answer this one in the app*. A dialog without a pending id (the bridge could not parse it), or a state
this app predates, has no actions: the tap opens the app. *Always allow* and
other elevated options are never offered. No shade action carries a
semantic action, so nothing in the system (Android Auto, an assistant)
triggers them by guessing; the only semantic actions are Android Auto's
own, invisible ([Android Auto](#android-auto)). None shows a user
interface. Nothing is answered while the phone is locked:
Android 12 and later ask you to unlock first, and on every version the
receiver refuses while the keyguard shows (*Unlock your phone to answer*),
also when a watch or another app fires the action, and also while Smart
Lock keeps the phone unlockable without a credential. The shade offers
no mark-as-read: the bridge has no API for it (a turn is unread until
herdr sees the pane).

Each tap is a broadcast to `NoticeActionReceiver` (explicit, not exported;
pending intents immutable except the two with a `RemoteInput`). It makes
an `OutboxJob` with a fresh idempotency key, appends the answer to the
conversation as your message with *Sending…* (and none of the shade's
actions, so it cannot be answered twice there; Android Auto's stay), and
queues it. A tap made for another pairing than the app's now queues
nothing (*Not paired: open the app*):

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
look at the pane). "Since before you replied" compares on the bridge's
clock: the reply's time is shifted by how far the bridge's `Date` header
on that same `GET /v1/agents` is from the phone's clock, and the agent
must have changed state more than 2 s after it (the header has whole
seconds, and the answer takes a while to arrive) to count as moved on.
The body is then frozen, and a retry resends it unchanged, so the bridge
replays a lost answer instead of deciding it again. The bridge's own rules still apply on top
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
| paired anew since you answered | nothing is sent and nothing shown; its notification, if it still shows, goes |

If news replaced the notification meanwhile (a new dialog, a new turn), the
news stays and the older answer's outcome is not shown. If Android Auto
spoke in the conversation meanwhile, its words stay: a sent answer leaves
them (until old news is cleared), and an answer not sent, or lost, is
repeated after them with its reason (`ActionFeedback.EXTRA_SPOKEN_OVER`).

**Old news is cleared.** The bridge sends no "resolved" message, so when
the app comes to the foreground, and after an answer was sent, a worker
reads `GET /v1/agents` (only if a notification is showing) and removes a
dialog's notification once the agent is no longer blocked, a finished
turn's once it is no longer unread, and any whose agent is gone (an
agent herdr cannot tell about keeps its notification). An answer's
notification still *Sending…* once its job is no longer queued (the
app died between showing it and queueing it) says *Not sent: open the
app*, never cleared as if sent; each tap schedules that check (and only that one) a little
past its minute of grace, so it does not wait for the app to open. The
frozen bodies of jobs that are gone are dropped. A
final outcome (*Already answered*, *Not sure it arrived*, ...) stays until
you dismiss or tap it, or news replaces it; if the notification was gone
by then (swiped, or tapped into the app), only an outcome you must act on
(*Not sure it arrived*, *Not sent*, *Not paired*, ...) comes back. A tap
on a notice that news replaced meanwhile queues nothing: each notification
names its question (`EXTRA_QUESTION`: the pending id, or `turn`); nor
does a tap whose notification is gone by the time it is shown as sending
(a pairing reset, or old news cleared, between the tap and then), or one
made before the app paired anew.

Answers are ordinary WorkManager jobs, not expedited ones: an expedited job
needs a foreground-service notification on Android 11 and older. A reply
from the shade wakes the app, so the job usually runs at once; under Doze
it can wait for the next maintenance window.

## Android Auto

Android Auto reads announced notices aloud and takes a spoken reply
([design §6.6](../design.md#66-notifications-and-android-auto)). There is
no car UI: the app is a messaging app for Android Auto
(`res/xml/automotive_app_desc.xml` with `<uses name="notification"/>`,
named by the manifest's `com.google.android.gms.car.application`
meta-data), and its notifications are what Google's "Extend messaging
notifications to Android Auto" asks for: `MessagingStyle` (one agent, one
non-group conversation, the agent as a bot `Person`), a reply action with
`SEMANTIC_ACTION_REPLY`, one `RemoteInput` and no user interface, and a
mark-as-read action with `SEMANTIC_ACTION_MARK_AS_READ`.

Those two are **invisible** actions (package `dev.passporter.car`), on an
announced notice only (`announce`: the bridge applies `car_announce_done`,
so a blocked agent always, a finished turn once its TL;DR has a *Decide*
line by default). Google requires both for Android Auto compliance, so
a notice that is not announced is expected to stay silent in the car.
That is Android Auto's behaviour, unverified until the DHU check below
(a blocking human check); even if Android Auto fell back to the shade's
visible *Reply*, that needs an unlocked phone and goes as
`origin: "notification"`. They
are the notification's only semantic actions: the shade's *Allow once*,
*Deny*, *Deny with note* and *Reply* carry none, so the car never reaches
them.

| Car action | Intent data | Does |
|---|---|---|
| Reply (`RemoteInput` `dev.passporter.car.UTTERANCE`) | `passporter://car/reply/<agent>/dialog/<pending>/<stamp>`, `…/blocked/<stamp>` or `…/turn/<stamp>` | Sends the utterance, as heard, to `POST /v1/agents/:id/car-reply` with a **car**-scope session |
| Mark as read | `passporter://car/read/<agent>/…/<stamp>` | Nothing: the bridge has no API for it, and Android Auto fires it on a tap in the car while it may still read or offer a reply, so the notification stays (the reconciler clears it as old news) |

The stamp is when the notice was posted (`EXTRA_STAMP` on the
notification, kept by every repost), so two turns of one agent have their
own intents, and an answer lands only on the notice the car heard.

Both are broadcasts to `CarReplyReceiver` (explicit, not exported; the
reply's pending intent is mutable for its `RemoteInput`, the other
immutable). It acts **only while Android Auto projects**, and then also
with the phone locked. Otherwise it sends nothing, and a reply (a watch or
the Assistant firing it) only says *Answer from your phone* under the
notification, which keeps its actions. It reads Android
Auto's connection provider (`content://androidx.car.app.connection`, the
source of `androidx.car.app`'s `CarConnection`) directly, and only when
that provider belongs to Android Auto's package; anything else is no car
(docs/security.md has why).

| File | What |
|---|---|
| `CarRequest.kt` | The two actions and the `CarRequest` in the intent's data (what the car read: a named dialog, an unnamed one, a turn) |
| `CarActions.kt`, `CarReplyReceiver.kt` | The invisible actions, and their receiver |
| `CarProjection.kt` | Whether Android Auto projects |
| `CarJob.kt`, `CarOutbox.kt` | The reply as a durable job (key, utterance, pending id fixed when the car hears it) and its outbox: the shade's backoff (30 s, doubling) and attempts (8), but never sent once 10 minutes old (a reply to a turn names no dialog, so the bridge could not tell it was said to an older one); replies to one agent go in order |
| `CarReplySender.kt` | One attempt over `DeviceAuth` with `Scope.CAR`, and the answer (`CarReply`: a sentence and the result) |
| `CarSpeech.kt` | The answer in the conversation, for the car to read aloud |

The utterance is made fit for the bridge's `Utterance` (one line, no
hidden characters, at most 1 KiB) and otherwise sent as heard: the bridge
maps it with its tiny vocabulary under the car policy (`yes` alone allows
once a dialog a hook matched exactly; `no …` denies, the rest as the note;
"yes, but…" is refused; anything else is a prompt only while no dialog is
open). The bridge's sentence ("Allowed: pnpm test", "Denied, and sent your
note.", "I'm not sure what you meant…") is added to the conversation as a
message from *Passporter*, after your words, as a new message the car
announces; the car actions stay, so you can answer again. When the bridge
gives no sentence the app says why itself (nothing heard, too long, not
paired, unreachable after the last retry or once too old, not sure it went
through). If news replaced the conversation meanwhile, the news stays; if
the conversation is gone (swiped, cleared as old news), the answer comes
back while Android Auto projects; off the car only one you must act on
comes back (refused, not sent, unsure, not paired), and off the car an
answer never alerts. The car's turn in the conversation no longer marks
an answer from the shade still on its way, so that answer's outcome
never replaces or clears the sentence you must hear (the cost: if that
shade answer then fails, its outcome is not shown over the car's turn);
an outcome already shown stays marked as one, not as news. After a sent answer
the car's reply stays on the notification (Google's compliance asks for
both car actions on a message the car should read); a second answer to
the same dialog is read back as *The agent has moved on*.

### Testing with the Desktop Head Unit

The Desktop Head Unit (DHU) runs Android Auto's car side on the dev
machine, with the phone on USB. You need a phone with Android Auto (built
in since Android 10) and the debug APK installed.

1. **Install the DHU** (into the same SDK as the rest):

   ```sh
   eval "$(MISE_ENV=android mise env --shell bash)"
   "$ANDROID_HOME"/cmdline-tools/latest/bin/sdkmanager --sdk_root="$ANDROID_HOME" --install 'extras;google;auto'
   ```

   It needs the host's `libc++.so.1` (on Arch `pacman -S libc++`; or point
   `LD_LIBRARY_PATH` at the NDK's
   `toolchains/llvm/prebuilt/linux-x86_64/lib/x86_64-unknown-linux-gnu`).
   Check it with `"$ANDROID_HOME"/extras/google/auto/desktop-head-unit --version`.
2. **Android Auto developer mode** on the phone: Settings → Connected
   devices → Connection preferences → Android Auto (or the Android Auto
   app), scroll to *Version* and tap it ten times. Then, in the menu's
   *Developer settings*, turn on **Unknown sources**: Android Auto ignores
   sideloaded apps otherwise (decision D3). Under *Notifications*, make
   sure messages are shown and read aloud.
3. **Start the head unit server**: in Android Auto's menu, *Start head
   unit server* (it stays on until you stop it).
4. **Forward the port and run the DHU**, with the phone on adb:

   ```sh
   adb forward tcp:5277 tcp:5277
   "$ANDROID_HOME"/extras/google/auto/desktop-head-unit
   ```

   The DHU window shows the car screen; the phone now projects (lock it to
   test as in the car). Type `help` in the DHU's console for its commands;
   `mic begin` speaks through the dev machine's microphone (or use the
   on-screen mic), `day`/`night` switch the theme.
5. **Send a notice.** The debug samples go through the same path (their
   agents are not on your bridge, so a reply is answered *That agent is
   gone*):

   ```sh
   adb shell am broadcast -n dev.passporter.app/dev.passporter.notify.SampleNoticeReceiver --es sample blocked
   ```

   For a real answer, leave the phone paired and let an agent on the
   bridge ask something (a Claude Code permission dialog the hook
   reports) or finish a turn whose TL;DR has a *Decide* line.

What to expect:

| Notice | In the car |
|---|---|
| `blocked` (announced) | Read aloud: "widget: Run `pnpm test`?"; reply offered. **Blocking**: this proves Android Auto takes the *invisible* reply; with the phone unlocked, a spoken "yes" must reach `car-reply` (the bridge's audit: `origin: "car"`), never the shade's *Deny with note* |
| A real hook-matched dialog, you say "yes" | "Allowed: …" read back; the agent continues (audit: `origin: "car"`) |
| The same, you say "no, use the other file" | "Denied, and sent your note."; the agent gets the note |
| You say "yes but run the tests first" | "I'm not sure what you meant. Say just yes to allow, or no and what to do instead."; nothing sent, you can answer again |
| A dialog only the screen parse described | Refused aloud; answer it on the phone |
| `decide` (announced finished turn) | Read aloud with its *Decide* line; a free reply goes to the agent as a prompt ("Sent.") |
| `finished` (not announced) | Silent in the car, no reply offered (**blocking**: the rule rests on this); shown on the phone |
| Tap a conversation in the car (mark-as-read) | It stays, still replyable |
| The phone off the car, locked or not | Car actions send nothing (*Answer from your phone*); the shade asks to unlock |

## Push

Agent news reaches the app as encrypted notices
([design §6.6](../design.md#66-notifications-and-android-auto)): a message
carries everything a notification shows, so it needs no network to show
it. Two transports bring them, one at a time ([Push notifications › The
Android app](../operations/push.md#the-android-app)):

- **Built-in**, the default whenever the bridge runs a companion
  (`[push] native_ntfy`): the app collects its notices from the bridge
  itself. Nothing to install.
- **UnifiedPush**: a distributor app on the phone (ntfy) holds one
  connection to its server for every app that uses it. Used when the
  bridge has no companion, or when you choose it.

`BuiltInPush` decides, on the push thread, whenever the activity resumes
and right after pairing: it registers the built-in transport if that is
the choice and the bridge has it, and only otherwise lets
`PushRegistration` register with a distributor as before. Taking over, it
drops the distributor's registration (bridge copy first, `PushSync.leave`);
an endpoint the distributor sends later is dropped, not posted. The bridge
gives all of a device's subscriptions one message id (`ping`), which the
notifier remembers, so a switch never notifies twice.

**Settings.** Inside the app the PWA's notification switch is replaced by
a light (*On*, *Needs attention*), the app's sentence on how news arrives
or what is wrong, and *Set up*, which opens the app's own dialog: *Retry*,
*Use the ntfy app* / *Use built-in*, and, for the built-in transport,
*Allow* to run without battery restrictions (Android's own prompt). The
launcher's *Notifications* shortcut and the built-in transport's ongoing
notification open the same dialog. The status is in-process: what the
service saw last (including messages refused as expired, which means the
phone's clock is off), whether notifications are allowed, and whether the
bridge holds the external registration.

### The built-in receiver

| File | What |
|---|---|
| `BuiltInPush.kt` | Chooses the transport, starts and stops the service; `NativeStatus`, the last state, for Settings |
| `NativeReceiver.kt` | Registers (`POST /v1/push/native`, app scope, once per process and pairing), leaves (drops the registration as its owner), resets, reads the stream (`listen`) and handles each line (`ingest`) |
| `NativeEnvelope.kt` | Checks an envelope before anything is decrypted: version, this registration, not expired (nor more than an hour and ten minutes ahead), and the bridge's ECDSA P-256 signature over the same text the bridge signs |
| `NativeBridge.kt` | The bridge as the receiver needs it, through `DeviceAuth`: the stream goes only as the device that registered (`DeviceAuth.token(scope, device)`), a 401 retried once |
| `NativeStore.kt` | The record (`passporter-builtin-push.json` in `noBackupFilesDir`): your choice, the owner device, the registration id, the signing key, the stream cursor |
| `NativePushService.kt` | The `specialUse` foreground service and its boot receiver |
| `NotificationSetup.kt` | The status Settings shows and the setup dialog |

- **Keys.** The receiver's Web Push keys are the connector's key
  manager's (`DefaultKeyManager`, instance `passporter-builtin`), apart
  from the distributor registration's. A new pairing, or none, deletes
  them and the registration, so whatever the old pairing's bridge still
  sends cannot be decrypted; `PairingReset` also stops the service.
- **What is shown.** Only an envelope that verifies with the key the
  bridge named at registration, is for this registration, has not expired
  and decrypts with this app's keys, and only while the device it was
  registered for is still the pairing (else dropped, as `IncomingPush`
  drops a push for older keys). Its notices go to `NoticeNotifier` as the
  owner's, so every action is stamped with the owner device.
- **The stream.** `?since=<last message id>` resumes after a reconnect; the
  cursor advances after a message is shown (a crash in between shows it
  once more at worst, and the notifier drops a ping it has shown). A 404
  means the bridge lost the registration: it registers again; so do three
  envelopes in a row that fail the signature (a new VAPID key on the
  bridge). A 400 drops the cursor. Lines over 16 KiB end the stream. A 409
  at registration (the device holds its 8 subscriptions) keeps the
  registration id for the next try.
- **The service.** One connection thread, reconnecting with a jittered
  backoff from 1 s doubling to 5 minutes, waiting for a network (a network
  callback wakes it), holding a 10-second wake lock per line. It stops
  itself when the transport is off; if that is because the bridge no
  longer offers it, it first posts *Agent news stopped* (a tap opens the
  setup). The external registration is dropped only once the service has
  started. `BOOT_COMPLETED` and
  `MY_PACKAGE_REPLACED` start it again if it was on; a force stop is final
  until you open the app. Its notification is on a minimum-importance
  channel (*Connection to your bridge*), which you may hide.

To try it against a real companion from the dev machine, run a disposable
ntfy on loopback (never a live one) and point the bridge's integration
tests at it:

```sh
mkdir -m 700 /tmp/pp-ntfy
docker run -d --rm --name pp-test-ntfy --network host --read-only --cap-drop ALL \
  --user "$(id -u):$(id -g)" -v /tmp/pp-ntfy:/var/cache/ntfy \
  binwiederhier/ntfy@sha256:1875ddcf815c8a3c981d6d78c67a93452d0abba79b1ab9520b1f5f5238902bd7 \
  serve --listen-http=127.0.0.1:12586 --cache-file=/var/cache/ntfy/cache.db --cache-duration=1h \
  --visitor-request-limit-exempt-hosts=127.0.0.1 --visitor-subscription-limit=64
PASSPORTER_TEST_NTFY=127.0.0.1:12586 cargo nextest run -p bridge native_push
docker stop pp-test-ntfy
```

`NativeEnvelopeTest` opens an envelope the bridge's own test pins
(`crates/bridge/src/push/fixtures/native-envelope-vector.json`, around the
notice vector's ciphertext), and `NativeReceiverTest` decrypts it end to
end with RFC 8291's example keys, so the two sides are proven to agree.

### UnifiedPush

What you set up, once, to use it:

1. Install ntfy on the phone (F-Droid or Google Play). To use your own ntfy
   server, set it as ntfy's default server first (*Settings → Default
   server*).
2. Allow that server on the bridge: `[push] ntfy_host = "ntfy.example.org"`
   (or `"ntfy.sh"`, the public one), and restart the bridge
   ([Deploy › Push](../deploy.md#push-wake-pings-and-native-notices)).
3. Open Passporter, paired (and, if the bridge has a companion, choose
   *Use the ntfy app* in the notification setup). It registers with ntfy
   and posts the endpoint to the bridge; the bridge's log shows
   `push subscription stored` with ntfy's host. Without ntfy installed,
   the app says once that notifications need it.

The Kotlin package `dev.passporter.push` (in `gen/android/app/src/main/java/`)
uses the official connector library, `org.unifiedpush.android:connector`
(pinned in `app/build.gradle.kts`), which makes the Web Push keys and
decrypts each message (RFC 8291, `aes128gcm`, with Tink):

| File | What |
|---|---|
| `PushRegistration.kt` | From the activity: on every resume and right after pairing, unless the built-in transport took over, if paired and the bridge has push on (`GET /v1/push/key`, asked once per process), registers with the saved distributor, the phone's default one (Android may show a picker, once per pairing), or says that none is installed; the picker and that dialog wait until you have answered the notification permission's prompt, so two prompts never come at once |
| `PassporterPushService.kt` | The connector's `PushService`, started by the distributor also with the app's process cold: messages go to `IncomingPush` at once, in the callback (local work only, never queued behind a bridge call); new endpoints and unregistrations go to `PushSync` on its worker thread |
| `PushSync.kt`, `PushStore.kt`, `PushBridge.kt` | Keeps the bridge's copy in step: posts a new endpoint (`POST /v1/push/subscriptions`, app scope, through `DeviceAuth`), drops the one it replaces (again at the next sync if the bridge could not be told), keeps the distributor you chose, posts it once more in each new process (in case the bridge dropped it), and when the app is unpaired or paired anew (perhaps with another bridge), or the built-in transport takes over (`leave`), forgets the registration and leaves the distributor, which deletes the keys; a new pairing registers with new keys. The state of the last step is a `PushState` (`Subscribed`, `Refused` with the bridge's reason, `BridgeOff`, `Left`, `RetryLater`, ...), logged under the tag `PassporterPush` |
| `IncomingPush.kt` | A message the connector decrypted, while the app holds a registration, goes to `NoticeNotifier.show`. One it could not decrypt with the app's keys is dropped unseen, so only the bridge can make the app post a notice |
| `Registration.kt` | The endpoint and public keys, and the subscribe body in the bridge's shape |

The private key and auth secret are the connector's: it seals the private
key with an AES key in the Android Keystore and keeps both in the app's
private storage, which backups and device transfers leave out. Doing the
ECDH inside the Keystore instead (`PURPOSE_AGREE_KEY`) needs Android 12 and
a custom key manager, and would protect little more: reading the wrapped
key back needs the app's own process, which sees every decrypted notice
anyway. The app's own
record (`passporter-push.json` in `noBackupFilesDir`) holds the endpoint,
the public keys and which device posted them.

`WebPushVectorTest` decrypts RFC 8291's example and a notice message the
bridge's own encryption produced (`crates/bridge/src/push/fixtures/`,
pinned by a test in `crates/bridge/src/push/ece.rs`), so the two sides are
proven to agree.

Where the page signs in without the native identity, Settings keeps the
PWA's switch, disabled, with the reason: on a WebView too old for native
IPC (`NativeOff`: update Android System WebView), or with a WebCrypto key
paired before the native layer could be used (`PairInApp`: forget and pair
again).

Known gaps: forgetting the pairing from the phone leaves the bridge's
copy of the old device's subscription (built-in or external: it can no
longer sign in to remove it), so nothing reaches the phone but the bridge
keeps it until you revoke the device with `passporter devices revoke`, as
you would anyway. Removing Passporter inside ntfy lasts until the app next
opens; to silence it, turn its notifications off in Android's settings. A
debug build can fake a message without either transport through the
sample receiver in [Notifications](#notifications).

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
  rotation and dark mode are listed, so they keep the webview), or after
  destroying it in the background. A tap also makes a new `MainActivity`
  in a live process: after Back finished the last one (Android 8 to 11),
  or after a swipe from Recents while dictation kept the process. wry then
  builds a new webview and loads the first page
  it was given at launch again (`FirstPage` in `destination.rs`). That page
  is the bridge's, except on the launch where you paired: there it is the
  Connect page, which the guard refuses once paired. So `WebViewBuilds`
  tells the shell about every webview after the process's first, and the
  shell then opens the bridge's start page (the Bridge page, if it does not
  load); then the tap that made the activity (`MainActivity.onCreate`, or a
  new intent delivered before the webview was built), so the agent you
  tapped opens last. The guard is unchanged: the bridge needs no allowance. A process
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
cached tokens), drops what the app kept for the old bridge (`PairingReset`,
below), and restarts the app in a new process, on the Connect
page. The restart is deliberate:
Tauri cannot revoke the capability it granted to the old origin, so only a
new process starts without it. The old bridge's data in the webview
(its service worker cache, IndexedDB) stays under its own origin, which
the app never opens again; clearing the app's storage removes it. The old
bridge still lists the device until you revoke it there.

**Nothing of one pairing reaches the next.** Agent ids repeat across
bridges (`w1:p1`), so on *Change address*, and after every pairing and
forget in the process (a listener `PassporterApplication` registers at
process start), `PairingReset` cancels the queued answers of the shade
and the car, forgets their frozen bodies, removes the agents'
notifications (the recorder's own stays) and their conversation
shortcuts (dynamic and the system's cached long-lived copies, so the
launcher and the share sheet show no old agent; one pinned to the home
screen cannot be removed, so it is disabled) and deletes the dictation takes
nobody released, which the new bridge's PWA would otherwise adopt (so a
new pairing, even with the same bridge, drops unsent dictation). As a
second line, every action intent is stamped with the device the notice
came to (`?device=dev-…` on its data; for a push, the device its keys
were registered for), and each job keeps it: a tap, a car reply or a
queued job made for another device than the one the app is paired as now
sends nothing (`DeviceStamp`, `NotSent.PAIRING_CHANGED`,
`CarTrouble.PAIRING_CHANGED`; `DeviceAuth.send` also refuses a token of
another pairing, should one be made between the check and the request,
as `OtherDevice`),
and an old conversation is never brought back for it. When the app
cannot tell what it is paired as (a busy Keystore just after boot), a
job waits and retries rather than being dropped. A push decrypted
with an older pairing's keys (not yet replaced) is dropped unseen; that
check reads the saved device id (`DeviceAuth.savedDevice`), never the
Keystore, since a push from cold arrives on the main thread. The reset
also runs when the PWA forgets the pairing, so that drops unsent
dictation too.

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
- gives a sign-in (both tries) a 30-second budget: each request's connect
  and read timeouts are cut to what is left, and none starts once it is
  spent (one slow request can still overrun it); `status`, `forget` and a cached token never
  wait for a sign-in in flight, and a token made for a pairing forgotten
  meanwhile is not handed out;
- forgets the pairing when the bridge answers `410 Gone` (revoked).

The PWA reaches it through `plugin:device|status`, `pair`, `session` (a
fresh **app**-scope token; the webview never gets a car token) and
`forget` (`crates/web/src/pair/native.rs`), plus `notificationStatus` and
`notificationSetup` for Settings' notification light ([Push](#push)).
Without the plugin (Chrome, an old WebView) the PWA uses WebCrypto,
unchanged.

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
// Told after every pairing and forget, on the thread that made it:
auth.addPairingListener { status -> if (status.paired) register() }
```

Failures are `DeviceAuthException` subclasses: `NotPaired`, `Forgotten`
(revoked; the pairing is gone), `Unreachable` and `ServerError` (retry
later), `RateLimited`, `Refused`, `Keystore`, and so on. Another plugin uses
it by depending on the library in its `android/build.gradle.kts`:
`implementation(project(":tauri-plugin-device"))`.

## Dictation

In the browser a take lives in the page: `MediaRecorder` holds it in
memory and the PWA stops it when the page is hidden ([design
§6.3](../design.md#63-dictation-first-class)). In the app the plugin in
`plugins/recorder` (Kotlin package `dev.passporter.recorder`) records
instead, so a long dictation survives the screen turning off, the webview
reloading and the app being killed.

| File | What |
|---|---|
| `Recorder.kt` | One take at a time: start, stop, cancel, status, read, release. A take ends when the PWA stops it, from the notification's Stop, at the length cap the PWA asked for (just under five minutes), when a call or another app takes the audio focus, or when the microphone fails; every way but a cancel keeps what was recorded |
| `RecorderService.kt` | The foreground service (type `microphone`) a take runs in, with an ongoing notification (time recorded, Stop; a tap opens the app). It goes when the take ends |
| `TakeStore.kt`, `Adts.kt` | Takes on disk, in `noBackupFilesDir/recordings`: `<id>.aac` and its journal `<id>.json` |
| `AudioInput.kt`, `AudioFocus.kt` | `MediaRecorder` (AAC-LC, mono, 16 kHz, 32 kbit/s, in ADTS frames) and audio focus, behind interfaces the tests fake |
| `RecorderPlugin.kt` | The PWA's commands, with strict arguments; asks for the microphone at the first `start` |

**What the PWA does.** `crates/web/src/voice/native.rs` feature-detects
the plugin (it answers `status`) and records through it; otherwise
(Chrome, an old WebView) dictation uses `MediaRecorder`, unchanged. The
PWA starts a take tagged with the agent's id, polls its level for the
meter, and on stop reads the audio over IPC (base64, 256 KiB at a time)
into the same clip a browser take makes. The clip is stored in IndexedDB
under the take's own id, and only then released natively, so the take is
always in at least one durable place; upload, transcription, the
transcript card and the offline retries are the PWA's as before.

**Why the PWA uploads, not native code.** The bridge's upload needs the
clip's size before the first byte (`POST /v1/audio`), so nothing can go
up until the take ends, and what comes back (the transcript) is only
useful in the PWA: sent as a prompt or shown on a card. A native upload
would duplicate the courier (renames, re-uploads, transcript polling)
for little gain, and `HttpsURLConnection` cannot send the `PATCH` the
chunks use. If the app dies after a take ends, the next launch stores
and uploads it in seconds.

**What survives what.**

- *Screen off, another app, the notification shade:* the take goes on in
  the foreground service; the PWA's meter catches up when it is visible
  again.
- *Task swiped away:* the service and the take go on (the notification
  stays, with Stop). Reopening the app reloads the PWA, which stops a
  take no page of its own started and puts it on its agent's card.
- *Process killed* (low memory, force stop): `MediaRecorder` writes each
  AAC frame to the file as it is encoded, so the file holds everything up
  to the encoder's last fraction of a second. The journal still says
  `recording`; at the next start the recorder cuts any torn last frame
  off, takes the length from the frames, and lists the take as
  `interrupted`. What is lost: the frames still in the encoder, and the
  rest of the dictation, since nothing records once the process is gone.
- *A call:* the call takes the audio focus, which ends the take, keeping
  what was said (a brief sound such as a notification only ducks it).

A take that ended with no page waiting for it is stored when an agent
screen next opens, and waits on its agent's card for review, never sent
unseen. So does one the page was waiting for that ended out of its sight:
at the length cap while the page was hidden, or from the notification's
Stop. Only the PWA's own stop, or the cap on screen, sends (when sending
on stop is on). Takes nobody released are deleted after seven days, as the PWA's
own clips are.

**Permissions.** `RECORD_AUDIO` is asked at the first `start` (the
permission alias `microphone`); a refusal reads "Allow it for Passporter
in Android's settings" in the PWA. The plugin's manifest adds
`FOREGROUND_SERVICE` and `FOREGROUND_SERVICE_MICROPHONE` and the service.
Without `POST_NOTIFICATIONS` (Android 13+, asked once a bridge is
connected or the app pairs) the service still records; Android then
lists it only in the quick settings' active-apps list.

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
| Desktop Head Unit (optional, installed by hand) | `extras;google;auto` 2.0 | `~/.local/share/android-sdk/extras/google/auto` | — ([Testing with the Desktop Head Unit](#testing-with-the-desktop-head-unit)) |

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
