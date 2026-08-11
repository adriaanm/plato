//! folder_hub -- serve one folder, read-only, to a Plato device on the LAN.
//!
//! Runs on the computer that holds the documents.  Two sockets: a UDP one that
//! answers discovery probes so the device never has to be told an IP address,
//! and a TCP one serving exactly two routes.
//!
//!     GET /manifest              the file table, plus the hub's clock
//!     GET /file/<encoded path>   the bytes
//!
//! Usage:
//!     folder_hub --root ~/Documents/reader [--port 8571]
//!                [--disco-port 30303] [--token SECRET]
//!
//! There is no write path and no directory traversal: a request is served only
//! if its resolved path is still inside `--root` after canonicalization, and
//! only if it is a regular file that appears in the manifest's own filter.

use std::env;
use std::fs::{self, File};
use std::io::{self, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process;
use std::thread;
use std::time::UNIX_EPOCH;

use foldersync::*;

/// Extensions Plato can open.  Everything else in the folder is ignored, so
/// the folder can also hold notes, `.DS_Store` and whatever else accumulates.
const KINDS: &[&str] = &["epub", "pdf", "cbz", "djvu", "fb2", "xps", "mobi", "txt", "html", "md"];

struct Config {
    root: PathBuf,
    port: u16,
    disco_port: u16,
    token: String,
}

fn main() {
    let config = match parse_args() {
        Ok(config) => config,
        Err(message) => {
            eprintln!("{}", message);
            process::exit(1);
        },
    };

    let listener = match TcpListener::bind((Ipv4Addr::UNSPECIFIED, config.port)) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("can't bind port {}: {}", config.port, e);
            process::exit(1);
        },
    };

    // List the root before announcing anything.  On macOS a launchd agent
    // reading a TCC-protected directory (~/Documents, ~/Desktop, ~/Downloads)
    // does not fail -- it *blocks*, and the only symptom is that every request
    // times out with the server apparently healthy.  Doing the read here means
    // a log that stops before "serving" names the problem.
    match fs::read_dir(&config.root) {
        Ok(listing) => println!("{} entries in {}",
                                listing.count(), config.root.display()),
        Err(e) => {
            eprintln!("can't read {}: {}", config.root.display(), e);
            process::exit(1);
        },
    }

    println!("serving {} on port {}, discovery on {}",
             config.root.display(), config.port, config.disco_port);

    {
        let (disco_port, port, token) = (config.disco_port, config.port, config.token.clone());
        thread::spawn(move || {
            if let Err(e) = serve_discovery(disco_port, port, token) {
                eprintln!("discovery died: {}", e);
            }
        });
    }

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let root = config.root.clone();
                let token = config.token.clone();
                thread::spawn(move || {
                    if let Err(e) = handle(stream, &root, &token) {
                        // A device that walks away mid-transfer is normal --
                        // Plato SIGTERMs the fetcher when the user leaves the
                        // folder -- so this is information, not an error.
                        eprintln!("connection: {}", e);
                    }
                });
            },
            Err(e) => eprintln!("accept: {}", e),
        }
    }
}

fn parse_args() -> Result<Config, String> {
    let mut root: Option<PathBuf> = None;
    let mut port = DEFAULT_HTTP_PORT;
    let mut disco_port = DEFAULT_DISCO_PORT;
    let mut token = String::new();

    let args: Vec<String> = env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let value = || args.get(i + 1).cloned()
                         .ok_or_else(|| format!("{} needs a value", args[i]));
        match args[i].as_str() {
            "--root" => root = Some(PathBuf::from(value()?)),
            "--port" => port = value()?.parse().map_err(|_| "bad --port".to_string())?,
            "--disco-port" => disco_port = value()?.parse()
                                                   .map_err(|_| "bad --disco-port".to_string())?,
            "--token" => token = value()?,
            "--help" | "-h" => return Err(usage()),
            other => return Err(format!("unknown option {}\n\n{}", other, usage())),
        }
        i += 2;
    }

    let root = root.ok_or_else(|| format!("--root is required\n\n{}", usage()))?;
    let root = root.canonicalize()
                   .map_err(|e| format!("--root {}: {}", root.display(), e))?;
    if !root.is_dir() {
        return Err(format!("--root {} is not a directory", root.display()));
    }

    Ok(Config { root, port, disco_port, token })
}

fn usage() -> String {
    "usage: folder_hub --root DIR [--port 8571] [--disco-port 30303] [--token SECRET]"
        .to_string()
}

fn handle(mut stream: TcpStream, root: &Path, token: &str) -> io::Result<()> {
    let (target, given) = read_request(&stream)?;

    if !token.is_empty() && given != token {
        return write_status(&mut stream, 403, "Forbidden");
    }

    if target == "/manifest" {
        let body = manifest(root);
        write_body_header(&mut stream, "text/plain; charset=utf-8", body.len() as u64)?;
        return stream.write_all(body.as_bytes());
    }

    if let Some(encoded) = target.strip_prefix("/file/") {
        return serve_file(&mut stream, root, &percent_decode(encoded));
    }

    write_status(&mut stream, 404, "Not Found")
}

fn serve_file(stream: &mut TcpStream, root: &Path, relative: &str) -> io::Result<()> {
    let candidate = root.join(relative);

    // Canonicalize, then require the result to still be under root.  This is
    // the whole traversal defence -- it also covers symlinks pointing out of
    // the folder, which a prefix check on the unresolved path would not.
    let path = match candidate.canonicalize() {
        Ok(path) if path.starts_with(root) && path.is_file() => path,
        _ => return write_status(stream, 404, "Not Found"),
    };

    if !is_document(&path) {
        return write_status(stream, 404, "Not Found");
    }

    let mut file = File::open(&path)?;
    let len = file.metadata()?.len();
    write_body_header(stream, "application/octet-stream", len)?;
    io::copy(&mut file, stream).map(|_| ())
}

fn manifest(root: &Path) -> String {
    let mut body = format!("#{} {} {}\n", MAGIC, epoch_secs(), utc_stamp());

    let mut entries = Vec::new();
    collect(root, root, &mut entries);
    entries.sort_by(|a, b| a.path.cmp(&b.path));

    for entry in entries {
        body.push_str(&format!("{}\t{}\t{}\n", entry.size, entry.mtime, entry.path));
    }

    body
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<Entry>) {
    let listing = match fs::read_dir(dir) {
        Ok(listing) => listing,
        Err(e) => {
            eprintln!("can't read {}: {}", dir.display(), e);
            return;
        },
    };

    for entry in listing.flatten() {
        let path = entry.path();

        if path.is_dir() {
            collect(root, &path, out);
            continue;
        }

        if !is_document(&path) {
            continue;
        }

        let relative = match path.strip_prefix(root).ok().and_then(Path::to_str) {
            Some(relative) => relative.to_string(),
            None => continue,
        };

        // The manifest is tab-separated and line-oriented, so a filename
        // containing either would corrupt it.  Skip loudly rather than ship a
        // manifest the device will silently mis-parse.
        if relative.contains('\t') || relative.contains('\n') {
            eprintln!("skipping {}: tab or newline in the name", relative);
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(..) => continue,
        };
        let mtime = metadata.modified().ok()
                            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                            .map_or(0, |d| d.as_secs() as i64);

        out.push(Entry { size: metadata.len(), mtime, path: relative });
    }
}

fn is_document(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .map_or(false, |e| KINDS.contains(&e.as_str()))
}

/// **UTC** wall-clock time as `YYYY-MM-DD HH:MM:SS`, which is what busybox
/// `date -u -s` accepts.  Computed by hand: this crate has no chrono, and the
/// device it serves has no correct clock to check the answer against.
///
/// UTC and not local time, learned the hard way on the first device run: the
/// hub sent CEST, the reader was on UTC, and `date -s` -- which interprets its
/// argument in the *reader's* zone -- duly set the clock two hours fast.
/// Sending UTC and setting with `-u` is right whatever either end's zone is.
fn utc_stamp() -> String {
    let (year, month, day, hour, minute, second) = civil_from_epoch(epoch_secs());
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", year, month, day, hour, minute, second)
}

/// Days-from-civil, inverted -- Howard Hinnant's algorithm.  Valid for any
/// date this program will ever see.
fn civil_from_epoch(epoch: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = epoch.div_euclid(86_400);
    let secs = epoch.rem_euclid(86_400);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    (y, m as u32, d as u32, (secs / 3600) as u32, ((secs % 3600) / 60) as u32, (secs % 60) as u32)
}
