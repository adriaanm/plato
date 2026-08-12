//! The receiver's behaviour: one request at a time, on stdin, as root.
//!
//! It lives in the library rather than in `main.rs` so the PUT/LIST/SWEEP
//! tests can drive it against a temp directory acting as the library root --
//! the whole of it is exercised on the host, with no device.
//!
//! Two rules run through everything below.
//!
//! **Validate on the device.** The Mac may call [`proto::validate_component`]
//! first, and does, but that is a courtesy to the user; the check that counts
//! is here, because this process is root and the client is a string.
//!
//! **Never desynchronise the stream.** A refusal still has to consume the
//! payload it refused, or the next request starts reading a document.  That is
//! why [`Session::put`] drains before it answers.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::proto::*;

/// `O_NONBLOCK`, so opening the FIFO for writing fails with `ENXIO` when
/// nothing is reading it instead of blocking forever.  That failure IS the
/// "Plato is not running" signal -- no timeout, no thread, no libc.
#[cfg(target_os = "linux")]
const O_NONBLOCK: i32 = 0o4000;
#[cfg(not(target_os = "linux"))]
const O_NONBLOCK: i32 = 0x0004;

pub struct Config {
    /// Canonical. Every resolved path is asserted to be inside it.
    pub root: PathBuf,
    pub fifo: PathBuf,
    pub sweep_folder: String,
}

impl Config {
    /// The device configuration.  The root is canonicalised once, here, so the
    /// containment assertion later compares two resolved paths.
    pub fn device() -> io::Result<Config> {
        Config::at(Path::new(LIBRARY_ROOT), Path::new(FIFO_PATH))
    }

    pub fn at(root: &Path, fifo: &Path) -> io::Result<Config> {
        Ok(Config {
            root: root.canonicalize()?,
            fifo: fifo.to_path_buf(),
            sweep_folder: SWEEP_FOLDER.to_string(),
        })
    }
}

fn refuse(status: Status, message: impl Into<String>) -> Response {
    Response::Err { status, message: message.into() }
}

fn epoch_secs(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

fn system_time(secs: i64) -> SystemTime {
    if secs >= 0 {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs())
    }
}

pub struct Session<R: Read, W: Write> {
    cfg: Config,
    input: R,
    output: W,
}

impl<R: Read, W: Write> Session<R, W> {
    pub fn new(cfg: Config, input: R, output: W) -> Session<R, W> {
        Session { cfg, input, output }
    }

    /// Greet, then serve until the client sends [`Request::Quit`] or closes
    /// stdin.  A protocol error ends the session: once the framing is in
    /// doubt, the next byte cannot be trusted to be an op.
    pub fn run(&mut self) -> io::Result<()> {
        self.output.write_all(GREETING)?;
        self.output.flush()?;
        loop {
            let req = match read_request(&mut self.input) {
                Ok(None) => return Ok(()),
                Ok(Some(req)) => req,
                Err(e) => {
                    log(&format!("protocol error: {}", e));
                    let _ = write_response(&mut self.output,
                                           &refuse(Status::Unsupported,
                                                   format!("protocol error: {}", e)));
                    return Err(e);
                }
            };
            if req == Request::Quit {
                write_response(&mut self.output, &Response::Done)?;
                return Ok(());
            }
            let resp = self.dispatch(req);
            if let Response::Err { status, message } = &resp {
                log(&format!("refused ({:?}): {}", status, message));
            }
            write_response(&mut self.output, &resp)?;
        }
    }

    fn dispatch(&mut self, req: Request) -> Response {
        match req {
            Request::Put { folder, filename, mtime, length } =>
                self.put(&folder, &filename, mtime, length),
            Request::Open { folder, filename } => self.open(&folder, &filename),
            Request::Import => self.fifo_line("import"),
            Request::List => self.list(),
            Request::Sweep { cutoff } => self.sweep(cutoff),
            Request::Quit => Response::Done,
        }
    }

    //
    // ------------------------------------------------------------------ PUT
    //

    fn put(&mut self, folder: &str, filename: &str, mtime: i64, length: u32)
           -> Response {
        // Resolve first, THEN drain: the payload has to leave the stream
        // whatever the answer is, or the next op byte is document content.
        let target = self.put_target(folder, filename);
        match target {
            Ok(path) => match self.write_payload(&path, length, mtime) {
                Ok(written) => {
                    log(&format!("PUT {}/{} {} bytes", folder, filename, written));
                    Response::Put { written }
                }
                Err(e) => refuse(Status::Io, format!("write failed: {}", e)),
            },
            Err(resp) => {
                if self.drain(length).is_err() {
                    // The stream is now unusable; say so rather than answering
                    // a question we can no longer ask the next one about.
                    return refuse(Status::Io, "payload could not be drained");
                }
                resp
            }
        }
    }

    fn put_target(&self, folder: &str, filename: &str) -> Result<PathBuf, Response> {
        if let Err(e) = validate_component("folder", folder) {
            return Err(refuse(Status::Invalid,
                              format!("{} {}", e, quote_for_log(folder))));
        }
        if let Err(e) = validate_component("filename", filename) {
            return Err(refuse(Status::Invalid,
                              format!("{} {}", e, quote_for_log(filename))));
        }
        let dir = self.root_join(folder, true)?;
        let path = dir.join(filename);
        // The folder is canonical and the filename holds no separator, so the
        // only remaining way out of the root is the target itself being a
        // symlink.  Refuse rather than follow it.
        if let Ok(meta) = fs::symlink_metadata(&path) {
            if meta.file_type().is_symlink() {
                return Err(refuse(Status::Invalid,
                                  "target exists and is a symlink"));
            }
        }
        Ok(path)
    }

    /// Create (if asked) and canonicalise `<root>/<folder>`, then assert the
    /// result is inside the root.  This is the containment check: a symlinked
    /// folder, or any encoding trick that survived validation, dies here.
    fn root_join(&self, folder: &str, create: bool) -> Result<PathBuf, Response> {
        let dir = self.cfg.root.join(folder);
        if create {
            if let Err(e) = fs::create_dir_all(&dir) {
                return Err(refuse(Status::Io, format!("mkdir failed: {}", e)));
            }
        }
        let canon = match dir.canonicalize() {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::NotFound =>
                return Err(refuse(Status::NotFound, "no such folder")),
            Err(e) => return Err(refuse(Status::Io, format!("{}", e))),
        };
        if !canon.starts_with(&self.cfg.root) || canon == self.cfg.root {
            // Deliberately vague to the client, loud in the log: the resolved
            // path is not the client's business, and it is the one string that
            // could name something outside the library.
            log(&format!("ESCAPE refused: folder {} resolved outside the root",
                         quote_for_log(folder)));
            return Err(refuse(Status::Invalid, "folder resolves outside the library"));
        }
        Ok(canon)
    }

    fn write_payload(&mut self, path: &Path, length: u32, mtime: i64)
                     -> io::Result<u64> {
        let mut file = File::create(path)?;
        let mut left = length as u64;
        let mut buf = vec![0u8; 64 * 1024];
        let mut written = 0u64;
        while left > 0 {
            let want = buf.len().min(left as usize);
            // read_exact, not read: a short stream must be an error, never a
            // silently truncated document.
            self.input.read_exact(&mut buf[..want])?;
            file.write_all(&buf[..want])?;
            written += want as u64;
            left -= want as u64;
        }
        file.flush()?;
        // The mtime is the Mac's clock; expiry is decided against it, so it is
        // as much a part of the document as the bytes.
        file.set_times(fs::FileTimes::new().set_modified(system_time(mtime)))?;
        file.sync_all()?;
        Ok(written)
    }

    fn drain(&mut self, length: u32) -> io::Result<()> {
        let mut left = length as u64;
        let mut buf = vec![0u8; 64 * 1024];
        while left > 0 {
            let want = buf.len().min(left as usize);
            self.input.read_exact(&mut buf[..want])?;
            left -= want as u64;
        }
        Ok(())
    }

    //
    // --------------------------------------------------------- OPEN / IMPORT
    //

    /// The Mac names a folder and a filename; the **receiver** builds the
    /// path.  An absolute path never crosses the wire, so there is nothing to
    /// validate about one.
    fn open(&mut self, folder: &str, filename: &str) -> Response {
        if let Err(e) = validate_component("folder", folder) {
            return refuse(Status::Invalid, format!("{} {}", e, quote_for_log(folder)));
        }
        if let Err(e) = validate_component("filename", filename) {
            return refuse(Status::Invalid,
                          format!("{} {}", e, quote_for_log(filename)));
        }
        let dir = match self.root_join(folder, false) {
            Ok(dir) => dir,
            Err(resp) => return resp,
        };
        let path = dir.join(filename);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_file() => {}
            Ok(_) => return refuse(Status::Invalid, "not a regular file"),
            Err(_) => return refuse(Status::NotFound, "no such document"),
        }
        let line = format!("open {}", path.display());
        self.fifo_line(&line)
    }

    /// One line into Plato's command FIFO.  The FIFO is created by `plato.sh`;
    /// a receiver that created it would be a receiver that silently succeeds
    /// at writing to a path nothing reads.
    fn fifo_line(&mut self, line: &str) -> Response {
        let meta = match fs::metadata(&self.cfg.fifo) {
            Ok(m) => m,
            Err(_) => return refuse(Status::NotFound,
                                    "Plato is not running its command listener"),
        };
        if !is_fifo(&meta) {
            return refuse(Status::Io, "the command path is not a FIFO");
        }
        match open_fifo_nonblocking(&self.cfg.fifo) {
            Ok(mut f) => match writeln!(f, "{}", line).and_then(|_| f.flush()) {
                Ok(()) => Response::Done,
                Err(e) => refuse(Status::Io, format!("FIFO write failed: {}", e)),
            },
            // ENXIO: opened for writing with no reader.  Exactly the case the
            // shell path had to spend a `timeout` to discover.
            Err(_) => refuse(Status::NotFound,
                             "Plato is not reading the command FIFO"),
        }
    }

    //
    // ------------------------------------------------------------ LIST/SWEEP
    //

    fn list(&mut self) -> Response {
        let mut entries = Vec::new();
        let dirs = match fs::read_dir(&self.cfg.root) {
            Ok(d) => d,
            Err(e) => return refuse(Status::Io, format!("{}", e)),
        };
        let mut folders: Vec<(String, PathBuf)> = Vec::new();
        for entry in dirs.flatten() {
            // file_type does not follow symlinks, so a symlinked directory is
            // skipped rather than walked out of the root.
            let Ok(ft) = entry.file_type() else { continue };
            if !ft.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            folders.push((name, entry.path()));
        }
        folders.sort_by(|a, b| a.0.cmp(&b.0));
        for (folder, path) in folders {
            let Ok(files) = fs::read_dir(&path) else { continue };
            for entry in files.flatten() {
                let Ok(ft) = entry.file_type() else { continue };
                if !ft.is_file() {
                    continue;
                }
                let Ok(meta) = entry.metadata() else { continue };
                entries.push(Entry {
                    folder: folder.clone(),
                    name: entry.file_name().to_string_lossy().into_owned(),
                    size: meta.len(),
                    mtime: meta.modified().map(epoch_secs).unwrap_or(0),
                });
                if entries.len() >= MAX_ENTRIES as usize {
                    return Response::List(entries);
                }
            }
        }
        Response::List(entries)
    }

    /// Hard-wired to `inbox/`: the folder is not a parameter, because the
    /// destination is what decides the lifetime and a named folder means
    /// "keep this".
    fn sweep(&mut self, cutoff: i64) -> Response {
        let dir = self.cfg.root.join(&self.cfg.sweep_folder);
        let files = match fs::read_dir(&dir) {
            Ok(f) => f,
            // No inbox yet is not an error; it is a device nothing has been
            // pushed to.
            Err(_) => return Response::Swept(Vec::new()),
        };
        let mut removed = Vec::new();
        for entry in files.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if !ft.is_file() {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let Ok(modified) = meta.modified() else { continue };
            if epoch_secs(modified) > cutoff {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            match fs::remove_file(entry.path()) {
                Ok(()) => removed.push(name),
                Err(e) => log(&format!("sweep: {} {}", quote_for_log(&name), e)),
            }
            if removed.len() >= MAX_ENTRIES as usize {
                break;
            }
        }
        Response::Swept(removed)
    }
}

//
// ------------------------------------------------------------------ platform
//

fn is_fifo(meta: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        meta.file_type().is_fifo()
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        false
    }
}

fn open_fifo_nonblocking(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .write(true)
            .custom_flags(O_NONBLOCK)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        fs::OpenOptions::new().write(true).open(path)
    }
}

/// Stderr goes back to the client and into dropbear's log.  Refusals are
/// logged with the offending input escaped, and resolved absolute paths are
/// never sent to the client -- only logged.
fn log(msg: &str) {
    eprintln!("platonic-recv: {}", msg);
}

//
// --------------------------------------------------------------------- tests
//

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch library root.  No tempfile crate: this crate is
    /// dependency-free and the need is one directory.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let base = std::env::temp_dir().join(format!(
                "platonic-recv-{}-{}-{}", tag, std::process::id(),
                SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
            fs::create_dir_all(&base).unwrap();
            Scratch(base)
        }
        fn cfg(&self) -> Config {
            Config::at(&self.0, &self.0.join("no-such-fifo")).unwrap()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Drive a whole session: encode the requests, run the server over them,
    /// decode the answers.  This is the client's code path too, which is the
    /// point of sharing the module.
    fn converse(cfg: Config, script: Vec<(Request, Vec<u8>)>) -> Vec<Response> {
        let mut input = Vec::new();
        let ops: Vec<Op> = script.iter().map(|(r, _)| r.op()).collect();
        for (req, payload) in &script {
            write_request(&mut input, req).unwrap();
            input.extend_from_slice(payload);
        }
        let mut output = Vec::new();
        let _ = Session::new(cfg, &input[..], &mut output).run();
        assert!(output.starts_with(GREETING), "no greeting");
        let mut cur = &output[GREETING.len()..];
        let mut responses = Vec::new();
        for op in ops {
            match read_response(&mut cur, op) {
                Ok(resp) => responses.push(resp),
                Err(_) => break,
            }
        }
        responses
    }

    fn put(folder: &str, name: &str, body: &[u8], mtime: i64) -> (Request, Vec<u8>) {
        (Request::Put {
            folder: folder.into(), filename: name.into(),
            mtime, length: body.len() as u32,
        }, body.to_vec())
    }

    #[test]
    fn put_writes_the_bytes_and_the_mtime() {
        let scratch = Scratch::new("put");
        let resp = converse(scratch.cfg(),
                            vec![put("inbox", "a.md", b"# hi\n", 1_700_000_000)]);
        assert_eq!(resp, vec![Response::Put { written: 5 }]);
        let path = scratch.0.join("inbox/a.md");
        assert_eq!(fs::read(&path).unwrap(), b"# hi\n");
        let meta = fs::metadata(&path).unwrap();
        assert_eq!(epoch_secs(meta.modified().unwrap()), 1_700_000_000);
    }

    #[test]
    fn a_zero_length_file_is_a_file() {
        let scratch = Scratch::new("empty");
        let resp = converse(scratch.cfg(), vec![put("inbox", "empty.md", b"", 0)]);
        assert_eq!(resp, vec![Response::Put { written: 0 }]);
        assert!(scratch.0.join("inbox/empty.md").is_file());
    }

    #[test]
    fn a_refused_put_still_consumes_its_payload() {
        // The desync test: a bad name followed by a good one.  If the refusal
        // did not drain, the second request would be read out of the first
        // one's bytes and the session would fall apart.
        let scratch = Scratch::new("drain");
        let resp = converse(scratch.cfg(), vec![
            put("../etc", "passwd", b"XXXXXXXXXX", 0),
            put("inbox", "good.md", b"ok\n", 0),
        ]);
        assert_eq!(resp.len(), 2);
        assert!(matches!(resp[0], Response::Err { status: Status::Invalid, .. }));
        assert_eq!(resp[1], Response::Put { written: 3 });
        assert_eq!(fs::read(scratch.0.join("inbox/good.md")).unwrap(), b"ok\n");
    }

    #[test]
    fn hostile_folders_and_filenames_are_refused_and_write_nothing() {
        let scratch = Scratch::new("hostile");
        let outside = scratch.0.join("outside.md");
        for (folder, name) in [
            ("..", "outside.md"), ("../..", "x.md"), ("/etc", "passwd"),
            (".", "x.md"), ("", "x.md"), ("inbox", ""), ("inbox", "../x.md"),
            ("inbox", "a\0b.md"), ("inbox", "a\nb.md"), ("inbox", "-rf"),
            ("inbox", "a/b.md"), ("in\0box", "x.md"),
        ] {
            let resp = converse(scratch.cfg(), vec![put(folder, name, b"pwned", 0)]);
            assert!(matches!(resp[0], Response::Err { status: Status::Invalid, .. }),
                    "{:?}/{:?} was not refused: {:?}", folder, name, resp[0]);
        }
        assert!(!outside.exists());
        assert!(!scratch.0.parent().unwrap().join("x.md").exists());
    }

    #[test]
    fn a_folder_symlinked_out_of_the_root_is_refused() {
        // Validation alone cannot catch this -- "escape" is a perfectly legal
        // name.  The containment assertion after canonicalize is what does.
        let scratch = Scratch::new("symlink");
        let elsewhere = Scratch::new("elsewhere");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&elsewhere.0, scratch.0.join("escape")).unwrap();
        let resp = converse(scratch.cfg(), vec![put("escape", "x.md", b"pwned", 0)]);
        assert!(matches!(resp[0], Response::Err { status: Status::Invalid, .. }),
                "{:?}", resp[0]);
        assert!(!elsewhere.0.join("x.md").exists());
    }

    #[test]
    fn a_file_symlinked_out_of_the_root_is_refused() {
        let scratch = Scratch::new("filelink");
        let elsewhere = Scratch::new("filelink-target");
        fs::create_dir_all(scratch.0.join("inbox")).unwrap();
        let victim = elsewhere.0.join("victim.md");
        fs::write(&victim, b"original\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&victim, scratch.0.join("inbox/x.md")).unwrap();
        let resp = converse(scratch.cfg(), vec![put("inbox", "x.md", b"pwned", 0)]);
        assert!(matches!(resp[0], Response::Err { status: Status::Invalid, .. }));
        assert_eq!(fs::read(&victim).unwrap(), b"original\n");
    }

    #[test]
    fn a_truncated_payload_ends_the_session_rather_than_writing_a_short_file() {
        let scratch = Scratch::new("truncated");
        let mut input = Vec::new();
        write_request(&mut input, &Request::Put {
            folder: "inbox".into(), filename: "a.md".into(),
            mtime: 0, length: 100,
        }).unwrap();
        input.extend_from_slice(b"only ten b");
        let mut output = Vec::new();
        let session = Session::new(scratch.cfg(), &input[..], &mut output).run();
        assert!(session.is_err() || output.len() > GREETING.len());
        // Whatever landed on disk, it is not a document the Mac was told
        // arrived: no Ok response was produced for it.
        let mut cur = &output[GREETING.len()..];
        if !cur.is_empty() {
            match read_response(&mut cur, Op::Put) {
                Ok(Response::Put { .. }) => panic!("short write reported as success"),
                _ => {}
            }
        }
    }

    #[test]
    fn list_reports_one_level_with_size_and_mtime() {
        let scratch = Scratch::new("list");
        converse(scratch.cfg(), vec![
            put("inbox", "a.md", b"12345", 1_700_000_000),
            put("papers", "b.pdf", b"abc", 1_700_000_100),
        ]);
        // Not one level down: must not appear.
        fs::create_dir_all(scratch.0.join("inbox/deeper")).unwrap();
        fs::write(scratch.0.join("inbox/deeper/hidden.md"), b"x").unwrap();
        fs::write(scratch.0.join("toplevel.md"), b"x").unwrap();

        let resp = converse(scratch.cfg(), vec![(Request::List, Vec::new())]);
        let Response::List(mut entries) = resp[0].clone() else { panic!("{:?}", resp) };
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(entries, vec![
            Entry { folder: "inbox".into(), name: "a.md".into(),
                    size: 5, mtime: 1_700_000_000 },
            Entry { folder: "papers".into(), name: "b.pdf".into(),
                    size: 3, mtime: 1_700_000_100 },
        ]);
    }

    #[test]
    fn sweep_expires_inbox_only_and_only_past_the_cutoff() {
        let scratch = Scratch::new("sweep");
        converse(scratch.cfg(), vec![
            put("inbox", "old.md", b"o", 1_000),
            put("inbox", "edge.md", b"e", 2_000),
            put("inbox", "fresh.md", b"f", 2_001),
            put("papers", "keep.pdf", b"k", 1),
        ]);
        let resp = converse(scratch.cfg(),
                            vec![(Request::Sweep { cutoff: 2_000 }, Vec::new())]);
        let Response::Swept(mut names) = resp[0].clone() else { panic!("{:?}", resp) };
        names.sort();
        assert_eq!(names, vec!["edge.md".to_string(), "old.md".to_string()]);
        assert!(!scratch.0.join("inbox/old.md").exists());
        assert!(scratch.0.join("inbox/fresh.md").exists());
        // A named folder means "keep this" -- the sweep must not reach it even
        // with a cutoff far in the future.
        assert!(scratch.0.join("papers/keep.pdf").exists());
    }

    #[test]
    fn sweep_on_a_device_with_no_inbox_is_not_an_error() {
        let scratch = Scratch::new("no-inbox");
        let resp = converse(scratch.cfg(),
                            vec![(Request::Sweep { cutoff: i64::MAX }, Vec::new())]);
        assert_eq!(resp[0], Response::Swept(Vec::new()));
    }

    #[test]
    fn open_needs_the_document_to_exist() {
        let scratch = Scratch::new("open-missing");
        fs::create_dir_all(scratch.0.join("inbox")).unwrap();
        let resp = converse(scratch.cfg(), vec![
            (Request::Open { folder: "inbox".into(), filename: "gone.md".into() },
             Vec::new())]);
        assert!(matches!(resp[0], Response::Err { status: Status::NotFound, .. }));
    }

    #[test]
    fn open_and_import_report_a_missing_listener_rather_than_hanging() {
        // The FIFO path in the test config does not exist, which is exactly
        // the "Plato is not running" case.  It must answer, not block.
        let scratch = Scratch::new("no-fifo");
        converse(scratch.cfg(), vec![put("inbox", "a.md", b"x", 0)]);
        let resp = converse(scratch.cfg(), vec![
            (Request::Open { folder: "inbox".into(), filename: "a.md".into() },
             Vec::new()),
            (Request::Import, Vec::new()),
        ]);
        for r in &resp {
            assert!(matches!(r, Response::Err { status: Status::NotFound, .. }),
                    "{:?}", r);
        }
    }

    #[test]
    fn quit_ends_the_session_and_later_bytes_are_ignored() {
        let scratch = Scratch::new("quit");
        let mut input = Vec::new();
        write_request(&mut input, &Request::Quit).unwrap();
        write_request(&mut input, &Request::List).unwrap();
        let mut output = Vec::new();
        Session::new(scratch.cfg(), &input[..], &mut output).run().unwrap();
        assert_eq!(output.len(), GREETING.len() + 1);
    }

    #[test]
    fn an_unknown_op_ends_the_session_with_a_named_refusal() {
        let scratch = Scratch::new("badop");
        let mut output = Vec::new();
        let r = Session::new(scratch.cfg(), &[99u8][..], &mut output).run();
        assert!(r.is_err());
        let resp = read_response(&mut &output[GREETING.len()..], Op::List).unwrap();
        assert!(matches!(resp, Response::Err { status: Status::Unsupported, .. }));
    }

    #[test]
    fn a_full_push_is_one_session() {
        // What `platonic docs/plan.md` actually sends.
        let scratch = Scratch::new("push");
        let resp = converse(scratch.cfg(), vec![
            put("inbox", "platokin-plan.md", b"# Plan\n", 1_786_527_005),
            (Request::Open { folder: "inbox".into(),
                             filename: "platokin-plan.md".into() }, Vec::new()),
            (Request::Sweep { cutoff: 1_785_317_405 }, Vec::new()),
            (Request::Quit, Vec::new()),
        ]);
        assert_eq!(resp[0], Response::Put { written: 7 });
        assert!(matches!(resp[1], Response::Err { status: Status::NotFound, .. }),
                "no FIFO in the test config");
        assert_eq!(resp[2], Response::Swept(Vec::new()));
        assert_eq!(resp[3], Response::Done);
    }
}
