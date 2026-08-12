//! What actually crosses the confirmed channel: two messages, four fields.
//!
//! The Mac sends the public key it just minted; the reader answers with its ssh
//! host key and a label.  Both strings end up in a *line-oriented* config file
//! -- `authorized_keys` on the device, `known_hosts` on the Mac -- so the
//! validation in [`validate_ssh_public_key`] is security-critical rather than
//! cosmetic: an embedded newline in the Mac's key would append a **second**,
//! attacker-chosen key to `authorized_keys`.
//!
//! # Pinned encoding
//!
//! Hand-rolled, in the spirit of `crates/foldersync`: no serde in the device
//! binary for 40 lines of work.
//!
//! ```text
//! message := u8(tag) field*
//! field   := u16be(len) bytes      // UTF-8, len <= FIELD_MAX
//! ```
//!
//! | tag | message | fields, in order |
//! |---|---|---|
//! | 1 | [`MacHello`] | `ssh_public_key` |
//! | 2 | [`ReaderReply`] | `host_public_key`, `device_label` |
//!
//! Trailing bytes are an error: a message is exactly its fields.

use std::io::{Read, Write};

use crate::{Error, Result, Session};

const TAG_MAC_HELLO: u8 = 1;
const TAG_READER_REPLY: u8 = 2;

/// Per-field ceiling.  An ssh-ed25519 line is ~100 bytes and an ssh-rsa line
/// under 800; 1 KiB is generous and still bounded.
pub const FIELD_MAX: usize = 1024;

/// Ceiling on the human-chosen device label.
pub const LABEL_MAX: usize = 64;

/// Mac → reader.  The `ssh_public_key` is a full `authorized_keys` line:
/// `ssh-ed25519 AAAA... platonic@hostname`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacHello {
    pub ssh_public_key: String,
}

/// Reader → Mac.  `host_public_key` is the `ssh-ed25519 AAAA...` portion the
/// Mac pastes into a `known_hosts` line keyed to the `platokin` alias.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReaderReply {
    pub host_public_key: String,
    pub device_label: String,
}

// ------------------------------------------------------------------ validate

/// Accept only something that is safely a single `authorized_keys` /
/// `known_hosts` entry.
///
/// The rules, and why each one is here:
///
/// * printable ASCII only -- this rejects `\n`, `\r` and `\0`, which is the
///   whole point: a newline would turn one key into two;
/// * no leading or trailing whitespace, so the caller can write the string out
///   verbatim without a normalisation step that might reintroduce a newline;
/// * two or three space-separated fields: type, base64 blob, optional comment;
/// * the type is `ssh-*` or `ecdsa-sha2-*`;
/// * the blob is non-trivial base64.
///
/// Deliberately *not* a full key parse: this is a shape check that makes the
/// string safe to write to a line-oriented file.  Whether the key is
/// cryptographically well-formed is sshd's business.
pub fn validate_ssh_public_key(s: &str) -> Result<()> {
    if s.is_empty() || s.len() > FIELD_MAX {
        return Err(Error::InvalidField(format!(
            "ssh public key must be 1..={} bytes, got {}",
            FIELD_MAX,
            s.len()
        )));
    }
    if let Some(bad) = s.chars().find(|c| !(' '..='~').contains(c)) {
        return Err(Error::InvalidField(format!(
            "ssh public key contains a non-printable character (U+{:04X}); \
             a newline here would add a second key",
            bad as u32
        )));
    }
    if s.trim() != s {
        return Err(Error::InvalidField(
            "ssh public key has leading or trailing whitespace".into(),
        ));
    }
    let parts: Vec<&str> = s.split(' ').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return Err(Error::InvalidField(format!(
            "ssh public key must be `<type> <base64>` with an optional comment, \
             got {} space-separated fields",
            parts.len()
        )));
    }
    let kind = parts[0];
    if !(kind.starts_with("ssh-") || kind.starts_with("ecdsa-sha2-")) {
        return Err(Error::InvalidField(format!(
            "unrecognised ssh key type {:?}",
            kind
        )));
    }
    let blob = parts[1];
    if blob.len() < 16
        || !blob
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=')
    {
        return Err(Error::InvalidField(
            "ssh public key blob is not plausible base64".into(),
        ));
    }
    Ok(())
}

/// A device label is shown to a human and nothing else, but it is still peer
/// input: printable ASCII, bounded, no newline.
pub fn validate_label(s: &str) -> Result<()> {
    if s.is_empty() || s.len() > LABEL_MAX {
        return Err(Error::InvalidField(format!(
            "label must be 1..={} bytes, got {}",
            LABEL_MAX,
            s.len()
        )));
    }
    if let Some(bad) = s.chars().find(|c| !(' '..='~').contains(c)) {
        return Err(Error::InvalidField(format!(
            "label contains a non-printable character (U+{:04X})",
            bad as u32
        )));
    }
    if s.trim() != s {
        return Err(Error::InvalidField(
            "label has leading or trailing whitespace".into(),
        ));
    }
    Ok(())
}

// -------------------------------------------------------------------- codec

fn put_field(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u16).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn take_field<'a>(buf: &mut &'a [u8], what: &str) -> Result<&'a str> {
    if buf.len() < 2 {
        return Err(Error::Protocol("truncated field header"));
    }
    let len = u16::from_be_bytes([buf[0], buf[1]]) as usize;
    if len > FIELD_MAX {
        return Err(Error::FrameTooLarge {
            claimed: len,
            max: FIELD_MAX,
        });
    }
    if buf.len() < 2 + len {
        return Err(Error::Protocol("truncated field body"));
    }
    let s = std::str::from_utf8(&buf[2..2 + len])
        .map_err(|_| Error::InvalidField(format!("{} is not UTF-8", what)))?;
    *buf = &buf[2 + len..];
    Ok(s)
}

fn expect_tag(buf: &mut &[u8], tag: u8) -> Result<()> {
    match buf.split_first() {
        Some((&t, rest)) if t == tag => {
            *buf = rest;
            Ok(())
        }
        Some(..) => Err(Error::Protocol("unexpected message tag")),
        None => Err(Error::Protocol("empty message")),
    }
}

fn expect_end(buf: &[u8]) -> Result<()> {
    if buf.is_empty() {
        Ok(())
    } else {
        Err(Error::Protocol("trailing bytes after message"))
    }
}

impl MacHello {
    /// Validates before encoding: a malformed key never reaches the wire.
    pub fn encode(&self) -> Result<Vec<u8>> {
        validate_ssh_public_key(&self.ssh_public_key)?;
        let mut out = vec![TAG_MAC_HELLO];
        put_field(&mut out, &self.ssh_public_key);
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<MacHello> {
        let mut b = bytes;
        expect_tag(&mut b, TAG_MAC_HELLO)?;
        let key = take_field(&mut b, "ssh public key")?.to_string();
        expect_end(b)?;
        validate_ssh_public_key(&key)?;
        Ok(MacHello {
            ssh_public_key: key,
        })
    }
}

impl ReaderReply {
    pub fn encode(&self) -> Result<Vec<u8>> {
        validate_ssh_public_key(&self.host_public_key)?;
        validate_label(&self.device_label)?;
        let mut out = vec![TAG_READER_REPLY];
        put_field(&mut out, &self.host_public_key);
        put_field(&mut out, &self.device_label);
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<ReaderReply> {
        let mut b = bytes;
        expect_tag(&mut b, TAG_READER_REPLY)?;
        let key = take_field(&mut b, "host public key")?.to_string();
        let label = take_field(&mut b, "device label")?.to_string();
        expect_end(b)?;
        validate_ssh_public_key(&key)?;
        validate_label(&label)?;
        Ok(ReaderReply {
            host_public_key: key,
            device_label: label,
        })
    }
}

// ------------------------------------------------------- session convenience

impl<S: Read + Write> Session<S> {
    pub fn send_mac_hello(&mut self, m: &MacHello) -> Result<()> {
        self.send(&m.encode()?)
    }

    pub fn recv_mac_hello(&mut self) -> Result<MacHello> {
        MacHello::decode(&self.recv()?)
    }

    pub fn send_reader_reply(&mut self, m: &ReaderReply) -> Result<()> {
        self.send(&m.encode()?)
    }

    pub fn recv_reader_reply(&mut self) -> Result<ReaderReply> {
        ReaderReply::decode(&self.recv()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKx7q0Zc9m2H5Jn1sRhQ0aBcDeFgHiJkLmNoPqRsTuVw platonic@mac";

    #[test]
    fn round_trip() {
        let h = MacHello {
            ssh_public_key: GOOD.into(),
        };
        assert_eq!(MacHello::decode(&h.encode().unwrap()).unwrap(), h);

        let r = ReaderReply {
            host_public_key: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKx7q0Zc9m2H5Jn1sRhQ0aBcDeFgHiJkLmNoPqRsTuVw".into(),
            device_label: "Adriaan's Kindle".into(),
        };
        assert_eq!(ReaderReply::decode(&r.encode().unwrap()).unwrap(), r);
    }

    #[test]
    fn newline_injection_is_rejected() {
        // The attack: a second authorized_keys line smuggled into the field.
        let attacks = [
            format!("{}\nssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEVIL0000000000000000000000000000000000 evil@attacker", GOOD),
            format!("{}\r\nssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEVIL0000000000000000000000000000000000 evil", GOOD),
            format!("{}\r", GOOD),
            format!("{}\0", GOOD),
            format!("\n{}", GOOD),
        ];
        for a in &attacks {
            assert!(
                validate_ssh_public_key(a).is_err(),
                "accepted {:?}",
                a
            );
            // and it cannot be smuggled past the decoder either
            let mut raw = vec![TAG_MAC_HELLO];
            put_field(&mut raw, a);
            assert!(MacHello::decode(&raw).is_err(), "decoded {:?}", a);
        }
    }

    #[test]
    fn shape_rules() {
        assert!(validate_ssh_public_key("ssh-ed25519").is_err(), "one field");
        assert!(validate_ssh_public_key("").is_err(), "empty");
        assert!(
            validate_ssh_public_key("rm -rf / AAAAC3NzaC1lZDI1NTE5AAAAIKx7q0Zc9m2H").is_err(),
            "bad type"
        );
        assert!(
            validate_ssh_public_key("ssh-ed25519 AAAA$$$$AAAAAAAAAAAA").is_err(),
            "non-base64 blob"
        );
        assert!(validate_ssh_public_key("ssh-ed25519 AAAA").is_err(), "tiny blob");
        assert!(
            validate_ssh_public_key(&format!("{} a b c", GOOD)).is_err(),
            "too many fields"
        );
        assert!(
            validate_ssh_public_key(&format!("ssh-rsa {}", "A".repeat(FIELD_MAX))).is_err(),
            "over the length cap"
        );
        // an ecdsa host key is a legitimate answer
        assert!(validate_ssh_public_key(
            "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTY="
        )
        .is_ok());
    }

    #[test]
    fn label_rules() {
        assert!(validate_label("kindle").is_ok());
        assert!(validate_label("").is_err());
        assert!(validate_label("two\nlines").is_err());
        assert!(validate_label(&"x".repeat(LABEL_MAX + 1)).is_err());
        assert!(validate_label(" padded ").is_err());
    }

    #[test]
    fn codec_rejects_junk() {
        assert!(matches!(
            MacHello::decode(&[]),
            Err(Error::Protocol("empty message"))
        ));
        assert!(matches!(
            MacHello::decode(&[TAG_READER_REPLY, 0, 0]),
            Err(Error::Protocol("unexpected message tag"))
        ));
        // declared length past the end of the buffer
        assert!(matches!(
            MacHello::decode(&[TAG_MAC_HELLO, 0x00, 0x20, b'a']),
            Err(Error::Protocol("truncated field body"))
        ));
        // declared length over the field cap
        assert!(matches!(
            MacHello::decode(&[TAG_MAC_HELLO, 0xff, 0xff]),
            Err(Error::FrameTooLarge { .. })
        ));
        // trailing garbage
        let mut raw = vec![TAG_MAC_HELLO];
        put_field(&mut raw, GOOD);
        raw.push(0);
        assert!(matches!(
            MacHello::decode(&raw),
            Err(Error::Protocol("trailing bytes after message"))
        ));
    }
}
