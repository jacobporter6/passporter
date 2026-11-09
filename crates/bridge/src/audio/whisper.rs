//! The transcription backend (design §6.3): whisper.cpp's `whisper-server`
//! on loopback, behind [`Transcriber`] so tests can swap in a fake.
//!
//! [`WhisperClient`] posts the clip to `whisper-server`'s `/inference`
//! endpoint as `multipart/form-data` and reads its `verbose_json` reply.
//! The server is started with `--convert` (deploy/whisper), so it accepts
//! whatever container the phone recorded (`WebM`/Opus, AAC, WAV).

use std::collections::VecDeque;
use std::convert::Infallible;
use std::fmt::{self, Write as _};
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Bytes;
use axum::http::header::{CONTENT_TYPE, HOST};
use axum::http::{Method, Request, StatusCode};
use http_body::{Body, Frame, SizeHint};
use http_body_util::{BodyExt, Limited};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use tokio::net::TcpStream;

/// A boxed transcription.
pub type TranscribeFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Recognized, TranscribeError>> + Send + 'a>>;

/// Turns one audio clip into text.
pub trait Transcriber: fmt::Debug + Send + Sync + 'static {
    /// Transcribes `audio`, the clip exactly as uploaded.
    fn transcribe(&self, audio: Bytes) -> TranscribeFuture<'_>;
}

/// What the backend heard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recognized {
    /// The recognized text, as the backend returned it.
    pub text: String,
    /// The clip's length as the backend decoded it, when it says.
    pub duration: Option<Duration>,
}

/// Why a transcription failed. Logged on the bridge; clients only learn
/// that it failed.
#[derive(Debug, thiserror::Error)]
pub enum TranscribeError {
    /// `whisper-server` is not listening (not started, or still loading
    /// the model).
    #[error("connecting to whisper-server at {addr}: {source}")]
    Connect {
        /// Where we tried.
        addr: SocketAddr,
        /// The underlying error.
        source: io::Error,
    },
    /// The HTTP exchange broke off.
    #[error("talking to whisper-server: {0}")]
    Http(#[from] hyper::Error),
    /// No reply in time.
    #[error("whisper-server did not answer within {0:?}")]
    Timeout(Duration),
    /// A non-success status.
    #[error("whisper-server answered HTTP {0}")]
    Status(StatusCode),
    /// The reply carried an `error` (e.g. an undecodable clip).
    #[error("whisper-server reported: {0}")]
    Server(String),
    /// The reply was not the JSON we expect.
    #[error("unreadable whisper-server reply: {0}")]
    Reply(String),
    /// The request could not be put together.
    #[error("preparing the whisper-server request: {0}")]
    Request(String),
}

impl TranscribeError {
    /// Whether whisper-server was merely unreachable or busy (not listening
    /// yet, or answering 503 while it loads its model), so the same clip is
    /// worth sending again. A connection that breaks mid-request is not:
    /// the clip may be what crashed the server, and resending it would
    /// crash it again while the queue waits.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Connect { .. } => true,
            Self::Status(status) => *status == StatusCode::SERVICE_UNAVAILABLE,
            Self::Http(_)
            | Self::Timeout(_)
            | Self::Server(_)
            | Self::Reply(_)
            | Self::Request(_) => false,
        }
    }
}

/// Largest reply read from `whisper-server`: a five-minute clip's
/// `verbose_json` (text plus segments) is a few tens of KiB.
const MAX_REPLY_BYTES: usize = 4 * 1024 * 1024;

/// A [`Transcriber`] backed by `whisper-server` over plain HTTP. The bridge
/// only ever points it at loopback (see [`crate::config`]), so audio never
/// leaves the machine.
#[derive(Debug, Clone)]
pub struct WhisperClient {
    addr: SocketAddr,
    timeout: Duration,
}

impl WhisperClient {
    /// A client of the `whisper-server` at `addr`; each transcription,
    /// connect to last byte, must finish within `timeout`.
    #[must_use]
    pub fn new(addr: SocketAddr, timeout: Duration) -> Self {
        Self { addr, timeout }
    }

    async fn exchange(&self, audio: Bytes) -> Result<Recognized, TranscribeError> {
        let stream =
            TcpStream::connect(self.addr)
                .await
                .map_err(|source| TranscribeError::Connect {
                    addr: self.addr,
                    source,
                })?;
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        // The connection ends once `sender` is dropped and the reply is read.
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                tracing::debug!(%error, "whisper-server connection ended with an error");
            }
        });

        let boundary = random_boundary()
            .map_err(|error| TranscribeError::Request(format!("picking a boundary: {error}")))?;
        let request = Request::builder()
            .method(Method::POST)
            .uri("/inference")
            .header(HOST, self.addr.to_string())
            .header(
                CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(multipart_body(&boundary, audio))
            .map_err(|error| TranscribeError::Request(error.to_string()))?;
        let response = sender.send_request(request).await?;
        let status = response.status();
        let body = Limited::new(response.into_body(), MAX_REPLY_BYTES)
            .collect()
            .await
            .map_err(|error| TranscribeError::Reply(error.to_string()))?
            .to_bytes();
        if !status.is_success() {
            return Err(TranscribeError::Status(status));
        }
        parse_reply(&body)
    }
}

impl Transcriber for WhisperClient {
    fn transcribe(&self, audio: Bytes) -> TranscribeFuture<'_> {
        Box::pin(async move {
            tokio::time::timeout(self.timeout, self.exchange(audio))
                .await
                .unwrap_or(Err(TranscribeError::Timeout(self.timeout)))
        })
    }
}

/// The parts of `whisper-server`'s `verbose_json` reply we use. It also
/// answers `{"error": "..."}` (with HTTP 200) for a clip it cannot decode.
#[derive(Debug, Deserialize)]
struct Reply {
    text: Option<String>,
    /// Seconds of decoded audio.
    duration: Option<f64>,
    error: Option<String>,
}

fn parse_reply(body: &[u8]) -> Result<Recognized, TranscribeError> {
    let reply: Reply =
        serde_json::from_slice(body).map_err(|error| TranscribeError::Reply(error.to_string()))?;
    if let Some(error) = reply.error {
        return Err(TranscribeError::Server(error));
    }
    let text = reply
        .text
        .ok_or_else(|| TranscribeError::Reply("no text in the reply".to_owned()))?;
    let duration = reply
        .duration
        .and_then(|secs| Duration::try_from_secs_f64(secs).ok());
    Ok(Recognized { text, duration })
}

/// A random multipart boundary: 128 random bits make a collision with
/// the audio vanishingly unlikely, so the clip is never scanned.
fn random_boundary() -> io::Result<String> {
    let bytes: [u8; 16] = super::random_bytes()?;
    let mut boundary = String::from("passporter-");
    for byte in bytes {
        let _ = write!(boundary, "{byte:02x}");
    }
    Ok(boundary)
}

/// The `/inference` form: the reply format, then the clip as `file`. The
/// clip's buffer is sent as is, between a small head and tail.
fn multipart_body(boundary: &str, audio: Bytes) -> Parts {
    let head = format!(
        "--{boundary}\r\n\
         Content-Disposition: form-data; name=\"response_format\"\r\n\r\n\
         verbose_json\r\n\
         --{boundary}\r\n\
         Content-Disposition: form-data; name=\"file\"; filename=\"clip\"\r\n\
         Content-Type: application/octet-stream\r\n\r\n"
    );
    let tail = format!("\r\n--{boundary}--\r\n");
    Parts::new([Bytes::from(head), audio, Bytes::from(tail)])
}

/// A request body sent as a few buffers in turn, with an exact length (so
/// hyper sends `Content-Length`).
#[derive(Debug)]
struct Parts {
    parts: VecDeque<Bytes>,
    remaining: u64,
}

impl Parts {
    fn new(parts: impl IntoIterator<Item = Bytes>) -> Self {
        let parts: VecDeque<Bytes> = parts.into_iter().filter(|p| !p.is_empty()).collect();
        let remaining = parts.iter().map(|p| p.len() as u64).sum();
        Self { parts, remaining }
    }
}

impl Body for Parts {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        let next = self.parts.pop_front();
        if let Some(part) = &next {
            self.remaining -= part.len() as u64;
        }
        Poll::Ready(next.map(|part| Ok(Frame::data(part))))
    }

    fn is_end_stream(&self) -> bool {
        self.parts.is_empty()
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.remaining)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::routing::post;

    use super::*;

    /// What the stand-in `whisper-server` received.
    #[derive(Debug, Default)]
    struct Seen {
        content_type: Option<String>,
        body: Vec<u8>,
    }

    /// Serves `reply` (with `status`) on `/inference` from loopback.
    async fn fake_server(
        status: StatusCode,
        reply: &'static str,
    ) -> (SocketAddr, Arc<Mutex<Seen>>) {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let app =
            Router::new()
                .route(
                    "/inference",
                    post(
                        move |State(seen): State<Arc<Mutex<Seen>>>,
                              headers: HeaderMap,
                              body: Bytes| async move {
                            let mut seen = seen.lock().expect("lock");
                            seen.content_type = headers
                                .get(CONTENT_TYPE)
                                .and_then(|v| v.to_str().ok())
                                .map(str::to_owned);
                            seen.body = body.to_vec();
                            (status, reply)
                        },
                    ),
                )
                .with_state(Arc::clone(&seen));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await });
        (addr, seen)
    }

    #[tokio::test]
    async fn posts_the_clip_as_a_form_and_reads_verbose_json() {
        let (addr, seen) = fake_server(
            StatusCode::OK,
            r#"{"task":"transcribe","language":"en","duration":2.5,"text":" yes, go ahead","segments":[]}"#,
        )
        .await;
        let client = WhisperClient::new(addr, Duration::from_secs(5));
        let recognized = client
            .transcribe(Bytes::from_static(b"RIFF fake audio"))
            .await
            .expect("transcribed");
        assert_eq!(recognized.text, " yes, go ahead");
        assert_eq!(recognized.duration, Some(Duration::from_millis(2500)));

        let seen = seen.lock().expect("lock");
        let content_type = seen.content_type.as_deref().expect("content type");
        let boundary = content_type
            .strip_prefix("multipart/form-data; boundary=")
            .expect("multipart");
        let body = String::from_utf8_lossy(&seen.body);
        assert!(body.contains("name=\"file\""), "{body}");
        assert!(body.contains("RIFF fake audio"), "{body}");
        assert!(body.contains("verbose_json"), "{body}");
        assert!(body.ends_with(&format!("--{boundary}--\r\n")), "{body}");
    }

    #[tokio::test]
    async fn an_error_reply_is_an_error() {
        let (addr, _) = fake_server(StatusCode::OK, r#"{"error":"failed to read audio"}"#).await;
        let client = WhisperClient::new(addr, Duration::from_secs(5));
        let error = client
            .transcribe(Bytes::from_static(b"junk"))
            .await
            .expect_err("server error");
        assert!(matches!(error, TranscribeError::Server(_)), "{error}");
    }

    #[tokio::test]
    async fn a_failure_status_is_an_error() {
        let (addr, _) = fake_server(StatusCode::INTERNAL_SERVER_ERROR, "{}").await;
        let client = WhisperClient::new(addr, Duration::from_secs(5));
        let error = client
            .transcribe(Bytes::from_static(b"x"))
            .await
            .expect_err("status");
        assert!(
            matches!(
                error,
                TranscribeError::Status(StatusCode::INTERNAL_SERVER_ERROR)
            ),
            "{error}"
        );
    }

    #[tokio::test]
    async fn nothing_listening_is_a_connect_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);
        let client = WhisperClient::new(addr, Duration::from_secs(5));
        let error = client
            .transcribe(Bytes::from_static(b"x"))
            .await
            .expect_err("refused");
        assert!(matches!(error, TranscribeError::Connect { .. }), "{error}");
    }

    #[tokio::test]
    async fn the_body_is_the_parts_in_order_with_an_exact_length() {
        let body = multipart_body("b", Bytes::from_static(b"AUDIO"));
        assert_eq!(body.size_hint().exact(), Some(body.remaining));
        let expected = body.remaining;
        let bytes = body.collect().await.expect("infallible").to_bytes();
        assert_eq!(bytes.len() as u64, expected);
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.starts_with("--b\r\n"), "{text}");
        assert!(text.contains("\r\n\r\nAUDIO\r\n--b--\r\n"), "{text}");
    }

    #[test]
    fn boundaries_are_random() {
        let a = random_boundary().expect("boundary");
        assert_ne!(a, random_boundary().expect("boundary"));
        assert_eq!(a.len(), "passporter-".len() + 32);
    }

    #[test]
    fn only_unreachable_or_busy_backends_are_transient() {
        let refused = TranscribeError::Connect {
            addr: "127.0.0.1:1".parse().expect("addr"),
            source: io::ErrorKind::ConnectionRefused.into(),
        };
        assert!(refused.is_transient());
        assert!(TranscribeError::Status(StatusCode::SERVICE_UNAVAILABLE).is_transient());
        assert!(!TranscribeError::Status(StatusCode::INTERNAL_SERVER_ERROR).is_transient());
        assert!(!TranscribeError::Server("bad audio".into()).is_transient());
        assert!(!TranscribeError::Timeout(Duration::from_secs(1)).is_transient());
    }

    #[test]
    fn a_reply_without_text_is_unreadable() {
        assert!(matches!(
            parse_reply(br#"{"duration": 1.0}"#),
            Err(TranscribeError::Reply(_))
        ));
        assert!(matches!(
            parse_reply(b"not json"),
            Err(TranscribeError::Reply(_))
        ));
    }
}
