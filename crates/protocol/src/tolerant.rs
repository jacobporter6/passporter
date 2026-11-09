//! Helpers for tolerant outbound lists: one element a client cannot parse
//! (e.g. an agent with an id from a newer bridge) must not blank the list.

use serde::de::IgnoredAny;
use serde::{Deserialize, Deserializer};

/// One list element that either parsed or is skipped.
///
/// Use `Vec<Lenient<T>>` for a top-level list response such as
/// `GET /v1/agents`, then [`Lenient::into_option`] to drop invalid entries.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Lenient<T> {
    /// The element parsed.
    Valid(T),
    /// The element did not parse as `T`; its content is discarded.
    Invalid(IgnoredAny),
}

impl<T> Lenient<T> {
    /// The parsed element, if any.
    pub fn into_option(self) -> Option<T> {
        match self {
            Self::Valid(value) => Some(value),
            Self::Invalid(_) => None,
        }
    }
}

/// `deserialize_with` helper: a list that silently skips elements that fail
/// to parse. The list itself must still be a list.
pub fn skip_invalid<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    let items = Vec::<Lenient<T>>::deserialize(deserializer)?;
    Ok(items.into_iter().filter_map(Lenient::into_option).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::AgentId;

    #[derive(Debug, Deserialize)]
    struct Wrapper {
        #[serde(deserialize_with = "skip_invalid")]
        ids: Vec<AgentId>,
    }

    #[test]
    fn skips_only_the_bad_elements() {
        let parsed: Wrapper =
            serde_json::from_str(r#"{"ids":["w1:p1","bad/id",7,"w2:p1~s"]}"#).unwrap();
        let ids: Vec<String> = parsed.ids.iter().map(ToString::to_string).collect();
        assert_eq!(ids, ["w1:p1", "w2:p1~s"]);
    }

    #[test]
    fn the_list_itself_must_be_a_list() {
        assert!(serde_json::from_str::<Wrapper>(r#"{"ids":"w1:p1"}"#).is_err());
    }

    #[test]
    fn top_level_lenient_list() {
        let items: Vec<Lenient<AgentId>> = serde_json::from_str(r#"["w1:p1",".."]"#).unwrap();
        let ids: Vec<AgentId> = items.into_iter().filter_map(Lenient::into_option).collect();
        assert_eq!(ids.len(), 1);
    }
}
