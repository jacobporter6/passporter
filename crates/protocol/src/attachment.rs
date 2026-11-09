//! Photo attachments (design §5): `POST /v1/attachments` uploads one image,
//! and a prompt names the uploads it carries ([`Attachments`], inbound:
//! strict). The upload answers with an [`AttachmentReceipt`] (outbound:
//! tolerant).
//!
//! ```json
//! { "attachmentId": "9ca09978-ef89-46fd-9a85-118b84bee7b4", "mediaType": "image/jpeg", "bytes": 48213, "width": 2048, "height": 1536 }
//! ```
//!
//! The upload's body is the raw image and its id travels in the
//! `Idempotency-Key` header, so a retry after a lost reply is harmless.
//! The bridge sniffs the bytes, never trusting the declared type, and
//! re-encodes the pixels, so metadata (EXIF, GPS) never reaches its disk.

use serde::{Deserialize, Serialize};

use crate::error::ValidationError;
use crate::ids::AttachmentId;

/// Most attachments one prompt may carry.
pub const MAX_ATTACHMENTS: usize = 4;

/// The header carrying an upload's [`AttachmentId`].
pub const IDEMPOTENCY_KEY_HEADER: &str = "idempotency-key";

/// The uploads a prompt carries: at most [`MAX_ATTACHMENTS`], each once.
/// Empty (the default) for a plain prompt.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(try_from = "Vec<AttachmentId>", into = "Vec<AttachmentId>")]
pub struct Attachments(Vec<AttachmentId>);

impl Attachments {
    /// Validates `ids`: at most [`MAX_ATTACHMENTS`], no id twice.
    pub fn new(ids: Vec<AttachmentId>) -> Result<Self, ValidationError> {
        if ids.len() > MAX_ATTACHMENTS {
            return Err(ValidationError::TooLong {
                field: "attachments",
                max: MAX_ATTACHMENTS,
                actual: ids.len(),
            });
        }
        if ids.iter().enumerate().any(|(i, id)| ids[..i].contains(id)) {
            return Err(ValidationError::Duplicate {
                field: "attachments",
            });
        }
        Ok(Self(ids))
    }

    /// The ids, in the order the prompt lists them.
    #[must_use]
    pub fn as_slice(&self) -> &[AttachmentId] {
        &self.0
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl TryFrom<Vec<AttachmentId>> for Attachments {
    type Error = ValidationError;

    fn try_from(ids: Vec<AttachmentId>) -> Result<Self, Self::Error> {
        Self::new(ids)
    }
}

impl From<Attachments> for Vec<AttachmentId> {
    fn from(attachments: Attachments) -> Self {
        attachments.0
    }
}

/// The format an attachment is stored in, after the bridge re-encoded it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ImageType {
    /// `image/jpeg` (photos, and any lossy upload).
    #[serde(rename = "image/jpeg")]
    Jpeg,
    /// `image/png` (screenshots stay lossless).
    #[serde(rename = "image/png")]
    Png,
}

impl ImageType {
    /// The media type, e.g. `image/jpeg`.
    #[must_use]
    pub const fn media_type(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
        }
    }

    /// The file extension, without the dot.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Png => "png",
        }
    }
}

/// What `POST /v1/attachments` answers: the stored image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentReceipt {
    /// The id to name in a prompt's `attachments`.
    pub attachment_id: AttachmentId,
    /// The stored format.
    pub media_type: ImageType,
    /// The stored size in bytes.
    pub bytes: u64,
    /// Stored width in pixels.
    pub width: u32,
    /// Stored height in pixels.
    pub height: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "00000000-0000-4000-8000-0000000000a1";
    const B: &str = "00000000-0000-4000-8000-0000000000b2";

    fn id(s: &str) -> AttachmentId {
        AttachmentId::new(s).unwrap()
    }

    #[test]
    fn attachments_are_bounded_and_unique() {
        assert!(Attachments::new(Vec::new()).unwrap().is_empty());
        let two = Attachments::new(vec![id(A), id(B)]).unwrap();
        assert_eq!(two.as_slice(), [id(A), id(B)]);
        assert_eq!(
            Attachments::new(vec![id(A), id(A)]),
            Err(ValidationError::Duplicate {
                field: "attachments"
            })
        );
        let many: Vec<_> = (0..=MAX_ATTACHMENTS)
            .map(|i| id(&format!("00000000-0000-4000-8000-00000000000{i}")))
            .collect();
        assert!(matches!(
            Attachments::new(many),
            Err(ValidationError::TooLong { .. })
        ));
    }

    #[test]
    fn attachments_deserialize_strictly() {
        let ok: Attachments = serde_json::from_str(&format!(r#"["{A}"]"#)).unwrap();
        assert_eq!(ok.as_slice(), [id(A)]);
        assert!(serde_json::from_str::<Attachments>(&format!(r#"["{A}","{A}"]"#)).is_err());
        assert!(serde_json::from_str::<Attachments>(r#"["../etc/passwd"]"#).is_err());
        assert!(
            serde_json::from_str::<Attachments>(r#"["A0000000-0000-4000-8000-000000000000"]"#)
                .is_err()
        );
    }

    #[test]
    fn receipts_use_media_types() {
        let receipt = AttachmentReceipt {
            attachment_id: id(A),
            media_type: ImageType::Png,
            bytes: 10,
            width: 3,
            height: 2,
        };
        let json = serde_json::to_string(&receipt).unwrap();
        assert!(json.contains(r#""mediaType":"image/png""#), "{json}");
        assert_eq!(
            serde_json::from_str::<AttachmentReceipt>(&json).unwrap(),
            receipt
        );
        assert_eq!(ImageType::Jpeg.extension(), "jpg");
        assert_eq!(ImageType::Jpeg.media_type(), "image/jpeg");
    }
}
