//! The wire protocol between `platonic` on a Mac and `platonic-recv` on the
//! reader, and the validation rules that go with it.
//!
//! Both ends compile this module, so a field cannot be added to one side only.
//!
//! # Why there is a protocol at all
//!
//! A paired Mac's key carries a forced command ([E22] in platokin
//! `docs/experiments.md`), so the only thing it can run is this receiver.  The
//! client's requested command string survives as `$SSH_ORIGINAL_COMMAND` and is
//! **ignored** (Adriaan, 2026-08-12): a client-controlled string must not reach
//! a decision made by a root program.  Every operation and all of its metadata
//! therefore travel here, on stdin, framed by us.
//!
//! # Framing
//!
//! ```text
//! greeting   (server first)  "PLATONIC-RECV/1\n"        16 bytes, fixed
//! request                    u8 op  ‖ per-op fields
//! response                   u8 status ‖ per-op body   (status 0)
//!                            u8 status ‖ str message   (status != 0)
//! str                        u16be len ‖ len bytes, UTF-8
//! ```
//!
//! One connection carries many requests: several PUTs, an OPEN and a SWEEP is
//! the normal push, and that is one ssh round trip instead of four.  The client
//! ends with [`Op::Quit`] or by closing stdin.
//!
//! PUT's payload is **not** part of the request frame: the header ends with the
//! length, and the bytes follow immediately.  That keeps a 40 MB paper off both
//! ends' heaps as a second copy, and it is why [`read_request`] returns the
//! header and leaves the reader positioned on the payload.
//!
//! # Limits
//!
//! Every one of these is checked *before* anything is allocated.
//!
//! | Limit | Value | Why |
//! |---|---|---|
//! | [`MAX_COMPONENT`] | 128 bytes | a folder or file name; the library's longest real name is ~70 |
//! | [`MAX_PUT`] | 64 MiB | documents are megabytes -- the biggest thing here is a scanned PDF |
//! | [`MAX_MESSAGE`] | 512 bytes | an error string |
//! | [`MAX_ENTRIES`] | 20 000 | a LIST or SWEEP result |
//!
//! [E22]: https://github.com/adriaanm/platokin

use std::io::{self, Read, Write};

/// Sent by the receiver before it reads anything.  Server-speaks-first is what
/// lets the Mac tell "the receiver is deployed" from "dropbear ran a shell and
/// printed nothing" without guessing, and it is legible in a terminal, so the
/// deploy check is one `ssh … platonic-recv < /dev/null`.
pub const GREETING: &[u8] = b"PLATONIC-RECV/1\n";

/// Where the binary lives on the device: p3, which survives the userstore
/// going away.  Pairing writes this path into the forced command, so changing
/// it is a change to already-installed `authorized_keys` lines.
pub const RECV_PATH: &str = "/var/local/ezssh/platonic-recv";

/// The library root.  Compiled in, never taken from the wire or the
/// environment: it is the boundary every path is asserted to be inside.
pub const LIBRARY_ROOT: &str = "/mnt/us/documents";

/// Plato's command FIFO (created by `plato.sh`, never by us).
pub const FIFO_PATH: &str = "/tmp/plato.cmd";

/// The one folder SWEEP may delete from.  Not a parameter: the destination is
/// what decides the lifetime, and a named folder means "keep this".
pub const SWEEP_FOLDER: &str = "inbox";

/// The one folder HIGHLIGHTS may read.  Not a parameter, for the same reason
/// [`SWEEP_FOLDER`] is not: no client string reaches a decision made by a root
/// program, and a request with no fields at all cannot smuggle one.  The
/// reader's "Export Highlights" writes here — and it is deliberately not
/// `inbox/`: the sweep judges inbox lifetimes against the Mac's clock, and a
/// file the device stamped (its clock reads 2023) would be judged ancient.
pub const HIGHLIGHTS_FOLDER: &str = "highlights";

pub const MAX_COMPONENT: usize = 128;
pub const MAX_PUT: u32 = 64 * 1024 * 1024;
/// A file HIGHLIGHTS ships back.  These are grep-style text listings of a
/// document's highlights, kilobytes in real life; a megabyte is the "someone
/// put something odd in the folder" ceiling, not a target.
pub const MAX_FILE: u32 = 1024 * 1024;
pub const MAX_MESSAGE: usize = 512;
/// A URL for OPEN_URL.  2 KiB is the customary practical ceiling for a URL a
/// person shares; anything longer is more likely an attack on the FIFO line
/// than a link somebody wants to read.
pub const MAX_URL: usize = 2048;
/// A name the receiver *reports* (LIST, SWEEP), which is not a name it was
/// asked to accept: files already on the library can be longer than
/// [`MAX_COMPONENT`] and need not be ASCII, and dropping them from a listing
/// would be worse than carrying them.
pub const MAX_LISTED: usize = 512;
pub const MAX_ENTRIES: u32 = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Put = 1,
    Open = 2,
    Import = 3,
    List = 4,
    Sweep = 5,
    Quit = 6,
    /// "Open this URL in the reader's article view, and keep it."  Added
    /// after 1..=6 shipped as op 7, which carried the URL alone; when the
    /// reader learned to save the article into `inbox/` it needed the Mac's
    /// clock too (the device's reads 2023 -- the sweep would judge a stamp
    /// from it as ancient), and rather than reshape op 7's frame, 7 was
    /// retired and 8 took its place.  There is no version negotiation to
    /// bump, because the existing compat story covers both directions: the
    /// mismatched end reads an unknown op byte, answers
    /// [`Status::Unsupported`] ("protocol error: unknown op byte ...") and
    /// ends the session -- one visible line naming the redeploy, never a
    /// hang, and never a frame misread as another frame.
    OpenUrl = 8,
    /// "Send back every file in `highlights/`."  The request carries no
    /// fields: which folder, and that the transfer is read-only, are compiled
    /// into the receiver, so this is strictly narrower than a general GET
    /// would be.  An old receiver answers the unknown op byte with
    /// [`Status::Unsupported`] and the Mac prints the redeploy hint.
    Highlights = 9,
}

impl Op {
    pub fn from_u8(b: u8) -> Option<Op> {
        match b {
            1 => Some(Op::Put),
            2 => Some(Op::Open),
            3 => Some(Op::Import),
            4 => Some(Op::List),
            5 => Some(Op::Sweep),
            6 => Some(Op::Quit),
            8 => Some(Op::OpenUrl),
            9 => Some(Op::Highlights),
            _ => None,
        }
    }
}

/// Kept distinguishable on purpose: the Mac reports a validation refusal very
/// differently from a device that ran out of disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok = 0,
    /// A rule in [`validate_component`] (or a limit) said no.
    Invalid = 1,
    NotFound = 2,
    Io = 3,
    /// Unknown op byte, or an op this build does not implement.
    Unsupported = 4,
}

impl Status {
    pub fn from_u8(b: u8) -> Option<Status> {
        match b {
            0 => Some(Status::Ok),
            1 => Some(Status::Invalid),
            2 => Some(Status::NotFound),
            3 => Some(Status::Io),
            4 => Some(Status::Unsupported),
            _ => None,
        }
    }
}

/// A request header.  For [`Request::Put`] the payload follows the header on
/// the wire and is not carried here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Put {
        folder: String,
        filename: String,
        /// Unix seconds, from the **Mac's** clock: the device's reads 2023
        /// until `clock-sync.sh` has run, so no age is ever computed there.
        mtime: i64,
        length: u32,
    },
    Open { folder: String, filename: String },
    Import,
    List,
    /// Delete everything in `inbox/` with an mtime at or below this, again on
    /// the Mac's clock.
    Sweep { cutoff: i64 },
    Quit,
    /// Open a web URL in the reader's article view.  The receiver's whole
    /// job is one validated `open-url` line into Plato's FIFO, the same seam
    /// PUT's open already uses; the reader does the fetching, and saves what
    /// it fetched into `inbox/` stamped with this mtime -- the **Mac's**
    /// clock, for the same reason PUT's is: the sweep judges inbox lifetimes
    /// against it, and the device's own clock reads 2023.
    OpenUrl { url: String, mtime: i64 },
    /// Fetch the exported highlight files.  No fields on purpose; see
    /// [`Op::Highlights`].
    Highlights,
}

impl Request {
    pub fn op(&self) -> Op {
        match self {
            Request::Put { .. } => Op::Put,
            Request::Open { .. } => Op::Open,
            Request::Import => Op::Import,
            Request::List => Op::List,
            Request::Sweep { .. } => Op::Sweep,
            Request::Quit => Op::Quit,
            Request::OpenUrl { .. } => Op::OpenUrl,
            Request::Highlights => Op::Highlights,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub folder: String,
    pub name: String,
    pub size: u64,
    pub mtime: i64,
}

/// One file HIGHLIGHTS ships back: its name in the folder, and its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedFile {
    pub name: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// Bytes actually written -- the byte-count verification the shell path
    /// did with `wc -c`, now answered by the writer itself.
    Put { written: u64 },
    Done,
    List(Vec<Entry>),
    Swept(Vec<String>),
    Highlights(Vec<NamedFile>),
    Err { status: Status, message: String },
}

//
// -------------------------------------------------------------- validation
//
// This runs as root.  Everything below is enforced on the DEVICE; the Mac
// calling the same function first is a convenience, not the check.

/// Everything a folder or file name may contain.
///
/// Conservative on purpose: `platonic` slugifies to `[A-Za-z0-9._-]` before it
/// ever sends a name, so this allowlist is already wider than the tool needs.
/// It exists to be argued about in one place rather than inferred from a regex
/// at the call site.
fn allowed(c: char) -> bool {
    c.is_ascii_alphanumeric() || " ._-+,()[]'&#@".contains(c)
}

/// One path component, validated for use under the library root.
///
/// The rules, and what each one stops:
///
/// * non-empty, and at most [`MAX_COMPONENT`] bytes
/// * no `/` and no `\` -- a component is a component
/// * no NUL, no control character (a newline in a name would corrupt the FIFO
///   line, and a NUL truncates every C API underneath us)
/// * must not begin with `-`, or a name becomes an option to something later
/// * must not begin with `.` -- which also disposes of `.` and `..`, and of
///   hidden files nobody asked for
/// * every byte in [`allowed`]
pub fn validate_component(what: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{}: empty", what));
    }
    if value.len() > MAX_COMPONENT {
        return Err(format!("{}: longer than {} bytes", what, MAX_COMPONENT));
    }
    if value.starts_with('-') {
        return Err(format!("{}: must not start with '-'", what));
    }
    if value.starts_with('.') {
        return Err(format!("{}: must not start with '.'", what));
    }
    for c in value.chars() {
        if c == '/' || c == '\\' {
            return Err(format!("{}: must be a single path component", what));
        }
        if (c as u32) < 0x20 || c as u32 == 0x7F {
            return Err(format!("{}: control character", what));
        }
        if !allowed(c) {
            return Err(format!("{}: character {:?} is not allowed", what, c));
        }
    }
    Ok(())
}

/// A URL fit to travel one line of Plato's FIFO and, from there, one HTTPS
/// request.  The rules, and what each one stops:
///
/// * non-empty, and at most [`MAX_URL`] bytes
/// * must begin with `http://` or `https://` -- the reader's article source
///   makes the same check (`news/article.rs`), so `file:`, `javascript:` and
///   every other scheme is refused before it crosses the wire, not after
/// * no whitespace and no control character -- a newline would smuggle a
///   second command into the FIFO line, and a space would at best be a URL
///   somebody forgot to percent-encode
pub fn validate_url(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("url: empty".to_string());
    }
    if value.len() > MAX_URL {
        return Err(format!("url: longer than {} bytes", MAX_URL));
    }
    if !value.starts_with("http://") && !value.starts_with("https://") {
        return Err("url: must start with http:// or https://".to_string());
    }
    for c in value.chars() {
        if c.is_whitespace() || (c as u32) < 0x20 || c as u32 == 0x7F {
            return Err("url: whitespace or control character".to_string());
        }
    }
    Ok(())
}

/// A refusal message quoting the client's own input, bounded and escaped, so a
/// hostile name cannot smuggle control characters into a log line.
pub fn quote_for_log(value: &str) -> String {
    let mut out = String::with_capacity(value.len().min(64) + 2);
    out.push('"');
    for c in value.chars().take(64) {
        match c {
            '"' | '\\' => { out.push('\\'); out.push(c); }
            c if (c as u32) < 0x20 || c as u32 == 0x7F =>
                out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    if value.chars().count() > 64 {
        out.push('…');
    }
    out
}

//
// ----------------------------------------------------------------- encoding
//

fn write_u8(w: &mut impl Write, v: u8) -> io::Result<()> {
    w.write_all(&[v])
}

fn write_u16(w: &mut impl Write, v: u16) -> io::Result<()> {
    w.write_all(&v.to_be_bytes())
}

fn write_u32(w: &mut impl Write, v: u32) -> io::Result<()> {
    w.write_all(&v.to_be_bytes())
}

fn write_u64(w: &mut impl Write, v: u64) -> io::Result<()> {
    w.write_all(&v.to_be_bytes())
}

fn write_i64(w: &mut impl Write, v: i64) -> io::Result<()> {
    w.write_all(&v.to_be_bytes())
}

/// Strings are capped at encode time too: a client bug must not be able to
/// send a frame the receiver will refuse to read, because the two ends would
/// then disagree about where the next request starts.
fn write_str(w: &mut impl Write, cap: usize, s: &str) -> io::Result<()> {
    if s.len() > cap {
        return Err(io::Error::new(io::ErrorKind::InvalidInput,
                                  "string longer than its cap"));
    }
    write_u16(w, s.len() as u16)?;
    w.write_all(s.as_bytes())
}

fn read_u8(r: &mut impl Read) -> io::Result<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)?;
    Ok(b[0])
}

fn read_u16(r: &mut impl Read) -> io::Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_be_bytes(b))
}

fn read_u32(r: &mut impl Read) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

fn read_u64(r: &mut impl Read) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_be_bytes(b))
}

fn read_i64(r: &mut impl Read) -> io::Result<i64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(i64::from_be_bytes(b))
}

/// Reads a length, refuses it against `cap`, and only then allocates.  Never
/// `Vec::with_capacity(len)` on a length off the wire.
fn read_str(r: &mut impl Read, cap: usize) -> io::Result<String> {
    let len = read_u16(r)? as usize;
    if len > cap {
        return Err(io::Error::new(io::ErrorKind::InvalidData,
                                  "string longer than its cap"));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    String::from_utf8(buf)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "not UTF-8"))
}

pub fn write_request(w: &mut impl Write, req: &Request) -> io::Result<()> {
    write_u8(w, req.op() as u8)?;
    match req {
        Request::Put { folder, filename, mtime, length } => {
            write_str(w, MAX_COMPONENT, folder)?;
            write_str(w, MAX_COMPONENT, filename)?;
            write_i64(w, *mtime)?;
            if *length > MAX_PUT {
                return Err(io::Error::new(io::ErrorKind::InvalidInput,
                                          "payload over MAX_PUT"));
            }
            write_u32(w, *length)?;
        }
        Request::Open { folder, filename } => {
            write_str(w, MAX_COMPONENT, folder)?;
            write_str(w, MAX_COMPONENT, filename)?;
        }
        Request::Sweep { cutoff } => write_i64(w, *cutoff)?,
        Request::OpenUrl { url, mtime } => {
            write_str(w, MAX_URL, url)?;
            write_i64(w, *mtime)?;
        }
        Request::Import | Request::List | Request::Quit | Request::Highlights => {}
    }
    Ok(())
}

/// `Ok(None)` means the client closed stdin between requests, which is a
/// normal end of session -- distinguishing it from a truncated frame is the
/// whole reason this returns an Option rather than an error.
///
/// For [`Request::Put`] the reader is left positioned on the payload.
pub fn read_request(r: &mut impl Read) -> io::Result<Option<Request>> {
    let mut b = [0u8; 1];
    match r.read(&mut b) {
        Ok(0) => return Ok(None),
        Ok(_) => {}
        Err(e) => return Err(e),
    }
    let op = Op::from_u8(b[0]).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData,
                       format!("unknown op byte {}", b[0]))
    })?;
    let req = match op {
        Op::Put => {
            let folder = read_str(r, MAX_COMPONENT)?;
            let filename = read_str(r, MAX_COMPONENT)?;
            let mtime = read_i64(r)?;
            let length = read_u32(r)?;
            if length > MAX_PUT {
                return Err(io::Error::new(io::ErrorKind::InvalidData,
                                          "payload over MAX_PUT"));
            }
            Request::Put { folder, filename, mtime, length }
        }
        Op::Open => Request::Open {
            folder: read_str(r, MAX_COMPONENT)?,
            filename: read_str(r, MAX_COMPONENT)?,
        },
        Op::Import => Request::Import,
        Op::List => Request::List,
        Op::Sweep => Request::Sweep { cutoff: read_i64(r)? },
        Op::Quit => Request::Quit,
        Op::OpenUrl => Request::OpenUrl {
            url: read_str(r, MAX_URL)?,
            mtime: read_i64(r)?,
        },
        Op::Highlights => Request::Highlights,
    };
    Ok(Some(req))
}

pub fn write_response(w: &mut impl Write, resp: &Response) -> io::Result<()> {
    match resp {
        Response::Put { written } => {
            write_u8(w, Status::Ok as u8)?;
            write_u64(w, *written)?;
        }
        Response::Done => write_u8(w, Status::Ok as u8)?,
        Response::List(entries) => {
            write_u8(w, Status::Ok as u8)?;
            write_u32(w, entries.len().min(MAX_ENTRIES as usize) as u32)?;
            for e in entries.iter().take(MAX_ENTRIES as usize) {
                write_str(w, MAX_LISTED, &e.folder)?;
                write_str(w, MAX_LISTED, &e.name)?;
                write_u64(w, e.size)?;
                write_i64(w, e.mtime)?;
            }
        }
        Response::Swept(names) => {
            write_u8(w, Status::Ok as u8)?;
            write_u32(w, names.len().min(MAX_ENTRIES as usize) as u32)?;
            for n in names.iter().take(MAX_ENTRIES as usize) {
                write_str(w, MAX_LISTED, n)?;
            }
        }
        Response::Highlights(files) => {
            write_u8(w, Status::Ok as u8)?;
            write_u32(w, files.len().min(MAX_ENTRIES as usize) as u32)?;
            for f in files.iter().take(MAX_ENTRIES as usize) {
                write_str(w, MAX_LISTED, &f.name)?;
                if f.data.len() > MAX_FILE as usize {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput,
                                              "file over MAX_FILE"));
                }
                write_u32(w, f.data.len() as u32)?;
                w.write_all(&f.data)?;
            }
        }
        Response::Err { status, message } => {
            write_u8(w, *status as u8)?;
            let mut msg: String = message.chars().take(MAX_MESSAGE / 4).collect();
            while msg.len() > MAX_MESSAGE {
                msg.pop();
            }
            write_str(w, MAX_MESSAGE, &msg)?;
        }
    }
    w.flush()
}

/// The op is needed because a success body is op-shaped; an error body never
/// is.  The client always knows which request it just sent.
pub fn read_response(r: &mut impl Read, op: Op) -> io::Result<Response> {
    let status = Status::from_u8(read_u8(r)?).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "unknown status byte")
    })?;
    if status != Status::Ok {
        return Ok(Response::Err {
            status,
            message: read_str(r, MAX_MESSAGE)?,
        });
    }
    let resp = match op {
        Op::Put => Response::Put { written: read_u64(r)? },
        Op::Open | Op::Import | Op::Quit | Op::OpenUrl => Response::Done,
        Op::List => {
            let n = read_u32(r)?;
            if n > MAX_ENTRIES {
                return Err(io::Error::new(io::ErrorKind::InvalidData,
                                          "too many entries"));
            }
            let mut entries = Vec::new();
            for _ in 0..n {
                entries.push(Entry {
                    folder: read_str(r, MAX_LISTED)?,
                    name: read_str(r, MAX_LISTED)?,
                    size: read_u64(r)?,
                    mtime: read_i64(r)?,
                });
            }
            Response::List(entries)
        }
        Op::Sweep => {
            let n = read_u32(r)?;
            if n > MAX_ENTRIES {
                return Err(io::Error::new(io::ErrorKind::InvalidData,
                                          "too many entries"));
            }
            let mut names = Vec::new();
            for _ in 0..n {
                names.push(read_str(r, MAX_LISTED)?);
            }
            Response::Swept(names)
        }
        Op::Highlights => {
            let n = read_u32(r)?;
            if n > MAX_ENTRIES {
                return Err(io::Error::new(io::ErrorKind::InvalidData,
                                          "too many entries"));
            }
            let mut files = Vec::new();
            for _ in 0..n {
                let name = read_str(r, MAX_LISTED)?;
                let len = read_u32(r)?;
                if len > MAX_FILE {
                    return Err(io::Error::new(io::ErrorKind::InvalidData,
                                              "file over MAX_FILE"));
                }
                let mut data = vec![0u8; len as usize];
                r.read_exact(&mut data)?;
                files.push(NamedFile { name, data });
            }
            Response::Highlights(files)
        }
    };
    Ok(resp)
}

//
// -------------------------------------------------------------------- tests
//

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(req: Request) {
        let mut buf = Vec::new();
        write_request(&mut buf, &req).unwrap();
        let mut cur = &buf[..];
        let back = read_request(&mut cur).unwrap().unwrap();
        assert_eq!(back, req);
        assert!(cur.is_empty(), "{:?} left {} bytes unread", req, cur.len());
    }

    #[test]
    fn every_request_round_trips() {
        roundtrip(Request::Put {
            folder: "inbox".into(),
            filename: "platokin-plan.md".into(),
            mtime: 1_786_527_005,
            length: 4096,
        });
        roundtrip(Request::Open {
            folder: "papers".into(),
            filename: "A File - Name.pdf".into(),
        });
        roundtrip(Request::Import);
        roundtrip(Request::List);
        roundtrip(Request::Sweep { cutoff: -1 });
        roundtrip(Request::Quit);
        roundtrip(Request::OpenUrl {
            url: "https://example.com/essay?a=1&b=2#top".into(),
            mtime: 1_786_527_005,
        });
        roundtrip(Request::Highlights);
    }

    #[test]
    fn an_oversized_url_is_refused_at_both_ends() {
        // At encode time, so a client bug cannot emit a frame the receiver
        // would refuse mid-stream...
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL));
        let mut buf = Vec::new();
        assert!(write_request(&mut buf, &Request::OpenUrl { url: long, mtime: 0 }).is_err());
        // ...and at decode time, before any allocation follows the header.
        let mut buf = vec![Op::OpenUrl as u8];
        buf.extend_from_slice(&((MAX_URL + 1) as u16).to_be_bytes());
        buf.extend_from_slice(&vec![b'a'; MAX_URL + 1]);
        assert!(read_request(&mut &buf[..]).is_err());
    }

    #[test]
    fn put_leaves_the_reader_on_the_payload() {
        let mut buf = Vec::new();
        write_request(&mut buf, &Request::Put {
            folder: "inbox".into(), filename: "a.md".into(),
            mtime: 0, length: 5,
        }).unwrap();
        buf.extend_from_slice(b"hello");
        let mut cur = &buf[..];
        read_request(&mut cur).unwrap().unwrap();
        assert_eq!(cur, b"hello");
    }

    #[test]
    fn every_response_round_trips() {
        for resp in [
            Response::Put { written: 12_345 },
            Response::Done,
            Response::List(vec![Entry {
                folder: "inbox".into(), name: "x.md".into(),
                size: 7, mtime: 1_786_000_000,
            }]),
            Response::Swept(vec!["old.md".into()]),
            Response::Highlights(vec![
                NamedFile { name: "plan.md".into(), data: b"plan.md:2: two\n".to_vec() },
                NamedFile { name: "notes.md".into(), data: Vec::new() },
            ]),
            Response::Highlights(Vec::new()),
            Response::Err { status: Status::Invalid, message: "folder: empty".into() },
        ] {
            let op = match &resp {
                Response::Put { .. } => Op::Put,
                Response::Done => Op::Import,
                Response::List(_) => Op::List,
                Response::Swept(_) => Op::Sweep,
                Response::Highlights(_) => Op::Highlights,
                Response::Err { .. } => Op::Put,
            };
            let mut buf = Vec::new();
            write_response(&mut buf, &resp).unwrap();
            assert_eq!(read_response(&mut &buf[..], op).unwrap(), resp);
        }
    }

    #[test]
    fn an_oversized_highlight_file_is_refused_at_both_ends() {
        // At encode time, so the two ends cannot desync mid-stream...
        let big = NamedFile { name: "big.md".into(),
                              data: vec![b'x'; MAX_FILE as usize + 1] };
        let mut buf = Vec::new();
        assert!(write_response(&mut buf, &Response::Highlights(vec![big])).is_err());
        // ...and at decode time, before the length is trusted with an
        // allocation.
        let mut buf = vec![Status::Ok as u8];
        buf.extend_from_slice(&1u32.to_be_bytes());
        buf.extend_from_slice(&6u16.to_be_bytes()); buf.extend_from_slice(b"big.md");
        buf.extend_from_slice(&(MAX_FILE + 1).to_be_bytes());
        assert!(read_response(&mut &buf[..], Op::Highlights).is_err());
    }

    #[test]
    fn a_truncated_highlight_body_is_an_error() {
        let mut buf = Vec::new();
        write_response(&mut buf, &Response::Highlights(vec![
            NamedFile { name: "plan.md".into(), data: b"plan.md:1: one\n".to_vec() },
        ])).unwrap();
        for cut in 1..buf.len() {
            assert!(read_response(&mut &buf[..cut], Op::Highlights).is_err(),
                    "truncation at {} accepted", cut);
        }
    }

    #[test]
    fn closed_stdin_between_requests_is_not_an_error() {
        assert_eq!(read_request(&mut &b""[..]).unwrap(), None);
    }

    #[test]
    fn unknown_op_byte_is_rejected() {
        // 7 is the retired clock-less OPEN_URL: an old Mac talking to this
        // receiver must get the loud unknown-op refusal, not a reinterpreted
        // frame.
        for b in [0u8, 7, 200, 255] {
            assert!(read_request(&mut &[b][..]).is_err(), "op {} accepted", b);
        }
    }

    #[test]
    fn a_truncated_frame_is_an_error_not_a_short_read() {
        let mut buf = Vec::new();
        write_request(&mut buf, &Request::Put {
            folder: "inbox".into(), filename: "a.md".into(),
            mtime: 0, length: 10,
        }).unwrap();
        for cut in 1..buf.len() {
            assert!(read_request(&mut &buf[..cut]).is_err(),
                    "truncation at {} accepted", cut);
        }
    }

    #[test]
    fn an_oversized_length_header_is_refused_before_any_allocation() {
        // op ‖ "inbox" ‖ "a.md" ‖ mtime ‖ 4 GiB - 1
        let mut buf = vec![Op::Put as u8];
        buf.extend_from_slice(&5u16.to_be_bytes()); buf.extend_from_slice(b"inbox");
        buf.extend_from_slice(&4u16.to_be_bytes()); buf.extend_from_slice(b"a.md");
        buf.extend_from_slice(&0i64.to_be_bytes());
        buf.extend_from_slice(&u32::MAX.to_be_bytes());
        assert!(read_request(&mut &buf[..]).is_err());
    }

    #[test]
    fn an_oversized_string_header_is_refused() {
        let mut buf = vec![Op::Open as u8];
        buf.extend_from_slice(&((MAX_COMPONENT + 1) as u16).to_be_bytes());
        buf.extend_from_slice(&vec![b'a'; MAX_COMPONENT + 1]);
        assert!(read_request(&mut &buf[..]).is_err());
    }

    #[test]
    fn a_non_utf8_string_is_refused() {
        let mut buf = vec![Op::Open as u8];
        buf.extend_from_slice(&2u16.to_be_bytes());
        buf.extend_from_slice(&[0xff, 0xfe]);
        assert!(read_request(&mut &buf[..]).is_err());
    }

    // ---- validation

    #[test]
    fn ordinary_names_pass() {
        for name in ["inbox", "papers", "platokin-plan.md", "A File - Name.pdf",
                     "notes_2026", "a.b", "x(1).pdf", "R&D [draft].epub"] {
            validate_component("filename", name).unwrap();
        }
    }

    #[test]
    fn hostile_names_are_refused() {
        for bad in [
            "", "..", ".", "/", "//", "/etc/passwd", "..%2f..", "../..",
            "../../etc/shadow", "a/b", "a\\b", "inbox/", ".ssh", ".",
            "-rf", "--force", "a\0b", "a\nb", "a\rb", "a\tb", "a\x7fb",
            "\u{202e}gpj.exe", "naïve.pdf", "a;rm -rf /", "$(id)", "`id`",
            "a|b", "a>b", "*.md", "a?b", "~root",
        ] {
            assert!(validate_component("filename", bad).is_err(),
                    "{:?} should be refused", bad);
        }
        assert!(validate_component("filename", &"a".repeat(MAX_COMPONENT + 1))
                .is_err());
        validate_component("filename", &"a".repeat(MAX_COMPONENT)).unwrap();
    }

    #[test]
    fn only_web_urls_pass_validation() {
        for good in ["https://example.com", "http://example.com/a?b=c&d=e#f",
                     "https://example.com/percent%20encoded"] {
            validate_url(good).unwrap();
        }
        for bad in [
            "", "example.com", "ftp://example.com", "file:///etc/passwd",
            "javascript:alert(1)", "https://example.com/a b",
            "https://example.com/a\nopen-url https://evil",
            "https://example.com/a\tb", "https://example.com/\x07",
        ] {
            assert!(validate_url(bad).is_err(), "{:?} should be refused", bad);
        }
        assert!(validate_url(&format!("https://e.com/{}", "a".repeat(MAX_URL)))
                .is_err());
    }

    #[test]
    fn log_quoting_escapes_and_bounds() {
        assert_eq!(quote_for_log("a\nb"), "\"a\\x0ab\"");
        assert_eq!(quote_for_log("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert!(quote_for_log(&"x".repeat(200)).ends_with("…"));
    }
}
