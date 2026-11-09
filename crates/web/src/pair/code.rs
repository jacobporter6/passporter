//! The one-time pairing code shown by `passporter pair` (design §8).
//!
//! 8 Crockford base32 symbols ([`protocol::PairingCode`]), displayed
//! `XXXX-XXXX`. `passporter pair` puts the bare code in its QR; a URL with a
//! `code=` query/fragment parameter is accepted too. Typed input is
//! forgiving (case, separators, `I`/`L`/`O` misreads).
//!
//! Scanned text is untrusted (any QR in view can be scanned): it is parsed
//! into a [`PairingCode`] or rejected, and never navigated to or rendered as
//! markup.

use core::fmt;

/// Number of symbols in a code.
pub const CODE_LEN: usize = 8;
/// Upper bound on scanned/typed/pasted input we even look at.
pub const MAX_INPUT_LEN: usize = 512;

/// A syntactically valid pairing code, normalized to upper-case Crockford
/// base32 without separators.
#[derive(Clone, PartialEq, Eq)]
pub struct PairingCode(protocol::PairingCode);

/// Input that is not a pairing code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CodeError {
    /// Nothing to parse.
    #[error("enter the code shown by `passporter pair`")]
    Empty,
    /// Wrong number of symbols.
    #[error("the code has {CODE_LEN} characters")]
    Length,
    /// A character outside Crockford base32.
    #[error("the code uses letters and digits only")]
    Character,
}

impl PairingCode {
    /// Parses what the user typed or the camera scanned.
    pub fn parse(input: &str) -> Result<Self, CodeError> {
        let input = input.trim();
        if input.is_empty() {
            return Err(CodeError::Empty);
        }
        if input.len() > MAX_INPUT_LEN {
            return Err(CodeError::Length);
        }
        Self::parse_bare(code_param(input).unwrap_or(input))
    }

    fn parse_bare(raw: &str) -> Result<Self, CodeError> {
        let mut code = String::with_capacity(CODE_LEN);
        for c in raw.chars() {
            match c {
                '-' | ' ' => {}
                _ => code.push(crockford(c).ok_or(CodeError::Character)?),
            }
        }
        match code.len() {
            0 => Err(CodeError::Empty),
            // Every symbol is in the alphabet, so only the length can fail.
            CODE_LEN => protocol::PairingCode::new(code)
                .map(Self)
                .map_err(|_| CodeError::Character),
            _ => Err(CodeError::Length),
        }
    }

    /// The normalized code, without separator.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// The code as the bridge API takes it.
    #[must_use]
    pub fn to_protocol(&self) -> protocol::PairingCode {
        self.0.clone()
    }
}

/// Displays as `XXXX-XXXX`.
impl fmt::Display for PairingCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Redacted: a live code is a credential for five minutes.
impl fmt::Debug for PairingCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairingCode(<redacted>)")
    }
}

/// The value of a `code=` parameter in the query or fragment of a URL-ish
/// input, if any.
fn code_param(input: &str) -> Option<&str> {
    let (_, params) = input.split_once(['?', '#'])?;
    params
        .split(['&', '#', '?'])
        .find_map(|pair| pair.strip_prefix("code="))
}

/// Normalizes one Crockford base32 symbol: case-insensitive, `I`/`L` read
/// as `1` and `O` as `0`; `U` and everything else are rejected.
fn crockford(c: char) -> Option<char> {
    let c = c.to_ascii_uppercase();
    match c {
        'O' => Some('0'),
        'I' | 'L' => Some('1'),
        'U' => None,
        '0'..='9' | 'A'..='Z' => Some(c),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_codes_normalize() {
        for input in ["ABCD-EF12", "abcd ef12", "  abcdef12 ", "ABCDEF12"] {
            assert_eq!(PairingCode::parse(input).unwrap().to_string(), "ABCD-EF12");
        }
        // Crockford aliases for misread characters.
        assert_eq!(
            PairingCode::parse("oil0-OIL0").unwrap().as_str(),
            "01100110"
        );
    }

    #[test]
    fn urls_carry_the_code_in_a_parameter() {
        for input in [
            "https://passporter.example.com/pair#code=ABCD-EF12",
            "https://passporter.example.com/pair?v=1&code=abcdef12",
            "passporter://pair?code=ABCD-EF12&exp=300",
        ] {
            assert_eq!(
                PairingCode::parse(input).unwrap().as_str(),
                "ABCDEF12",
                "{input}"
            );
        }
    }

    #[test]
    fn junk_is_rejected() {
        assert_eq!(PairingCode::parse(""), Err(CodeError::Empty));
        assert_eq!(PairingCode::parse(" - "), Err(CodeError::Empty));
        assert_eq!(PairingCode::parse("ABC"), Err(CodeError::Length));
        assert_eq!(PairingCode::parse("ABCDEF123"), Err(CodeError::Length));
        assert_eq!(PairingCode::parse("ABCD-EFGU"), Err(CodeError::Character));
        assert_eq!(PairingCode::parse("<script>"), Err(CodeError::Character));
        assert_eq!(
            PairingCode::parse("https://evil.example/"),
            Err(CodeError::Character)
        );
        assert_eq!(PairingCode::parse(&"A".repeat(600)), Err(CodeError::Length));
    }

    #[test]
    fn every_parsed_code_is_a_protocol_code() {
        for input in ["abcd-ef12", "oil0-OIL0", "ZZZZ-9999"] {
            let code = PairingCode::parse(input).unwrap();
            assert_eq!(code.to_protocol().as_str(), code.as_str());
        }
    }

    #[test]
    fn debug_is_redacted() {
        let code = PairingCode::parse("ABCDEF12").unwrap();
        assert!(!format!("{code:?}").contains("ABCD"));
    }
}
