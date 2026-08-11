//! Shared pieces of the folder-sync protocol: discovery, a minimal HTTP/1.0
//! client and server, the manifest format, and just enough JSON escaping to
//! talk to Plato.
//!
//! Everything here is std-only on purpose.  The reader this was written for is
//! an armv7 soft-float device on Linux 3.0.35 whose CA store is empty and whose
//! clock is years out of date, so TLS to anything is a non-starter and every
//! added dependency is another thing to cross-compile.  The transport is
//! therefore plain HTTP on the LAN, and the whole protocol fits in this file.

use std::fmt::Write as _;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, UdpSocket};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Wire version.  Bump on any incompatible change to discovery or the manifest.
pub const MAGIC: &str = "PLATOSYNC1";

pub const DEFAULT_HTTP_PORT: u16 = 8571;
pub const DEFAULT_DISCO_PORT: u16 = 30303;

//
// ---------------------------------------------------------------- discovery
//
// The device has no mDNS resolver (no avahi, no nss-mdns), and multicast on
// cheap SDIO WiFi parts is not something to bet a feature on.  Since we own
// both ends, discovery is a UDP broadcast probe answered by unicast: the hub's
// address is simply whoever replied.
//
//   device -> 255.255.255.255:disco   "PLATOSYNC1 DISCOVER <token>"
//   hub    -> back to sender          "PLATOSYNC1 OFFER <http_port> <epoch>"
//
// A wrong or missing token is answered with silence rather than an error, so a
// stray probe on someone else's network learns nothing.

/// Broadcast for a hub and return the first one that answers.
///
/// Tries `attempts` times with `timeout` per attempt; a single dropped packet
/// is normal right after associating, which is exactly when this runs.
pub fn discover(disco_port: u16, token: &str, attempts: u32,
                timeout: Duration) -> io::Result<(SocketAddr, u16)> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_broadcast(true)?;
    socket.set_read_timeout(Some(timeout))?;

    let probe = format!("{} DISCOVER {}", MAGIC, token);
    let target = SocketAddrV4::new(Ipv4Addr::BROADCAST, disco_port);

    for _ in 0..attempts {
        socket.send_to(probe.as_bytes(), target)?;

        let mut buf = [0u8; 256];
        // Drain whatever arrives within the window; anything that isn't a
        // well-formed offer is someone else's traffic, so keep listening
        // rather than giving up on the attempt.
        while let Ok((n, from)) = socket.recv_from(&mut buf) {
            if let Some(port) = parse_offer(&buf[..n]) {
                return Ok((from, port));
            }
        }
    }

    Err(io::Error::new(io::ErrorKind::NotFound, "no hub answered"))
}

fn parse_offer(bytes: &[u8]) -> Option<u16> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut parts = text.split_whitespace();
    if parts.next()? != MAGIC || parts.next()? != "OFFER" {
        return None;
    }
    parts.next()?.parse().ok()
}

/// Serve discovery probes forever.  Runs on the hub.
pub fn serve_discovery(disco_port: u16, http_port: u16, token: String) -> io::Result<()> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, disco_port))?;
    let mut buf = [0u8; 256];

    loop {
        let (n, from) = match socket.recv_from(&mut buf) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("discovery: {}", e);
                continue;
            },
        };

        let text = match std::str::from_utf8(&buf[..n]) {
            Ok(t) => t,
            Err(..) => continue,
        };

        let mut parts = text.split_whitespace();
        if parts.next() != Some(MAGIC) || parts.next() != Some("DISCOVER") {
            continue;
        }
        // An absent token field reads as the empty string, which is the
        // correct answer when the hub is running without a token.
        if parts.next().unwrap_or("") != token {
            continue;
        }

        let offer = format!("{} OFFER {} {}", MAGIC, http_port, epoch_secs());
        if let Err(e) = socket.send_to(offer.as_bytes(), from) {
            eprintln!("discovery: reply to {}: {}", from, e);
        }
    }
}

//
// ----------------------------------------------------------------- manifest
//
// A tab-separated table, because the device side has no JSON parser and does
// not need one.  Line 1 is a header carrying the hub's clock in two forms;
// every later line is one file.
//
//   #PLATOSYNC1 <epoch> <YYYY-MM-DD HH:MM:SS>
//   <size>\t<mtime-epoch>\t<relative/path.epub>
//
// The formatted time exists so the device can `date -s` it without owning a
// calendar implementation -- it has no working clock of its own and its CA
// store is empty, so this is also the only time source it is going to get.

#[derive(Debug, Clone)]
pub struct Entry {
    pub size: u64,
    pub mtime: i64,
    pub path: String,
}

#[derive(Debug, Clone)]
pub struct Manifest {
    pub epoch: i64,
    /// The hub's local time, preformatted for `date -s`.
    pub stamp: String,
    pub entries: Vec<Entry>,
}

pub fn parse_manifest(text: &str) -> io::Result<Manifest> {
    let mut lines = text.lines();
    let header = lines.next().unwrap_or_default();

    let mut fields = header.splitn(3, ' ');
    if fields.next() != Some(&format!("#{}", MAGIC)) {
        return Err(io::Error::new(io::ErrorKind::InvalidData,
                                  "not a folder-sync manifest"));
    }
    let epoch = fields.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let stamp = fields.next().unwrap_or_default().to_string();

    let mut entries = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let mut cols = line.splitn(3, '\t');
        let size = cols.next().and_then(|v| v.parse().ok());
        let mtime = cols.next().and_then(|v| v.parse().ok());
        let path = cols.next().map(str::to_string);
        if let (Some(size), Some(mtime), Some(path)) = (size, mtime, path) {
            entries.push(Entry { size, mtime, path });
        }
    }

    Ok(Manifest { epoch, stamp, entries })
}

//
// -------------------------------------------------------------- HTTP client
//
// HTTP/1.0 with `Connection: close`, so the body is "everything until EOF" and
// there is no chunked decoding, no keep-alive state and no Content-Length to
// trust.  One request per connection; the device makes them strictly in series.

/// Issue `GET path` and stream the response body into `sink`.
pub fn http_get(addr: SocketAddr, path: &str, token: &str,
                timeout: Duration, sink: &mut dyn Write) -> io::Result<u64> {
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    let mut request = String::new();
    write!(request, "GET {} HTTP/1.0\r\n", path).ok();
    write!(request, "Host: {}\r\n", addr).ok();
    if !token.is_empty() {
        write!(request, "X-Sync-Token: {}\r\n", token).ok();
    }
    request.push_str("Connection: close\r\n\r\n");
    stream.write_all(request.as_bytes())?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);

    let mut status = String::new();
    reader.read_line(&mut status)?;
    let code = status.split_whitespace().nth(1).unwrap_or("");
    if code != "200" {
        return Err(io::Error::new(io::ErrorKind::Other,
                                  format!("server said {}", status.trim())));
    }

    // Skip headers.
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
            break;
        }
    }

    io::copy(&mut reader, sink)
}

/// `http_get` into memory, for the manifest.
pub fn http_get_string(addr: SocketAddr, path: &str, token: &str,
                       timeout: Duration) -> io::Result<String> {
    let mut buf = Vec::new();
    http_get(addr, path, token, timeout, &mut buf)?;
    String::from_utf8(buf)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "manifest is not UTF-8"))
}

//
// ------------------------------------------------------------- HTTP server
//

/// Read a request line plus headers from `stream`.
/// Returns `(target, token)`; the body is ignored -- GET only.
pub fn read_request(stream: &TcpStream) -> io::Result<(String, String)> {
    let mut reader = BufReader::new(stream);

    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    if method != "GET" {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "GET only"));
    }

    let mut token = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("X-Sync-Token:") {
            token = value.trim().to_string();
        }
    }

    Ok((target, token))
}

pub fn write_status(stream: &mut TcpStream, code: u16, reason: &str) -> io::Result<()> {
    write!(stream, "HTTP/1.0 {} {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
           code, reason)
}

pub fn write_body_header(stream: &mut TcpStream, kind: &str, len: u64) -> io::Result<()> {
    write!(stream, "HTTP/1.0 200 OK\r\nConnection: close\r\n\
                    Content-Type: {}\r\nContent-Length: {}\r\n\r\n", kind, len)
}

//
// -------------------------------------------------------------- small tools
//

pub fn epoch_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH)
                     .map(|d| d.as_secs() as i64)
                     .unwrap_or(0)
}

/// Percent-encode everything outside the unreserved set.  Paths come from a
/// user's filenames, so spaces, quotes and non-ASCII are the norm, not the
/// exception.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' |
            b'-' | b'.' | b'_' | b'~' | b'/' => out.push(*byte as char),
            _ => { write!(out, "%{:02X}", byte).ok(); },
        }
    }
    out
}

pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Escape a string for a JSON double-quoted literal.  Plato's fetcher protocol
/// is the only JSON either end emits, and it is emitted, never parsed, so a
/// full serializer would be dead weight.
pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => { write!(out, "\\u{:04x}", c as u32).ok(); },
            c => out.push(c),
        }
    }
    out
}

/// A minimal `key = value` config, because there is no TOML parser here and
/// the file has five keys.  `#` starts a comment.
pub fn parse_config(text: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            pairs.push((key.trim().to_string(), value.trim().to_string()));
        }
    }
    pairs
}

/// Read the whole of `path` as a string, or the empty string if it isn't there.
pub fn read_to_string_or_empty(path: &std::path::Path) -> String {
    std::fs::File::open(path).ok()
        .and_then(|mut f| {
            let mut s = String::new();
            f.read_to_string(&mut s).ok().map(|_| s)
        })
        .unwrap_or_default()
}
