//! The key allowlist for the `keys` verb (design §5, §8).
//!
//! Raw keys answer TUI dialogs; nothing outside this closed set can be sent.
//! Sending any keys requires step-up ([`crate::policy`]).
//!
//! A [`KeySequence`] is fully deserialized before its length check runs; the
//! bridge's HTTP body limit bounds that allocation.

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::error::ValidationError;

/// Maximum number of keys in one [`KeySequence`].
pub const MAX_KEYS: usize = 16;

/// One key a client may send to an agent's terminal.
///
/// Wire names: `"0"`..`"9"`, `"up"`, `"down"`, `"left"`, `"right"`, `"enter"`,
/// `"esc"`, `"tab"`, `"ctrl+c"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "&'static str")]
pub enum AllowedKey {
    /// A digit key, `0`..=`9` (menu choices).
    Digit(Digit),
    /// Arrow up.
    Up,
    /// Arrow down.
    Down,
    /// Arrow left.
    Left,
    /// Arrow right.
    Right,
    /// Enter / return.
    Enter,
    /// Escape.
    Esc,
    /// Tab.
    Tab,
    /// Ctrl+C.
    CtrlC,
}

/// A single decimal digit, `0..=9`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digit(u8);

impl Digit {
    /// Returns the digit if `value <= 9`.
    #[must_use]
    pub const fn new(value: u8) -> Option<Self> {
        if value <= 9 { Some(Self(value)) } else { None }
    }

    /// The digit's value, `0..=9`.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

const DIGIT_NAMES: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];

impl AllowedKey {
    /// Every allowed key, in wire-name order.
    pub const ALL: [Self; 18] = [
        Self::Digit(Digit(0)),
        Self::Digit(Digit(1)),
        Self::Digit(Digit(2)),
        Self::Digit(Digit(3)),
        Self::Digit(Digit(4)),
        Self::Digit(Digit(5)),
        Self::Digit(Digit(6)),
        Self::Digit(Digit(7)),
        Self::Digit(Digit(8)),
        Self::Digit(Digit(9)),
        Self::Up,
        Self::Down,
        Self::Left,
        Self::Right,
        Self::Enter,
        Self::Esc,
        Self::Tab,
        Self::CtrlC,
    ];

    /// The key's wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Digit(d) => DIGIT_NAMES[d.0 as usize],
            Self::Up => "up",
            Self::Down => "down",
            Self::Left => "left",
            Self::Right => "right",
            Self::Enter => "enter",
            Self::Esc => "esc",
            Self::Tab => "tab",
            Self::CtrlC => "ctrl+c",
        }
    }
}

impl core::str::FromStr for AllowedKey {
    type Err = ValidationError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|key| key.as_str() == name)
            .ok_or_else(|| ValidationError::KeyNotAllowed(name.to_owned()))
    }
}

impl TryFrom<String> for AllowedKey {
    type Error = ValidationError;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        name.parse()
    }
}

impl From<AllowedKey> for &'static str {
    fn from(key: AllowedKey) -> Self {
        key.as_str()
    }
}

impl fmt::Display for AllowedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A non-empty sequence of at most [`MAX_KEYS`] allowed keys, sent in order.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "Vec<AllowedKey>", into = "Vec<AllowedKey>")]
pub struct KeySequence(Vec<AllowedKey>);

impl KeySequence {
    /// Validates the length of `keys` (each key is valid by construction).
    pub fn new(keys: Vec<AllowedKey>) -> Result<Self, ValidationError> {
        if keys.is_empty() {
            return Err(ValidationError::Empty { field: "keys" });
        }
        if keys.len() > MAX_KEYS {
            return Err(ValidationError::TooLong {
                field: "keys",
                max: MAX_KEYS,
                actual: keys.len(),
            });
        }
        Ok(Self(keys))
    }

    /// The keys, in send order.
    #[must_use]
    pub fn as_slice(&self) -> &[AllowedKey] {
        &self.0
    }
}

impl TryFrom<Vec<AllowedKey>> for KeySequence {
    type Error = ValidationError;

    fn try_from(keys: Vec<AllowedKey>) -> Result<Self, Self::Error> {
        Self::new(keys)
    }
}

impl From<KeySequence> for Vec<AllowedKey> {
    fn from(keys: KeySequence) -> Self {
        keys.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_round_trips_by_name() {
        let names: Vec<&str> = AllowedKey::ALL.iter().map(|k| k.as_str()).collect();
        assert_eq!(
            names,
            [
                "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "up", "down", "left", "right",
                "enter", "esc", "tab", "ctrl+c"
            ]
        );
        for key in AllowedKey::ALL {
            assert_eq!(key.as_str().parse::<AllowedKey>(), Ok(key));
            let json = serde_json::to_string(&key).unwrap();
            assert_eq!(json, format!("\"{key}\""));
            assert_eq!(serde_json::from_str::<AllowedKey>(&json).unwrap(), key);
        }
    }

    #[test]
    fn rejects_keys_off_the_allowlist() {
        for bad in [
            "10", "a", "ENTER", "Enter", "ctrl+d", "ctrl+C", "f1", "space", "", " 1",
        ] {
            assert_eq!(
                bad.parse::<AllowedKey>(),
                Err(ValidationError::KeyNotAllowed(bad.to_owned())),
                "{bad:?}"
            );
        }
        assert!(serde_json::from_str::<AllowedKey>("\"ctrl+z\"").is_err());
        assert!(serde_json::from_str::<AllowedKey>("1").is_err());
    }

    #[test]
    fn digit_is_bounded() {
        assert_eq!(Digit::new(9).map(Digit::get), Some(9));
        assert_eq!(Digit::new(10), None);
    }

    #[test]
    fn sequence_length_is_bounded() {
        assert_eq!(
            KeySequence::new(vec![]),
            Err(ValidationError::Empty { field: "keys" })
        );
        assert!(KeySequence::new(vec![AllowedKey::Enter; MAX_KEYS]).is_ok());
        assert_eq!(
            KeySequence::new(vec![AllowedKey::Enter; MAX_KEYS + 1]),
            Err(ValidationError::TooLong {
                field: "keys",
                max: MAX_KEYS,
                actual: MAX_KEYS + 1
            })
        );
        assert!(serde_json::from_str::<KeySequence>("[]").is_err());
        assert!(serde_json::from_str::<KeySequence>("[\"1\",\"rm\"]").is_err());
        let seq: KeySequence = serde_json::from_str("[\"down\",\"enter\"]").unwrap();
        assert_eq!(seq.as_slice(), [AllowedKey::Down, AllowedKey::Enter]);
    }
}
