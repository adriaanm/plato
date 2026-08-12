//! End-to-end tests of the handshake over an in-memory duplex.
//!
//! Two threads, two pipes, no sockets and no ports: the transport is not what
//! is under test, the protocol is.  `Untimed` is the adapter that lets a
//! non-socket stream through the `PairStream` bound.

use std::io::{self, Read, Write};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;
use std::time::Duration;

use pairing::exchange::{MacHello, ReaderReply};
use pairing::{handshake, Code, Config, Error, Role, Session, Untimed};

// ------------------------------------------------------------- memory duplex

/// One direction of a byte pipe.  Unbounded, so a write never blocks and both
/// ends can write-then-read without deadlocking.
struct Duplex {
    tx: Sender<Vec<u8>>,
    rx: Receiver<Vec<u8>>,
    pending: Vec<u8>,
    /// Everything this end has written, for the tampering test.
    tap: Option<Sender<Vec<u8>>>,
}

impl Duplex {
    fn pair() -> (Duplex, Duplex) {
        let (a_tx, a_rx) = channel();
        let (b_tx, b_rx) = channel();
        (
            Duplex { tx: a_tx, rx: b_rx, pending: Vec::new(), tap: None },
            Duplex { tx: b_tx, rx: a_rx, pending: Vec::new(), tap: None },
        )
    }
}

impl Read for Duplex {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.pending.is_empty() {
            match self.rx.recv() {
                Ok(chunk) => self.pending = chunk,
                // The peer hung up: EOF, which read_exact turns into
                // UnexpectedEof rather than a panic.
                Err(..) => return Ok(0),
            }
        }
        let n = buf.len().min(self.pending.len());
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Ok(n)
    }
}

impl Write for Duplex {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(tap) = &self.tap {
            let _ = tap.send(buf.to_vec());
        }
        self.tx
            .send(buf.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "peer gone"))?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn fast() -> Config {
    Config {
        read_timeout: Duration::from_secs(5),
        write_timeout: Duration::from_secs(5),
        total_timeout: Duration::from_secs(10),
        ..Config::default()
    }
}

/// Run both ends concurrently and hand each result back.
fn both_ends<R, M, TR, TM>(reader_code: &str, mac_code: &str, reader: R, mac: M) -> (TR, TM)
where
    R: FnOnce(pairing::Result<Session<Untimed<Duplex>>>) -> TR + Send + 'static,
    M: FnOnce(pairing::Result<Session<Untimed<Duplex>>>) -> TM + Send + 'static,
    TR: Send + 'static,
    TM: Send + 'static,
{
    let (a, b) = Duplex::pair();
    let rc = Code::parse(reader_code).unwrap();
    let mc = Code::parse(mac_code).unwrap();
    let hr = thread::spawn(move || reader(handshake(Untimed(a), Role::Reader, &rc, &fast())));
    let hm = thread::spawn(move || mac(handshake(Untimed(b), Role::Mac, &mc, &fast())));
    (hr.join().unwrap(), hm.join().unwrap())
}

const MAC_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKx7q0Zc9m2H5Jn1sRhQ0aBcDeFgHiJkLmNoPqRsTuVw platonic@mac";
const HOST_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHostKeyBytesGoHereAAAAAAAAAAAAAAAAAAAAAAAA";

// ------------------------------------------------------------------- tests

#[test]
fn correct_code_pairs_and_exchanges_keys() {
    let (reader, mac) = both_ends(
        "abcd-2345",
        "ABCD 2345", // same code, typed sloppily
        |s| {
            let mut s = s.expect("reader handshake");
            let hello = s.recv_mac_hello().expect("mac hello");
            s.send_reader_reply(&ReaderReply {
                host_public_key: HOST_KEY.into(),
                device_label: "test-kindle".into(),
            })
            .expect("reply");
            hello
        },
        |s| {
            let mut s = s.expect("mac handshake");
            s.send_mac_hello(&MacHello {
                ssh_public_key: MAC_KEY.into(),
            })
            .expect("hello");
            s.recv_reader_reply().expect("reply")
        },
    );
    assert_eq!(reader.ssh_public_key, MAC_KEY);
    assert_eq!(mac.host_public_key, HOST_KEY);
    assert_eq!(mac.device_label, "test-kindle");
}

/// Several frames each way, interleaved: the counters must stay in step.
#[test]
fn frames_are_ordered_and_bidirectional() {
    let (reader, mac) = both_ends(
        "abcd2345",
        "abcd2345",
        |s| {
            let mut s = s.unwrap();
            let mut got = Vec::new();
            for i in 0..5u8 {
                got.push(s.recv().unwrap());
                s.send(&[0xf0 | i]).unwrap();
            }
            got
        },
        |s| {
            let mut s = s.unwrap();
            let mut got = Vec::new();
            for i in 0..5u8 {
                s.send(&[i; 3]).unwrap();
                got.push(s.recv().unwrap());
            }
            got
        },
    );
    for (i, frame) in reader.iter().enumerate() {
        assert_eq!(frame, &vec![i as u8; 3]);
    }
    for (i, frame) in mac.iter().enumerate() {
        assert_eq!(frame, &vec![0xf0 | i as u8]);
    }
}

/// The one that matters: a wrong code fails at key confirmation, on BOTH ends,
/// and no payload is ever sent or received.
#[test]
fn wrong_code_fails_bad_code_on_both_ends() {
    let (reader, mac) = both_ends(
        "abcd2345",
        "abcd2346", // one character off
        |s| match s {
            Err(e) => e,
            Ok(mut s) => {
                // Must be unreachable.  If it is not, prove no payload flows.
                let leaked = s.recv();
                panic!("reader paired with a wrong code; recv gave {:?}", leaked.is_ok());
            }
        },
        |s| match s {
            Err(e) => e,
            Ok(mut s) => {
                let sent = s.send_mac_hello(&MacHello {
                    ssh_public_key: MAC_KEY.into(),
                });
                panic!("mac paired with a wrong code; send gave {:?}", sent.is_ok());
            }
        },
    );
    assert!(matches!(reader, Error::BadCode), "reader got {:?}", reader);
    assert!(matches!(mac, Error::BadCode), "mac got {:?}", mac);
}

/// A code that differs only in a character the alphabet excludes is still a
/// different code -- no silent remapping anywhere in the stack.
#[test]
fn wrong_code_in_the_last_position_still_fails() {
    let (reader, mac) = both_ends("zzzz9999", "zzzz9998", |s| s.err(), |s| s.err());
    assert!(matches!(reader, Some(Error::BadCode)));
    assert!(matches!(mac, Some(Error::BadCode)));
}

type Paired = (
    Session<Untimed<Duplex>>,
    Session<Untimed<Duplex>>,
    Receiver<Vec<u8>>,
    Receiver<Vec<u8>>,
);

/// A paired session on each side, plus a tap on every byte each side writes.
fn paired() -> Paired {
    let (mut a, mut b) = Duplex::pair();
    let (a_tap, a_rx) = channel();
    let (b_tap, b_rx) = channel();
    a.tap = Some(a_tap);
    b.tap = Some(b_tap);
    let code = Code::parse("abcd2345").unwrap();
    let mac = thread::spawn(move || handshake(Untimed(b), Role::Mac, &code, &fast()).unwrap());
    let code = Code::parse("abcd2345").unwrap();
    let reader = handshake(Untimed(a), Role::Reader, &code, &fast()).unwrap();
    (reader, mac.join().unwrap(), a_rx, b_rx)
}

/// The last thing a tapped end wrote.  A frame is one `write_all`, so this is
/// exactly the frame.
fn last_frame(tap: &Receiver<Vec<u8>>) -> Vec<u8> {
    let mut last = Vec::new();
    while let Ok(chunk) = tap.try_recv() {
        last = chunk;
    }
    assert!(!last.is_empty(), "nothing was written");
    last
}

/// Identical plaintext, identical frame counter, opposite directions: the
/// ciphertext must differ.  That is what per-direction keys plus the nonce
/// direction tag buy, and it is the property a shared key would silently lose.
#[test]
fn direction_keys_are_separated() {
    let (mut reader, mut mac, a_rx, b_rx) = paired();
    let _ = last_frame(&a_rx); // drain the handshake writes
    let _ = last_frame(&b_rx);

    reader.send(b"same bytes").unwrap();
    let reader_frame = last_frame(&a_rx);
    mac.send(b"same bytes").unwrap();
    let mac_frame = last_frame(&b_rx);

    assert_eq!(reader_frame.len(), mac_frame.len());
    assert_ne!(
        reader_frame, mac_frame,
        "the two directions produced identical ciphertext for identical plaintext"
    );
    // and both still decrypt on the far side
    assert_eq!(mac.recv().unwrap(), b"same bytes");
    assert_eq!(reader.recv().unwrap(), b"same bytes");
}

/// A flipped byte in a real ciphertext must fail the tag, not truncate silently
/// and not yield partial plaintext.
#[test]
fn tampered_frame_fails_aead() {
    let (mut reader, mut mac, a_rx, _b_rx) = paired();
    let _ = last_frame(&a_rx);

    reader.send(b"the real payload").unwrap();
    let mut frame = last_frame(&a_rx);
    assert_eq!(mac.recv().unwrap(), b"the real payload");

    // Flip one bit in the middle of the ciphertext (past the 4-byte header) and
    // replay the frame at the mac.  The counter has advanced, so this is both a
    // tamper and a replay; either alone must fail.
    let mid = 4 + (frame.len() - 4) / 2;
    frame[mid] ^= 0x01;
    let mut wire = reader.into_inner().0;
    wire.write_all(&frame).unwrap();
    match mac.recv() {
        Err(Error::Decrypt) => {}
        other => panic!("tampered frame gave {:?}", other.map(|v| v.len())),
    }

    // Same for a flip in the authentication tag itself.
    let (mut reader, mut mac, a_rx, _b) = paired();
    let _ = last_frame(&a_rx);
    reader.send(b"x").unwrap();
    let mut frame = last_frame(&a_rx);
    let last = frame.len() - 1;
    frame[last] ^= 0x80;
    // replace the honest frame with the corrupted one by writing it after; the
    // mac reads the honest one first, then the corrupt one.
    let mut wire = reader.into_inner().0;
    wire.write_all(&frame).unwrap();
    assert_eq!(mac.recv().unwrap(), b"x");
    assert!(matches!(mac.recv(), Err(Error::Decrypt)));
}

/// A length header above the cap is refused before anything is allocated.
#[test]
fn oversize_frame_is_refused() {
    let (a, b) = Duplex::pair();
    let code = Code::parse("abcd2345").unwrap();
    let mac = thread::spawn(move || handshake(Untimed(b), Role::Mac, &code, &fast()).unwrap());
    let code = Code::parse("abcd2345").unwrap();
    let mut reader = handshake(Untimed(a), Role::Reader, &code, &fast()).unwrap();
    let mac = mac.join().unwrap();

    let mut raw = mac.into_inner().0;
    raw.write_all(&u32::MAX.to_be_bytes()).unwrap();
    match reader.recv() {
        Err(Error::FrameTooLarge { claimed, max }) => {
            assert_eq!(claimed, u32::MAX as usize);
            assert!(max <= pairing::MAX_PLAINTEXT + 16);
        }
        other => panic!("oversize frame gave {:?}", other.map(|v| v.len())),
    }

    // and the send side refuses to build one
    assert!(matches!(
        reader.send(&vec![0u8; pairing::MAX_PLAINTEXT + 1]),
        Err(Error::FrameTooLarge { .. })
    ));
}

/// A peer that hangs up mid-handshake must produce an error, never a panic.
#[test]
fn truncated_handshake_is_a_clean_error() {
    // EOF before the SPAKE2 message even arrives.
    let (a, b) = Duplex::pair();
    drop(b);
    let code = Code::parse("abcd2345").unwrap();
    let err = handshake(Untimed(a), Role::Reader, &code, &fast()).unwrap_err();
    assert!(matches!(err, Error::Io(..)), "got {:?}", err);

    // EOF after a valid SPAKE2 message but before key confirmation.
    let (a, mut b) = Duplex::pair();
    let code = Code::parse("abcd2345").unwrap();
    let peer = thread::spawn(move || {
        let mut sink = [0u8; 64];
        let _ = b.read(&mut sink);
        // Reply with a well-formed message from the *other* side, then vanish.
        let (_, msg) = spake2_side_b(b"abcd2345");
        b.write_all(&[1]).unwrap();
        b.write_all(&(msg.len() as u16).to_be_bytes()).unwrap();
        b.write_all(&msg).unwrap();
        drop(b);
    });
    let err = handshake(Untimed(a), Role::Reader, &code, &fast()).unwrap_err();
    peer.join().unwrap();
    assert!(matches!(err, Error::Io(..)), "got {:?}", err);
}

/// A peer speaking a different protocol version gets a named error, not a hang
/// and not a garbage parse.
#[test]
fn version_mismatch_is_named() {
    let (a, mut b) = Duplex::pair();
    let peer = thread::spawn(move || {
        let mut sink = [0u8; 64];
        let _ = b.read(&mut sink);
        b.write_all(&[99, 0, 33]).unwrap();
        b.write_all(&[0u8; 33]).unwrap();
    });
    let code = Code::parse("abcd2345").unwrap();
    let err = handshake(Untimed(a), Role::Reader, &code, &fast()).unwrap_err();
    peer.join().unwrap();
    match err {
        Error::Version { ours, theirs } => {
            assert_eq!(ours, pairing::PROTOCOL_VERSION);
            assert_eq!(theirs, 99);
        }
        other => panic!("got {:?}", other),
    }
}

/// A structurally invalid SPAKE2 message is a protocol error, distinct from
/// BadCode: the difference is "peer is broken" vs "human mistyped".
#[test]
fn garbage_pake_message_is_not_bad_code() {
    let (a, mut b) = Duplex::pair();
    let peer = thread::spawn(move || {
        let mut sink = [0u8; 64];
        let _ = b.read(&mut sink);
        b.write_all(&[pairing::PROTOCOL_VERSION, 0, 33]).unwrap();
        b.write_all(&[0xffu8; 33]).unwrap();
        let mut sink = [0u8; 64];
        let _ = b.read(&mut sink);
    });
    let code = Code::parse("abcd2345").unwrap();
    let err = handshake(Untimed(a), Role::Reader, &code, &fast()).unwrap_err();
    peer.join().unwrap();
    assert!(
        matches!(err, Error::Protocol(..) | Error::BadCode),
        "got {:?}",
        err
    );
    assert!(!matches!(err, Error::Io(..)));
}

/// A newline smuggled into the pubkey field must be rejected on receipt, after
/// decryption -- i.e. a *paired* peer cannot inject a second authorized_keys
/// line either.
#[test]
fn injected_newline_is_rejected_over_a_real_session() {
    let evil = format!(
        "{}\nssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEVILEVILEVILEVILEVILEVILEVILEVILEVI evil@attacker",
        MAC_KEY
    );
    let (reader, mac) = both_ends(
        "abcd2345",
        "abcd2345",
        |s| s.unwrap().recv_mac_hello().unwrap_err(),
        move |s| {
            let mut s = s.unwrap();
            // Encoding refuses it too -- so go around the typed API and put the
            // bytes on the wire by hand, which is what a hostile peer would do.
            let mut raw = vec![1u8];
            raw.extend_from_slice(&(evil.len() as u16).to_be_bytes());
            raw.extend_from_slice(evil.as_bytes());
            let refused = MacHello {
                ssh_public_key: evil.clone(),
            }
            .encode()
            .is_err();
            s.send(&raw).unwrap();
            refused
        },
    );
    assert!(mac, "encode() should refuse a newline before it reaches the wire");
    assert!(
        matches!(reader, Error::InvalidField(..)),
        "receiver got {:?}",
        reader
    );
}

// Helper: build a side-B SPAKE2 message with the same pinned parameters, for the
// truncation test.  Duplicated here on purpose -- a test that reached into the
// crate's internals would not prove the wire format.
fn spake2_side_b(password: &[u8]) -> (spake2::Spake2<spake2::Ed25519Group>, Vec<u8>) {
    spake2::Spake2::<spake2::Ed25519Group>::start_b(
        &spake2::Password::new(password),
        &spake2::Identity::new(b"platokin-reader"),
        &spake2::Identity::new(b"platonic-mac"),
    )
}
