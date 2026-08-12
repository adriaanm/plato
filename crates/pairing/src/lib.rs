//! Pairing a Mac with the reader: a typed 8-character code, a SPAKE2 handshake,
//! and one encrypted round trip that swaps ssh public keys.
//!
//! This crate is linked by **both** ends -- the `plato` binary runs it in a
//! pairing thread, the `platonic` CLI runs it on the Mac.  That is the point:
//! every wire parameter below exists exactly once, so the two ends cannot
//! disagree about one.  Design and rationale: platokin `docs/pairing-candidates.md`.
//!
//! No async, no runtime: `std::net` and `std::thread` only.
//!
//! # Pinned wire parameters
//!
//! Changing any line in this table is a wire break and requires bumping
//! [`PROTOCOL_VERSION`].  They are gathered here so there is one place to read.
//!
//! | Parameter | Value |
//! |---|---|
//! | PAKE | `spake2` 0.4, **warner's construction** (RustCrypto/PAKEs). *Not* RFC 9382 -- the two are wire-incompatible and must never be mixed. |
//! | Group | `Ed25519Group` |
//! | Sides | the **reader is always side A** (`start_a`), the **Mac is always side B** (`start_b`). Never symmetric, never swapped. |
//! | Identity A | `b"platokin-reader"` |
//! | Identity B | `b"platonic-mac"` |
//! | Password | the canonical code: 8 lowercase characters of [`code::ALPHABET`], no dash, no whitespace ([`code::Code::password_bytes`]) |
//! | Transcript | `SHA-256("platokin-pair-v1" ‖ version ‖ u16be(len msg_A) ‖ msg_A ‖ u16be(len msg_B) ‖ msg_B)`, where A/B are by **role**, not by send order |
//! | KDF | `HKDF-SHA256`, salt = the transcript hash, IKM = the SPAKE2 key, 32-byte output per label |
//! | Label `k_confirm_reader` | `b"platokin-pair-v1 confirm reader"` |
//! | Label `k_confirm_mac` | `b"platokin-pair-v1 confirm mac"` |
//! | Label `k_reader_to_mac` | `b"platokin-pair-v1 stream reader->mac"` |
//! | Label `k_mac_to_reader` | `b"platokin-pair-v1 stream mac->reader"` |
//! | Confirmation | `HMAC-SHA256(k_confirm_<side>, transcript_hash)`, 32 bytes, exchanged and verified in constant time **before any payload** |
//! | AEAD | `ChaCha20Poly1305`, per-direction key |
//! | Nonce | 12 bytes: `u32be(direction tag) ‖ u64be(counter)`; counter starts at 0 and never repeats; reader→mac = 1, mac→reader = 2 |
//! | Frame | `u32be(ciphertext length) ‖ ciphertext‖tag`; ciphertext length is refused above [`MAX_PLAINTEXT`] + 16 before any allocation |
//! | Handshake message | `u8(version) ‖ u16be(len) ‖ spake2 message` |
//!
//! The discovery constants are pinned in [`discovery`], and the payload field
//! encoding in [`exchange`].
//!
//! # Shape of a session
//!
//! ```no_run
//! use pairing::{Code, Config, Role, handshake, exchange::MacHello};
//! use std::net::TcpStream;
//!
//! # fn main() -> Result<(), pairing::Error> {
//! let code = Code::parse("abcd-2345")?;
//! let stream = TcpStream::connect("192.168.1.10:30305")?;
//! let mut session = handshake(stream, Role::Mac, &code, &Config::default())?;
//! session.send_mac_hello(&MacHello { ssh_public_key: "ssh-ed25519 AAAA... platonic@host".into() })?;
//! let reply = session.recv_reader_reply()?;
//! # let _ = reply;
//! # Ok(())
//! # }
//! ```

pub mod code;
pub mod discovery;
pub mod exchange;
pub mod handshake;

pub use code::Code;
pub use handshake::{handshake, Config, PairStream, Role, Session, Untimed, PROTOCOL_VERSION};

use std::fmt;
use std::io;

/// Largest plaintext a single frame may carry.
///
/// Pairing payloads are a few hundred bytes; the cap exists so a hostile or
/// confused peer cannot make the reader allocate.  A length above this is
/// rejected *before* any buffer is reserved.
pub const MAX_PLAINTEXT: usize = 8 * 1024;

/// Everything that can go wrong, kept distinguishable on purpose.
///
/// [`Error::BadCode`] is the one the user interface cares about: it means the
/// typed code was wrong (or someone is guessing), and it is raised at key
/// confirmation, before any payload exists to leak.  It must never be conflated
/// with [`Error::Io`] (peer vanished, timeout) or [`Error::Version`] (peer is a
/// different build).
#[derive(Debug)]
pub enum Error {
    /// Key confirmation failed: the two sides do not hold the same code.
    ///
    /// Both ends raise this.  One online guess has been burned; the caller
    /// should tear the listener down rather than allow a retry loop.
    BadCode,
    /// The peer speaks a different protocol version.
    Version { ours: u8, theirs: u8 },
    /// The typed code could not be read as a code at all.
    InvalidCode(InvalidCode),
    /// The peer sent something structurally wrong.
    Protocol(&'static str),
    /// A frame claimed a length above the cap; nothing was allocated.
    FrameTooLarge { claimed: usize, max: usize },
    /// AEAD open failed: the frame was tampered with, reordered or replayed.
    Decrypt,
    /// A field failed validation on receipt.  Security-critical for ssh keys:
    /// see [`exchange::validate_ssh_public_key`].
    InvalidField(String),
    /// The whole handshake ran out of time.
    Timeout,
    Io(io::Error),
}

/// Why a typed code was rejected.  Carries enough to tell the human what to fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvalidCode {
    /// A visually ambiguous character.  Deliberately **not** silently remapped:
    /// a silent remap turns a mistyped code into what looks like a protocol
    /// failure, and the user never learns which character was wrong.
    Ambiguous { found: char, resembles: char },
    /// A character outside the alphabet entirely.
    NotInAlphabet { found: char },
    /// Wrong number of characters after stripping dashes and whitespace.
    WrongLength { found: usize, expected: usize },
}

impl fmt::Display for InvalidCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InvalidCode::Ambiguous { found, resembles } => write!(
                f,
                "'{}' is not used in pairing codes because it looks like '{}' \
                 -- check the screen and type what is shown",
                found, resembles
            ),
            InvalidCode::NotInAlphabet { found } => write!(
                f,
                "'{}' is not a pairing-code character (letters a-z without \
                 l/o, digits 2-9)",
                found
            ),
            InvalidCode::WrongLength { found, expected } => write!(
                f,
                "a pairing code is {} characters (dashes and spaces ignored), got {}",
                expected, found
            ),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::BadCode => write!(
                f,
                "wrong pairing code: the two sides did not agree on a key"
            ),
            Error::Version { ours, theirs } => write!(
                f,
                "protocol version mismatch: we speak {}, the peer speaks {}",
                ours, theirs
            ),
            Error::InvalidCode(e) => write!(f, "{}", e),
            Error::Protocol(m) => write!(f, "protocol error: {}", m),
            Error::FrameTooLarge { claimed, max } => write!(
                f,
                "frame claims {} bytes, cap is {}",
                claimed, max
            ),
            Error::Decrypt => write!(f, "frame failed authentication"),
            Error::InvalidField(m) => write!(f, "invalid field: {}", m),
            Error::Timeout => write!(f, "pairing timed out"),
            Error::Io(e) => write!(f, "i/o error: {}", e),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        // A read timeout is a stalled peer, not a mystery: name it.
        match e.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => Error::Timeout,
            _ => Error::Io(e),
        }
    }
}

impl From<InvalidCode> for Error {
    fn from(e: InvalidCode) -> Error {
        Error::InvalidCode(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
