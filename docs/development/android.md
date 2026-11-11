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
as usual. Native plugins (notifications, UnifiedPush, the recorder,
Android Auto) are later additions.

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

It runs clippy on the shell for the Android target, the shell's Rust tests
(also part of `just verify`), and Gradle's JVM unit tests
(`testUniversalDebugUnitTest`, in `gen/android/app/src/test/`). Nothing
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
browser. The app remembers the address. To use a different bridge, clear
the app's storage (*Settings → Apps → Passporter → Storage → Clear
storage*); that also forgets the pairing.

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
| `src/lib.rs`, `src/shell.rs` | The Tauri app: the main window, the `connect` command, the navigation guard, the bridge capability. `shell.rs` builds for Android only |
| `src/origin.rs`, `src/navigation.rs`, `src/store.rs` | The rules, in plain Rust tested on any host: what counts as a bridge origin, which pages the webview may show, where the origin is saved |
| `connect/` | The bundled Connect page (`frontendDist`) |
| `tauri.conf.json` | App id `dev.passporter.app`, name, the Connect page's CSP |
| `gen/android/` | The Gradle project from `cargo tauri android init`, trimmed: manifest, theme, icons, network security config, signing, JVM tests. Build output and the files Tauri regenerates on each build are gitignored |

Only Passporter's own pages reach native IPC, by three fences:

1. **Navigation.** The webview shows the bundled Connect page until you
   connect, then only the paired origin, and refuses every other
   navigation (`navigation.rs`). Back is a history navigation the guard
   does not see, so the Connect page removes itself from history when it
   opens the bridge.
   The PWA's CSP (`default-src 'self'`) keeps other origins out of frames
   and scripts.
2. **Capability.** At launch, or when you connect, the shell grants a
   remote capability to exactly `https://<bridge>/*` in the main window.
   The address must be a plain DNS name or IPv4 address, so the pattern
   holds no wildcard syntax; `localhost` names are refused.
   It lists the plugin permissions the PWA may use (none yet).
3. **App commands.** The app has no ACL manifest, so Tauri refuses its own
   commands (`connect`) to every remote page; only the bundled Connect
   page can call them.

Everything on the bridge's origin counts as Passporter, so serve nothing
else on that host name: another app behind the same proxy name would get
the same access.

The address is saved in the app's private storage (`bridge-origin` in the
data directory); it is not a secret. The device key and session tokens
stay in the PWA's own storage, as in the browser. Neither backups nor a
device-to-device transfer copy the app's data (`allowBackup="false"` plus
backup and data-extraction rules that exclude everything), so the key
stays on the phone; a new phone pairs afresh.

Fence 1 does not see POST navigations (Android does not report them); the
pages' CSP `form-action` (`'self'` on the bridge, `'none'` on the Connect
page) covers form submissions instead.

Fence 2 tells frames apart only on a current WebView: Tauri learns each
frame's own origin through `WEB_MESSAGE_LISTENER` (Android System WebView
86 and later). On an older WebView every message is labelled with the URL
the main page last started loading, so only the PWA's CSP keeps a foreign
frame from borrowing the bridge capability, and a message sent while the
Connect page is being replaced can even count as local. That is harmless
while the capability grants nothing and no local capability exists; the
first plugin task gates the grant on the feature and must not add a local
capability.

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

Known gaps: the shell does not yet navigate to the agent on a tap, and
nothing clears a notification once its news is old (a dialog you answered
on the desktop stays until you tap or swipe it), because the bridge sends
no "resolved" message. Each agent's conversation shortcut stays too;
Android drops the oldest once the app reaches its shortcut limit.

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
test1` to two broadcasts to send the same message twice, and the second
must not buzz. Release builds leave the receiver out.

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
