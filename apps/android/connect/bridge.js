// The Bridge page: the paired address, a way back to it, and "Change
// address". The shell (src/shell.rs) answers `bridge` with the saved origin
// and `forget_bridge` by forgetting it and restarting the app. `replace`
// keeps this page out of history, so Back never returns here.
"use strict";

const invoke = window.__TAURI__.core.invoke;
const unreachable = new URLSearchParams(window.location.search).has("unreachable");
const error = document.getElementById("error");
const openButton = document.getElementById("open");
const actions = document.getElementById("actions");
const confirmBox = document.getElementById("confirm");
const forget = document.getElementById("forget");

if (unreachable) {
  document.getElementById("title").textContent = "Can't reach your bridge";
  document.getElementById("unreachable").hidden = false;
  openButton.textContent = "Retry";
}

let origin = null;

async function load() {
  try {
    origin = await invoke("bridge");
  } catch (reason) {
    error.textContent = String(reason);
    return;
  }
  if (origin === null) {
    // Not connected: the Connect page asks for an address.
    window.location.replace("index.html");
    return;
  }
  document.getElementById("address").textContent = origin;
  document.getElementById("confirm-address").textContent = origin;
}

openButton.addEventListener("click", () => {
  if (origin !== null) window.location.replace(origin);
});

document.getElementById("change").addEventListener("click", () => {
  actions.hidden = true;
  confirmBox.hidden = false;
  forget.focus();
});

document.getElementById("cancel").addEventListener("click", () => {
  confirmBox.hidden = true;
  actions.hidden = false;
});

forget.addEventListener("click", async () => {
  error.textContent = "";
  forget.disabled = true;
  try {
    // The app restarts on the Connect page; nothing after this runs.
    await invoke("forget_bridge");
  } catch (reason) {
    error.textContent = String(reason);
    forget.disabled = false;
  }
});

load();
