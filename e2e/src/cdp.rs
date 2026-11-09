//! A minimal Chrome `DevTools` Protocol client: system Chromium, headless,
//! driven over one WebSocket.
//!
//! Only what the smoke tests need: open a page, set the viewport, evaluate
//! JavaScript, type text, and collect console errors, uncaught exceptions
//! and browser log errors (CSP violations, failed loads) as they happen.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// Environment variable naming the browser binary, overriding the search.
pub const CHROMIUM_ENV: &str = "PASSPORTER_CHROMIUM";

/// Binaries searched for on `PATH`, in order.
const CANDIDATES: &[&str] = &[
    "chromium",
    "chromium-browser",
    "google-chrome-stable",
    "google-chrome",
    "chrome",
];

/// How long a single CDP command may take.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

/// How long Chromium may take to print its `DevTools` endpoint.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(20);

/// What went wrong talking to the browser.
#[derive(Debug, thiserror::Error)]
pub enum CdpError {
    /// Chromium could not be started.
    #[error("launching {}: {source}", path.display())]
    Launch {
        /// The binary.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },
    /// Chromium exited or never announced its `DevTools` endpoint.
    #[error("chromium did not print a DevTools endpoint: {0}")]
    NoEndpoint(String),
    /// The WebSocket failed.
    #[error("devtools websocket: {0}")]
    Socket(#[from] tokio_tungstenite::tungstenite::Error),
    /// The browser answered a command with an error.
    #[error("{method}: {error}")]
    Command {
        /// The CDP method.
        method: String,
        /// The browser's error object.
        error: Value,
    },
    /// No answer in time, or the connection closed first.
    #[error("{method}: no reply")]
    NoReply {
        /// The CDP method.
        method: String,
    },
    /// A page script threw.
    #[error("script threw: {0}")]
    Script(String),
}

/// Finds a Chromium-family browser: [`CHROMIUM_ENV`], else the first of
/// [`CANDIDATES`] on `PATH`.
#[must_use]
pub fn find_chromium() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(CHROMIUM_ENV).filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(path));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| CANDIDATES.iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file())
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, Value>>>>>;
type Sink = futures_util::stream::SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;

/// A headless Chromium and its `DevTools` connection. The process is
/// killed when this is dropped.
#[derive(Debug)]
pub struct Browser {
    _child: Child,
    outgoing: mpsc::UnboundedSender<String>,
    pending: Pending,
    problems: Arc<Mutex<Vec<String>>>,
    events: Arc<Mutex<Vec<Value>>>,
    next_id: Mutex<u64>,
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
}

impl Browser {
    /// Starts `binary` headless with a fresh profile in `profile`.
    /// `extra_args` are passed through (e.g. host resolver rules).
    pub async fn launch(
        binary: &Path,
        profile: &Path,
        extra_args: &[String],
    ) -> Result<Self, CdpError> {
        let mut child = Command::new(binary)
            .args([
                "--headless=new",
                "--remote-debugging-port=0",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-extensions",
                // Keep Chromium's own traffic off the network: the page
                // only ever talks to the loopback bridge.
                "--disable-background-networking",
                "--disable-component-update",
                "--disable-domain-reliability",
                "--disable-client-side-phishing-detection",
                "--no-pings",
                "--disable-sync",
                "--disable-gpu",
                "--mute-audio",
                "--password-store=basic",
                "--use-mock-keychain",
            ])
            .arg(format!("--user-data-dir={}", profile.display()))
            .args(extra_args)
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| CdpError::Launch {
                path: binary.to_path_buf(),
                source,
            })?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| CdpError::NoEndpoint("no stderr pipe".to_owned()))?;
        let url = tokio::time::timeout(LAUNCH_TIMEOUT, devtools_url(stderr))
            .await
            .map_err(|_| CdpError::NoEndpoint("timed out".to_owned()))??;
        let (socket, _) = tokio_tungstenite::connect_async(url.as_str()).await?;
        let (sink, stream) = socket.split();
        let (outgoing, rx) = mpsc::unbounded_channel();
        let pending: Pending = Arc::default();
        let problems = Arc::default();
        let events = Arc::default();
        let writer = tokio::spawn(write_loop(sink, rx));
        let reader = tokio::spawn(read_loop(
            stream,
            Arc::clone(&pending),
            Arc::clone(&problems),
            Arc::clone(&events),
        ));
        Ok(Self {
            _child: child,
            outgoing,
            pending,
            problems,
            events,
            next_id: Mutex::new(0),
            reader,
            writer,
        })
    }

    /// Sends a browser-level command and waits for its result.
    pub async fn browser_call(&self, method: &str, params: Value) -> Result<Value, CdpError> {
        self.send(None, method, params).await
    }

    /// Opens a new tab on `about:blank` and attaches to it, with console,
    /// exception and log collection on.
    pub async fn new_page(&self) -> Result<Page<'_>, CdpError> {
        let target = self
            .browser_call("Target.createTarget", json!({"url": "about:blank"}))
            .await?;
        let target_id = target["targetId"].clone();
        let attached = self
            .browser_call(
                "Target.attachToTarget",
                json!({"targetId": target_id, "flatten": true}),
            )
            .await?;
        let session = attached["sessionId"]
            .as_str()
            .ok_or_else(|| CdpError::Command {
                method: "Target.attachToTarget".to_owned(),
                error: attached.clone(),
            })?
            .to_owned();
        let page = Page {
            browser: self,
            session,
        };
        for domain in ["Page.enable", "Runtime.enable", "Log.enable"] {
            page.call(domain, json!({})).await?;
        }
        Ok(page)
    }

    /// The `ServiceWorker.*` events seen so far (after `ServiceWorker.enable`
    /// on a page), oldest first.
    #[must_use]
    pub fn service_worker_events(&self) -> Vec<Value> {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Console errors, uncaught exceptions and browser log errors seen so far.
    #[must_use]
    pub fn problems(&self) -> Vec<String> {
        self.problems
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    async fn send(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, CdpError> {
        let id = {
            let mut next = self.next_id.lock().unwrap_or_else(PoisonError::into_inner);
            *next += 1;
            *next
        };
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, tx);
        let mut message = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        let no_reply = || CdpError::NoReply {
            method: method.to_owned(),
        };
        self.outgoing
            .send(message.to_string())
            .map_err(|_| no_reply())?;
        match tokio::time::timeout(COMMAND_TIMEOUT, rx).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(error))) => Err(CdpError::Command {
                method: method.to_owned(),
                error,
            }),
            Ok(Err(_)) | Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&id);
                Err(no_reply())
            }
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
    }
}

/// Reads Chromium's stderr until `DevTools listening on ws://...`.
async fn devtools_url(stderr: tokio::process::ChildStderr) -> Result<String, CdpError> {
    let mut lines = BufReader::new(stderr).lines();
    let mut seen = Vec::new();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(url) = line.strip_prefix("DevTools listening on ") {
            let url = url.trim().to_owned();
            // Keep draining so Chromium never blocks on a full pipe.
            tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
            return Ok(url);
        }
        seen.push(line);
    }
    Err(CdpError::NoEndpoint(seen.join("\n")))
}

async fn write_loop(mut sink: Sink, mut rx: mpsc::UnboundedReceiver<String>) {
    while let Some(text) = rx.recv().await {
        if sink.send(Message::text(text)).await.is_err() {
            break;
        }
    }
}

async fn read_loop(
    mut stream: futures_util::stream::SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    pending: Pending,
    problems: Arc<Mutex<Vec<String>>>,
    events: Arc<Mutex<Vec<Value>>>,
) {
    while let Some(Ok(message)) = stream.next().await {
        let Message::Text(text) = message else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
            continue;
        };
        if let Some(id) = value["id"].as_u64() {
            let waiter = pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
            if let Some(waiter) = waiter {
                let outcome = match value.get("error") {
                    Some(error) => Err(error.clone()),
                    None => Ok(value["result"].clone()),
                };
                let _ = waiter.send(outcome);
            }
        } else if value["method"]
            .as_str()
            .is_some_and(|m| m.starts_with("ServiceWorker."))
        {
            events
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(value);
        } else if let Some(problem) = problem(&value) {
            problems
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(problem);
        }
    }
}

/// The text of an event that counts as a page error, if it is one.
fn problem(event: &Value) -> Option<String> {
    let params = &event["params"];
    match event["method"].as_str()? {
        "Runtime.consoleAPICalled" if params["type"] == "error" => {
            let args: Vec<String> = params["args"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|arg| {
                    arg["value"]
                        .as_str()
                        .map(str::to_owned)
                        .or_else(|| arg["description"].as_str().map(str::to_owned))
                        .unwrap_or_else(|| arg["value"].to_string())
                })
                .collect();
            Some(format!("console.error: {}", args.join(" ")))
        }
        "Runtime.exceptionThrown" => {
            let details = &params["exceptionDetails"];
            let text = details["exception"]["description"]
                .as_str()
                .or_else(|| details["text"].as_str())
                .unwrap_or("exception");
            Some(format!("uncaught: {text}"))
        }
        "Log.entryAdded" if params["entry"]["level"] == "error" => {
            let entry = &params["entry"];
            Some(format!(
                "log ({}): {} {}",
                entry["source"].as_str().unwrap_or("?"),
                entry["text"].as_str().unwrap_or_default(),
                entry["url"].as_str().unwrap_or_default(),
            ))
        }
        _ => None,
    }
}

/// One attached tab.
#[derive(Debug)]
pub struct Page<'b> {
    browser: &'b Browser,
    session: String,
}

impl Page<'_> {
    /// Sends a command to this tab.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, CdpError> {
        self.browser.send(Some(&self.session), method, params).await
    }

    /// Emulates a touch phone viewport of `width` x `height` CSS pixels.
    pub async fn set_viewport(&self, width: u32, height: u32) -> Result<(), CdpError> {
        self.call(
            "Emulation.setDeviceMetricsOverride",
            json!({"width": width, "height": height, "deviceScaleFactor": 3, "mobile": true}),
        )
        .await?;
        self.call(
            "Emulation.setTouchEmulationEnabled",
            json!({"enabled": true, "maxTouchPoints": 5}),
        )
        .await?;
        Ok(())
    }

    /// Navigates to `url` (does not wait for the app to render).
    pub async fn goto(&self, url: &str) -> Result<(), CdpError> {
        let result = self.call("Page.navigate", json!({"url": url})).await?;
        match result["errorText"].as_str() {
            Some(error) if !error.is_empty() => Err(CdpError::Command {
                method: "Page.navigate".to_owned(),
                error: json!(error),
            }),
            _ => Ok(()),
        }
    }

    /// Evaluates `expression` (promises awaited) and returns its value.
    pub async fn eval(&self, expression: &str) -> Result<Value, CdpError> {
        let result = self
            .call(
                "Runtime.evaluate",
                json!({"expression": expression, "returnByValue": true, "awaitPromise": true}),
            )
            .await?;
        if let Some(details) = result.get("exceptionDetails") {
            let text = details["exception"]["description"]
                .as_str()
                .or_else(|| details["text"].as_str())
                .unwrap_or("exception");
            return Err(CdpError::Script(text.to_owned()));
        }
        Ok(result["result"]["value"].clone())
    }

    /// Polls `expression` until it is truthy or `timeout` passes; returns
    /// the last value either way, so a failed wait can say what it saw.
    pub async fn wait_for(&self, expression: &str, timeout: Duration) -> Result<Value, CdpError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let value = self.eval(expression).await?;
            let truthy = match &value {
                Value::Null => false,
                Value::Bool(b) => *b,
                Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
                Value::String(s) => !s.is_empty(),
                Value::Array(_) | Value::Object(_) => true,
            };
            if truthy || tokio::time::Instant::now() >= deadline {
                return Ok(value);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Types `text` into the focused element, as an IME commit would.
    pub async fn insert_text(&self, text: &str) -> Result<(), CdpError> {
        self.call("Input.insertText", json!({"text": text}))
            .await
            .map(drop)
    }
}
