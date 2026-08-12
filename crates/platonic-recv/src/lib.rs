//! `platonic-recv` — the one program a paired Mac may run on the reader.
//!
//! A paired key's `authorized_keys` entry carries a forced command
//! ([E22](https://github.com/adriaanm/platokin) in platokin
//! `docs/experiments.md`, Confirmed on this dropbear):
//!
//! ```text
//! command="/var/local/ezssh/platonic-recv",no-port-forwarding,no-agent-forwarding,no-X11-forwarding,no-pty ssh-ed25519 AAAA… platonic@mac
//! ```
//!
//! So this binary is the **entire** API surface a paired Mac has.  Anything it
//! cannot do would have to come back as shell access, which is the thing being
//! removed — which is why the op set is exactly what `platonic` does, no more
//! and no less.
//!
//! The crate is split so both ends can use what they need:
//!
//! * [`proto`] — the wire types, the framing and the validation rules.  The
//!   `platonic` CLI links this crate for it, so a field cannot be added to one
//!   side only.  Same reasoning as `crates/pairing`; a separate `-proto` crate
//!   would be a third `Cargo.toml` for one module, and the validation belongs
//!   next to the types it validates.
//! * [`server`] — the behaviour, driven over any `Read`/`Write` pair so the
//!   tests exercise it against a temp directory, on the host, with no device.
//!
//! `main.rs` is then only: refuse a tty, build the device [`server::Config`],
//! and run.

pub mod proto;
pub mod server;

pub use proto::{Entry, Op, Request, Response, Status};
pub use server::{Config, Session};
