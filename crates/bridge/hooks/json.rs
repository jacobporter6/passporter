//! A JSON document that keeps object members in file order and every key
//! and scalar exactly as written, so rewriting a settings file changes
//! only what the installer added (and the layout, which is two-space
//! indentation). `serde_json`'s `Value` would sort keys unless its
//! workspace-wide `preserve_order` feature is on, turn `1e3` into `1000.0`,
//! lose big integers without `arbitrary_precision` (also workspace-wide,
//! and it changes every `Number` in the bridge), and re-spell escapes such
//! as `é` in keys and strings.
//!
//! Objects and arrays are parsed member by member; keys, numbers, strings,
//! booleans and `null` stay [`RawValue`]s, printed back byte for byte.

use std::fmt;

use serde::Deserialize;
use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde_json::value::RawValue;

/// Deepest nesting accepted (the parse recurses once per level).
const MAX_DEPTH: usize = 64;

/// Indentation per nesting level when printing.
const INDENT: &str = "  ";

/// A JSON value with ordered object members and verbatim keys and scalars.
#[derive(Debug, Clone)]
pub(crate) enum Json {
    /// A number, string, boolean or `null`, as written.
    Scalar(Box<RawValue>),
    Array(Vec<Json>),
    Object(Vec<(Key, Json)>),
}

/// An object key: the string it names, and how the file spells it.
#[derive(Debug, Clone)]
pub(crate) struct Key {
    name: String,
    raw: Box<RawValue>,
}

impl Key {
    fn parse(raw: Box<RawValue>) -> serde_json::Result<Self> {
        Ok(Self {
            name: serde_json::from_str(raw.get())?,
            raw,
        })
    }

    fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            raw: string_raw(name),
        }
    }

    /// Whether this key names `name`, however it is spelled.
    pub(crate) fn is(&self, name: &str) -> bool {
        self.name == name
    }
}

/// Spelled the same: an unchanged document compares equal to its parse.
impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.raw.get() == other.raw.get()
    }
}

impl PartialEq for Json {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Scalar(a), Self::Scalar(b)) => a.get() == b.get(),
            (Self::Array(a), Self::Array(b)) => a == b,
            (Self::Object(a), Self::Object(b)) => a == b,
            _ => false,
        }
    }
}

impl Json {
    /// Parses a document.
    pub(crate) fn parse(text: &str) -> serde_json::Result<Self> {
        let raw: Box<RawValue> = serde_json::from_str(text)?;
        Self::from_raw(raw, 0)
    }

    fn from_raw(raw: Box<RawValue>, depth: usize) -> serde_json::Result<Self> {
        if depth > MAX_DEPTH {
            return Err(de::Error::custom("nested too deeply"));
        }
        match raw.get().trim_start().as_bytes().first() {
            Some(b'{') => {
                let Members(members) = serde_json::from_str(raw.get())?;
                members
                    .into_iter()
                    .map(|(key, value)| Ok((Key::parse(key)?, Self::from_raw(value, depth + 1)?)))
                    .collect::<serde_json::Result<_>>()
                    .map(Self::Object)
            }
            Some(b'[') => {
                let items: Vec<Box<RawValue>> = serde_json::from_str(raw.get())?;
                items
                    .into_iter()
                    .map(|item| Self::from_raw(item, depth + 1))
                    .collect::<serde_json::Result<_>>()
                    .map(Self::Array)
            }
            _ => Ok(Self::Scalar(raw)),
        }
    }

    /// A string value.
    pub(crate) fn string(value: &str) -> Self {
        Self::Scalar(string_raw(value))
    }

    /// A number value.
    pub(crate) fn number(value: u64) -> Self {
        Self::Scalar(raw(value.to_string()))
    }

    /// An object from `(key, value)` pairs, in order.
    pub(crate) fn object<const N: usize>(members: [(&str, Self); N]) -> Self {
        Self::Object(
            members
                .into_iter()
                .map(|(key, value)| (Key::new(key), value))
                .collect(),
        )
    }

    /// The first member named `key`, if this is an object that has one.
    pub(crate) fn member_mut(&mut self, key: &str) -> Option<&mut Self> {
        let Self::Object(members) = self else {
            return None;
        };
        members
            .iter_mut()
            .find(|(k, _)| k.is(key))
            .map(|(_, value)| value)
    }

    /// The first member named `key` of this object, inserted at the end as
    /// `default()` if missing. `None` if this is not an object.
    pub(crate) fn entry(&mut self, key: &str, default: impl FnOnce() -> Self) -> Option<&mut Self> {
        let Self::Object(members) = self else {
            return None;
        };
        let index = members
            .iter()
            .position(|(k, _)| k.is(key))
            .unwrap_or_else(|| {
                members.push((Key::new(key), default()));
                members.len() - 1
            });
        Some(&mut members[index].1)
    }

    /// The string, if this is one.
    pub(crate) fn as_str(&self) -> Option<String> {
        match self {
            Self::Scalar(raw) if raw.get().starts_with('"') => serde_json::from_str(raw.get()).ok(),
            _ => None,
        }
    }

    /// The document with two-space indentation (the layout Claude Code
    /// writes), keys and scalars as written.
    pub(crate) fn pretty(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, 0);
        out
    }

    fn write(&self, out: &mut String, depth: usize) {
        match self {
            Self::Scalar(raw) => out.push_str(raw.get()),
            Self::Array(items) => {
                write_members(out, depth, ('[', ']'), items, |out, item| {
                    item.write(out, depth + 1);
                });
            }
            Self::Object(members) => {
                write_members(out, depth, ('{', '}'), members, |out, (key, value)| {
                    out.push_str(key.raw.get());
                    out.push_str(": ");
                    value.write(out, depth + 1);
                });
            }
        }
    }
}

/// `items` between `open` and `close`, one per line at `depth + 1`, or
/// `[]`/`{}` when there are none.
fn write_members<T>(
    out: &mut String,
    depth: usize,
    (open, close): (char, char),
    items: &[T],
    mut write: impl FnMut(&mut String, &T),
) {
    out.push(open);
    for (i, item) in items.iter().enumerate() {
        out.push_str(if i == 0 { "\n" } else { ",\n" });
        out.push_str(&INDENT.repeat(depth + 1));
        write(out, item);
    }
    if !items.is_empty() {
        out.push('\n');
        out.push_str(&INDENT.repeat(depth));
    }
    out.push(close);
}

/// `value` as a JSON string literal.
fn string_raw(value: &str) -> Box<RawValue> {
    raw(serde_json::Value::from(value).to_string())
}

/// `json`, which the caller built as valid JSON.
fn raw(json: String) -> Box<RawValue> {
    RawValue::from_string(json).unwrap_or_else(|_| unreachable!("built as valid JSON"))
}

/// An object's members in file order, keys and values unparsed.
struct Members(Vec<(Box<RawValue>, Box<RawValue>)>);

impl<'de> Deserialize<'de> for Members {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct MembersVisitor;

        impl<'de> Visitor<'de> for MembersVisitor {
            type Value = Members;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Members, A::Error> {
                let mut members = Vec::new();
                while let Some(member) = map.next_entry()? {
                    members.push(member);
                }
                Ok(Members(members))
            }
        }

        deserializer.deserialize_map(MembersVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_keeping_order_keys_and_scalars_verbatim() {
        let text = "{\n  \"zeta\": 1e3,\n  \"alpha\": [\n    true,\n    null,\n    1.50,\n    -2,\n    123456789012345678901234567890,\n    -0.0,\n    1E-7\n  ],\n  \"caf\\u00e9\": {\n    \"b\": \"caf\\u00e9 \\/ \\ud83d\\ude00\",\n    \"a\": {},\n    \"c\": [],\n    \"a\": 2\n  }\n}";
        let value = Json::parse(text).expect("parses");
        assert_eq!(value.pretty(), text);
    }

    #[test]
    fn prints_like_serde_json_pretty() {
        let text = r#"{"a":[1,{"b":[]},{}],"c":{"d":"e"},"f":[]}"#;
        let ours = Json::parse(text).expect("parses").pretty();
        let theirs = serde_json::to_string_pretty(
            &serde_json::from_str::<serde_json::Value>(text).expect("valid"),
        )
        .expect("prints");
        assert_eq!(ours, theirs);
    }

    #[test]
    fn strings_read_back_and_build() {
        let mut value = Json::parse(r#"{"s": "a\"b", "n": 5, "sk": true}"#).expect("parses");
        let Json::Object(members) = &value else {
            panic!("object")
        };
        assert_eq!(members[0].1.as_str().as_deref(), Some("a\"b"));
        assert_eq!(members[1].1.as_str(), None);
        assert_eq!(Json::string("a\"b"), members[0].1);
        assert_eq!(Json::number(5), members[1].1);
        assert!(members[2].0.is("sk"), "keys are matched decoded");
        assert!(value.member_mut("sk").is_some());
        assert!(value.member_mut("missing").is_none());
    }

    #[test]
    fn entry_appends_missing_members() {
        let mut value = Json::object([("b", Json::number(1))]);
        *value.entry("a", || Json::number(0)).expect("object") = Json::string("x");
        assert_eq!(value.pretty(), "{\n  \"b\": 1,\n  \"a\": \"x\"\n}");
        assert!(Json::number(1).entry("a", || Json::number(0)).is_none());
    }

    #[test]
    fn deep_nesting_is_refused() {
        let deep = format!("{}{}", "[".repeat(100), "]".repeat(100));
        assert!(Json::parse(&deep).is_err());
        assert!(Json::parse("[1, 2").is_err());
    }
}
