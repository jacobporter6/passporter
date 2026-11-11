//! The Rust half of the golden vector the Kotlin tests also check
//! (`android/src/test/resources/device-auth-golden.json`): the message the
//! native client signs is byte for byte `protocol::signed_message`, and the
//! `r || s` signature its DER conversion produces verifies the way the
//! bridge verifies it.

use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, VerifyingKey};
use protocol::{Challenge, DeviceId, signed_message};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Golden {
    device_id: String,
    challenge: String,
    message: String,
    sec1: String,
    p1363: String,
}

fn golden() -> Golden {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/android/src/test/resources/device-auth-golden.json"
    );
    let text = std::fs::read_to_string(path).expect("golden file");
    serde_json::from_str(&text).expect("golden json")
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
        .collect()
}

#[test]
fn the_signed_message_is_the_protocols() {
    let golden = golden();
    let device = DeviceId::new(golden.device_id).expect("device id");
    let challenge = Challenge::new(golden.challenge).expect("challenge");
    assert_eq!(signed_message(&device, &challenge), unhex(&golden.message));
}

#[test]
fn the_converted_signature_verifies_like_the_bridge() {
    let golden = golden();
    let key = VerifyingKey::from_sec1_bytes(&unhex(&golden.sec1)).expect("public point");
    let signature = Signature::from_slice(&unhex(&golden.p1363)).expect("r || s");
    key.verify(&unhex(&golden.message), &signature)
        .expect("the bridge accepts the signature");
}
