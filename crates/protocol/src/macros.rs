//! Shared plumbing for validated `String` newtypes.

/// Defines a validated `String` newtype that (de)serializes as a JSON string.
///
/// `$check` is a `fn(&str) -> Result<(), ValidationError>`; it runs in `new`,
/// and deserialization goes through `new` (`#[serde(try_from = "String")]`).
/// Note that serde materializes the whole `String` before the check runs; the
/// HTTP body limit is what bounds that allocation.
macro_rules! validated_string {
    ($(#[$meta:meta])* $name:ident, $check:expr) => {
        $(#[$meta])*
        #[derive(
            Debug,
            Clone,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            ::serde::Serialize,
            ::serde::Deserialize,
        )]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Validates and wraps `value`; the rules are in the type's docs.
            pub fn new(value: impl Into<String>) -> Result<Self, $crate::ValidationError> {
                let value = value.into();
                ($check)(value.as_str())?;
                Ok(Self(value))
            }

            /// The validated string.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = $crate::ValidationError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl ::core::fmt::Display for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

pub(crate) use validated_string;
