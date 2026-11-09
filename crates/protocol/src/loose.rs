//! Single-pass decoding for flattened, internally tagged outbound types.
//!
//! `#[serde(flatten)]` and `#[serde(tag = "...")]` make serde buffer the
//! whole object before it knows the variant. That costs runtime work, and in
//! the web app's WASM it costs about 30 KB of generated code per type. The
//! hand-written `Deserialize` impls read each object once instead:
//!
//! - [`ThreadItem`](crate::ThreadItem) and
//!   [`PendingInput`](crate::PendingInput) hold the variant's fields as
//!   [`Loose`] values until the tag is known. A `Loose` value can be any
//!   JSON value, so a variant this app predates still parses whatever shape
//!   its fields have.
//! - [`AgentEvent`](crate::AgentEvent) and
//!   [`ActionResult`](crate::ActionResult) have nested objects, but each
//!   field name has one type in every variant ([`Tagged`]). A field is
//!   decoded with that type once the tag says the variant has it, ignored
//!   once the tag says it doesn't, and read leniently while the tag is not
//!   known yet ([`read_tagged`]).
//!
//! `Serialize` stays derived, so the wire format is unchanged (the golden
//! tests check both directions).

use core::fmt;

use serde::Deserializer;
use serde::de::{self, IgnoredAny, IntoDeserializer, MapAccess, SeqAccess, Unexpected, Visitor};

use crate::tolerant::Lenient;

/// One field value, read without knowing its expected type yet.
///
/// Only the shapes the tagged variants use are kept; anything else is
/// consumed and remembered as [`Loose::Other`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Loose {
    /// `null`.
    Null,
    /// A string.
    Str(String),
    /// A non-negative integer.
    Uint(u64),
    /// A list whose elements are all strings.
    Strings(Vec<String>),
    /// Anything else: a boolean, a negative or fractional number, an object,
    /// or a list holding a non-string.
    Other,
}

impl Loose {
    /// The value as a string.
    pub(crate) fn string<E: de::Error>(self) -> Result<String, E> {
        match self {
            Self::Str(s) => Ok(s),
            other => Err(other.invalid("a string")),
        }
    }

    /// The value as an optional string: `null` is `None`.
    pub(crate) fn opt_string<E: de::Error>(self) -> Result<Option<String>, E> {
        match self {
            Self::Null => Ok(None),
            other => other.string().map(Some),
        }
    }

    /// The value as a non-negative integer.
    pub(crate) fn uint<E: de::Error>(self) -> Result<u64, E> {
        match self {
            Self::Uint(n) => Ok(n),
            other => Err(other.invalid("a non-negative integer")),
        }
    }

    /// The value as a `u32`.
    pub(crate) fn u32<E: de::Error>(self) -> Result<u32, E> {
        let n = self.uint()?;
        u32::try_from(n).map_err(|_| E::invalid_value(Unexpected::Unsigned(n), &"a u32"))
    }

    /// The value as a list of strings.
    pub(crate) fn strings<E: de::Error>(self) -> Result<Vec<String>, E> {
        match self {
            Self::Strings(list) => Ok(list),
            other => Err(other.invalid("a list of strings")),
        }
    }

    /// Decodes a string value with `T`'s own `Deserialize` (e.g. a unit-only
    /// enum), so the accepted names stay defined in one place.
    pub(crate) fn parse_str<'de, T, E>(self) -> Result<T, E>
    where
        T: de::Deserialize<'de>,
        E: de::Error,
    {
        T::deserialize(self.string::<E>()?.into_deserializer())
    }

    fn invalid<E: de::Error>(&self, expected: &'static str) -> E {
        let unexpected = match self {
            Self::Null => Unexpected::Unit,
            Self::Str(s) => Unexpected::Str(s),
            Self::Uint(n) => Unexpected::Unsigned(*n),
            Self::Strings(_) => Unexpected::Seq,
            Self::Other => Unexpected::Other("a value of another type"),
        };
        E::invalid_type(unexpected, &expected)
    }
}

impl<'de> de::Deserialize<'de> for Loose {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(LooseVisitor)
    }
}

struct LooseVisitor;

impl<'de> Visitor<'de> for LooseVisitor {
    type Value = Loose;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any value")
    }

    fn visit_bool<E>(self, _: bool) -> Result<Loose, E> {
        Ok(Loose::Other)
    }

    fn visit_i64<E>(self, n: i64) -> Result<Loose, E> {
        Ok(u64::try_from(n).map_or(Loose::Other, Loose::Uint))
    }

    fn visit_u64<E>(self, n: u64) -> Result<Loose, E> {
        Ok(Loose::Uint(n))
    }

    fn visit_f64<E>(self, _: f64) -> Result<Loose, E> {
        Ok(Loose::Other)
    }

    fn visit_str<E>(self, s: &str) -> Result<Loose, E> {
        Ok(Loose::Str(s.to_owned()))
    }

    fn visit_string<E>(self, s: String) -> Result<Loose, E> {
        Ok(Loose::Str(s))
    }

    fn visit_bytes<E>(self, _: &[u8]) -> Result<Loose, E> {
        Ok(Loose::Other)
    }

    fn visit_none<E>(self) -> Result<Loose, E> {
        Ok(Loose::Null)
    }

    fn visit_unit<E>(self) -> Result<Loose, E> {
        Ok(Loose::Null)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Loose, D::Error> {
        deserializer.deserialize_any(self)
    }

    fn visit_newtype_struct<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Loose, D::Error> {
        deserializer.deserialize_any(self)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Loose, A::Error> {
        let mut strings = Some(Vec::new());
        while let Some(element) = seq.next_element::<Loose>()? {
            match (element, &mut strings) {
                (Loose::Str(s), Some(list)) => list.push(s),
                _ => strings = None,
            }
        }
        Ok(strings.map_or(Loose::Other, Loose::Strings))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Loose, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(Loose::Other)
    }
}

/// Stores a field's value, rejecting a second occurrence as serde's derive
/// does.
pub(crate) fn set_once<T, E: de::Error>(
    slot: &mut Option<T>,
    field: &'static str,
    value: T,
) -> Result<(), E> {
    if slot.is_some() {
        return Err(E::duplicate_field(field));
    }
    *slot = Some(value);
    Ok(())
}

/// A required field's value.
pub(crate) fn required<T, E: de::Error>(slot: Option<T>, field: &'static str) -> Result<T, E> {
    slot.ok_or_else(|| E::missing_field(field))
}

/// The fields of an internally tagged type whose field names each have one
/// type across its variants, collected in one pass by [`read_tagged`].
pub(crate) trait Tagged: Default {
    /// Which variant a tag names (with a fallback for unknown tags).
    type Kind: Copy;
    /// A field some variant has.
    type Field: Copy;
    /// The tag's key, e.g. `"type"`.
    const TAG: &'static str;

    /// The variant `tag` names.
    fn kind(tag: &str) -> Self::Kind;
    /// The field named `key`, if any variant has one.
    fn field(key: &str) -> Option<Self::Field>;
    /// Whether variant `kind` has `field`: the one field-to-variant table.
    fn has(kind: Self::Kind, field: Self::Field) -> bool;
    /// Reads `field`'s value from `map` into its slot with [`read_slot`].
    fn read<'de, A: MapAccess<'de>>(
        &mut self,
        field: Self::Field,
        map: &mut A,
        tag_known: bool,
    ) -> Result<(), A::Error>;
}

/// Reads a tagged object's fields in one pass and returns its variant with
/// them. A field the variant lacks is skipped once the tag is known;
/// before that it is read leniently, so a value that turns out to belong to
/// no field of the variant can't fail the object.
pub(crate) fn read_tagged<'de, F: Tagged, A: MapAccess<'de>>(
    mut map: A,
) -> Result<(F::Kind, F), A::Error> {
    let mut kind = None;
    let mut fields = F::default();
    while let Some(key) = map.next_key::<String>()? {
        if key == F::TAG {
            set_once(&mut kind, F::TAG, F::kind(&map.next_value::<String>()?))?;
            continue;
        }
        match F::field(&key) {
            Some(field) if kind.is_none_or(|kind| F::has(kind, field)) => {
                fields.read(field, &mut map, kind.is_some())?;
            }
            _ => {
                map.next_value::<IgnoredAny>()?;
            }
        }
    }
    Ok((required(kind, F::TAG)?, fields))
}

/// A field's value as [`read_tagged`] collected it.
#[derive(Debug)]
pub(crate) enum Slot<T> {
    /// Read once, and of the field's type.
    Value(T),
    /// Read before the tag and not of the field's type.
    Invalid,
    /// Read more than once before the tag.
    Repeated,
}

/// Stores the next value in `slot`: decoded as `T` once the tag is known
/// (the variant has the field), leniently before that. Problems found
/// before the tag (a value of another type, a repeated key) only count
/// if the variant turns out to have the field ([`take`]).
pub(crate) fn read_slot<'de, A, T>(
    map: &mut A,
    slot: &mut Option<Slot<T>>,
    field: &'static str,
    tag_known: bool,
) -> Result<(), A::Error>
where
    A: MapAccess<'de>,
    T: de::Deserialize<'de>,
{
    let value = if tag_known {
        Slot::Value(map.next_value()?)
    } else {
        match map.next_value()? {
            Lenient::Valid(value) => Slot::Value(value),
            Lenient::Invalid(_) => Slot::Invalid,
        }
    };
    put(slot, field, value, tag_known)
}

/// Like [`read_slot`] for a field that is lenient by design: a value of
/// another type (`null` included) reads as absent, wherever it comes.
pub(crate) fn read_lenient<'de, A, T>(
    map: &mut A,
    slot: &mut Option<Slot<Lenient<T>>>,
    field: &'static str,
    tag_known: bool,
) -> Result<(), A::Error>
where
    A: MapAccess<'de>,
    T: de::Deserialize<'de>,
{
    let value = Slot::Value(map.next_value()?);
    put(slot, field, value, tag_known)
}

fn put<T, E: de::Error>(
    slot: &mut Option<Slot<T>>,
    field: &'static str,
    value: Slot<T>,
    tag_known: bool,
) -> Result<(), E> {
    match slot {
        None => *slot = Some(value),
        // The variant has the field, so a second value is an error.
        Some(_) if tag_known => return Err(E::duplicate_field(field)),
        Some(_) => *slot = Some(Slot::Repeated),
    }
    Ok(())
}

/// A field of the variant: an error if missing, invalid or repeated.
pub(crate) fn take<T, E: de::Error>(slot: Option<Slot<T>>, field: &'static str) -> Result<T, E> {
    match slot {
        Some(Slot::Value(value)) => Ok(value),
        Some(Slot::Invalid) => Err(E::custom(format_args!("invalid value for `{field}`"))),
        Some(Slot::Repeated) => Err(E::duplicate_field(field)),
        None => Err(E::missing_field(field)),
    }
}

/// An optional field of the variant: its default when missing.
pub(crate) fn take_or_default<T: Default, E: de::Error>(
    slot: Option<Slot<T>>,
    field: &'static str,
) -> Result<T, E> {
    slot.map_or_else(|| Ok(T::default()), |slot| take(Some(slot), field))
}

/// A lenient field of the variant ([`read_lenient`]): `None` when missing
/// or unparseable.
pub(crate) fn take_lenient<T, E: de::Error>(
    slot: Option<Slot<Lenient<T>>>,
    field: &'static str,
) -> Result<Option<T>, E> {
    Ok(slot
        .map(|slot| take(Some(slot), field))
        .transpose()?
        .and_then(Lenient::into_option))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loose(json: &str) -> Loose {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn keeps_the_shapes_variants_use() {
        assert_eq!(loose("null"), Loose::Null);
        assert_eq!(loose(r#""hi""#), Loose::Str("hi".into()));
        assert_eq!(loose("7"), Loose::Uint(7));
        assert_eq!(
            loose(r#"["a","b"]"#),
            Loose::Strings(vec!["a".into(), "b".into()])
        );
        assert_eq!(loose("[]"), Loose::Strings(Vec::new()));
    }

    #[test]
    fn consumes_every_other_shape() {
        for json in [
            "true",
            "-1",
            "1.5",
            r#"{"a":[1,{"b":2}]}"#,
            r#"["a",1]"#,
            "[[]]",
        ] {
            assert_eq!(loose(json), Loose::Other, "{json}");
        }
    }

    #[test]
    fn conversions_check_the_type() {
        type E = serde_json::Error;
        assert_eq!(loose("4").u32::<E>().unwrap(), 4);
        assert!(loose("4294967296").u32::<E>().is_err());
        assert!(loose(r#""4""#).uint::<E>().is_err());
        assert_eq!(loose("null").opt_string::<E>().unwrap(), None);
        assert!(loose("3").opt_string::<E>().is_err());
        assert!(loose(r#"["a",1]"#).strings::<E>().is_err());
    }
}
