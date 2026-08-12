//! The 8-character pairing code: generation, e-ink display, and parsing what a
//! human typed.
//!
//! ~40 bits of entropy.  That is not a lot, and it does not have to be: SPAKE2
//! makes the code useless to a passive eavesdropper and gives an active attacker
//! exactly **one** online guess per pairing window.  The security story is the
//! window and the PAKE, not the length.

use std::fmt;

use crate::InvalidCode;

/// Lowercase base32 minus the four glyphs that get misread on e-ink: no `l`
/// (vs `1`), no `1`, no `0`, no `o`.  **Pinned wire parameter** -- the password
/// fed to SPAKE2 is a string over this alphabet.
pub const ALPHABET: &[u8; 32] = b"abcdefghijkmnpqrstuvwxyz23456789";

/// The four glyphs left out of the alphabet, and what each is confused with.
/// Typing one is rejected with a message naming it, never remapped: see
/// [`InvalidCode::Ambiguous`].  Every entry here is absent from [`ALPHABET`].
const CONFUSABLE: &[(char, char)] = &[('l', 'i'), ('1', 'i'), ('0', 'o'), ('o', '0')];

pub const CODE_LEN: usize = 8;

/// A pairing code in canonical form: [`CODE_LEN`] lowercase alphabet characters,
/// no dash, no whitespace.
///
/// [`fmt::Display`] renders the grouped `xxxx-xxxx` form for the screen.  The
/// bytes handed to SPAKE2 are always [`Code::password_bytes`], i.e. the
/// canonical form -- never the grouped one.
#[derive(Clone, PartialEq, Eq)]
pub struct Code([u8; CODE_LEN]);

impl Code {
    /// Fresh code from the OS CSPRNG.
    ///
    /// 32 divides 256, so masking a uniform byte with `& 31` is itself uniform:
    /// no rejection sampling and therefore no modulo bias.
    pub fn generate() -> Result<Code, getrandom::Error> {
        let mut raw = [0u8; CODE_LEN];
        getrandom::getrandom(&mut raw)?;
        let mut out = [0u8; CODE_LEN];
        for (o, r) in out.iter_mut().zip(raw.iter()) {
            *o = ALPHABET[(*r & 31) as usize];
        }
        Ok(Code(out))
    }

    /// Read a code the user typed.
    ///
    /// Case-insensitive; dashes and all whitespace are stripped.  An ambiguous
    /// character is a hard error naming it, because silently remapping `l`→`i`
    /// makes a genuinely mistyped code surface later as `BadCode`, which reads
    /// like the protocol is broken.
    pub fn parse(s: &str) -> Result<Code, InvalidCode> {
        let mut out = [0u8; CODE_LEN];
        let mut n = 0;
        for ch in s.chars() {
            if ch == '-' || ch.is_whitespace() {
                continue;
            }
            let lower = ch.to_ascii_lowercase();
            if !lower.is_ascii() || !ALPHABET.contains(&(lower as u8)) {
                if let Some(&(_, resembles)) = CONFUSABLE.iter().find(|(c, _)| *c == lower) {
                    return Err(InvalidCode::Ambiguous {
                        found: ch,
                        resembles,
                    });
                }
                return Err(InvalidCode::NotInAlphabet { found: ch });
            }
            if n < CODE_LEN {
                out[n] = lower as u8;
            }
            n += 1;
        }
        if n != CODE_LEN {
            return Err(InvalidCode::WrongLength {
                found: n,
                expected: CODE_LEN,
            });
        }
        Ok(Code(out))
    }

    /// The canonical 8 characters, no dash.  This is what is displayed grouped,
    /// stored, and -- as bytes -- fed to SPAKE2.
    pub fn canonical(&self) -> &str {
        // Every byte came from ALPHABET, which is ASCII.
        std::str::from_utf8(&self.0).expect("code is ASCII by construction")
    }

    /// The password bytes for SPAKE2.  **Pinned wire parameter**: the canonical
    /// lowercase 8 characters, no dash, no whitespace, no trailing NUL.
    pub fn password_bytes(&self) -> &[u8] {
        &self.0
    }

    /// `xxxx-xxxx`, for the e-ink.  Never fed to SPAKE2.
    pub fn grouped(&self) -> String {
        let s = self.canonical();
        format!("{}-{}", &s[..4], &s[4..])
    }
}

/// Grouped form -- this is the one a human reads.
impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.grouped())
    }
}

/// Deliberately does not print the code: a pairing code in a log is a pairing
/// code an attacker can read.
impl fmt::Debug for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Code(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alphabet_is_sane() {
        assert_eq!(ALPHABET.len(), 32);
        for bad in *b"l10o" {
            assert!(!ALPHABET.contains(&bad), "{} must not be in the alphabet", bad as char);
        }
        let mut seen = ALPHABET.to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 32, "alphabet has duplicates");
    }

    #[test]
    fn generate_round_trips() {
        for _ in 0..64 {
            let c = Code::generate().unwrap();
            assert_eq!(c.canonical().len(), CODE_LEN);
            assert_eq!(c.grouped().len(), CODE_LEN + 1);
            assert_eq!(Code::parse(&c.grouped()).unwrap(), c);
            assert_eq!(Code::parse(c.canonical()).unwrap(), c);
        }
    }

    #[test]
    fn parse_is_forgiving_about_shape() {
        let want = Code::parse("abcd2345").unwrap();
        for s in ["abcd-2345", "ABCD-2345", " abcd 2345 ", "AbCd-23 45", "a-b-c-d-2-3-4-5"] {
            assert_eq!(Code::parse(s).unwrap(), want, "failed on {:?}", s);
        }
    }

    #[test]
    fn password_bytes_are_the_canonical_form() {
        let c = Code::parse("ABCD-2345").unwrap();
        assert_eq!(c.password_bytes(), b"abcd2345");
        assert!(!c.password_bytes().contains(&b'-'));
    }

    #[test]
    fn ambiguous_characters_are_rejected_by_name() {
        for (input, ch) in [
            ("abcdl345", 'l'),
            ("abcd1345", '1'),
            ("abcd0345", '0'),
            ("abcdo345", 'o'),
        ] {
            match Code::parse(input) {
                Err(InvalidCode::Ambiguous { found, .. }) => assert_eq!(found, ch),
                other => panic!("{:?} should be Ambiguous({}), got {:?}", input, ch, other.is_ok()),
            }
            // and the message actually names the character
            let msg = Code::parse(input).unwrap_err().to_string();
            assert!(msg.contains(ch), "message {:?} does not name {:?}", msg, ch);
        }
    }

    #[test]
    fn other_junk_is_rejected_too() {
        assert!(matches!(
            Code::parse("abcd234!"),
            Err(InvalidCode::NotInAlphabet { found: '!' })
        ));
        assert!(matches!(
            Code::parse("abcd234é"),
            Err(InvalidCode::NotInAlphabet { .. })
        ));
        assert!(matches!(
            Code::parse("abcd234"),
            Err(InvalidCode::WrongLength { found: 7, expected: 8 })
        ));
        assert!(matches!(
            Code::parse("abcd23456"),
            Err(InvalidCode::WrongLength { found: 9, expected: 8 })
        ));
        assert!(matches!(
            Code::parse(""),
            Err(InvalidCode::WrongLength { found: 0, .. })
        ));
    }

    #[test]
    fn debug_does_not_leak_the_code() {
        let c = Code::parse("abcd2345").unwrap();
        assert!(!format!("{:?}", c).contains("abcd"));
    }
}
