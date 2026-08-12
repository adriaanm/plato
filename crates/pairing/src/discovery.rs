//! Finding the reader on an unknown router, rung 2 of the ladder in
//! `docs/pairing-candidates.md`: a UDP probe sent to the subnet broadcast
//! address **and** to 224.0.0.251, answered **unicast**.
//!
//! Unicast is the direction that always works.  Multicast and broadcast are
//! both filtered by some consumer APs, and this project has watched exactly
//! that happen, so neither is load-bearing: the reader also shows its IP next
//! to the code, and that path needs no packets at all.
//!
//! std only.  **No mDNS crate** -- deliberately deferred; if it is ever wanted,
//! `mdns-sd` is the candidate that fits a threads-only app.
//!
//! # Pinned wire parameters
//!
//! ```text
//! probe  := "PLATOKIN-PAIR <version> PROBE"
//! offer  := "PLATOKIN-PAIR <version> OFFER <tcp_port> <label>"
//! ```
//!
//! The label runs to the end of the datagram and may contain spaces.  Nothing
//! else is in the reply: **no serial, no MAC address, no hostname** -- a probe
//! from a stranger on the café WiFi learns only that someone chose to arm
//! pairing, and what they called their device.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

use crate::exchange::validate_label;
use crate::handshake::PROTOCOL_VERSION;

/// Magic string.  Pinned; shared by both ends because they are the same crate.
pub const DISCOVERY_MAGIC: &str = "PLATOKIN-PAIR";

/// UDP port the reader listens on while pairing is armed.
pub const DEFAULT_DISCOVERY_PORT: u16 = 30304;

/// TCP port the pairing handshake runs on.
pub const DEFAULT_PAIRING_PORT: u16 = 30305;

/// The mDNS group, borrowed as a plain multicast address -- we speak our own
/// protocol on it, not mDNS.  Joining it is best effort: whether ath6kl on
/// Linux 3.0.35 supports a group join is **Open**, so a failure is logged and
/// the responder keeps working on broadcast alone.
pub const MULTICAST_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);

/// Longest datagram either side will look at.  A probe is ~20 bytes and an
/// offer under 128.
const MAX_DATAGRAM: usize = 512;

/// What a reader answered with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    /// Where the reply came from.  This, not anything inside the payload, is
    /// the address to connect to.
    pub addr: IpAddr,
    pub tcp_port: u16,
    pub label: String,
}

impl Offer {
    /// The address to hand to [`std::net::TcpStream::connect`].
    pub fn pairing_addr(&self) -> SocketAddr {
        SocketAddr::new(self.addr, self.tcp_port)
    }
}

pub fn probe_bytes() -> Vec<u8> {
    format!("{} {} PROBE", DISCOVERY_MAGIC, PROTOCOL_VERSION).into_bytes()
}

/// `Some(())` if this datagram is a probe we should answer.  Anything else --
/// wrong magic, wrong version, someone else's traffic -- is answered with
/// silence rather than an error, so a stray probe learns nothing.
pub fn parse_probe(buf: &[u8]) -> Option<()> {
    let s = std::str::from_utf8(buf).ok()?;
    let mut it = s.split(' ');
    if it.next()? != DISCOVERY_MAGIC {
        return None;
    }
    if it.next()?.parse::<u8>().ok()? != PROTOCOL_VERSION {
        return None;
    }
    if it.next()? != "PROBE" {
        return None;
    }
    it.next().is_none().then_some(())
}

pub fn offer_bytes(tcp_port: u16, label: &str) -> Vec<u8> {
    format!(
        "{} {} OFFER {} {}",
        DISCOVERY_MAGIC, PROTOCOL_VERSION, tcp_port, label
    )
    .into_bytes()
}

/// Parse a reply.  The label is peer input, so it is validated here rather than
/// wherever it eventually gets printed.
pub fn parse_offer(buf: &[u8], from: IpAddr) -> Option<Offer> {
    let s = std::str::from_utf8(buf).ok()?;
    let mut it = s.splitn(5, ' ');
    if it.next()? != DISCOVERY_MAGIC {
        return None;
    }
    if it.next()?.parse::<u8>().ok()? != PROTOCOL_VERSION {
        return None;
    }
    if it.next()? != "OFFER" {
        return None;
    }
    let tcp_port: u16 = it.next()?.parse().ok()?;
    if tcp_port == 0 {
        return None;
    }
    let label = it.next()?;
    validate_label(label).ok()?;
    Some(Offer {
        addr: from,
        tcp_port,
        label: label.to_string(),
    })
}

// ------------------------------------------------------------- device side

/// The reader's discovery responder.
///
/// **Only construct this while pairing mode is armed, and drop it when the
/// window closes.** There is no "off" switch by design: a responder that
/// exists answers, a responder that does not exist is silent, and that is one
/// less piece of state to get wrong.
pub struct Responder {
    socket: UdpSocket,
    tcp_port: u16,
    label: String,
    join_error: Option<io::Error>,
}

impl Responder {
    /// Bind the discovery port and try to join the multicast group.
    ///
    /// A failed join is **not fatal** -- see [`Responder::join_error`], which
    /// the caller should log.  Broadcast still reaches us, and the shown-IP
    /// fallback always does.
    pub fn bind(discovery_port: u16, tcp_port: u16, label: &str) -> io::Result<Responder> {
        validate_label(label).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, discovery_port))?;
        let join_error = socket
            .join_multicast_v4(&MULTICAST_GROUP, &Ipv4Addr::UNSPECIFIED)
            .err();
        Ok(Responder {
            socket,
            tcp_port,
            label: label.to_string(),
            join_error,
        })
    }

    /// Why the multicast join failed, if it did.  Log it; do not act on it.
    pub fn join_error(&self) -> Option<&io::Error> {
        self.join_error.as_ref()
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Answer probes until `deadline`.  Returns the number of probes answered.
    ///
    /// Blocks in slices of at most `poll` so the pairing window closes on time
    /// even when the network is silent.
    pub fn serve_until(&self, deadline: Instant, poll: Duration) -> io::Result<usize> {
        let mut answered = 0;
        let mut buf = [0u8; MAX_DATAGRAM];
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Ok(answered);
            }
            let slice = poll.min(deadline - now);
            self.socket.set_read_timeout(Some(slice))?;
            match self.socket.recv_from(&mut buf) {
                Ok((n, from)) => {
                    if parse_probe(&buf[..n]).is_some() {
                        // Unicast back to whoever asked: the direction that is
                        // not filtered by anything.
                        let reply = offer_bytes(self.tcp_port, &self.label);
                        self.socket.send_to(&reply, from)?;
                        answered += 1;
                    }
                }
                Err(e) if would_block(&e) => {}
                Err(e) => return Err(e),
            }
        }
    }
}

fn would_block(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
}

// ---------------------------------------------------------------- Mac side

/// Probe for armed readers and collect unicast replies for `window`.
///
/// Sends to the global broadcast address and to the multicast group *at the
/// same time* rather than in sequence: which of the two survives an arbitrary
/// AP is unknowable, and both together cost one extra datagram.
pub fn discover(discovery_port: u16, window: Duration) -> io::Result<Vec<Offer>> {
    discover_with_extra_targets(discovery_port, window, &[])
}

/// As [`discover`], plus explicit unicast targets -- a directed broadcast
/// address the caller worked out, or a known address being re-probed.
pub fn discover_with_extra_targets(
    discovery_port: u16,
    window: Duration,
    extra: &[IpAddr],
) -> io::Result<Vec<Offer>> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_broadcast(true).ok(); // not fatal: multicast may still work
    socket.set_multicast_loop_v4(true).ok();

    let mut targets: Vec<SocketAddr> = vec![
        SocketAddrV4::new(Ipv4Addr::BROADCAST, discovery_port).into(),
        SocketAddrV4::new(MULTICAST_GROUP, discovery_port).into(),
    ];
    targets.extend(extra.iter().map(|ip| SocketAddr::new(*ip, discovery_port)));

    let probe = probe_bytes();
    let deadline = Instant::now() + window;
    let mut offers: Vec<Offer> = Vec::new();
    let mut buf = [0u8; MAX_DATAGRAM];
    let mut sent_any = false;
    let mut last_error = None;

    // Re-probe periodically: a single dropped datagram right after associating
    // is normal, and this runs exactly then.
    let mut next_probe = Instant::now();
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        if now >= next_probe {
            for t in &targets {
                match socket.send_to(&probe, t) {
                    Ok(..) => sent_any = true,
                    Err(e) => last_error = Some(e),
                }
            }
            next_probe = now + Duration::from_millis(500);
        }
        let slice = Duration::from_millis(200).min(deadline - Instant::now().min(deadline));
        socket.set_read_timeout(Some(slice.max(Duration::from_millis(1))))?;
        match socket.recv_from(&mut buf) {
            Ok((n, from)) => {
                if let Some(offer) = parse_offer(&buf[..n], from.ip()) {
                    if !offers.iter().any(|o| o.addr == offer.addr && o.tcp_port == offer.tcp_port) {
                        offers.push(offer);
                    }
                }
            }
            Err(e) if would_block(&e) => {}
            Err(e) => return Err(e),
        }
    }

    if !sent_any {
        return Err(last_error.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::AddrNotAvailable, "nowhere to probe")
        }));
    }
    Ok(offers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exchange::LABEL_MAX;

    #[test]
    fn probe_round_trip() {
        assert!(parse_probe(&probe_bytes()).is_some());
        assert!(parse_probe(b"PLATOKIN-PAIR 1 PROBE extra").is_none());
        assert!(parse_probe(b"PLATOSYNC1 DISCOVER x").is_none());
        assert!(parse_probe(b"PLATOKIN-PAIR 99 PROBE").is_none());
        assert!(parse_probe(b"\xff\xfe").is_none());
        assert!(parse_probe(b"").is_none());
    }

    #[test]
    fn offer_round_trip() {
        let from: IpAddr = Ipv4Addr::new(192, 168, 1, 7).into();
        let o = parse_offer(&offer_bytes(30305, "Adriaan's Kindle"), from).unwrap();
        assert_eq!(
            o,
            Offer {
                addr: from,
                tcp_port: 30305,
                label: "Adriaan's Kindle".into()
            }
        );
        assert_eq!(o.pairing_addr().port(), 30305);
    }

    #[test]
    fn offer_rejects_junk() {
        let from: IpAddr = Ipv4Addr::LOCALHOST.into();
        assert!(parse_offer(b"PLATOKIN-PAIR 1 OFFER 0 x", from).is_none());
        assert!(parse_offer(b"PLATOKIN-PAIR 1 OFFER notaport x", from).is_none());
        assert!(parse_offer(b"PLATOKIN-PAIR 1 OFFER 30305", from).is_none());
        assert!(parse_offer(b"PLATOKIN-PAIR 2 OFFER 30305 x", from).is_none());
        // a label that would inject a newline into whatever prints it
        assert!(parse_offer(b"PLATOKIN-PAIR 1 OFFER 30305 a\nb", from).is_none());
        let long = format!("PLATOKIN-PAIR 1 OFFER 30305 {}", "x".repeat(LABEL_MAX + 1));
        assert!(parse_offer(long.as_bytes(), from).is_none());
    }

    #[test]
    fn offer_carries_nothing_identifying() {
        // The payload is exactly magic, version, port and label -- if this ever
        // grows a field, this test is where to justify it.
        let bytes = offer_bytes(30305, "kindle");
        let s = std::str::from_utf8(&bytes).unwrap();
        assert_eq!(s.split(' ').count(), 5);
    }

    /// Loopback end-to-end: the responder answers a probe, unicast.
    #[test]
    fn responder_answers_a_unicast_probe() {
        let responder = Responder::bind(0, DEFAULT_PAIRING_PORT, "test-kindle").unwrap();
        let port = responder.local_addr().unwrap().port();
        let deadline = Instant::now() + Duration::from_secs(3);
        let handle = std::thread::spawn(move || {
            responder
                .serve_until(deadline, Duration::from_millis(100))
                .unwrap()
        });

        let client = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        client
            .send_to(&probe_bytes(), (Ipv4Addr::LOCALHOST, port))
            .unwrap();
        let mut buf = [0u8; MAX_DATAGRAM];
        let (n, from) = client.recv_from(&mut buf).unwrap();
        let offer = parse_offer(&buf[..n], from.ip()).expect("a well-formed offer");
        assert_eq!(offer.tcp_port, DEFAULT_PAIRING_PORT);
        assert_eq!(offer.label, "test-kindle");

        // junk must be met with silence, not a reply
        client
            .send_to(b"not for you", (Ipv4Addr::LOCALHOST, port))
            .unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        assert!(client.recv_from(&mut buf).is_err(), "answered a stray probe");

        assert_eq!(handle.join().unwrap(), 1);
    }
}
