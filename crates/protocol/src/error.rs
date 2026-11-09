//! The single error type returned by every validating constructor.

use core::fmt;

/// Why a value was rejected by a validating constructor or during
/// deserialization of an inbound type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    /// A required value was empty (or whitespace only, for text).
    Empty {
        /// The name of the rejected field.
        field: &'static str,
    },
    /// A value exceeded its size limit.
    TooLong {
        /// The name of the rejected field.
        field: &'static str,
        /// The limit, in the field's unit (bytes for text, items for lists).
        max: usize,
        /// The actual size, in the same unit.
        actual: usize,
    },
    /// Text contained a character that could drive the terminal or disguise
    /// the text: a control character (other than `\n` where allowed), a bidi
    /// control or a Unicode line/paragraph separator. See [`crate::text`].
    ControlCharacter {
        /// The name of the rejected field.
        field: &'static str,
    },
    /// An identifier contained a character outside its allowed set.
    InvalidCharacter {
        /// The name of the rejected field.
        field: &'static str,
    },
    /// A UUID was not in canonical lowercase 8-4-4-4-12 hex form.
    InvalidUuid {
        /// The name of the rejected field.
        field: &'static str,
    },
    /// A number was above its maximum.
    OutOfRange {
        /// The name of the rejected field.
        field: &'static str,
        /// The largest allowed value.
        max: u64,
        /// The actual value.
        actual: u64,
    },
    /// Two options of one pending input share an id.
    DuplicateOptionId(String),
    /// A list named the same item twice.
    Duplicate {
        /// The name of the rejected field.
        field: &'static str,
    },
    /// A key name was not on the allowlist (design §8).
    KeyNotAllowed(String),
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty { field } => write!(f, "{field} must not be empty"),
            Self::TooLong { field, max, actual } => {
                write!(f, "{field} is too long ({actual} > {max})")
            }
            Self::ControlCharacter { field } => {
                write!(f, "{field} contains a disallowed control character")
            }
            Self::InvalidCharacter { field } => {
                write!(f, "{field} contains a disallowed character")
            }
            Self::InvalidUuid { field } => {
                write!(f, "{field} must be a lowercase hyphenated UUID")
            }
            Self::OutOfRange { field, max, actual } => {
                write!(f, "{field} is out of range ({actual} > {max})")
            }
            Self::DuplicateOptionId(id) => write!(f, "option id {id:?} is not unique"),
            Self::Duplicate { field } => write!(f, "{field} names an item twice"),
            Self::KeyNotAllowed(key) => write!(f, "key {key:?} is not on the allowlist"),
        }
    }
}

impl core::error::Error for ValidationError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_is_human_readable() {
        let cases = [
            (
                ValidationError::Empty { field: "text" },
                "text must not be empty",
            ),
            (
                ValidationError::TooLong {
                    field: "text",
                    max: 2,
                    actual: 3,
                },
                "text is too long (3 > 2)",
            ),
            (
                ValidationError::ControlCharacter { field: "text" },
                "text contains a disallowed control character",
            ),
            (
                ValidationError::InvalidCharacter { field: "optionId" },
                "optionId contains a disallowed character",
            ),
            (
                ValidationError::InvalidUuid {
                    field: "idempotencyKey",
                },
                "idempotencyKey must be a lowercase hyphenated UUID",
            ),
            (
                ValidationError::OutOfRange {
                    field: "confidence",
                    max: 100,
                    actual: 101,
                },
                "confidence is out of range (101 > 100)",
            ),
            (
                ValidationError::Duplicate {
                    field: "attachments",
                },
                "attachments names an item twice",
            ),
            (
                ValidationError::DuplicateOptionId("deny".into()),
                "option id \"deny\" is not unique",
            ),
            (
                ValidationError::KeyNotAllowed("f1".into()),
                "key \"f1\" is not on the allowlist",
            ),
        ];
        for (err, want) in cases {
            assert_eq!(err.to_string(), want);
        }
    }
}
