//! The remote-control seam (ezkindle `docs/platonic.md`): a thread blocks
//! reading a FIFO and maps each line to an event, the same shape as the input
//! threads.
//!
//! Five verbs. `import` re-scans the library, `open <path>` re-scans and
//! then opens the document exactly as a tap on its row would, and
//! `open-url <url>` opens a web page in the News view's article reader --
//! all three driven by `platonic` from the Mac.
//!
//! `wifi-up [ADDR]` and `wifi-down` are driven by the DEVICE's own WiFi
//! scripts, and exist because the scripts are the chokepoint: every path that
//! changes the radio goes through them, including `just wifi-up` over ssh and
//! an unattended reassociation, neither of which Plato can otherwise see. They
//! carry the mDNS responder's lifecycle (`crate::mdns` in the plato crate).
//! The address is optional and advisory -- the responder re-measures `wlan0`
//! -- so a script that knows the address may pass it and one that does not can
//! stay silent.

use std::env;
use std::fs::File;
use std::ffi::CString;
use std::io::{BufRead, BufReader};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::io::FromRawFd;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::thread;
use crate::view::Event;

pub const DEFAULT_FIFO_PATH: &str = "/tmp/plato.cmd";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Import,
    Open(PathBuf),
    /// `open-url <url>`: show a web page in the News view's article reader.
    /// Driven by `platonic URL` from the Mac; nothing was pushed, so there is
    /// no path and no import -- the reader fetches the page itself.
    OpenUrl(String),
    /// The radio came up (or reassociated, or changed address).
    WifiUp(Option<String>),
    /// The radio is about to go down. Sent BEFORE the teardown, so the
    /// responder's goodbye packets still have a link to leave by.
    WifiDown,
}

/// `/tmp/plato.cmd` on the device; `PLATO_FIFO` overrides it for host runs.
pub fn fifo_path() -> PathBuf {
    env::var_os("PLATO_FIFO").map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_FIFO_PATH))
}

/// The rest of an `open` line is the path verbatim — no quoting, so paths
/// with spaces need no escaping and none is interpreted.
pub fn parse_command(line: &str) -> Option<Command> {
    let line = line.trim();
    if line == "import" {
        return Some(Command::Import);
    }
    if let Some(rest) = line.strip_prefix("open ") {
        let rest = rest.trim();
        if !rest.is_empty() {
            return Some(Command::Open(PathBuf::from(rest)));
        }
    }
    // Before `open`? No -- `open-url ` and `open ` cannot shadow each other,
    // the space after the verb keeps them distinct.  The rest of the line is
    // the URL verbatim, same rule as `open`'s path.
    if let Some(rest) = line.strip_prefix("open-url ") {
        let rest = rest.trim();
        if !rest.is_empty() {
            return Some(Command::OpenUrl(rest.to_string()));
        }
    }
    if line == "wifi-down" {
        return Some(Command::WifiDown);
    }
    // The address is optional: `wifi-up` alone is as valid as `wifi-up 1.2.3.4`,
    // because the responder measures the interface either way.
    if line == "wifi-up" {
        return Some(Command::WifiUp(None));
    }
    if let Some(rest) = line.strip_prefix("wifi-up ") {
        let rest = rest.trim();
        return Some(Command::WifiUp(
            if rest.is_empty() { None } else { Some(rest.to_string()) }));
    }
    None
}

/// Spawn the listener. Creates the FIFO if the path is absent (plato.sh also
/// creates it, belt and braces), but refuses to start if something that is
/// not a FIFO already sits there — a pipe nothing reads is the silent failure
/// mode, so say so and touch nothing.
pub fn spawn_fifo_listener(path: PathBuf, tx: Sender<Event>) {
    match path.metadata() {
        Ok(md) => {
            if !md.file_type().is_fifo() {
                eprintln!("Can't listen on {}: exists and is not a FIFO.", path.display());
                return;
            }
        },
        Err(..) => {
            let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
            let ret = unsafe { libc::mkfifo(c_path.as_ptr(), 0o622) };
            if ret != 0 {
                eprintln!("Can't create FIFO {}: {}.",
                          path.display(), std::io::Error::last_os_error());
                return;
            }
        },
    }

    thread::spawn(move || {
        // O_RDWR: holding a write end ourselves means readline never sees EOF
        // when a writer closes, so there is no reopen loop.
        let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDWR) };
        if fd < 0 {
            eprintln!("Can't open FIFO {}: {}.",
                      path.display(), std::io::Error::last_os_error());
            return;
        }
        let file = unsafe { File::from_raw_fd(fd) };
        for line in BufReader::new(file).lines() {
            let Ok(line) = line else { break };
            match parse_command(&line) {
                Some(Command::Import) => { tx.send(Event::ImportLibrary).ok(); },
                Some(Command::Open(path)) => { tx.send(Event::OpenByPath(path)).ok(); },
                Some(Command::OpenUrl(url)) => { tx.send(Event::OpenUrl(url)).ok(); },
                Some(Command::WifiUp(addr)) => { tx.send(Event::WifiUp(addr)).ok(); },
                Some(Command::WifiDown) => { tx.send(Event::WifiDown).ok(); },
                None => {
                    if !line.trim().is_empty() {
                        eprintln!("Unknown command on {}: {}.", path.display(), line.trim());
                    }
                },
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_import() {
        assert_eq!(parse_command("import"), Some(Command::Import));
        assert_eq!(parse_command("  import\n"), Some(Command::Import));
    }

    #[test]
    fn parse_open_with_spaces_unquoted() {
        assert_eq!(parse_command("open /mnt/us/documents/inbox/Auth Rework - Final.pdf"),
                   Some(Command::Open(PathBuf::from("/mnt/us/documents/inbox/Auth Rework - Final.pdf"))));
    }

    #[test]
    fn parse_open_relative() {
        assert_eq!(parse_command("open inbox/foo.md\n"),
                   Some(Command::Open(PathBuf::from("inbox/foo.md"))));
    }

    #[test]
    fn parse_open_url() {
        assert_eq!(parse_command("open-url https://example.com/essay?a=1&b=2\n"),
                   Some(Command::OpenUrl("https://example.com/essay?a=1&b=2".to_string())));
        // A bare verb points at nothing, same as a bare `open`.
        assert_eq!(parse_command("open-url"), None);
        assert_eq!(parse_command("open-url   "), None);
    }

    #[test]
    fn open_and_open_url_do_not_shadow_each_other() {
        // A path that merely mentions a scheme is still a path, and a URL is
        // never mistaken for a document named `-url ...`.
        assert_eq!(parse_command("open /mnt/us/documents/https-notes.md"),
                   Some(Command::Open(PathBuf::from("/mnt/us/documents/https-notes.md"))));
        assert_eq!(parse_command("open-url https://a.b/c"),
                   Some(Command::OpenUrl("https://a.b/c".to_string())));
    }

    #[test]
    fn parse_wifi_verbs() {
        assert_eq!(parse_command("wifi-down"), Some(Command::WifiDown));
        assert_eq!(parse_command(" wifi-down\n"), Some(Command::WifiDown));
        assert_eq!(parse_command("wifi-up"), Some(Command::WifiUp(None)));
        assert_eq!(parse_command("wifi-up\n"), Some(Command::WifiUp(None)));
        assert_eq!(parse_command("wifi-up 192.168.178.190"),
                   Some(Command::WifiUp(Some("192.168.178.190".to_string()))));
        // A script that word-splits to nothing must not become a different
        // verb: `wifi-up $ADDR` with ADDR unset is still a plain wifi-up.
        assert_eq!(parse_command("wifi-up   "), Some(Command::WifiUp(None)));
    }

    #[test]
    fn wifi_verbs_are_not_confused_with_their_prefixes() {
        assert_eq!(parse_command("wifi"), None);
        assert_eq!(parse_command("wifi-upgrade"), None);
        assert_eq!(parse_command("wifi-downgrade"), None);
    }

    #[test]
    fn rejects_unknown_and_empty() {
        assert_eq!(parse_command(""), None);
        assert_eq!(parse_command("   "), None);
        assert_eq!(parse_command("open"), None);
        assert_eq!(parse_command("open   "), None);
        assert_eq!(parse_command("imported"), None);
        assert_eq!(parse_command("frobnicate /a/b"), None);
    }
}
