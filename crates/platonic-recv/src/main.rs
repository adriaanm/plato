//! The receiver's entry point: refuse a tty, greet, serve stdin, exit.
//!
//! Deliberately thin.  Everything worth testing is in the library half, and
//! everything worth arguing about is in `proto.rs`.
//!
//! **`$SSH_ORIGINAL_COMMAND` is not read here, and must never be** (Adriaan,
//! 2026-08-12).  The client's requested command string survives the forced
//! command as that variable; parsing it would put a client-controlled string
//! back into a decision made by a root program.  Grep for it: there should be
//! no hit anywhere in this crate.

use std::io::{self, IsTerminal};
use std::process::ExitCode;

use platonic_recv::proto::{FIFO_PATH, LIBRARY_ROOT};
use platonic_recv::server::{Config, Session};

fn main() -> ExitCode {
    // `no-pty` is in every entry pairing writes, so this cannot happen through
    // the front door -- which is the reason to check.  A tty here means the
    // binary was run by hand or the restriction is not what we think it is,
    // and in both cases a framed protocol on a terminal is nonsense.
    if io::stdin().is_terminal() {
        eprintln!("platonic-recv: stdin is a terminal. This program speaks a \
                   framed protocol and is meant to be run as an ssh forced \
                   command, not by hand.");
        return ExitCode::from(2);
    }

    let cfg = match Config::device() {
        Ok(cfg) => cfg,
        Err(e) => {
            // The library root is on the userstore, which goes away (E10).
            // Say which path failed: this is the one message a person reads.
            eprintln!("platonic-recv: cannot use {}: {} — is the userstore \
                       mounted?", LIBRARY_ROOT, e);
            return ExitCode::from(3);
        }
    };
    let _ = FIFO_PATH; // the FIFO is Config::device()'s business, not main's

    let stdin = io::stdin();
    let stdout = io::stdout();
    match Session::new(cfg, stdin.lock(), stdout.lock()).run() {
        Ok(()) => ExitCode::SUCCESS,
        // A protocol error has already been reported to the client and logged;
        // the exit code is for whoever reads dropbear's log.
        Err(e) => {
            eprintln!("platonic-recv: session ended: {}", e);
            ExitCode::from(1)
        }
    }
}
