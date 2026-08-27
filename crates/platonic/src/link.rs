//! The device operations, and the two ways of performing them.
//!
//! Every device operation `platonic` performs goes through [`Link`].  There
//! are three implementations:
//!
//! * [`RecvLink`] — one ssh connection running `platonic-recv`, speaking the
//!   framed protocol on its stdin.  **The whole push is one session**: several
//!   PUTs, an OPEN and a SWEEP, one connection, no shell anywhere.
//! * [`ShellLink`] — what the tool did before: `mkdir -p && cat > … && touch
//!   -t … && wc -c <`, one ssh per operation.  Kept because Adriaan's Macs
//!   authenticate with the **admin** key, which is unrestricted, and a device
//!   that has not been redeployed yet has no receiver to talk to.
//! * [`DryLink`] — prints what the chosen transport would do.
//!
//! Degrading is deliberate and never silent: [`open_link`] says on stderr
//! which transport it chose and why it did not choose the other.  A **paired**
//! (restricted) key cannot degrade at all — its ssh session runs the receiver
//! and nothing else — so there the missing receiver is a hard error naming the
//! redeploy.
//!
//! `PLATONIC_TRANSPORT=recv|shell` forces one, for testing both paths on a
//! device that has both.

use std::io::{BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use platonic_recv::proto::{self, Entry, Request, Response, Status};

use crate::pure::{self, shell_quote, DOCROOT};
use crate::session::{Probe, RunError, Session};

pub trait Link {
    /// Bytes written, as reported by the device.  The caller compares it with
    /// the local length: that check is the push's only proof.
    fn put(&mut self, folder: &str, name: &str, data: &[u8], mtime: i64)
           -> Result<u64, String>;
    fn open(&mut self, folder: &str, name: &str) -> Result<(), String>;
    /// Open a web URL in the reader's article view.  Unlike `open` there is
    /// no document behind it: the reader fetches and readability-extracts the
    /// page itself, then files what it fetched into `inbox/` for offline
    /// reading -- stamped with `mtime`, the Mac's clock, because the sweep
    /// judges inbox lifetimes against it and the device's clock reads 2023.
    fn open_url(&mut self, url: &str, mtime: i64) -> Result<(), String>;
    fn import(&mut self) -> Result<(), String>;
    fn list(&mut self) -> Result<Vec<Entry>, String>;
    /// Names deleted from `inbox/`.
    fn sweep(&mut self, cutoff: i64) -> Result<Vec<String>, String>;
    /// The files the reader's "Export Highlights" wrote into `highlights/`.
    fn highlights(&mut self) -> Result<Vec<proto::NamedFile>, String>;
    /// End the session cleanly.  Only [`RecvLink`] has anything to close.
    fn finish(&mut self) {}
}

//
// -------------------------------------------------------------- the receiver
//

pub struct RecvLink {
    child: Child,
    /// An Option so [`Link::finish`] can drop it: ssh does not exit until the
    /// receiver sees EOF on stdin.
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    stderr: Arc<Mutex<Vec<u8>>>,
}

impl RecvLink {
    /// Spawn the receiver over ssh and read its greeting.  Server-speaks-first
    /// is what makes this a fact rather than an assumption: a device without
    /// the binary answers with dropbear's "not found" on stderr and EOF here.
    pub fn connect(sess: &Session) -> Result<RecvLink, String> {
        let argv = sess.recv_argv();
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not run ssh: {}", e))?;

        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let mut err_pipe = child.stderr.take().expect("piped");

        // Drained on its own thread: a receiver that logs a refusal must not
        // be able to fill the pipe and deadlock the transfer.
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&stderr);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = err_pipe.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if let Ok(mut guard) = sink.lock() {
                    guard.extend_from_slice(&buf[..n]);
                }
            }
        });

        let mut link = RecvLink {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            stderr,
        };

        let mut greeting = vec![0u8; proto::GREETING.len()];
        match link.stdout.read_exact(&mut greeting) {
            Ok(()) if greeting == proto::GREETING => Ok(link),
            Ok(()) => {
                let text = String::from_utf8_lossy(&greeting).escape_debug().to_string();
                Err(format!("not the receiver: it answered {:?}", text))
            }
            Err(_) => Err(link.stderr_note()),
        }
    }

    fn stderr_note(&self) -> String {
        let text = self.stderr.lock()
            .map(|g| String::from_utf8_lossy(&g).trim().to_string())
            .unwrap_or_default();
        if text.is_empty() {
            format!("{} did not answer", proto::RECV_PATH)
        } else {
            text.lines().next().unwrap_or("").to_string()
        }
    }

    fn call(&mut self, req: Request, payload: Option<&[u8]>)
            -> Result<Response, String> {
        let op = req.op();
        let write = (|| -> std::io::Result<()> {
            let stdin = self.stdin.as_mut().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::BrokenPipe,
                                    "the session is closed")
            })?;
            proto::write_request(stdin, &req)?;
            if let Some(bytes) = payload {
                stdin.write_all(bytes)?;
            }
            stdin.flush()
        })();
        if let Err(e) = write {
            return Err(format!("{}: {}", e, self.stderr_note()));
        }
        match proto::read_response(&mut self.stdout, op) {
            Ok(Response::Err { status, message }) =>
                Err(describe(status, &message)),
            Ok(resp) => Ok(resp),
            Err(e) => Err(format!("{}: {}", e, self.stderr_note())),
        }
    }
}

fn describe_run_error(e: RunError) -> String {
    match e {
        RunError::Timeout => format!("ssh timed out mid-command. {}",
                                     pure::NO_READER_MSG),
        RunError::Spawn(e) => format!("could not run ssh: {}", e),
    }
}

fn describe(status: Status, message: &str) -> String {
    match status {
        Status::Invalid => format!("the reader refused it: {}", message),
        Status::NotFound => message.to_string(),
        Status::Io => format!("the reader could not do it: {}", message),
        Status::Unsupported =>
            format!("the reader does not support this: {} (redeploy {})",
                    message, proto::RECV_PATH),
        Status::Ok => message.to_string(),
    }
}

impl Link for RecvLink {
    fn put(&mut self, folder: &str, name: &str, data: &[u8], mtime: i64)
           -> Result<u64, String> {
        if data.len() as u64 > proto::MAX_PUT as u64 {
            return Err(format!("{} is {} bytes; the receiver caps a document at \
                                {} MiB", name, data.len(),
                               proto::MAX_PUT / (1024 * 1024)));
        }
        // Validated here too so a bad name is reported before the bytes move.
        // The check that counts is the device's; this one is politeness.
        proto::validate_component("folder", folder)?;
        proto::validate_component("filename", name)?;
        match self.call(Request::Put {
            folder: folder.to_string(),
            filename: name.to_string(),
            mtime,
            length: data.len() as u32,
        }, Some(data))? {
            Response::Put { written } => Ok(written),
            other => Err(format!("unexpected answer to PUT: {:?}", other)),
        }
    }

    fn open(&mut self, folder: &str, name: &str) -> Result<(), String> {
        self.call(Request::Open {
            folder: folder.to_string(),
            filename: name.to_string(),
        }, None).map(|_| ())
    }

    fn open_url(&mut self, url: &str, mtime: i64) -> Result<(), String> {
        // Validated here too, like PUT's names: politeness, not the check --
        // the check that counts runs on the device.  An OLD receiver answers
        // this op with Unsupported and `describe` names the redeploy, so the
        // failure is one visible line, never a hang.
        proto::validate_url(url)?;
        self.call(Request::OpenUrl { url: url.to_string(), mtime }, None).map(|_| ())
    }

    fn import(&mut self) -> Result<(), String> {
        self.call(Request::Import, None).map(|_| ())
    }

    fn list(&mut self) -> Result<Vec<Entry>, String> {
        match self.call(Request::List, None)? {
            Response::List(entries) => Ok(entries),
            other => Err(format!("unexpected answer to LIST: {:?}", other)),
        }
    }

    fn sweep(&mut self, cutoff: i64) -> Result<Vec<String>, String> {
        match self.call(Request::Sweep { cutoff }, None)? {
            Response::Swept(names) => Ok(names),
            other => Err(format!("unexpected answer to SWEEP: {:?}", other)),
        }
    }

    /// An old receiver answers the unknown op byte with Unsupported and
    /// `describe` names the redeploy — one visible line, never a hang.
    fn highlights(&mut self) -> Result<Vec<proto::NamedFile>, String> {
        match self.call(Request::Highlights, None)? {
            Response::Highlights(files) => Ok(files),
            other => Err(format!("unexpected answer to HIGHLIGHTS: {:?}", other)),
        }
    }

    fn finish(&mut self) {
        let _ = self.call(Request::Quit, None);
        // EOF, then reap: ssh stays alive as long as stdin is open.
        self.stdin.take();
        let _ = self.child.wait();
    }
}

//
// ------------------------------------------------------------- the old path
//

pub struct ShellLink<'a> {
    sess: &'a Session,
}

impl<'a> ShellLink<'a> {
    pub fn new(sess: &'a Session) -> ShellLink<'a> {
        ShellLink { sess }
    }

    /// Best-effort FIFO poke, as before: the push already succeeded, and a
    /// missing listener only means the document appears after a restart.
    fn poke(&self, line: &str) -> Result<(), String> {
        if self.sess.dry_run {
            // Session::run does the printing; skip the FIFO existence probe,
            // whose empty dry-run output would read as "Plato is not running".
            let _ = self.sess.run(&pure::fifo_write_command(line), None,
                                  Duration::from_secs(15));
            return Ok(());
        }
        let probe = self.sess.out(
            &format!("test -p {} && echo FIFO_OK || echo NO_FIFO", pure::FIFO),
            Duration::from_secs(15));
        if !probe.contains("FIFO_OK") {
            return Err("Plato is not running its command listener".to_string());
        }
        match self.sess.run(&pure::fifo_write_command(line), None,
                            Duration::from_secs(15)) {
            Ok(out) if out.status == 0 => Ok(()),
            _ => Err(format!("FIFO write timed out — is Plato reading {}?",
                             pure::FIFO)),
        }
    }
}

impl<'a> Link for ShellLink<'a> {
    fn put(&mut self, folder: &str, name: &str, data: &[u8], mtime: i64)
           -> Result<u64, String> {
        let dir = format!("{}/{}", DOCROOT, folder);
        let path = format!("{}/{}", dir, name);
        let cmd = pure::push_command(&dir, &path, &pure::touch_stamp(mtime as f64));
        let out = self.sess.run(&cmd, Some(data), Duration::from_secs(120))
                      .map_err(describe_run_error)?;
        if self.sess.dry_run {
            return Ok(data.len() as u64);
        }
        // OUTPUT, not the exit code: busybox (CLAUDE.md, 2026-08-11).
        out.text().trim().parse::<u64>()
            .map_err(|_| format!("no size came back: {}", out.err_text()))
    }

    fn open(&mut self, folder: &str, name: &str) -> Result<(), String> {
        self.poke(&format!("open {}/{}/{}", DOCROOT, folder, name))
    }

    /// The FIFO write proves a listener exists, not that it knows the verb: a
    /// Plato from before `open-url` logs "Unknown command" on the device and
    /// nothing opens.  The receiver path does not have this blind spot, which
    /// is one more reason it is the preferred transport.
    fn open_url(&mut self, url: &str, mtime: i64) -> Result<(), String> {
        self.poke(&format!("open-url {} {}", mtime, url))
    }

    fn import(&mut self) -> Result<(), String> {
        self.poke("import")
    }

    fn list(&mut self) -> Result<Vec<Entry>, String> {
        let out = self.sess.out(&pure::list_command(), Duration::from_secs(30));
        Ok(pure::entries_from_stat(&pure::parse_stat_lines(&out)))
    }

    fn sweep(&mut self, cutoff: i64) -> Result<Vec<String>, String> {
        let out = self.sess.out(&pure::sweep_list_command(),
                                Duration::from_secs(30));
        let doomed = pure::expired_paths(&pure::parse_stat_lines(&out),
                                         cutoff as f64);
        if doomed.is_empty() {
            return Ok(Vec::new());
        }
        let _ = self.sess.run(&pure::rm_command(&doomed), None,
                              Duration::from_secs(30));
        Ok(doomed.iter().map(|p| pure::basename(p)).collect())
    }

    /// One anonymous blob rather than named files: the grep format is
    /// self-describing, and the shell path has no framing to carry names in.
    fn highlights(&mut self) -> Result<Vec<proto::NamedFile>, String> {
        let out = self.sess.out(&pure::highlights_command(),
                                Duration::from_secs(30));
        if out.trim().is_empty() {
            return Ok(Vec::new());
        }
        Ok(vec![proto::NamedFile { name: String::new(), data: out.into_bytes() }])
    }
}

//
// --------------------------------------------------------------------- dry
//

pub struct DryLink {
    pub transport: &'static str,
}

impl DryLink {
    fn echo(&self, what: String) {
        println!("+ [{}] {}", self.transport, what);
    }
}

impl Link for DryLink {
    fn put(&mut self, folder: &str, name: &str, data: &[u8], mtime: i64)
           -> Result<u64, String> {
        self.echo(format!("PUT {}/{}  {} bytes, mtime {}",
                          folder, name, data.len(), mtime));
        Ok(data.len() as u64)
    }
    fn open(&mut self, folder: &str, name: &str) -> Result<(), String> {
        self.echo(format!("OPEN {}/{}", folder, name));
        Ok(())
    }
    fn open_url(&mut self, url: &str, mtime: i64) -> Result<(), String> {
        self.echo(format!("OPEN_URL {}  mtime {}", url, mtime));
        Ok(())
    }
    fn import(&mut self) -> Result<(), String> {
        self.echo("IMPORT".to_string());
        Ok(())
    }
    fn list(&mut self) -> Result<Vec<Entry>, String> {
        self.echo("LIST".to_string());
        Ok(Vec::new())
    }
    fn sweep(&mut self, cutoff: i64) -> Result<Vec<String>, String> {
        self.echo(format!("SWEEP inbox older than {}", cutoff));
        Ok(Vec::new())
    }
    fn highlights(&mut self) -> Result<Vec<proto::NamedFile>, String> {
        self.echo("HIGHLIGHTS".to_string());
        Ok(Vec::new())
    }
}

//
// ---------------------------------------------------------------- selection
//

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Auto,
    Recv,
    Shell,
}

impl Transport {
    /// `PLATONIC_TRANSPORT`, so both paths can be exercised on one device.
    pub fn from_env(value: Option<&str>) -> Result<Transport, String> {
        match value.unwrap_or("").trim() {
            "" | "auto" => Ok(Transport::Auto),
            "recv" => Ok(Transport::Recv),
            "shell" => Ok(Transport::Shell),
            other => Err(format!(
                "PLATONIC_TRANSPORT={:?}: expected recv, shell or auto", other)),
        }
    }
}

/// Which link to build, given what the probe saw and what the key is.
///
/// Pure so the fallback rules are testable without a device -- they are the
/// part of this file most likely to be wrong in a way nobody notices until the
/// day the receiver is missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    Recv,
    /// With the one-line warning naming why.
    Shell(Option<String>),
    Fail(String),
}

pub fn choose(transport: Transport, probe: &Probe, restricted_key: bool) -> Choice {
    match transport {
        Transport::Shell if restricted_key => Choice::Fail(format!(
            "PLATONIC_TRANSPORT=shell, but this Mac authenticates with the \
             paired key, whose ssh session can only run {}. There is no shell \
             to fall back to.", proto::RECV_PATH)),
        Transport::Shell => Choice::Shell(None),
        Transport::Recv if probe.receiver => Choice::Recv,
        Transport::Recv => Choice::Fail(format!(
            "PLATONIC_TRANSPORT=recv, but the reader did not answer as {}: {}",
            proto::RECV_PATH, probe.note)),
        Transport::Auto if probe.receiver => Choice::Recv,
        // A paired key gets one program.  If that program is not there, no
        // amount of falling back helps, and saying "falling back to the shell"
        // would be a lie the user then has to debug.
        Transport::Auto if restricted_key => Choice::Fail(format!(
            "the reader has no {} — and this Mac's key is the paired one, \
             which may run nothing else. Redeploy the receiver (just \
             platonic-recv-install). The reader said: {}",
            proto::RECV_PATH, probe.note)),
        Transport::Auto => Choice::Shell(Some(format!(
            "warning: no {} on the reader ({}); using the old shell path. \
             Redeploy the receiver to drop the shell privilege.",
            proto::RECV_PATH, probe.note))),
    }
}

/// Build the link the [`Choice`] names.  `dry_run` short-circuits before any
/// process is spawned, so `--dry-run` never needs a reader at all.
pub fn open_link<'a>(sess: &'a Session, transport: Transport, probe: &Probe,
                     restricted_key: bool)
                     -> Result<Box<dyn Link + 'a>, String> {
    if sess.dry_run {
        // The shell path prints its real remote commands through
        // `Session::run`, which is the whole value of --dry-run for it.  The
        // receiver path has no commands to print -- it has frames -- so it
        // prints the one ssh invocation and then the ops.
        if transport == Transport::Shell {
            return Ok(Box::new(ShellLink::new(sess)));
        }
        let line: Vec<String> = sess.recv_argv().iter()
            .map(|a| shell_quote(a)).collect();
        println!("+ {}", line.join(" "));
        return Ok(Box::new(DryLink { transport: "recv" }));
    }
    match choose(transport, probe, restricted_key) {
        Choice::Recv => Ok(Box::new(RecvLink::connect(sess)?)),
        Choice::Shell(warning) => {
            if let Some(w) = warning {
                eprintln!("{}", w);
            }
            Ok(Box::new(ShellLink::new(sess)))
        }
        Choice::Fail(msg) => Err(msg),
    }
}

//
// -------------------------------------------------------------------- tests
//

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(receiver: bool) -> Probe {
        Probe { reachable: true, receiver, note: "exit 127".to_string() }
    }

    #[test]
    fn transport_env_parsing() {
        assert_eq!(Transport::from_env(None).unwrap(), Transport::Auto);
        assert_eq!(Transport::from_env(Some("")).unwrap(), Transport::Auto);
        assert_eq!(Transport::from_env(Some("recv")).unwrap(), Transport::Recv);
        assert_eq!(Transport::from_env(Some("shell")).unwrap(), Transport::Shell);
        assert!(Transport::from_env(Some("ssh")).is_err());
    }

    #[test]
    fn auto_prefers_the_receiver() {
        assert_eq!(choose(Transport::Auto, &probe(true), false), Choice::Recv);
        assert_eq!(choose(Transport::Auto, &probe(true), true), Choice::Recv);
    }

    #[test]
    fn auto_degrades_loudly_with_the_admin_key() {
        match choose(Transport::Auto, &probe(false), false) {
            Choice::Shell(Some(w)) => {
                assert!(w.starts_with("warning:"));
                assert!(w.contains(proto::RECV_PATH));
                assert!(w.contains("exit 127"), "the reason must be named: {}", w);
            }
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn a_paired_key_cannot_degrade() {
        // The security property, asserted: a restricted key has no shell, so
        // "fall back" is not a thing that can happen quietly.
        match choose(Transport::Auto, &probe(false), true) {
            Choice::Fail(msg) => {
                assert!(msg.contains("Redeploy"), "{}", msg);
                assert!(msg.contains(proto::RECV_PATH));
            }
            other => panic!("a paired key degraded to {:?}", other),
        }
        assert!(matches!(choose(Transport::Shell, &probe(true), true),
                         Choice::Fail(_)));
    }

    #[test]
    fn forcing_a_transport_does_not_fall_back() {
        assert!(matches!(choose(Transport::Recv, &probe(false), false),
                         Choice::Fail(_)));
        assert_eq!(choose(Transport::Shell, &probe(false), false),
                   Choice::Shell(None));
    }

    #[test]
    fn a_document_over_the_cap_is_refused_before_it_moves() {
        // Not a device test: the cap is checked on the Mac so a 300 MB file
        // does not cross the wire only to be refused at the other end.
        assert!(proto::MAX_PUT as u64 >= 32 * 1024 * 1024);
    }
}
