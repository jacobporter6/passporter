//! MVP auth end to end: the static bearer token over HTTP, and the stream's
//! first-message `StreamAuth`.

use std::sync::Arc;
use std::time::Duration;

use bridge::server::{ApiToken, StaticToken};
use futures_util::{SinkExt, StreamExt};
use protocol::AgentEvent;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use crate::harness::{Bridge, CuttableHerdr, PATIENCE, Stream};

const SECRET: &str = "feedfacefeedfacefeedfacefeedface";

async fn bridge_with_token(auth_timeout: Duration) -> Bridge {
    let token = ApiToken::new(SECRET).expect("valid token");
    Bridge::start_full(
        CuttableHerdr::new(),
        Arc::new(StaticToken::new(token)),
        move |settings| settings.server.stream.auth_timeout = auth_timeout,
    )
    .await
}

/// Reads until the server closes, failing on any data message: an
/// unauthenticated client must see nothing.
async fn expect_policy_close(stream: &mut Stream<TcpStream>) {
    let wait = async {
        loop {
            match stream.ws.next().await {
                Some(Ok(Message::Close(frame))) => return frame.map(|f| f.code),
                Some(Ok(Message::Text(text))) => panic!("data before auth: {text}"),
                Some(Ok(_)) => {}
                Some(Err(_)) | None => return None,
            }
        }
    };
    let code = tokio::time::timeout(PATIENCE, wait)
        .await
        .expect("closed in time");
    assert_eq!(code, Some(CloseCode::Policy));
}

async fn upgrade(bridge: &Bridge) -> Stream<TcpStream> {
    let tcp = TcpStream::connect(bridge.addr).await.expect("connect");
    Stream::upgrade(tcp).await.expect("handshake")
}

#[tokio::test]
async fn static_token_guards_the_http_api() {
    let bridge = bridge_with_token(Duration::from_secs(5)).await;
    let good = format!("Bearer {SECRET}");
    let ok = bridge
        .http("GET", "/v1/agents", &[("authorization", &good)])
        .await;
    assert_eq!(ok.status, 200);
    let wrong = format!("Bearer {}", SECRET.replace('d', "c"));
    let truncated = format!("Bearer {}", &SECRET[..31]);
    for headers in [
        &[][..],
        &[("authorization", wrong.as_str())][..],
        &[("authorization", truncated.as_str())][..],
        &[("authorization", SECRET)][..],
    ] {
        let response = bridge.http("GET", "/v1/agents", headers).await;
        assert_eq!(response.status, 401, "{headers:?}");
    }
    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn the_stream_starts_after_a_valid_auth_message() {
    let bridge = bridge_with_token(Duration::from_secs(5)).await;
    let mut stream = upgrade(&bridge).await;
    stream.send_auth(SECRET).await;
    assert!(matches!(
        stream.next_event().await,
        AgentEvent::Snapshot { .. }
    ));
    stream.close().await;
    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_wrong_or_malformed_auth_message_closes_the_stream() {
    let bridge = bridge_with_token(Duration::from_secs(5)).await;
    let wrong = format!(r#"{{"token":"{}"}}"#, SECRET.replace('d', "c"));
    let extra = format!(r#"{{"token":"{SECRET}","device":"x"}}"#);
    for first in [
        Message::Text(wrong.into()),
        Message::Text(extra.into()),
        Message::Text("hello".into()),
        Message::Binary(SECRET.as_bytes().to_vec().into()),
    ] {
        let mut stream = upgrade(&bridge).await;
        stream.ws.send(first).await.expect("send");
        expect_policy_close(&mut stream).await;
    }
    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_missing_or_late_auth_message_closes_the_stream() {
    let bridge = bridge_with_token(Duration::from_millis(200)).await;
    // Nothing at all.
    let mut silent = upgrade(&bridge).await;
    expect_policy_close(&mut silent).await;
    // The right token, too late.
    let mut late = upgrade(&bridge).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let auth = format!(r#"{{"token":"{SECRET}"}}"#);
    let _ = late.ws.send(Message::Text(auth.into())).await;
    expect_policy_close(&mut late).await;
    bridge.stop().await.expect("clean stop");
}
