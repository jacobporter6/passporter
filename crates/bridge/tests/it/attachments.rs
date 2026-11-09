//! `POST /v1/attachments` and prompts that carry photos: validation, the
//! metadata strip, idempotency, device scoping, the car, and what the
//! agent is sent.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bridge::server::{AuthFuture, AuthRejection, AuthRequest, Authenticator, Principal};
use herdr::{AgentStatus, Call, Herdr, PaneId, Target};
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, Rgb, RgbImage};
use protocol::{ActionResult, AttachmentReceipt, ImageType, RefusalReason};
use serde_json::{Value, json};
use tokio::net::TcpStream;

use crate::harness::{self, Bridge, CuttableHerdr, HttpResponse};

const PHONE: &str = "dev-00000000000000aa";
const TABLET: &str = "dev-00000000000000bb";
const PHOTO: &str = "00000000-0000-4000-8000-00000000a771";
const OTHER: &str = "00000000-0000-4000-8000-00000000a772";
const KEY1: &str = "00000000-0000-4000-8000-000000000001";
const KEY2: &str = "00000000-0000-4000-8000-000000000002";
const KEY3: &str = "00000000-0000-4000-8000-000000000003";
/// Planted in the photo's EXIF GPS block and in a JPEG comment.
const PLANTED: &[u8] = b"GPS 51.5007N 0.1246W";

/// Every bearer token is its own device.
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

/// A 64 x 48 JPEG carrying an EXIF APP1 segment (a TIFF block whose GPS
/// IFD holds [`PLANTED`]) and a comment with the same text.
fn photo_with_gps() -> Vec<u8> {
    let pixels = RgbImage::from_fn(64, 48, |x, y| {
        Rgb([
            u8::try_from(x * 4).unwrap_or(0),
            u8::try_from(y * 5).unwrap_or(0),
            90,
        ])
    });
    let mut jpeg = Vec::new();
    DynamicImage::ImageRgb8(pixels)
        .write_with_encoder(JpegEncoder::new_with_quality(&mut jpeg, 90))
        .expect("encode");
    // TIFF: IFD0 with one entry, the GPS IFD pointer; the GPS IFD with one
    // entry, GPSProcessingMethod (UNDEFINED) pointing at the marker.
    let mut tiff = b"II*\0".to_vec();
    tiff.extend_from_slice(&8u32.to_le_bytes());
    let entry = |t: &mut Vec<u8>, tag: u16, kind: u16, count: u32, value: u32| {
        t.extend_from_slice(&tag.to_le_bytes());
        t.extend_from_slice(&kind.to_le_bytes());
        t.extend_from_slice(&count.to_le_bytes());
        t.extend_from_slice(&value.to_le_bytes());
    };
    let gps_ifd = 8 + 2 + 12 + 4;
    tiff.extend_from_slice(&1u16.to_le_bytes());
    entry(&mut tiff, 0x8825, 4, 1, gps_ifd);
    tiff.extend_from_slice(&0u32.to_le_bytes());
    let marker = gps_ifd + 2 + 12 + 4;
    tiff.extend_from_slice(&1u16.to_le_bytes());
    let len = u32::try_from(PLANTED.len()).expect("short");
    entry(&mut tiff, 0x001B, 7, len, marker);
    tiff.extend_from_slice(&0u32.to_le_bytes());
    tiff.extend_from_slice(PLANTED);
    let mut app1 = b"Exif\0\0".to_vec();
    app1.extend_from_slice(&tiff);
    let mut out = jpeg[..2].to_vec();
    for (kind, payload) in [(0xE1u8, app1), (0xFE, PLANTED.to_vec())] {
        out.extend_from_slice(&[0xFF, kind]);
        let len = u16::try_from(payload.len() + 2).expect("short segment");
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&payload);
    }
    out.extend_from_slice(&jpeg[2..]);
    out
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

struct Fixture {
    bridge: Bridge,
    pane: PaneId,
    agent_id: String,
    dir: PathBuf,
    audit: PathBuf,
    _tmp: tempfile::TempDir,
}

async fn fixture(kind: &str, adjust: impl FnOnce(&mut bridge::Settings)) -> Fixture {
    let tmp = tempfile::tempdir().expect("tmp");
    let dir = tmp.path().join("attachments");
    let audit = tmp.path().join("audit.jsonl");
    let herdr = CuttableHerdr::new();
    let pane = herdr.fake.add_agent("worker", kind, AgentStatus::Idle);
    let (settings_dir, settings_audit) = (dir.clone(), audit.clone());
    let bridge = Bridge::start_full(herdr, Arc::new(EachTokenItsDevice), move |settings| {
        settings.attachments.dir = settings_dir;
        settings.actions.audit_log = settings_audit;
        adjust(settings);
    })
    .await;
    let agent_id = bridge.wait_for_agents("the agent", |a| a.len() == 1).await[0]
        .id
        .to_string();
    Fixture {
        bridge,
        pane,
        agent_id,
        dir,
        audit,
        _tmp: tmp,
    }
}

impl Fixture {
    async fn send(
        &self,
        device: &str,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> HttpResponse {
        let stream = TcpStream::connect(self.bridge.addr).await.expect("connect");
        let auth = format!("Bearer {device}");
        let mut all = vec![("authorization", auth.as_str())];
        all.extend_from_slice(headers);
        harness::request(stream, method, path, &all, body).await
    }

    async fn upload(
        &self,
        device: &str,
        id: &str,
        content_type: &str,
        body: &[u8],
    ) -> HttpResponse {
        let headers = [("idempotency-key", id), ("content-type", content_type)];
        self.send(device, "POST", "/v1/attachments", &headers, body)
            .await
    }

    async fn prompt(
        &self,
        device: &str,
        key: &str,
        text: &str,
        attachments: &[&str],
        origin: &str,
    ) -> (u16, ActionResult) {
        let seq = self
            .bridge
            .fake()
            .agent(&Target::from(&self.pane))
            .await
            .expect("agent")
            .state_change_seq;
        let body = json!({
            "idempotencyKey": key,
            "agentId": self.agent_id,
            "expectedSeq": seq,
            "createdAt": 1_762_500_000_000_u64,
            "kind": "prompt",
            "payload": {"text": text, "attachments": attachments},
            "origin": origin,
        })
        .to_string();
        let headers = [("content-type", "application/json")];
        let response = self
            .send(device, "POST", "/v1/actions", &headers, body.as_bytes())
            .await;
        let result = serde_json::from_str(&response.body)
            .unwrap_or_else(|e| panic!("{e}: {}", response.body));
        (response.status, result)
    }

    fn prompts(&self) -> Vec<String> {
        self.bridge
            .fake()
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Prompt { text, .. } => Some(text),
                _ => None,
            })
            .collect()
    }

    fn audit_lines(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.audit)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).expect("json line"))
            .collect()
    }

    fn files(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.dir)
            .map(|dir| {
                dir.map(|e| e.expect("entry").file_name().into_string().expect("utf-8"))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }
}

fn refusal(result: &ActionResult) -> Option<RefusalReason> {
    match result {
        ActionResult::Refused { reason, .. } => Some(*reason),
        _ => None,
    }
}

fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).expect("meta").permissions().mode() & 0o777
}

#[tokio::test]
async fn a_photo_is_stored_without_its_metadata_and_replayed_idempotently() {
    let f = fixture("claude", |_| {}).await;
    let upload = photo_with_gps();
    assert!(contains(&upload, PLANTED) && contains(&upload, b"Exif\0\0"));

    let created = f.upload(PHONE, PHOTO, "image/jpeg", &upload).await;
    assert_eq!(created.status, 201, "{}", created.body);
    let receipt: AttachmentReceipt = serde_json::from_str(&created.body).expect("receipt");
    assert_eq!(receipt.attachment_id.as_str(), PHOTO);
    assert_eq!(receipt.media_type, ImageType::Jpeg);
    assert_eq!((receipt.width, receipt.height), (64, 48));

    let stored_path = f.dir.join(format!("{PHOTO}.jpg"));
    let stored = std::fs::read(&stored_path).expect("stored");
    assert_eq!(stored.len() as u64, receipt.bytes);
    assert!(
        !contains(&stored, PLANTED),
        "GPS and comments never reach disk"
    );
    assert!(!contains(&stored, b"Exif"), "no EXIF block at all");
    assert_eq!(mode(&stored_path), 0o600);
    assert_eq!(mode(&f.dir), 0o700);
    let record = std::fs::read(f.dir.join(format!("{PHOTO}.json"))).expect("record");
    assert!(!contains(&record, PLANTED));

    // A lost reply: the same upload again is the same attachment.
    let replay = f.upload(PHONE, PHOTO, "image/jpeg", &upload).await;
    assert_eq!(replay.status, 200, "{}", replay.body);
    assert_eq!(
        serde_json::from_str::<AttachmentReceipt>(&replay.body).expect("receipt"),
        receipt
    );
    // Other bytes, or another device, under the same id: refused.
    let mut altered = upload.clone();
    let last = altered.len() - 3;
    altered[last] ^= 0x55;
    assert_eq!(
        f.upload(PHONE, PHOTO, "image/jpeg", &altered).await.status,
        409
    );
    assert_eq!(
        f.upload(TABLET, PHOTO, "image/jpeg", &upload).await.status,
        409
    );
    assert_eq!(
        f.files(),
        [format!("{PHOTO}.jpg"), format!("{PHOTO}.json")],
        "one attachment, nothing else"
    );
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn uploads_are_validated_by_their_bytes_and_capped() {
    let f = fixture("claude", |settings| {
        settings.attachments.max_upload_bytes = 64 * 1024;
    })
    .await;
    let photo = photo_with_gps();
    // A decoy: declared a PNG, actually a script.
    let decoy = f
        .upload(PHONE, PHOTO, "image/png", b"<script>alert(1)</script>")
        .await;
    assert_eq!(decoy.status, 415, "{}", decoy.body);
    // Declared as something other than an image, whatever the bytes.
    for declared in ["text/html", "image/svg+xml", "application/octet-stream"] {
        let refused = f.upload(PHONE, PHOTO, declared, &photo).await;
        assert_eq!(refused.status, 415, "{declared}: {}", refused.body);
    }
    // Looks like a JPEG, does not decode.
    let truncated = &photo[..photo.len() / 3];
    assert_eq!(
        f.upload(PHONE, PHOTO, "image/jpeg", truncated).await.status,
        422
    );
    // Over the size cap.
    let mut huge = photo.clone();
    huge.resize(65 * 1024, 0);
    assert_eq!(
        f.upload(PHONE, PHOTO, "image/jpeg", &huge).await.status,
        413
    );
    // No (or a bad) idempotency key.
    let headers = [("content-type", "image/jpeg")];
    let keyless = f
        .send(PHONE, "POST", "/v1/attachments", &headers, &photo)
        .await;
    assert_eq!(keyless.status, 400);
    assert_eq!(
        f.upload(PHONE, "retry-1", "image/jpeg", &photo)
            .await
            .status,
        400
    );
    // Without a credential.
    let stream = TcpStream::connect(f.bridge.addr).await.expect("connect");
    let anonymous = harness::request(
        stream,
        "POST",
        "/v1/attachments",
        &[("idempotency-key", PHOTO), ("content-type", "image/jpeg")],
        &photo,
    )
    .await;
    assert_eq!(anonymous.status, 401);
    assert_eq!(f.files(), Vec::<String>::new(), "nothing was stored");
    // There is no way to read an attachment back.
    let read = f
        .send(PHONE, "GET", &format!("/v1/attachments/{PHOTO}"), &[], b"")
        .await;
    assert_eq!(read.status, 404);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_prompt_names_the_photo_and_the_audit_lists_it() {
    let f = fixture("claude", |_| {}).await;
    let created = f
        .upload(PHONE, PHOTO, "image/jpeg", &photo_with_gps())
        .await;
    assert_eq!(created.status, 201, "{}", created.body);
    let receipt: AttachmentReceipt = serde_json::from_str(&created.body).expect("receipt");

    let (status, result) = f
        .prompt(PHONE, KEY1, "What is this error?", &[PHOTO], "app")
        .await;
    assert_eq!(status, 200, "{result:?}");
    let path = std::fs::canonicalize(f.dir.join(format!("{PHOTO}.jpg"))).expect("path");
    assert_eq!(
        f.prompts(),
        [format!(
            "What is this error?\n\nAttached image: @{}",
            path.display()
        )]
    );

    let audit = f.audit_lines();
    assert_eq!(audit[0]["phase"], "execute");
    assert_eq!(audit[0]["detail"], "What is this error?");
    assert_eq!(
        audit[0]["attachments"],
        json!([{"id": PHOTO, "mediaType": "image/jpeg", "bytes": receipt.bytes}])
    );
    let line = audit[0].to_string();
    assert!(
        !line.contains(&path.display().to_string()),
        "no path: {line}"
    );
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn codex_gets_the_plain_path() {
    let f = fixture("codex", |_| {}).await;
    let created = f
        .upload(PHONE, PHOTO, "image/jpeg", &photo_with_gps())
        .await;
    assert_eq!(created.status, 201, "{}", created.body);
    let (status, _) = f.prompt(PHONE, KEY1, "Look", &[PHOTO], "app").await;
    assert_eq!(status, 200);
    let path = std::fs::canonicalize(f.dir.join(format!("{PHOTO}.jpg"))).expect("path");
    assert_eq!(
        f.prompts(),
        [format!("Look\n\nAttached image: {}", path.display())]
    );
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn another_devices_photo_or_the_car_sends_nothing() {
    let f = fixture("claude", |_| {}).await;
    let created = f
        .upload(PHONE, PHOTO, "image/jpeg", &photo_with_gps())
        .await;
    assert_eq!(created.status, 201, "{}", created.body);

    let (status, result) = f.prompt(TABLET, KEY1, "Mine now", &[PHOTO], "app").await;
    assert_eq!(status, 422, "{result:?}");
    assert_eq!(refusal(&result), Some(RefusalReason::UnknownAttachment));

    let (status, result) = f.prompt(PHONE, KEY2, "Gone", &[OTHER], "app").await;
    assert_eq!(status, 422, "{result:?}");
    assert_eq!(refusal(&result), Some(RefusalReason::UnknownAttachment));

    let (status, result) = f.prompt(PHONE, KEY3, "From the car", &[PHOTO], "car").await;
    assert_eq!(status, 403, "{result:?}");
    assert_eq!(refusal(&result), Some(RefusalReason::CarPolicy));

    assert_eq!(
        f.prompts(),
        Vec::<String>::new(),
        "nothing reached the agent"
    );
    assert!(
        f.audit_lines()
            .iter()
            .all(|line| line["phase"] != "execute"),
        "refusals never execute"
    );
    f.bridge.stop().await.expect("clean stop");
}
