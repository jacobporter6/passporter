//! `ReplaySocket`: a herdr unix socket that answers from recorded replies.
//!
//! Where [`FakeHerdr`](crate::FakeHerdr) is an in-memory model behind the
//! [`Herdr`](crate::Herdr) trait, this sits on the other side of
//! [`SocketClient`](crate::SocketClient): real JSON lines over a real unix
//! socket. Tests hand it herdr's replies as captured from a live server
//! (`herdr agent list`, `herdr workspace list`, …), so the client and
//! everything above it decode exactly the shape herdr sends, and a test
//! fails when that shape changes in a way the client cannot read.
//!
//! One request per connection, as herdr does; `events.subscribe` stays
//! open (sending nothing more) until the client hangs up.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;

/// What the socket answers to one request: a `result` object, or herdr's
/// error `code` and `message`.
pub type Answer = Result<Value, (String, String)>;

type Responder = dyn Fn(&str, &Value) -> Answer + Send + Sync;

/// A running replay socket; stops serving when dropped.
pub struct ReplaySocket {
    path: PathBuf,
    methods: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}

impl std::fmt::Debug for ReplaySocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplaySocket")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl ReplaySocket {
    /// Binds `path` and answers each request with `respond(method, params)`.
    /// Must be called inside a tokio runtime.
    ///
    /// # Errors
    /// If the socket cannot be bound.
    pub fn serve(
        path: &Path,
        respond: impl Fn(&str, &Value) -> Answer + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        let listener = UnixListener::bind(path)?;
        let respond: Arc<Responder> = Arc::new(respond);
        let methods = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&methods);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(connection(stream, Arc::clone(&respond), Arc::clone(&seen)));
            }
        });
        Ok(Self {
            path: path.to_path_buf(),
            methods,
            task,
        })
    }

    /// The socket path to connect a client to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every method asked so far, in arrival order.
    #[must_use]
    pub fn methods(&self) -> Vec<String> {
        self.methods
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Drop for ReplaySocket {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The `result` of a reply recorded with `herdr … --json`-style output
/// (`{"id": …, "result": {…}}`), for handing to [`ReplaySocket::serve`].
///
/// # Panics
/// If `recorded` is not such a reply (a broken fixture).
#[must_use]
pub fn recorded_result(recorded: &str) -> Value {
    let reply: Value = serde_json::from_str(recorded)
        .unwrap_or_else(|e| panic!("recorded herdr reply is not JSON: {e}"));
    match reply.get("result") {
        Some(result) if result.is_object() => result.clone(),
        _ => panic!("recorded herdr reply has no result object"),
    }
}

async fn connection(stream: UnixStream, respond: Arc<Responder>, methods: Arc<Mutex<Vec<String>>>) {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let Ok(Some(line)) = lines.next_line().await else {
        return;
    };
    let Ok(request) = serde_json::from_str::<Value>(&line) else {
        return;
    };
    let method = request["method"].as_str().unwrap_or_default().to_owned();
    methods
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(method.clone());
    let reply = match respond(&method, &request["params"]) {
        Ok(result) => json!({"id": request["id"], "result": result}),
        Err((code, message)) => {
            json!({"id": request["id"], "error": {"code": code, "message": message}})
        }
    };
    if write
        .write_all(format!("{reply}\n").as_bytes())
        .await
        .is_err()
        || method != "events.subscribe"
    {
        return;
    }
    // A subscription stays open until the client closes its side.
    while let Ok(Some(_)) = lines.next_line().await {}
}
