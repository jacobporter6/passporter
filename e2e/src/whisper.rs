//! A fake `whisper-server`: answers every `POST /inference` with one canned
//! `verbose_json` transcript, so the bridge's real transcription path runs
//! end to end without a model. Loopback only, like the real one.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// The running fake; stops serving when dropped.
#[derive(Debug)]
pub struct FakeWhisper {
    addr: SocketAddr,
    served: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl FakeWhisper {
    /// Listens on a free loopback port, transcribing every clip as `text`.
    pub async fn serve(text: &str) -> std::io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let addr = listener.local_addr()?;
        let body = serde_json::json!({"text": text, "duration": 1.5}).to_string();
        let served = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&served);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (body, count) = (body.clone(), Arc::clone(&count));
                tokio::spawn(async move {
                    if answer(stream, &body).await.is_ok() {
                        count.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
        });
        Ok(Self { addr, served, task })
    }

    /// The address to put in the bridge's `[audio] whisper`.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// How many clips it has transcribed.
    #[must_use]
    pub fn served(&self) -> usize {
        self.served.load(Ordering::Relaxed)
    }
}

impl Drop for FakeWhisper {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Reads one request (headers, then `Content-Length` bytes of form) and
/// answers it with `body`.
async fn answer(stream: TcpStream, body: &str) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream);
    let mut length = 0_usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut form = vec![0; length];
    reader.read_exact(&mut form).await?;
    let reply = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = reader.into_inner();
    stream.write_all(reply.as_bytes()).await?;
    stream.shutdown().await
}
