//! The remote-control seam (ezkindle `docs/platonic.md`): a thread blocks
//! reading a FIFO and maps each line to an event, the same shape as the input
//! threads. Two verbs: `import` re-scans the library, `open <path>` re-scans
//! and then opens the document exactly as a tap on its row would.

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
    fn rejects_unknown_and_empty() {
        assert_eq!(parse_command(""), None);
        assert_eq!(parse_command("   "), None);
        assert_eq!(parse_command("open"), None);
        assert_eq!(parse_command("open   "), None);
        assert_eq!(parse_command("imported"), None);
        assert_eq!(parse_command("frobnicate /a/b"), None);
    }
}
