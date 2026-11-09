// Passporter service worker: app-shell caching (design §6.5) and the push
// wake (design §6.6).
//
// Deliberately thin. It never touches the API (/v1/*) itself: data caching,
// the outbox and sync live in Rust, in IndexedDB, and so does the push wake
// (notice.js + notice_bg.wasm, built from src/bin/notice.rs). The two
// placeholders below are filled at build time by scripts/stamp-sw.sh.
const BUILD = "__PASSPORTER_BUILD__";
const SHELL = __PASSPORTER_SHELL__;
const PREFIX = "passporter-shell-";
const CACHE = PREFIX + BUILD;
const SHELL_PATHS = new Set(SHELL);
// Never answered with the shell document, even when navigated to.
const NOT_SHELL = new Set(["/sw.js", "/boot.js", "/manifest.webmanifest"]);

// The push wake's glue defines `wasm_bindgen`. A missing file must not stop
// the worker installing (the shell matters more), so pushes then fall back
// to the generic notification.
try {
  importScripts("/notice.js");
} catch (error) {
  console.error("push wake unavailable", error);
}
const NOTICE_WASM = "/notice_bg.wasm";
// Shown when the wake can't run at all. Same text as Notice::generic.
const GENERIC = {
  title: "Passporter",
  body: "Your agents have news. Open Passporter to see.",
  tag: "passporter-agents",
  path: "/",
};

self.addEventListener("install", (event) => {
  // `cache: "reload"` bypasses the HTTP cache, so a new build never
  // precaches a stale file. No skipWaiting here: a new version waits until
  // the app asks for it (Reload on the "new version available" banner, see
  // src/version.rs) or every tab closes, so a running page never loses the
  // cache it was built against.
  event.waitUntil(
    caches
      .open(CACHE)
      .then((cache) => cache.addAll(SHELL.map((url) => new Request(url, { cache: "reload" })))),
  );
});

self.addEventListener("message", (event) => {
  if (event.data === "skip-waiting") self.skipWaiting();
});

self.addEventListener("activate", (event) => {
  // Reached only once no page runs the old build, or the user chose to
  // reload into this one, so older caches are safe to drop.
  event.waitUntil(
    caches
      .keys()
      .then((keys) =>
        Promise.all(
          keys
            .filter((key) => key.startsWith(PREFIX) && key !== CACHE)
            .map((key) => caches.delete(key)),
        ),
      )
      .then(() => self.clients.claim()),
  );
});

self.addEventListener("fetch", (event) => {
  const request = event.request;
  const url = new URL(request.url);
  if (request.method !== "GET" || url.origin !== self.location.origin) return;
  if (url.pathname.startsWith("/v1/")) return;

  let key;
  if (SHELL_PATHS.has(url.pathname)) {
    key = url.pathname; // a precached file: cache first
  } else if (request.mode === "navigate" && !NOT_SHELL.has(url.pathname)) {
    key = "/index.html"; // an app route: the client-side router takes it
  } else {
    return; // anything else goes to the network untouched
  }
  event.respondWith(
    caches
      .open(CACHE)
      .then((cache) => cache.match(key))
      .then((hit) => hit || fetch(request)),
  );
});

// How long the wake may take before the generic notification is shown
// instead. A hung tunnel must not leave the push without a notification:
// browsers then show their own, or (Safari) drop the subscription.
const WAKE_TIMEOUT_MS = 8000;

// How long a quiet notice stays before it closes itself. It must outlive
// the push event: Chrome checks for a visible notification only once the
// event (and its waitUntil promise) has finished. Push messages are
// userVisibleOnly, and when that check finds none Chrome treats the push
// as silent and spends the site's budget (2 per silent push; about 6 a
// day for a fully engaged site), then shows its own "This site has been
// updated in the background" notification. A notification shown and
// closed inside waitUntil counts as none. A visible tab of the site waives
// the check. Sources (Chromium, chrome/browser/push_messaging/):
// push_messaging_service_impl.cc (DeliverMessageCallback enforces after
// delivery, "even if the website's event handler failed"),
// push_messaging_notification_manager.cc (EnforceUserVisibleOnlyRequirements
// counts visible notifications, or spends budget and forces the generic
// one) and budget_database.cc (the costs). So the close runs on a timer,
// outside waitUntil; if the browser stops the worker first, the silent
// notice simply stays.
const QUIET_CLOSE_MS = 3000;

// A wake ping. Its payload is an opaque id and is never read: the wake
// (Rust) signs in, fetches the agents and picks the text. Every push shows
// exactly one notification, whatever fails, but only news alerts: a quiet
// notice (the ping's reason went away before it arrived) never does.
self.addEventListener("push", (event) => {
  let timer;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error("push wake timed out")), WAKE_TIMEOUT_MS);
  });
  const shown = Promise.race([wake(), timeout])
    .catch((error) => {
      console.error("push wake failed", error);
      return GENERIC;
    })
    .then((notice) => {
      clearTimeout(timer);
      return notice.quiet ? showQuiet(notice) : show(notice).then(() => false);
    });
  event.waitUntil(shown);
  // A failure here already failed the push event: nothing more to report.
  shown
    .then((closeLater) => closeLater && setTimeout(closeQuiet, QUIET_CLOSE_MS))
    .catch(() => {});
});

// The loaded instance, shared by later pings. A failed load is forgotten
// (as the glue forgets it too), so the next ping tries again. A trapped
// instance can't be replaced (the glue keeps a successful one), which
// costs generic notifications until the browser stops this idle worker,
// within minutes; the timeout covers a hang.
let noticeReady = null;

async function wake() {
  noticeReady ??= caches
    .open(CACHE)
    .then((cache) => cache.match(NOTICE_WASM))
    .then((hit) => wasm_bindgen({ module_or_path: hit || fetch(NOTICE_WASM) }))
    .catch((error) => {
      noticeReady = null;
      throw error;
    });
  await noticeReady;
  return JSON.parse(await wasm_bindgen.wake());
}

function show(notice) {
  return self.registration.showNotification(notice.title, {
    body: notice.body,
    tag: notice.tag,
    renotify: true,
    icon: "/icons/icon-192.png",
    data: { path: notice.path },
  });
}

// Nothing needs the user. An alert still showing is the last news: show
// it again in place (the push needs a notification), silently. Otherwise
// show the quiet notice silently; true when it should close itself.
async function showQuiet(notice) {
  const showing = await self.registration.getNotifications({ tag: notice.tag });
  const alert = showing.find((n) => !n.data?.quiet);
  const options = { tag: notice.tag, renotify: false, silent: true, icon: "/icons/icon-192.png" };
  if (alert) {
    await self.registration.showNotification(alert.title, {
      ...options,
      body: alert.body,
      data: alert.data,
    });
    return false;
  }
  await self.registration.showNotification(notice.title, {
    ...options,
    body: notice.body,
    data: { path: notice.path, quiet: true },
  });
  return true;
}

// Closes the quiet notice, never an alert that has replaced it since.
function closeQuiet() {
  return self.registration
    .getNotifications({ tag: GENERIC.tag })
    .then((showing) => showing.filter((n) => n.data?.quiet).forEach((n) => n.close()));
}

// Focus an open window and route it in-app (see push::open_from_notifications), or
// open the app at the notification's path.
self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  const path = event.notification.data?.path || "/";
  event.waitUntil(
    self.clients.matchAll({ type: "window", includeUncontrolled: true }).then((windows) => {
      // The window the user was last in, rather than whichever comes first.
      const open =
        windows.find((w) => w.focused) ||
        windows.find((w) => w.visibilityState === "visible") ||
        windows[0];
      if (!open) return self.clients.openWindow(path);
      open.postMessage({ open: path });
      return open.focus();
    }),
  );
});
