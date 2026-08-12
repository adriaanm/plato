//! The device side of pairing: a bounded window in which a Mac that knows the
//! displayed code earns push access.  Design: platokin `docs/pairing-candidates.md`.
//!
//! The cryptography and the wire live in `crates/pairing`, compiled into both
//! ends so the two cannot disagree about a parameter.  What is here is
//! everything that is *device*: the firewall, the listener's lifetime, the two
//! key files, and the reporting back to the event loop.
//!
//! ## What this grants
//!
//! Adriaan, 2026-08-12: *"I'm having second thoughts about leveraging ssh,
//! since this represents a large area of privilege escalation just to send a
//! file"* -- a bare `authorized_keys` line is a full root shell in exchange for
//! dropping a file in one directory.  The call: **ssh stays as the transport,
//! and a paired key is restricted to one program.**  So every line this module
//! writes carries a forced command, and [`KEY_OPTIONS`] is the security
//! boundary rather than a decoration.  There is no unrestricted fallback: if
//! the full line cannot be constructed, the grant is refused.
//!
//! **dropbear implements only a subset of OpenSSH's `authorized_keys` options
//! and silently ignores the rest**, which is why this was measured rather than
//! assumed.  **Confirmed on this dropbear build, 2026-08-12**, on a throwaway
//! instance on port 2223: `command=` is honoured for an explicit command, for
//! no command at all, and for a piped one (`cat > /tmp/...` created nothing);
//! the client's request appears only in `$SSH_ORIGINAL_COMMAND`, and `no-pty`
//! is enforced.
//!
//! The grant is still **one function with one call site** ([`authorize_mac`]),
//! so its shape can change without touching anything else here.
//!
//! ## Why the file handling is paranoid
//!
//! `authorized_keys` lives on p3, where USB mass storage cannot reach it.  A
//! truncated or wrongly-permissioned file costs ssh, recoverable only through
//! the EZ_PLATO flag and KUAL.  Hence: append only, never rewrite; re-assert
//! mode 600 on every path, because **dropbear refuses a group- or
//! world-writable `authorized_keys`** and p3 is group-writable, so a fresh file
//! left at the default mode disables key auth entirely -- silently; and refuse
//! rather than repair when anything looks wrong.
//!
//! The live file is `settings/SSH/authorized_keys`, resolved by the patched
//! dropbear **relative to its cwd** (`/var/local/ezssh`).  The sibling
//! `/var/local/ezssh/authorized_keys` is a drop-box that `ezssh-boot.sh` moves
//! into place only when the destination is empty; appending there would do
//! nothing on a device that is already provisioned.
//!
//! ## Threads
//!
//! The event loop is never blocked.  One thread owns the window; a second
//! answers discovery probes.  Outcomes travel back as [`Event::Pairing`], the
//! same shape as the FIFO listener and the WiFi scripts.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use pairing::discovery::{Responder, DEFAULT_DISCOVERY_PORT, DEFAULT_PAIRING_PORT};
use pairing::exchange::{validate_ssh_public_key, MacHello, ReaderReply};
use pairing::{handshake, Code, Config, Error as PairError, Role};
use plato_core::view::pairing::PairingStatus;
use plato_core::view::Event;

use crate::mdns::iface_addr;

/// How long the window stays armed.  Long enough to walk to the Mac, start the
/// CLI and type eight characters; short enough that a window forgotten about is
/// not a standing exposure.  Single-use besides: the first success ends it.
const WINDOW: Duration = Duration::from_secs(180);

/// Wrong codes do **not** end the window -- one wrong guess out of ~40 bits is
/// not an attack, it is a typo, and ending the window on it would make the
/// feature unusable.  What bounds an attacker is this cap plus the delay: ten
/// guesses in three minutes against 2^40 is not a threat.
const MAX_ATTEMPTS: usize = 10;
const FAILURE_DELAY: Duration = Duration::from_secs(2);

/// Accept-loop poll.  The listener is non-blocking so the window can close on
/// time even if nobody ever connects.
const ACCEPT_POLL: Duration = Duration::from_millis(250);

/// How often the remaining time is repainted.  Every tick is an e-ink update,
/// so this is a deliberate compromise rather than a second.
const TICK: Duration = Duration::from_secs(15);

/// Pairing happens over the radio only.  usb0 is a cable to a Mac that needs no
/// pairing, and Amazon's INPUT chain accepts everything on it anyway.
const IFACE: &str = "wlan0";

// ------------------------------------------------------------------ key files

/// dropbear's cwd is `/var/local/ezssh`; it resolves `settings/SSH/...`
/// relative to that, so this is the directory that actually matters.
const SSH_DIR: &str = "/var/local/ezssh/settings/SSH";
const AUTHORIZED_KEYS: &str = "authorized_keys";
const HOST_KEY: &str = "dropbear_ed25519_host_key";

/// Override for host runs and tests.  There is no device story here: on the
/// Kindle the constant above is the truth.
const SSH_DIR_ENV: &str = "PLATOKIN_SSH_DIR";

/// The mode dropbear requires.  Re-asserted after every write.
const KEYS_MODE: u32 = 0o600;

/// The receiver a paired key is confined to.  It is written by another change;
/// the path is fixed, not a placeholder.  It ignores `$SSH_ORIGINAL_COMMAND`
/// entirely -- every request is framed on stdin -- so nothing here may be
/// designed around the client's command string meaning anything.
const RECV_PROGRAM: &str = "/var/local/ezssh/platonic-recv";

/// The option prefix written before every paired key.  **This is the security
/// boundary** (see the module docs, and its confirmation on this dropbear
/// build): without it a paired Mac has a root shell.  Never write a key
/// without it -- a silent downgrade to a bare entry is the entire risk, which
/// is why there is no code path that produces one.
fn key_options() -> String {
    format!("command=\"{}\",no-port-forwarding,no-agent-forwarding,\
             no-X11-forwarding,no-pty", RECV_PROGRAM)
}

fn ssh_dir() -> PathBuf {
    std::env::var_os(SSH_DIR_ENV).map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(SSH_DIR))
}

// ------------------------------------------------------------------- arming

/// What the caller needs in order to put a window on the screen.
pub struct Armed {
    /// The grouped code, `abcd-2345`.  The `Code` itself stays in the thread.
    pub code: String,
    /// The address to show beside it -- the discovery rung nothing can filter.
    pub address: Ipv4Addr,
    pub port: u16,
    pub window: Duration,
}

/// Check every precondition, then arm the window.
///
/// Everything that can be known before a code is displayed is checked here, so
/// a window is never shown that cannot be honoured: the radio has to be up (a
/// code nobody can reach is worse than a refusal) and the host key has to parse
/// (there is no point completing a handshake we cannot answer).
pub fn arm(label: &str, hub: &Sender<Event>) -> Result<Armed, String> {
    // One window at a time.  Two would race for the same TCP port, and the
    // loser's firewall teardown would report a rule it never inserted.
    if ARMED.swap(true, Ordering::SeqCst) {
        return Err("A pairing window is already open.".to_string());
    }
    match arm_inner(label, hub) {
        Ok(armed) => Ok(armed),
        Err(e) => {
            ARMED.store(false, Ordering::SeqCst);
            Err(e)
        },
    }
}

static ARMED: AtomicBool = AtomicBool::new(false);

fn arm_inner(label: &str, hub: &Sender<Event>) -> Result<Armed, String> {
    let Some(address) = iface_addr(IFACE) else {
        return Err(format!("{} has no address -- turn WiFi on first.", IFACE));
    };

    let dir = ssh_dir();
    let host_public_key = read_host_public_key(&dir.join(HOST_KEY))
        .map_err(|e| format!("Can't read the ssh host key: {}.", e))?;

    let code = Code::generate().map_err(|e| format!("Can't generate a code: {}.", e))?;
    // The grouped form goes to the screen and nowhere else: a pairing code in
    // a log is a pairing code an attacker can read.
    let display = code.grouped();
    let label = label.trim();
    let label = if label.is_empty() { "platokin".to_string() } else { label.to_string() };

    println!("pairing: armed for {} s on {}:{} (discovery udp/{}).",
             WINDOW.as_secs(), address, DEFAULT_PAIRING_PORT, DEFAULT_DISCOVERY_PORT);

    let hub = hub.clone();
    thread::spawn(move || {
        run(code, label, host_public_key, hub);
        ARMED.store(false, Ordering::SeqCst);
    });

    Ok(Armed { code: display, address, port: DEFAULT_PAIRING_PORT, window: WINDOW })
}

/// The window, start to finish.  Exactly one terminal status is sent.
fn run(code: Code, label: String, host_public_key: String, hub: Sender<Event>) {
    let deadline = Instant::now() + WINDOW;

    // Opened here, closed by Drop -- on every path out of this function,
    // including a panic.  A rule left behind is a standing hole in a firewall
    // whose whole job is that there are none.
    let _firewall = Firewall::open(&[Rule { proto: "udp", port: DEFAULT_DISCOVERY_PORT },
                                     Rule { proto: "tcp", port: DEFAULT_PAIRING_PORT }]);

    let listener = match TcpListener::bind((Ipv4Addr::UNSPECIFIED, DEFAULT_PAIRING_PORT)) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("pairing: can't listen on {}: {}.", DEFAULT_PAIRING_PORT, e);
            hub.send(Event::Pairing(PairingStatus::Failed(
                format!("Can't listen on {}.", DEFAULT_PAIRING_PORT)))).ok();
            return;
        },
    };
    if let Err(e) = listener.set_nonblocking(true) {
        eprintln!("pairing: can't poll the listener: {}.", e);
        hub.send(Event::Pairing(PairingStatus::Failed("Can't poll the listener.".to_string()))).ok();
        return;
    }

    let stop = Arc::new(AtomicBool::new(false));
    let discovery = spawn_discovery(&label, deadline, stop.clone());

    let status = accept_loop(&listener, deadline, &code, &label, &host_public_key, &hub);

    stop.store(true, Ordering::SeqCst);
    if let Some(handle) = discovery {
        handle.join().ok();
    }

    match &status {
        PairingStatus::Paired(summary) => println!("pairing: {}", summary),
        PairingStatus::Expired => println!("pairing: window closed with no pairing."),
        PairingStatus::Failed(reason) => eprintln!("pairing: failed -- {}", reason),
        other => println!("pairing: ended in {:?}.", other),
    }
    hub.send(Event::Pairing(status)).ok();
}

/// Answer discovery probes for as long as the window lasts.
///
/// A failure here is **not** fatal: the address is on the screen, which is the
/// rung of the ladder nothing can filter, so a Mac can always be pointed at the
/// reader by hand.
fn spawn_discovery(label: &str, deadline: Instant, stop: Arc<AtomicBool>)
                   -> Option<thread::JoinHandle<()>> {
    let responder = match Responder::bind(DEFAULT_DISCOVERY_PORT, DEFAULT_PAIRING_PORT, label) {
        Ok(responder) => responder,
        Err(e) => {
            eprintln!("pairing: no discovery responder ({}); the Mac must be given \
                       the address shown on screen.", e);
            return None;
        },
    };
    if let Some(e) = responder.join_error() {
        // Logged, not acted on: broadcast still reaches a bound socket, and
        // this is the one thing that could differ on other hardware.
        eprintln!("pairing: multicast join failed ({}); broadcast probes still work.", e);
    }

    Some(thread::spawn(move || {
        // Sliced so the success path can stop the responder early rather than
        // leave a socket bound behind a firewall that has just been closed.
        while !stop.load(Ordering::SeqCst) {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let slice = now + Duration::from_millis(500).min(deadline - now);
            if let Err(e) = responder.serve_until(slice, Duration::from_millis(250)) {
                eprintln!("pairing: discovery responder stopped: {}.", e);
                break;
            }
        }
    }))
}

fn accept_loop(listener: &TcpListener, deadline: Instant, code: &Code, label: &str,
               host_public_key: &str, hub: &Sender<Event>) -> PairingStatus {
    let mut attempts = 0;
    let mut next_tick = Instant::now();

    loop {
        let now = Instant::now();
        if now >= deadline {
            return PairingStatus::Expired;
        }
        if now >= next_tick {
            hub.send(Event::Pairing(PairingStatus::Tick((deadline - now).as_secs()))).ok();
            next_tick = now + TICK;
        }

        match listener.accept() {
            Ok((stream, peer)) => {
                // accept(2) does not inherit O_NONBLOCK, but say so rather than
                // depend on it: the handshake needs a blocking socket with its
                // own timeouts.
                stream.set_nonblocking(false).ok();
                println!("pairing: connection from {}.", peer);
                match attempt(stream, code, label, host_public_key) {
                    Attempt::Paired(summary) => return PairingStatus::Paired(summary),
                    Attempt::WrongCode => {
                        attempts += 1;
                        println!("pairing: wrong code ({} of {}).", attempts, MAX_ATTEMPTS);
                        hub.send(Event::Pairing(PairingStatus::WrongCode {
                            attempts, max: MAX_ATTEMPTS })).ok();
                        if attempts >= MAX_ATTEMPTS {
                            return PairingStatus::Failed(
                                format!("Too many wrong codes ({}).", MAX_ATTEMPTS));
                        }
                        // Rate limit, so the window cannot be hammered.
                        thread::sleep(FAILURE_DELAY);
                        next_tick = Instant::now();
                    },
                    Attempt::Broken(reason) => {
                        // Not a guess: a peer that vanished, timed out or is a
                        // different build.  It costs no attempt, and the window
                        // stays open.
                        eprintln!("pairing: attempt from {} broke: {}.", peer, reason);
                    },
                    Attempt::Fatal(reason) => return PairingStatus::Failed(reason),
                }
            },
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL);
            },
            Err(e) => {
                return PairingStatus::Failed(format!("Listener failed: {}.", e));
            },
        }
    }
}

enum Attempt {
    Paired(String),
    /// Key confirmation failed: someone typed the code wrong, or is guessing.
    WrongCode,
    /// This connection failed; the window survives.
    Broken(String),
    /// The window cannot continue.
    Fatal(String),
}

/// One connection: handshake, receive, grant, answer.
///
/// The order is the safety property.  The key is written **before** the reply
/// is sent, so a Mac that is told "you are paired" always is; and the host key
/// was read at arming time, so the reply can never fail after a grant.
fn attempt(stream: TcpStream, code: &Code, label: &str, host_public_key: &str) -> Attempt {
    let mut session = match handshake(stream, Role::Reader, code, &Config::default()) {
        Ok(session) => session,
        Err(PairError::BadCode) => return Attempt::WrongCode,
        Err(e) => return Attempt::Broken(e.to_string()),
    };

    let hello: MacHello = match session.recv_mac_hello() {
        Ok(hello) => hello,
        Err(e) => return Attempt::Broken(e.to_string()),
    };

    // Belt and braces: `recv_mac_hello` already validates.  This is the check
    // that stops a newline smuggling a second, attacker-chosen key into
    // authorized_keys, so it is asserted at the call site that writes the file.
    if let Err(e) = validate_ssh_public_key(&hello.ssh_public_key) {
        return Attempt::Broken(format!("the Mac sent an unusable key: {}", e));
    }

    let grant = match authorize_mac(&hello.ssh_public_key) {
        Ok(grant) => grant,
        // A grant we could not write is fatal to the window: retrying would
        // fail identically, and the human needs to see it.
        Err(e) => return Attempt::Fatal(format!("Can't authorize the Mac: {}.", e)),
    };

    let reply = ReaderReply {
        host_public_key: host_public_key.to_string(),
        device_label: label.to_string(),
    };
    if let Err(e) = session.send_reader_reply(&reply) {
        // The key is already written, so the Mac may well have access it does
        // not know about.  Harmless -- it can pair again -- but say so.
        return Attempt::Broken(format!("paired, but the reply did not get through: {}", e));
    }

    Attempt::Paired(grant.summary())
}

// -------------------------------------------------------------------- grant

/// What a successful pairing changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Grant {
    Added { total: usize },
    AlreadyPresent { total: usize },
}

impl Grant {
    fn summary(&self) -> String {
        match self {
            Grant::Added { total } => format!("Paired. {} authorized.", keys(*total)),
            Grant::AlreadyPresent { total } =>
                format!("Already paired. {} authorized.", keys(*total)),
        }
    }
}

fn keys(n: usize) -> String {
    if n == 1 { "1 key".to_string() } else { format!("{} keys", n) }
}

/// **The one place that grants a paired Mac access.**  One call site, on
/// purpose: the shape of the grant is an open decision (see the module docs),
/// and changing it must touch nothing else.
pub fn authorize_mac(ssh_public_key: &str) -> Result<Grant, String> {
    append_authorized_key(&ssh_dir(), ssh_public_key)
}

/// Append-only, idempotent, and mode 600 on every path.
///
/// Refuses rather than repairs: a missing directory, a path that is not a
/// regular file, or a key that does not parse means something is not as this
/// code believes, and writing into that is how ssh gets lost.
fn append_authorized_key(dir: &Path, ssh_public_key: &str) -> Result<Grant, String> {
    validate_ssh_public_key(ssh_public_key).map_err(|e| e.to_string())?;
    let Some(incoming) = key_material(ssh_public_key) else {
        return Err("the key has no recognisable type and blob".to_string());
    };
    if !dir.is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }

    let path = dir.join(AUTHORIZED_KEYS);
    let existing = match fs::metadata(&path) {
        Ok(md) if md.is_file() => fs::read_to_string(&path)
            .map_err(|e| format!("can't read {}: {}", path.display(), e))?,
        Ok(_) => return Err(format!("{} exists and is not a regular file", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("can't stat {}: {}", path.display(), e)),
    };

    let mut present = false;
    let mut total = 0;
    for line in existing.lines() {
        if let Some(material) = key_material(line) {
            total += 1;
            if material == incoming {
                present = true;
            }
        }
    }

    if present {
        // Mode is re-asserted even when nothing was written: this is the
        // invariant, not a side effect of writing.
        set_mode(&path)?;
        return Ok(Grant::AlreadyPresent { total });
    }

    let mut file = OpenOptions::new().append(true).create(true).open(&path)
        .map_err(|e| format!("can't open {} for appending: {}", path.display(), e))?;
    // A file whose last line has no newline would otherwise get this key glued
    // onto it, destroying both.
    let mut entry = String::new();
    if !existing.is_empty() && !existing.ends_with('\n') {
        entry.push('\n');
    }
    entry.push_str(&key_options());
    entry.push(' ');
    entry.push_str(ssh_public_key);
    entry.push('\n');
    file.write_all(entry.as_bytes())
        .map_err(|e| format!("can't append to {}: {}", path.display(), e))?;
    file.sync_all()
        .map_err(|e| format!("can't flush {}: {}", path.display(), e))?;
    drop(file);

    set_mode(&path)?;
    Ok(Grant::Added { total: total + 1 })
}

/// dropbear refuses a group- or world-writable `authorized_keys`, and p3 is
/// group-writable, so this is not hygiene -- it is the difference between key
/// auth working and silently not working.
fn set_mode(path: &Path) -> Result<(), String> {
    fs::set_permissions(path, fs::Permissions::from_mode(KEYS_MODE))
        .map_err(|e| format!("can't chmod {} to {:o}: {}", path.display(), KEYS_MODE, e))
}

/// The `(type, blob)` pair out of an `authorized_keys` line, ignoring any
/// option prefix and any comment.
///
/// Identity is the key material and nothing else: the same key with different
/// options or a different comment is the same key, and appending it twice would
/// leave a stale grant behind on revocation.
fn key_material(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let fields: Vec<&str> = line.split_whitespace().collect();
    fields.windows(2)
          .find(|w| is_key_type(w[0]) && looks_base64(w[1]))
          .map(|w| (w[0], w[1]))
}

fn is_key_type(field: &str) -> bool {
    field.starts_with("ssh-") || field.starts_with("ecdsa-sha2-")
}

fn looks_base64(field: &str) -> bool {
    field.len() >= 16
        && field.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=')
}

// ----------------------------------------------------------------- host key

/// dropbear's Ed25519 host key file, as confirmed on the device by
/// reconstructing the key and matching it against a `known_hosts` entry the Mac
/// already trusted:
///
/// ```text
/// u32be(11) | "ssh-ed25519" | u32be(64) | 32-byte seed | 32-byte public
/// ```
///
/// **The first 64 of those bytes are the private key.**  They are never logged,
/// never displayed and never returned; only the trailing 32 leave this
/// function.  There is no `dropbearkey` and no `ssh-keygen` on the device, so
/// parsing the file is the only way to answer.
const HOST_KEY_LEN: usize = 83;
const HOST_KEY_ALGO: &str = "ssh-ed25519";

fn read_host_public_key(path: &Path) -> Result<String, String> {
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|mut f| f.read_to_end(&mut bytes))
        .map_err(|e| format!("{}: {}", path.display(), e))?;
    host_public_key_from_bytes(&bytes)
}

/// Split out so it can be tested against a synthetic file: the real one cannot
/// go anywhere near a test, and its layout is the thing worth pinning.
fn host_public_key_from_bytes(bytes: &[u8]) -> Result<String, String> {
    if bytes.len() != HOST_KEY_LEN {
        return Err(format!("expected {} bytes, got {}", HOST_KEY_LEN, bytes.len()));
    }
    let algo = HOST_KEY_ALGO.as_bytes();
    if read_u32(&bytes[0..4]) != algo.len() as u32 || &bytes[4..4 + algo.len()] != algo {
        return Err("not an ssh-ed25519 key file".to_string());
    }
    let rest = 4 + algo.len();
    if read_u32(&bytes[rest..rest + 4]) != 64 {
        return Err("the key blob is not 64 bytes".to_string());
    }
    // seed = bytes[rest+4 .. rest+36]; PRIVATE, deliberately not bound to a name.
    let public = &bytes[rest + 36..];

    let mut blob = Vec::with_capacity(4 + algo.len() + 4 + 32);
    blob.extend_from_slice(&(algo.len() as u32).to_be_bytes());
    blob.extend_from_slice(algo);
    blob.extend_from_slice(&32u32.to_be_bytes());
    blob.extend_from_slice(public);
    Ok(format!("{} {}", HOST_KEY_ALGO, base64(&blob)))
}

fn read_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// Standard base64 with padding.  Twenty lines beats a dependency in a binary
/// that encodes exactly one 51-byte blob, once.
fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[n as usize & 63] as char } else { '=' });
    }
    out
}

// ----------------------------------------------------------------- firewall

/// One volatile INPUT rule.  The house idiom (`scripts/wifi-up.sh`): `-C`
/// first so re-runs do not stack duplicates, scoped to the interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rule {
    proto: &'static str,
    port: u16,
}

impl Rule {
    /// `op` is `-C`, `-I` or `-D`; the rest of the rule is identical for all
    /// three, which is the point -- a check that does not match the insert
    /// deletes nothing and reports success.
    fn args(&self, op: &str) -> Vec<String> {
        vec![op.to_string(), "INPUT".to_string(),
             "-i".to_string(), IFACE.to_string(),
             "-p".to_string(), self.proto.to_string(),
             "--dport".to_string(), self.port.to_string(),
             "-j".to_string(), "ACCEPT".to_string()]
    }
}

/// Opens the pairing ports and closes them again when dropped.
///
/// RAII rather than a call at the end of the happy path: the window ends by
/// timeout, by success, by error and by panic, and a rule left behind is a hole
/// in a firewall whose entire premise is that there are none.  Amazon's INPUT
/// policy is DROP with accept-all only on usb0 (E21), so without these rules
/// the listener binds, logs, and never hears anything.
struct Firewall {
    /// Only what *we* inserted.  A rule that was already there belongs to
    /// something else -- deleting it would close a port we did not open.
    opened: Vec<Rule>,
}

impl Firewall {
    fn open(rules: &[Rule]) -> Firewall {
        let mut opened = Vec::new();
        for rule in rules {
            if iptables(&rule.args("-C")) {
                println!("pairing: {}/{} already open on {}.", rule.proto, rule.port, IFACE);
            } else if iptables(&rule.args("-I")) {
                println!("pairing: opened {}/{} on {}.", rule.proto, rule.port, IFACE);
                opened.push(*rule);
            } else {
                eprintln!("pairing: WARN could not open {}/{} on {} -- the Mac will \
                           not reach us.", rule.proto, rule.port, IFACE);
            }
        }
        Firewall { opened }
    }
}

impl Drop for Firewall {
    fn drop(&mut self) {
        for rule in &self.opened {
            if iptables(&rule.args("-D")) {
                println!("pairing: closed {}/{} on {}.", rule.proto, rule.port, IFACE);
            } else {
                eprintln!("pairing: WARN could not close {}/{} on {}; it is volatile \
                           and goes on reboot.", rule.proto, rule.port, IFACE);
            }
        }
    }
}

fn iptables(args: &[String]) -> bool {
    Command::new("iptables").args(args)
            .stdout(Stdio::null()).stderr(Stdio::null())
            .status().map(|s| s.success()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------- host key

    /// The layout confirmed on the device, built by hand so the parser is
    /// tested against the spec rather than against itself.
    fn synthetic_host_key(seed: u8, public: u8) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&11u32.to_be_bytes());
        bytes.extend_from_slice(b"ssh-ed25519");
        bytes.extend_from_slice(&64u32.to_be_bytes());
        bytes.extend_from_slice(&[seed; 32]);
        bytes.extend_from_slice(&[public; 32]);
        assert_eq!(bytes.len(), HOST_KEY_LEN);
        bytes
    }

    #[test]
    fn host_key_yields_the_openssh_line() {
        let line = host_public_key_from_bytes(&synthetic_host_key(0x11, 0xAB)).unwrap();
        // u32be(11) "ssh-ed25519" u32be(32) 0xAB * 32, base64'd.
        assert_eq!(line, "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKurq6urq6urq6urq6urq6ur\
                          q6urq6urq6urq6urq6ur");
    }

    /// The seed is the private key.  If it ever reaches the output, this fails.
    #[test]
    fn host_key_never_leaks_the_seed() {
        let line = host_public_key_from_bytes(&synthetic_host_key(0xFF, 0x00)).unwrap();
        let blob = line.split(' ').nth(1).unwrap();
        let public_only = host_public_key_from_bytes(&synthetic_host_key(0x00, 0x00)).unwrap();
        // Same public half, different seed: the answer must be identical.
        assert_eq!(line, public_only);
        assert!(!blob.contains("//////"), "the seed must not appear in the blob");
    }

    #[test]
    fn host_key_refuses_anything_unexpected() {
        assert!(host_public_key_from_bytes(&[]).is_err());
        let mut short = synthetic_host_key(1, 2);
        short.pop();
        assert!(host_public_key_from_bytes(&short).is_err());
        let mut wrong_algo = synthetic_host_key(1, 2);
        wrong_algo[4] = b'x';
        assert!(host_public_key_from_bytes(&wrong_algo).is_err());
        let mut wrong_len = synthetic_host_key(1, 2);
        wrong_len[18] = 63;
        assert!(host_public_key_from_bytes(&wrong_len).is_err());
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    // ------------------------------------------------------ authorized_keys

    const KEY_A: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA platonic@mac-a";
    const KEY_B: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB platonic@mac-b";

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("plato-pairing-{}-{}", name, std::process::id()));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn appends_to_a_missing_file_and_locks_it_down() {
        let dir = tmpdir("missing");
        let grant = append_authorized_key(&dir, KEY_A).unwrap();
        assert_eq!(grant, Grant::Added { total: 1 });
        let path = dir.join(AUTHORIZED_KEYS);
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.ends_with('\n'));
        assert!(body.starts_with(&key_options()), "the forced command must precede the key");
        assert!(body.contains(KEY_A));
        assert_eq!(mode(&path), KEYS_MODE);
        fs::remove_dir_all(&dir).ok();
    }

    /// The forced command is the security boundary: there must be no path that
    /// writes a bare key.
    #[test]
    fn every_written_key_carries_the_forced_command() {
        let dir = tmpdir("forced");
        append_authorized_key(&dir, KEY_A).unwrap();
        append_authorized_key(&dir, KEY_B).unwrap();
        let body = fs::read_to_string(dir.join(AUTHORIZED_KEYS)).unwrap();
        for line in body.lines() {
            assert!(line.starts_with(&format!("command=\"{}\"", RECV_PROGRAM)),
                    "unrestricted entry written: {}", line);
            assert!(line.contains("no-pty"));
        }
        fs::remove_dir_all(&dir).ok();
    }

    /// An existing bare entry -- the admin key, say -- is recognised as that
    /// key and left exactly as it is.  Never rewrite, never "upgrade": the
    /// admin key has other uses, and rewriting a line is how ssh gets lost.
    #[test]
    fn an_existing_bare_entry_is_neither_duplicated_nor_upgraded() {
        let dir = tmpdir("bare");
        let path = dir.join(AUTHORIZED_KEYS);
        let bare = format!("{}\n", KEY_A);
        fs::write(&path, &bare).unwrap();
        assert_eq!(append_authorized_key(&dir, KEY_A).unwrap(),
                   Grant::AlreadyPresent { total: 1 });
        assert_eq!(fs::read_to_string(&path).unwrap(), bare);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn never_touches_what_is_already_there() {
        let dir = tmpdir("preserve");
        let path = dir.join(AUTHORIZED_KEYS);
        // The admin key: nothing may ever remove or rewrite this line.
        let admin = format!("{}\n", KEY_B);
        fs::write(&path, &admin).unwrap();
        append_authorized_key(&dir, KEY_A).unwrap();
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.starts_with(&admin));
        assert_eq!(body.lines().count(), 2);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_file_without_a_final_newline_does_not_glue_two_keys_together() {
        let dir = tmpdir("nonewline");
        let path = dir.join(AUTHORIZED_KEYS);
        fs::write(&path, KEY_B).unwrap();
        append_authorized_key(&dir, KEY_A).unwrap();
        let body = fs::read_to_string(&path).unwrap();
        assert_eq!(body.lines().count(), 2);
        assert_eq!(key_material(body.lines().next().unwrap()), key_material(KEY_B));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_same_key_is_never_appended_twice() {
        let dir = tmpdir("dedupe");
        assert_eq!(append_authorized_key(&dir, KEY_A).unwrap(), Grant::Added { total: 1 });
        assert_eq!(append_authorized_key(&dir, KEY_A).unwrap(),
                   Grant::AlreadyPresent { total: 1 });
        // A different comment on the same key material is the same key.
        let renamed = KEY_A.replace("mac-a", "mac-a-renamed");
        assert_eq!(append_authorized_key(&dir, &renamed).unwrap(),
                   Grant::AlreadyPresent { total: 1 });
        assert_eq!(append_authorized_key(&dir, KEY_B).unwrap(), Grant::Added { total: 2 });
        assert_eq!(fs::read_to_string(dir.join(AUTHORIZED_KEYS)).unwrap().lines().count(), 2);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_wrong_mode_is_re_asserted_even_when_nothing_is_written() {
        let dir = tmpdir("mode");
        append_authorized_key(&dir, KEY_A).unwrap();
        let path = dir.join(AUTHORIZED_KEYS);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o664)).unwrap();
        append_authorized_key(&dir, KEY_A).unwrap();
        assert_eq!(mode(&path), KEYS_MODE);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refuses_a_key_that_could_carry_a_second_line() {
        let dir = tmpdir("injection");
        let injected = format!("{}\nssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEVIL evil@mac", KEY_A);
        assert!(append_authorized_key(&dir, &injected).is_err());
        assert!(!dir.join(AUTHORIZED_KEYS).exists(), "nothing may be written on refusal");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refuses_a_missing_directory() {
        let dir = tmpdir("absent").join("nope");
        assert!(append_authorized_key(&dir, KEY_A).is_err());
    }

    #[test]
    fn key_material_ignores_options_and_comments() {
        let with_options = format!("{} {}", key_options(), KEY_A);
        assert_eq!(key_material(&with_options), key_material(KEY_A));
        assert_eq!(key_material("# a comment"), None);
        assert_eq!(key_material("   "), None);
        assert_eq!(key_material("garbage"), None);
    }

    // -------------------------------------------------------------- firewall

    #[test]
    fn firewall_rules_are_scoped_and_symmetric() {
        let rule = Rule { proto: "udp", port: DEFAULT_DISCOVERY_PORT };
        assert_eq!(rule.args("-C"),
                   ["-C", "INPUT", "-i", "wlan0", "-p", "udp", "--dport", "30304",
                    "-j", "ACCEPT"]);
        // The check, the insert and the delete must differ in the verb ALONE,
        // or a rule gets inserted and never removed.
        let check = rule.args("-C");
        for op in ["-I", "-D"] {
            let other = rule.args(op);
            assert_eq!(other[0], op);
            assert_eq!(other[1..], check[1..]);
        }
        assert_eq!(Rule { proto: "tcp", port: DEFAULT_PAIRING_PORT }.args("-I")[5], "tcp");
    }
}
