// The Connect page: sends the typed address to the shell's `connect`
// command (src/shell.rs), which checks and saves it and answers the URL to
// open. `replace` drops this page from history, so Back never returns here.
"use strict";

const form = document.getElementById("connect");
const address = document.getElementById("address");
const error = document.getElementById("error");
const button = form.querySelector("button");

form.addEventListener("submit", async (event) => {
  event.preventDefault();
  error.textContent = "";
  button.disabled = true;
  try {
    const url = await window.__TAURI__.core.invoke("connect", { address: address.value });
    window.location.replace(url);
  } catch (reason) {
    error.textContent = String(reason);
    button.disabled = false;
    address.focus();
  }
});
