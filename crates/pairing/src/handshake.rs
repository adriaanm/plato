//! SPAKE2 handshake, explicit key confirmation, and the encrypted frame layer.
//!
//! The order is fixed and is the whole security argument:
//!
//! 1. version byte + SPAKE2 message, both directions;
//! 2. HKDF into four keys;
//! 3. **key confirmation, verified in constant time, before anything else**;
//! 4. only then, AEAD frames.
//!
//! A wrong code cannot get past step 3, on either side, so no payload ever
//! exists to leak.  One pairing window is therefore one online guess.
//!
//! Every wire parameter used here is listed in the crate-level table.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use subtle::ConstantTimeEq;

use crate::{Code, Error, Result, MAX_PLAINTEXT};

type HmacSha256 = Hmac<Sha256>;

// ---------------------------------------------------------------- pinned wire

/// Bumped on any incompatible change to the handshake, the frames or the
/// payload encoding.  Sent as the first byte, so a mismatched peer gets a clear
/// error instead of a hang or a garbage parse.
pub const PROTOCOL_VERSION: u8 = 1;

/// SPAKE2 identity of side A.  Pinned; never varies with hostname or serial.
pub const IDENTITY_READER: &[u8] = b"platokin-reader";
/// SPAKE2 identity of side B.  Pinned.
pub const IDENTITY_MAC: &[u8] = b"platonic-mac";

/// Domain separator mixed into the transcript hash.
const TRANSCRIPT_LABEL: &[u8] = b"platokin-pair-v1";

// The four HKDF info labels, in one table so the two ends cannot drift.  These
// are wire parameters: changing a byte changes the derived keys and breaks
// interop with every other build.
const INFO_CONFIRM_READER: &[u8] = b"platokin-pair-v1 confirm reader";
const INFO_CONFIRM_MAC: &[u8] = b"platokin-pair-v1 confirm mac";
const INFO_READER_TO_MAC: &[u8] = b"platokin-pair-v1 stream reader->mac";
const INFO_MAC_TO_READER: &[u8] = b"platokin-pair-v1 stream mac->reader";

/// Nonce direction tags.  Wire parameters.  The keys are already
/// direction-separated; the tag is a second, cheap guarantee that a frame
/// reflected back at its sender cannot verify.
const DIR_READER_TO_MAC: u32 = 1;
const DIR_MAC_TO_READER: u32 = 2;

/// Largest SPAKE2 message we will read.  Ed25519Group sends 33 bytes; the cap
/// is generous but finite.
const MAX_PAKE_MSG: usize = 256;

/// Ciphertext cap, derived: plaintext cap plus the Poly1305 tag.
const MAX_CIPHERTEXT: usize = MAX_PLAINTEXT + 16;

// ---------------------------------------------------------------------- roles

/// Which end of the wire this is.  Fixed by device, never negotiated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The Kindle.  Always SPAKE2 side A, always identity [`IDENTITY_READER`].
    Reader,
    /// The Mac.  Always SPAKE2 side B, always identity [`IDENTITY_MAC`].
    Mac,
}

impl Role {
    fn peer(self) -> Role {
        match self {
            Role::Reader => Role::Mac,
            Role::Mac => Role::Reader,
        }
    }
}

// ------------------------------------------------------------------- streams

/// A stream the handshake can bound in time.
///
/// The handshake is generic so tests can drive it over memory, but a real
/// socket **must** be able to time out: without it a stalled or hostile peer
/// pins the reader's pairing thread forever.  Hence the trait rather than a
/// bare `Read + Write` bound.
pub trait PairStream: Read + Write {
    fn apply_timeouts(&mut self, read: Duration, write: Duration) -> std::io::Result<()>;
}

impl PairStream for TcpStream {
    fn apply_timeouts(&mut self, read: Duration, write: Duration) -> std::io::Result<()> {
        self.set_read_timeout(Some(read))?;
        self.set_write_timeout(Some(write))
    }
}

/// Adapter for streams that carry no timeout of their own -- in-memory duplexes
/// in tests, mostly.  The overall deadline in [`Config::total_timeout`] still
/// applies between steps, but a read that blocks forever will still block
/// forever, so do not wrap a socket in this.
pub struct Untimed<S>(pub S);

impl<S: Read> Read for Untimed<S> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl<S: Write> Write for Untimed<S> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl<S: Read + Write> PairStream for Untimed<S> {
    fn apply_timeouts(&mut self, _read: Duration, _write: Duration) -> std::io::Result<()> {
        Ok(())
    }
}

// -------------------------------------------------------------------- config

/// Timeouts and caps.  Taken as parameters so the reader can be stricter than
/// the Mac; the defaults are what both ends use unless told otherwise.
#[derive(Debug, Clone)]
pub struct Config {
    /// Per-read socket timeout.
    pub read_timeout: Duration,
    /// Per-write socket timeout.
    pub write_timeout: Duration,
    /// Ceiling on the whole handshake, checked before each blocking step, so a
    /// peer that dribbles one byte per timeout window still loses.
    pub total_timeout: Duration,
    /// Largest plaintext a frame may carry.  Clamped to [`MAX_PLAINTEXT`].
    pub max_plaintext: usize,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            read_timeout: Duration::from_secs(20),
            write_timeout: Duration::from_secs(20),
            total_timeout: Duration::from_secs(60),
            max_plaintext: MAX_PLAINTEXT,
        }
    }
}

// ----------------------------------------------------------------- handshake

/// Run the whole handshake and return an encrypted session.
///
/// Returns [`Error::BadCode`] -- on **both** ends -- when the codes differ.  The
/// session does not exist until confirmation has passed, so a caller cannot
/// accidentally send a payload to an unauthenticated peer.
pub fn handshake<S: PairStream>(
    mut stream: S,
    role: Role,
    code: &Code,
    cfg: &Config,
) -> Result<Session<S>> {
    let started = Instant::now();
    stream.apply_timeouts(cfg.read_timeout, cfg.write_timeout)?;

    // 1. SPAKE2.  Sides are fixed by role and never negotiated.
    let password = Password::new(code.password_bytes());
    let id_reader = Identity::new(IDENTITY_READER);
    let id_mac = Identity::new(IDENTITY_MAC);
    let (state, our_msg) = match role {
        Role::Reader => Spake2::<Ed25519Group>::start_a(&password, &id_reader, &id_mac),
        Role::Mac => Spake2::<Ed25519Group>::start_b(&password, &id_reader, &id_mac),
    };

    write_pake_msg(&mut stream, &our_msg)?;
    stream.flush()?;
    check_deadline(started, cfg)?;
    let their_msg = read_pake_msg(&mut stream)?;

    // A malformed group element is a broken or hostile peer, not a wrong code;
    // keep the two distinguishable.
    let key = state
        .finish(&their_msg)
        .map_err(|_| Error::Protocol("peer sent an invalid SPAKE2 message"))?;

    // 2. Transcript, by role rather than by send order, so both ends hash the
    //    same bytes in the same sequence.
    let (msg_a, msg_b) = match role {
        Role::Reader => (our_msg.as_slice(), their_msg.as_slice()),
        Role::Mac => (their_msg.as_slice(), our_msg.as_slice()),
    };
    let transcript = transcript_hash(msg_a, msg_b);

    let hk = Hkdf::<Sha256>::new(Some(&transcript), &key);
    let k_confirm_reader = expand(&hk, INFO_CONFIRM_READER);
    let k_confirm_mac = expand(&hk, INFO_CONFIRM_MAC);
    let k_reader_to_mac = expand(&hk, INFO_READER_TO_MAC);
    let k_mac_to_reader = expand(&hk, INFO_MAC_TO_READER);

    // 3. Explicit key confirmation, before any payload.  Both sides write
    //    before either reads: 32 bytes each way fits in any socket buffer, and
    //    it means a wrong code fails on BOTH ends rather than one end hanging.
    let (our_confirm_key, their_confirm_key) = match role {
        Role::Reader => (&k_confirm_reader, &k_confirm_mac),
        Role::Mac => (&k_confirm_mac, &k_confirm_reader),
    };
    let ours = confirm_tag(our_confirm_key, &transcript);
    stream.write_all(&ours)?;
    stream.flush()?;
    check_deadline(started, cfg)?;

    let mut theirs = [0u8; 32];
    stream.read_exact(&mut theirs)?;
    let expected = confirm_tag(their_confirm_key, &transcript);
    if expected.ct_eq(&theirs).unwrap_u8() != 1 {
        return Err(Error::BadCode);
    }

    // 4. Confirmed.  Per-direction AEAD keys, counters at 0.
    let (send_key, recv_key, send_dir, recv_dir) = match role {
        Role::Reader => (
            k_reader_to_mac,
            k_mac_to_reader,
            DIR_READER_TO_MAC,
            DIR_MAC_TO_READER,
        ),
        Role::Mac => (
            k_mac_to_reader,
            k_reader_to_mac,
            DIR_MAC_TO_READER,
            DIR_READER_TO_MAC,
        ),
    };

    Ok(Session {
        stream,
        role,
        send: ChaCha20Poly1305::new(Key::from_slice(&send_key)),
        recv: ChaCha20Poly1305::new(Key::from_slice(&recv_key)),
        send_dir,
        recv_dir,
        send_ctr: 0,
        recv_ctr: 0,
        max_plaintext: cfg.max_plaintext.min(MAX_PLAINTEXT),
    })
}

fn check_deadline(started: Instant, cfg: &Config) -> Result<()> {
    if started.elapsed() > cfg.total_timeout {
        return Err(Error::Timeout);
    }
    Ok(())
}

fn expand(hk: &Hkdf<Sha256>, info: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    hk.expand(info, &mut out)
        .expect("32 bytes is within HKDF-SHA256's output limit");
    out
}

fn transcript_hash(msg_a: &[u8], msg_b: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(TRANSCRIPT_LABEL);
    h.update([PROTOCOL_VERSION]);
    h.update((msg_a.len() as u16).to_be_bytes());
    h.update(msg_a);
    h.update((msg_b.len() as u16).to_be_bytes());
    h.update(msg_b);
    h.finalize().into()
}

fn confirm_tag(key: &[u8; 32], transcript: &[u8; 32]) -> [u8; 32] {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(transcript);
    mac.finalize().into_bytes().into()
}

fn write_pake_msg<W: Write>(w: &mut W, msg: &[u8]) -> Result<()> {
    let mut out = Vec::with_capacity(3 + msg.len());
    out.push(PROTOCOL_VERSION);
    out.extend_from_slice(&(msg.len() as u16).to_be_bytes());
    out.extend_from_slice(msg);
    w.write_all(&out)?;
    Ok(())
}

fn read_pake_msg<R: Read>(r: &mut R) -> Result<Vec<u8>> {
    let mut head = [0u8; 3];
    r.read_exact(&mut head)?;
    if head[0] != PROTOCOL_VERSION {
        return Err(Error::Version {
            ours: PROTOCOL_VERSION,
            theirs: head[0],
        });
    }
    let len = u16::from_be_bytes([head[1], head[2]]) as usize;
    if len == 0 || len > MAX_PAKE_MSG {
        return Err(Error::FrameTooLarge {
            claimed: len,
            max: MAX_PAKE_MSG,
        });
    }
    let mut msg = vec![0u8; len];
    r.read_exact(&mut msg)?;
    Ok(msg)
}

// ------------------------------------------------------------------- session

/// A confirmed, encrypted channel.  Existence of this value means key
/// confirmation passed; it cannot be constructed any other way.
pub struct Session<S> {
    stream: S,
    role: Role,
    send: ChaCha20Poly1305,
    recv: ChaCha20Poly1305,
    send_dir: u32,
    recv_dir: u32,
    send_ctr: u64,
    recv_ctr: u64,
    max_plaintext: usize,
}

/// Never prints keys or counters' contents in a way that helps an attacker;
/// exists so callers can `unwrap_err()` on a handshake result.
impl<S> std::fmt::Debug for Session<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("role", &self.role)
            .field("sent", &self.send_ctr)
            .field("received", &self.recv_ctr)
            .finish_non_exhaustive()
    }
}

impl<S> Session<S> {
    pub fn role(&self) -> Role {
        self.role
    }

    pub fn peer_role(&self) -> Role {
        self.role.peer()
    }

    /// Give the socket back, e.g. to close it explicitly.
    pub fn into_inner(self) -> S {
        self.stream
    }

    fn nonce(dir: u32, ctr: u64) -> Nonce {
        let mut n = [0u8; 12];
        n[..4].copy_from_slice(&dir.to_be_bytes());
        n[4..].copy_from_slice(&ctr.to_be_bytes());
        *Nonce::from_slice(&n)
    }
}

impl<S: Read + Write> Session<S> {
    /// Encrypt and send one frame.
    ///
    /// The frame index is authenticated as associated data, so a peer cannot
    /// reorder, drop or replay frames without the tag failing.
    pub fn send(&mut self, plaintext: &[u8]) -> Result<()> {
        if plaintext.len() > self.max_plaintext {
            return Err(Error::FrameTooLarge {
                claimed: plaintext.len(),
                max: self.max_plaintext,
            });
        }
        let nonce = Self::nonce(self.send_dir, self.send_ctr);
        let aad = frame_aad(self.send_dir, self.send_ctr);
        let ct = self
            .send
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .map_err(|_| Error::Protocol("AEAD seal failed"))?;
        self.send_ctr = self
            .send_ctr
            .checked_add(1)
            .ok_or(Error::Protocol("frame counter exhausted"))?;

        let mut out = Vec::with_capacity(4 + ct.len());
        out.extend_from_slice(&(ct.len() as u32).to_be_bytes());
        out.extend_from_slice(&ct);
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        Ok(())
    }

    /// Receive and authenticate one frame.
    ///
    /// The declared length is checked against the cap **before** any buffer is
    /// reserved, so an oversize claim costs nothing.
    pub fn recv(&mut self) -> Result<Vec<u8>> {
        let mut head = [0u8; 4];
        self.stream.read_exact(&mut head)?;
        let len = u32::from_be_bytes(head) as usize;
        let max = self.max_plaintext + 16;
        if len < 16 || len > max.min(MAX_CIPHERTEXT) {
            return Err(Error::FrameTooLarge {
                claimed: len,
                max: max.min(MAX_CIPHERTEXT),
            });
        }
        let mut ct = vec![0u8; len];
        self.stream.read_exact(&mut ct)?;

        let nonce = Self::nonce(self.recv_dir, self.recv_ctr);
        let aad = frame_aad(self.recv_dir, self.recv_ctr);
        let pt = self
            .recv
            .decrypt(
                &nonce,
                Payload {
                    msg: &ct,
                    aad: &aad,
                },
            )
            .map_err(|_| Error::Decrypt)?;
        self.recv_ctr = self
            .recv_ctr
            .checked_add(1)
            .ok_or(Error::Protocol("frame counter exhausted"))?;
        Ok(pt)
    }
}

fn frame_aad(dir: u32, ctr: u64) -> [u8; 12] {
    let mut aad = [0u8; 12];
    aad[..4].copy_from_slice(&dir.to_be_bytes());
    aad[4..].copy_from_slice(&ctr.to_be_bytes());
    aad
}
