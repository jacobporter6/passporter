//! Dictation over HTTP: resumable uploads, caps, and the transcript
//! endpoint in front of a fake whisper-server.

use std::sync::Arc;
use std::time::Duration;

use bridge::server::{AuthFuture, AuthRejection, AuthRequest, Authenticator, Principal};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::harness::{self, Bridge, CuttableHerdr, FakeWhisper, HttpResponse, PATIENCE, TOKEN};

const CLIP: &str = "00000000-0000-4000-8000-00000000d1c7";

fn declare(total_bytes: u64, duration_ms: u64) -> String {
    json!({ "audioId": CLIP, "totalBytes": total_bytes, "durationMs": duration_ms }).to_string()
}

fn json_body(response: &HttpResponse) -> Value {
    serde_json::from_str(&response.body).unwrap_or_else(|e| panic!("{e}: {}", response.body))
}

async fn chunk(bridge: &Bridge, offset: u64, bytes: &str) -> HttpResponse {
    let offset = offset.to_string();
    bridge
        .send_authed(
            "PATCH",
            &format!("/v1/audio/{CLIP}"),
            &[
                ("upload-offset", &offset),
                ("content-type", "application/octet-stream"),
            ],
            bytes,
        )
        .await
}

/// Polls the transcript until it is no longer pending.
async fn final_transcript(bridge: &Bridge) -> HttpResponse {
    let poll = async {
        loop {
            let response = bridge
                .get_authed(&format!("/v1/audio/{CLIP}/transcript"))
                .await;
            if response.status != 202 {
                return response;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(PATIENCE, poll)
        .await
        .expect("transcribed in time")
}

#[tokio::test]
async fn an_interrupted_upload_resumes_at_the_bridge_offset() {
    let bridge = Bridge::start(CuttableHerdr::new()).await;
    let created = bridge.post_authed("/v1/audio", &declare(10, 1500)).await;
    assert_eq!(created.status, 201, "{}", created.body);
    assert_eq!(json_body(&created)["offset"], 0);
    assert_eq!(json_body(&created)["state"], "uploading");

    let first = chunk(&bridge, 0, "01234").await;
    assert_eq!(first.status, 200, "{}", first.body);
    assert_eq!(json_body(&first)["offset"], 5);

    // The phone lost the reply and retries the same chunk: refused, with
    // the offset to resume from.
    let retried = chunk(&bridge, 0, "01234").await;
    assert_eq!(retried.status, 409);
    assert_eq!(json_body(&retried)["offset"], 5);

    // After a restart of the app it asks again, both ways.
    let status = bridge.get_authed(&format!("/v1/audio/{CLIP}")).await;
    assert_eq!(json_body(&status)["offset"], 5);
    let redeclared = bridge.post_authed("/v1/audio", &declare(10, 1500)).await;
    assert_eq!(redeclared.status, 200);
    assert_eq!(json_body(&redeclared)["offset"], 5);

    // A different clip under the same id is a conflict.
    let other = bridge.post_authed("/v1/audio", &declare(11, 1500)).await;
    assert_eq!(other.status, 409);

    let past_end = chunk(&bridge, 5, "567890").await;
    assert_eq!(past_end.status, 400);
    let last = chunk(&bridge, 5, "56789").await;
    assert_eq!(last.status, 200, "{}", last.body);
    assert_eq!(json_body(&last)["offset"], 10);
    assert_ne!(json_body(&last)["state"], "uploading");

    let transcript = final_transcript(&bridge).await;
    assert_eq!(transcript.status, 200, "{}", transcript.body);
    let transcript: protocol::Transcript =
        serde_json::from_str(&transcript.body).expect("a Transcript");
    assert_eq!(transcript.audio_id.as_str(), CLIP);
    assert_eq!(transcript.text, "heard 10 bytes");
    assert_eq!(transcript.duration_ms, 1500);
    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn the_transcript_is_pending_until_whisper_answers() {
    let whisper = FakeWhisper::gated();
    let gate = whisper.clone();
    let bridge = Bridge::start_with(CuttableHerdr::new(), move |settings| {
        settings.audio.transcriber = whisper;
    })
    .await;
    let path = format!("/v1/audio/{CLIP}/transcript");
    assert_eq!(bridge.get_authed(&path).await.status, 404);

    bridge.post_authed("/v1/audio", &declare(3, 900)).await;
    let pending = bridge.get_authed(&path).await;
    assert_eq!(pending.status, 202);
    assert_eq!(json_body(&pending)["state"], "uploading");

    assert_eq!(chunk(&bridge, 0, "abc").await.status, 200);
    let pending = bridge.get_authed(&path).await;
    assert_eq!(pending.status, 202);
    let state = json_body(&pending)["state"].clone();
    assert!(state == "queued" || state == "transcribing", "{state}");

    gate.release();
    let done = final_transcript(&bridge).await;
    assert_eq!(done.status, 200, "{}", done.body);
    assert_eq!(json_body(&done)["text"], "heard 3 bytes");
    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn caps_and_malformed_requests_are_refused() {
    let bridge = Bridge::start_with(CuttableHerdr::new(), |settings| {
        settings.audio.max_upload_bytes = 100;
        settings.audio.max_duration = Duration::from_secs(10);
        settings.audio.max_chunk_bytes = 8;
    })
    .await;
    assert_eq!(
        bridge
            .post_authed("/v1/audio", &declare(101, 1000))
            .await
            .status,
        413
    );
    assert_eq!(
        bridge
            .post_authed("/v1/audio", &declare(50, 10_001))
            .await
            .status,
        422
    );
    assert_eq!(
        bridge
            .post_authed("/v1/audio", &declare(0, 1000))
            .await
            .status,
        400
    );
    let unknown_field = json!({ "totalBytes": 5, "durationMs": 5, "codec": "opus" });
    assert_eq!(
        bridge
            .post_authed("/v1/audio", &unknown_field.to_string())
            .await
            .status,
        400
    );

    // Without an id the bridge picks one.
    let created = bridge
        .post_authed("/v1/audio", r#"{"totalBytes":50,"durationMs":1000}"#)
        .await;
    assert_eq!(created.status, 201);
    let picked = json_body(&created)["audioId"]
        .as_str()
        .expect("id")
        .to_owned();
    assert_ne!(picked, CLIP);

    // Chunks above the chunk limit, without an offset, or for no clip.
    assert_eq!(
        bridge
            .post_authed("/v1/audio", &declare(50, 1000))
            .await
            .status,
        201
    );
    assert_eq!(chunk(&bridge, 0, "123456789").await.status, 413);
    let no_offset = bridge
        .send_authed("PATCH", &format!("/v1/audio/{CLIP}"), &[], "1234")
        .await;
    assert_eq!(no_offset.status, 400);
    let unknown = bridge
        .send_authed(
            "PATCH",
            "/v1/audio/00000000-0000-4000-8000-000000000000",
            &[("upload-offset", "0")],
            "1",
        )
        .await;
    assert_eq!(unknown.status, 404);
    assert_eq!(bridge.get_authed("/v1/audio/not-a-uuid").await.status, 400);

    // Everything needs auth.
    let anonymous = bridge.http("GET", &format!("/v1/audio/{CLIP}"), &[]).await;
    assert_eq!(anonymous.status, 401);
    bridge.stop().await.expect("clean stop");
}

/// Sends `body` in two halves with `pause` in between (a slow uplink) and
/// returns the reply's status code.
async fn send_slowly(
    bridge: &Bridge,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
    pause: Duration,
) -> u16 {
    let mut stream = TcpStream::connect(bridge.addr).await.expect("connect");
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nhost: bridge\r\nconnection: close\r\n\
         authorization: Bearer {TOKEN}\r\ncontent-length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    let (first, rest) = body.split_at(body.len() / 2);
    stream
        .write_all(format!("{head}{first}").as_bytes())
        .await
        .expect("send head");
    tokio::time::sleep(pause).await;
    // The server may have answered (and closed) already; that is the reply.
    let _ = stream.write_all(rest.as_bytes()).await;
    let mut raw = Vec::new();
    tokio::time::timeout(PATIENCE, stream.read_to_end(&mut raw))
        .await
        .expect("reply in time")
        .expect("read");
    let raw = String::from_utf8_lossy(&raw);
    raw.split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {raw:?}"))
}

#[tokio::test]
async fn a_slow_chunk_gets_the_chunk_timeout_not_the_request_timeout() {
    let bridge = Bridge::start_with(CuttableHerdr::new(), |settings| {
        settings.server.request_timeout = Duration::from_millis(200);
        settings.audio.chunk_timeout = PATIENCE;
    })
    .await;
    let pause = Duration::from_millis(600);
    // Other routes keep the short timeout.
    let declared = send_slowly(&bridge, "POST", "/v1/audio", &[], &declare(10, 1500), pause).await;
    assert_eq!(declared, 408);

    assert_eq!(
        bridge
            .post_authed("/v1/audio", &declare(10, 1500))
            .await
            .status,
        201
    );
    let path = format!("/v1/audio/{CLIP}");
    let chunk = send_slowly(
        &bridge,
        "PATCH",
        &path,
        &[("upload-offset", "0")],
        "0123456789",
        pause,
    )
    .await;
    assert_eq!(chunk, 200);
    bridge.stop().await.expect("clean stop");
}

/// Authenticates `Bearer <device id>` as that device, so one bridge can
/// serve several devices.
#[derive(Debug)]
struct EachTokenItsDevice;

impl Authenticator for EachTokenItsDevice {
    fn authenticate<'a>(&'a self, request: AuthRequest<'a>) -> AuthFuture<'a> {
        let device = request
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(Principal::new);
        Box::pin(async move { device.ok_or(AuthRejection::Unauthenticated) })
    }
}

/// A request as `device`.
async fn as_device(
    bridge: &Bridge,
    device: &str,
    method: &str,
    path: &str,
    extra: &[(&str, &str)],
    body: &str,
) -> HttpResponse {
    let stream = TcpStream::connect(bridge.addr).await.expect("connect");
    let auth = format!("Bearer {device}");
    let mut headers = vec![("authorization", auth.as_str())];
    headers.extend_from_slice(extra);
    harness::request(stream, method, path, &headers, body).await
}

#[tokio::test]
async fn a_clip_is_invisible_to_other_devices() {
    const PHONE: &str = "dev-00000000000000aa";
    const TABLET: &str = "dev-00000000000000bb";
    let bridge =
        Bridge::start_full(CuttableHerdr::new(), Arc::new(EachTokenItsDevice), |_| {}).await;
    let json_type = [("content-type", "application/json")];
    let created = as_device(
        &bridge,
        PHONE,
        "POST",
        "/v1/audio",
        &json_type,
        &declare(4, 1500),
    )
    .await;
    assert_eq!(created.status, 201, "{}", created.body);
    let status_path = format!("/v1/audio/{CLIP}");
    let transcript_path = format!("/v1/audio/{CLIP}/transcript");
    let chunk_headers = [
        ("upload-offset", "0"),
        ("content-type", "application/octet-stream"),
    ];

    for (method, path, extra, body) in [
        ("GET", status_path.as_str(), &[][..], ""),
        ("GET", transcript_path.as_str(), &[][..], ""),
        ("PATCH", status_path.as_str(), &chunk_headers[..], "abcd"),
    ] {
        let response = as_device(&bridge, TABLET, method, path, extra, body).await;
        assert_eq!(response.status, 404, "{method} {path}: {}", response.body);
    }
    let taken = as_device(
        &bridge,
        TABLET,
        "POST",
        "/v1/audio",
        &json_type,
        &declare(4, 1500),
    )
    .await;
    assert_eq!(taken.status, 409, "{}", taken.body);
    assert!(
        !taken.body.contains(CLIP),
        "no status for another device's clip: {}",
        taken.body
    );

    // The owner's clip is untouched by all that.
    let own = as_device(&bridge, PHONE, "GET", &status_path, &[], "").await;
    assert_eq!(own.status, 200, "{}", own.body);
    assert_eq!(json_body(&own)["offset"], 0);
    bridge.stop().await.expect("clean stop");
}
