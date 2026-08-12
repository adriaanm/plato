//! `platonic --pair`: the Mac half of the pairing window.
//!
//! The counterpart is `crates/plato/src/pairing.rs`; the wire, the code
//! alphabet and the crypto are `crates/pairing`, linked by both ends so there
//! is exactly one definition of every parameter.  Nothing here re-implements a
//! byte of the protocol.
//!
//! What a pairing establishes is two files, one on each side:
//!
//! * on the reader, one `authorized_keys` line carrying the forced command --
//!   **the device writes it**, from the public key sent over the confirmed
//!   channel.  The Mac never asks for a bare key and has no way to ask for one;
//! * on the Mac, one `known_hosts` line keyed to the `platokin` alias, from the
//!   host key the reader sends back.  That is what makes every later push
//!   address-independent (`HostKeyAlias`) and non-interactive.
//!
//! Only the **public** halves ever move.  The private key is generated locally
//! by `ssh-keygen` and is never read by this program, let alone transmitted.
//!
//! Finding the reader is not the push ladder.  That ladder probes by running
//! the receiver over ssh, and a Mac that has not paired yet has no credentials
//! and no host key -- every rung of it would fail by construction.  Pairing
//! uses the UDP rung instead (`pairing::discovery`, udp/30304, Confirmed on
//! this hardware over this AP by E21), seeded with the same *names* the push
//! ladder uses as extra unicast targets: unicast is the direction nothing
//! filters, and a name that resolves costs one datagram to try.

use std::io::{BufRead, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use pairing::discovery::{
    discover_with_extra_targets, Offer, DEFAULT_DISCOVERY_PORT, DEFAULT_PAIRING_PORT,
};
use pairing::exchange::{validate_ssh_public_key, MacHello, ReaderReply};
use pairing::{handshake, Code, Config, Error as PairError, Role};

use crate::pure::{dns_names, ALIAS, SSH_PORT, USB_ADDR};
use crate::session::{probe, resolve_all};
use crate::{die, Args, Ctx, Identity};

/// How long to listen for offers.  The reader re-answers every probe and the
/// prober re-sends every 500 ms, so this is four chances, not one.
const DISCOVERY_WINDOW: Duration = Duration::from_secs(4);

/// Mirrors `MAX_ATTEMPTS` in `crates/plato/src/pairing.rs`.  The device is the
/// authority -- it ends the window at ten wrong codes -- and matching it here
/// is only so the Mac stops asking instead of running into a refused port.
const MAX_ATTEMPTS: usize = 10;

/// The device closes the socket after every attempt, wrong code or not, so a
/// retry is a fresh connection.  Bounded because a reader that has gone to
/// sleep must not hang the prompt.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

//
// ------------------------------------------------------------- pure helpers
//

/// The `known_hosts` entry, keyed to the alias rather than to an address.
///
/// Both spellings are listed because ssh looks up `[host]:port` for a
/// non-default port and the bare name otherwise, and this file is read by both
/// shapes of invocation over the tool's life.
pub fn alias_line(host_public_key: &str) -> String {
    format!("{},[{}]:{} {}", ALIAS, ALIAS, SSH_PORT, host_public_key)
}

/// What merging the new host key into an existing `known_hosts` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KnownHosts {
    Added,
    /// The same key was already trusted under the alias; the file is unchanged.
    AlreadyTrusted,
    /// A *different* key answered for the alias and was replaced.  This is the
    /// reader having been re-flashed or its host key regenerated; the peer just
    /// proved it holds the typed code, which is a stronger claim than an old
    /// line in a file, so the new key wins -- but it is said out loud.
    Replaced,
}

/// Fold the reader's host key into the alias entry, dropping any stale alias
/// line and leaving every other entry (the usbnet address entries, other
/// hosts) exactly as it is.
///
/// Pure, because this is the one operation in `--pair` that can silently break
/// every later push.
pub fn merge_known_hosts(existing: &str, host_public_key: &str) -> (String, KnownHosts) {
    let wanted = alias_line(host_public_key);
    let bracketed = format!("[{}]:{}", ALIAS, SSH_PORT);

    let mut kept: Vec<&str> = Vec::new();
    let mut had_alias = false;
    let mut had_exact = false;
    for line in existing.lines() {
        let trimmed = line.trim();
        // A hashed entry (`|1|…`) cannot be read, so it cannot be judged --
        // and it cannot be ours either: this file is only ever written here,
        // unhashed.  Keep it.
        let is_alias = !trimmed.is_empty()
            && !trimmed.starts_with('#')
            && !trimmed.starts_with('|')
            && trimmed
                .split_whitespace()
                .next()
                .unwrap_or("")
                .split(',')
                .any(|h| h == ALIAS || h == bracketed);
        if is_alias {
            had_alias = true;
            if trimmed == wanted {
                had_exact = true;
            }
            continue;
        }
        kept.push(line);
    }

    let outcome = match (had_alias, had_exact) {
        (_, true) => KnownHosts::AlreadyTrusted,
        (true, false) => KnownHosts::Replaced,
        (false, false) => KnownHosts::Added,
    };

    let mut out = String::new();
    for line in kept {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&wanted);
    out.push('\n');
    (out, outcome)
}

/// `ssh-keygen` argv for a fresh per-Mac key.  `-N ''` because the key is an
/// appliance credential used by a non-interactive tool: a passphrase it cannot
/// answer would only be a passphrase stored somewhere else.
pub fn keygen_argv(path: &Path, comment: &str) -> Vec<String> {
    vec![
        "ssh-keygen".to_string(),
        "-t".to_string(), "ed25519".to_string(),
        "-N".to_string(), String::new(),
        "-C".to_string(), comment.to_string(),
        "-f".to_string(), path.display().to_string(),
    ]
}

/// `platonic@<host>`, with the host reduced to something that cannot break the
/// single-line `authorized_keys` entry it ends up in: printable ASCII, no
/// space, no `.local` suffix (every Mac has one and it says nothing).
pub fn key_comment(hostname: &str) -> String {
    let base = hostname.trim().trim_end_matches(".local");
    let cleaned: String = base
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
        .collect();
    let cleaned = cleaned.trim_matches('.').to_string();
    if cleaned.is_empty() {
        "platonic@mac".to_string()
    } else {
        format!("platonic@{}", cleaned)
    }
}

/// Reduce whatever is in a `.pub` file to exactly `<type> <blob> <comment>`.
///
/// An existing key's comment is often several words (`ssh-keygen` puts
/// `user@host` there, but people edit them), and
/// [`validate_ssh_public_key`] refuses more than three fields -- correctly, since
/// the string becomes one line of `authorized_keys`.  Rewriting the comment
/// rather than refusing the key means a Mac whose key predates this tool can
/// still pair, and it makes the device-side entry name which Mac it is.
pub fn normalize_public_key(line: &str, comment: &str) -> Result<String, String> {
    let mut fields = line.split_whitespace();
    let (Some(kind), Some(blob)) = (fields.next(), fields.next()) else {
        return Err("not an ssh public key: expected `<type> <base64>`".to_string());
    };
    let normalized = format!("{} {} {}", kind, blob, comment);
    validate_ssh_public_key(&normalized).map_err(|e| e.to_string())?;
    Ok(normalized)
}

/// `~/.ssh/platonic_ed25519` -> `~/.ssh/platonic_ed25519.pub`, the way
/// `ssh-keygen` names it.
pub fn public_key_path(private: &Path) -> PathBuf {
    let mut name = private.as_os_str().to_os_string();
    name.push(".pub");
    PathBuf::from(name)
}

/// Which offer to pair with, when several readers answered.
///
/// Refuses rather than guesses: pairing grants push access, so picking one of
/// two strangers' readers is not a choice this program gets to make silently.
pub fn choose_offer(offers: &[Offer]) -> Result<&Offer, String> {
    match offers.len() {
        0 => Err(no_reader_message()),
        1 => Ok(&offers[0]),
        _ => {
            let mut msg = String::from(
                "several readers answered; name one with --host ADDRESS \
                 (it is shown under the code):\n",
            );
            for o in offers {
                msg.push_str(&format!("  {}  ({})\n", o.addr, o.label));
            }
            Err(msg)
        }
    }
}

/// The message that has to name the true cause almost every time.
pub fn no_reader_message() -> String {
    format!(
        "no reader is offering to pair.\n\
         On the Kindle: Applications ▸ Pair a Mac. The window lasts 3 minutes.\n\
         Then run this again — or, if discovery is blocked, give the address \
         shown under the code:\n    platonic --pair --host 192.168.178.x\n\
         (pairing talks udp/{} and tcp/{}, not ssh.)",
        DEFAULT_DISCOVERY_PORT, DEFAULT_PAIRING_PORT
    )
}

/// Turn a failed TCP connect into something actionable.  Refused is the common
/// one and it has exactly one cause: no window is open.
pub fn describe_connect_error(addr: &SocketAddr, e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::ConnectionRefused => format!(
            "{} refused the pairing connection — no window is open.\n\
             On the Kindle: Applications ▸ Pair a Mac, then run this again.",
            addr
        ),
        std::io::ErrorKind::TimedOut => format!(
            "{} did not answer in {}s. Is the reader awake with WiFi on, and \
             is the pairing window still up?",
            addr,
            CONNECT_TIMEOUT.as_secs()
        ),
        _ => format!("could not reach {}: {}", addr, e),
    }
}

/// Everything a handshake can fail with, in the words of what to do next.
pub fn describe_pair_error(e: &PairError) -> String {
    match e {
        // Handled by the retry loop; here for completeness.
        PairError::BadCode => "wrong code".to_string(),
        PairError::Version { ours, theirs } => format!(
            "this platonic speaks pairing protocol {} and the reader speaks {}. \
             One of the two is an old build — redeploy Plato, or rebuild \
             platonic from the same checkout.",
            ours, theirs
        ),
        PairError::Timeout => "the reader stopped answering mid-handshake; the \
             window may have expired (it lasts 3 minutes). Arm it again and \
             retry."
            .to_string(),
        PairError::Io(io) if io.kind() == std::io::ErrorKind::UnexpectedEof =>
            "the reader closed the connection mid-handshake. If it had already \
             taken ten wrong codes the window is over — arm it again."
                .to_string(),
        other => other.to_string(),
    }
}

//
// ---------------------------------------------------------------------- keys
//

/// The key whose **public** half gets sent, generating one if there is none.
///
/// The resolution order is `pure::choose_key`'s, unchanged, so a Mac that
/// already has a key keeps pairing with it: `PLATONIC_KEY`, else the per-Mac
/// key, else the admin key.  Only when *nothing* exists is a key minted, and
/// then it is the per-Mac one -- individually revocable, one Mac's blast
/// radius (Adriaan, 2026-08-12).
/// Returns `None` under `--dry-run` when a key would have to be minted:
/// nothing on this path may have an effect, and generating a key pair is the
/// most durable effect the command has.
fn resolve_key(ctx: &Ctx, home: &Path) -> Option<PathBuf> {
    if ctx.key.exists() {
        return Some(ctx.key.clone());
    }
    if ctx.identity == Identity::Env {
        // The escape hatch is explicit; silently minting a key somewhere else
        // would be answering a different question than the one asked.
        die(format!(
            "PLATONIC_KEY points at {}, which does not exist. Unset it to have \
             --pair generate a per-Mac key.",
            ctx.key.display()
        ));
    }
    let path = home.join(".ssh/platonic_ed25519");
    if path.exists() {
        return Some(path);
    }
    if ctx.dry_run {
        println!("[dry-run] would generate {} ({})",
                 path.display(), key_comment(&hostname()));
        return None;
    }
    Some(generate_key(&path))
}

fn generate_key(path: &Path) -> PathBuf {
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            die(format!("could not create {}: {}", parent.display(), e));
        }
    }
    let comment = key_comment(&hostname());
    let argv = keygen_argv(path, &comment);
    println!("no push key yet — generating {} ({})", path.display(), comment);
    let status = Command::new(&argv[0]).args(&argv[1..]).status();
    match status {
        Ok(s) if s.success() && path.exists() => path.to_path_buf(),
        Ok(s) => die(format!("ssh-keygen exited {}; no key was generated",
                             s.code().unwrap_or(-1))),
        Err(e) => die(format!("could not run ssh-keygen: {}", e)),
    }
}

/// The public key line to send.  Read from `<key>.pub` if it is there, else
/// derived from the private key with `ssh-keygen -y` -- a private key without
/// its `.pub` is normal enough (dotfile syncs drop it) that refusing would be
/// the wrong answer.
///
/// **The private key itself is never read by this program and never leaves the
/// Mac.**  `ssh-keygen -y` prints only the public half.
fn public_key_line(key: &Path) -> String {
    let pub_path = public_key_path(key);
    let raw = match std::fs::read_to_string(&pub_path) {
        Ok(text) => text,
        Err(_) => {
            let out = Command::new("ssh-keygen")
                .args(["-y", "-f"])
                .arg(key)
                .output();
            match out {
                Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
                Ok(o) => die(format!(
                    "no {} and ssh-keygen could not derive it: {}",
                    pub_path.display(),
                    String::from_utf8_lossy(&o.stderr).trim()
                )),
                Err(e) => die(format!("could not run ssh-keygen: {}", e)),
            }
        }
    };
    let first = raw.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    match normalize_public_key(first, &key_comment(&hostname())) {
        Ok(line) => line,
        Err(e) => die(format!("{}: {}", pub_path.display(), e)),
    }
}

fn hostname() -> String {
    Command::new("hostname")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "mac".to_string())
}

//
// ----------------------------------------------------------------- discovery
//

/// Candidate unicast targets for the probe: everything the push ladder would
/// have tried, resolved to addresses.  A probe to a name that does not resolve
/// costs nothing; a probe to one that does is the whole discovery on a router
/// that registers client names.
fn extra_targets(ctx: &Ctx) -> Vec<IpAddr> {
    let mut out: Vec<IpAddr> = Vec::new();
    let mut push = |s: &str| {
        if let Ok(ip) = s.parse::<IpAddr>() {
            if !out.contains(&ip) {
                out.push(ip);
            }
        }
    };
    if let Some(cached) = ctx.read_cache() {
        push(&cached);
    }
    for name in dns_names(ALIAS) {
        for addr in resolve_all(&name) {
            push(&addr);
        }
    }
    push(USB_ADDR);
    out
}

/// `--host`/`PLATONIC_HOST` -> that address and nothing else; otherwise the
/// UDP rung.
fn find_reader(ctx: &Ctx, host_opt: &Option<String>) -> (SocketAddr, String) {
    let explicit = host_opt
        .clone()
        .or_else(|| std::env::var("PLATONIC_HOST").ok())
        .filter(|s| !s.is_empty());

    if let Some(host) = explicit {
        // No fallback past the escape hatch, as with the push path.
        let addr = (host.as_str(), DEFAULT_PAIRING_PORT)
            .to_socket_addrs()
            .ok()
            .and_then(|mut it| it.find(|a| a.is_ipv4()).or_else(|| it.next()));
        match addr {
            Some(addr) => return (addr, host),
            None => die(format!("could not resolve {}", host)),
        }
    }

    println!("looking for a reader offering to pair (udp/{}) …",
             DEFAULT_DISCOVERY_PORT);
    let offers = discover_with_extra_targets(
        DEFAULT_DISCOVERY_PORT, DISCOVERY_WINDOW, &extra_targets(ctx))
        .unwrap_or_else(|e| die(format!("could not probe for readers: {}\n{}",
                                        e, no_reader_message())));
    let offer = choose_offer(&offers).unwrap_or_else(|e| die(e));
    println!("found {} at {}", offer.label, offer.addr);
    (offer.pairing_addr(), offer.label.clone())
}

//
// ------------------------------------------------------------------- the run
//

/// Ask until something parses as a code.  A malformed code never reaches the
/// wire, so a typo costs nothing: only a *well-formed wrong* code burns one of
/// the reader's ten attempts.
fn read_code(attempt: usize) -> Code {
    loop {
        if attempt == 1 {
            print!("code shown on the reader (xxxx-xxxx): ");
        } else {
            print!("code ({} of {}): ", attempt, MAX_ATTEMPTS);
        }
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        match std::io::stdin().lock().read_line(&mut line) {
            Ok(0) => die("no code given (stdin closed)"),
            Ok(_) => {}
            Err(e) => die(format!("could not read the code: {}", e)),
        }
        match Code::parse(&line) {
            Ok(code) => return code,
            // The crate names the offending character rather than remapping
            // it, and that message is the whole value: a silently remapped
            // `l`→`i` surfaces later as a wrong code, which reads like the
            // protocol is broken.  Print it and ask again -- this costs the
            // reader nothing, because nothing was sent.
            Err(e) => eprintln!("{}", e),
        }
    }
}

/// Type a code, connect, handshake, swap keys.  Ten tries, **one connection
/// each** -- `attempt()` on the device owns the stream and drops it whichever
/// way it ends, so retrying in place is not something the protocol offers.
///
/// `codes` is a parameter rather than a call to [`read_code`] so the whole
/// exchange can be run against the real [`Role::Reader`] end in a test: this
/// function and the device's `attempt()` are the two halves that have to agree,
/// and a test that drives both is the only thing that proves they do without
/// the Kindle in the room.
fn exchange(addr: SocketAddr, ssh_public_key: &str,
            codes: &mut dyn FnMut(usize) -> Code) -> Result<ReaderReply, String> {
    for attempt in 1..=MAX_ATTEMPTS {
        let code = codes(attempt);
        let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
            .map_err(|e| describe_connect_error(&addr, &e))?;
        let mut session = match handshake(stream, Role::Mac, &code, &Config::default()) {
            Ok(session) => session,
            Err(PairError::BadCode) => {
                // The reader counts this too, shows it, and waits 2 s before
                // accepting again -- which retyping covers.
                eprintln!("wrong code ({} of {}). The reader is showing it too.",
                          attempt, MAX_ATTEMPTS);
                continue;
            }
            Err(e) => return Err(describe_pair_error(&e)),
        };

        // Only now is there a confirmed channel, and only a confirmed channel
        // ever sees the key.
        session.send_mac_hello(&MacHello {
            ssh_public_key: ssh_public_key.to_string(),
        }).map_err(|e| describe_pair_error(&e))?;

        return session.recv_reader_reply().map_err(|e| format!(
            "the reader accepted the code but the reply did not arrive: {}\n\
             It may already have authorized this Mac; try a push, and pair \
             again if it is refused.", describe_pair_error(&e)));
    }
    Err(format!(
        "{} wrong codes — the reader has closed its window. Nothing was \
         changed. Arm it again on the Kindle and read the code carefully.",
        MAX_ATTEMPTS))
}

/// Write the alias entry.  Via a temporary file and a rename: this file is
/// what makes every later push non-interactive, and a half-written one would
/// be discovered days later as a host-key prompt in a BatchMode session.
fn record_host_key(path: &Path, host_public_key: &str) -> KnownHosts {
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            die(format!("could not create {}: {}", parent.display(), e));
        }
    }
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let (merged, outcome) = merge_known_hosts(&existing, host_public_key);
    if outcome == KnownHosts::AlreadyTrusted {
        // The exact line is already in the file.  Not rewriting it is not an
        // optimisation: this file also holds the address-keyed entries the
        // admin key has used since 2026-08-08, and the safest write is the one
        // that does not happen.
        return outcome;
    }
    let tmp = path.with_extension("platonic-tmp");
    if let Err(e) = std::fs::write(&tmp, &merged) {
        die(format!("could not write {}: {}", tmp.display(), e));
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        die(format!("could not replace {}: {}", path.display(), e));
    }
    outcome
}

pub fn cmd_pair(ctx: &Ctx, args: &Args) {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let key = resolve_key(ctx, &home);
    let ssh_public_key = key.as_deref().map(public_key_line);

    if ctx.dry_run {
        // No prompt, no packets, no files: --dry-run must never need a reader,
        // and it must not mint a key or rewrite known_hosts either.
        println!("[dry-run] would pair with:");
        match (&key, &ssh_public_key) {
            (Some(key), Some(line)) => {
                println!("  key          {}", key.display());
                println!("  sending      {}", line);
            }
            _ => println!("  sending      the public half of that new key"),
        }
        println!("  known_hosts  {} (entry for {})", ctx.known.display(),
                 alias_line("ssh-ed25519 <reader host key>"));
        match args.host.clone()
                  .or_else(|| std::env::var("PLATONIC_HOST").ok())
                  .filter(|s| !s.is_empty()) {
            Some(host) => println!("  target       {}:{} (given; no discovery)",
                                   host, DEFAULT_PAIRING_PORT),
            None => println!("  discovery    udp/{} broadcast + 224.0.0.251, \
                              plus {:?}; handshake on tcp/{}",
                             DEFAULT_DISCOVERY_PORT, extra_targets(ctx),
                             DEFAULT_PAIRING_PORT),
        }
        return;
    }

    // Unreachable off the dry-run path: resolve_key only declines to answer
    // when it has been told not to have effects.
    let (key, ssh_public_key) = match (key, ssh_public_key) {
        (Some(key), Some(line)) => (key, line),
        _ => die("no ssh key to pair with"),
    };

    let (addr, _label) = find_reader(ctx, &args.host);
    let reply = exchange(addr, &ssh_public_key, &mut read_code)
        .unwrap_or_else(|e| die(e));

    println!();
    println!("paired with {}", reply.device_label);
    println!("  sent      {}", ssh_public_key);
    println!("  received  {}", reply.host_public_key);

    match record_host_key(&ctx.known, &reply.host_public_key) {
        KnownHosts::Added => println!(
            "  recorded  {} as {} in {}", reply.host_public_key.split(' ').next()
                .unwrap_or("host key"), ALIAS, ctx.known.display()),
        KnownHosts::AlreadyTrusted => println!(
            "  known     {} already answered for {}", ctx.known.display(), ALIAS),
        KnownHosts::Replaced => println!(
            "  REPLACED  a different host key was recorded for {} in {} — the \
             reader's key has changed", ALIAS, ctx.known.display()),
    }

    // The address the pairing came from is a good first guess for the next
    // push, and it costs one file to remember.
    ctx.write_cache(&addr.ip().to_string());

    // "A pairing that has never moved a file is not paired" (docs/platonic.md).
    // This is the cheapest honest version of that: connect over ssh with the
    // key that was just authorized, verified against the host key that was just
    // recorded.  It proves both files, without needing the reader to be doing
    // anything in particular.
    println!();
    println!("checking ssh with the paired key …");
    let p = probe(&addr.ip().to_string(), &key, &ctx.known, "yes");
    if p.receiver {
        println!("ok — the reader answered as the document receiver.");
        println!("Now: platonic FILE");
    } else if p.reachable {
        println!("ssh works, but the receiver did not answer ({}).", p.note);
        println!("Deploy it (just platonic-recv-install) — the paired key may \
                  run nothing else.");
    } else {
        println!("ssh did not go through ({}).", p.note);
        println!("The keys are in place; try `platonic --list` once the reader \
                  is back on WiFi.");
    }
}

//
// --------------------------------------------------------------------- tests
//

#[cfg(test)]
mod tests {
    use super::*;
    // The reader of these entries in every other code path.  Using it here is
    // the check that what --pair writes is what the tool later recognises.
    use crate::pure::alias_present;

    const KEY_A: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const KEY_B: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";

    // ---- the known_hosts entry

    #[test]
    fn the_alias_line_carries_both_spellings_ssh_looks_up() {
        let line = alias_line(KEY_A);
        assert!(line.starts_with("platokin,[platokin]:2222 "));
        assert!(line.ends_with(KEY_A));
        // and what it produces is what alias_present recognises
        assert!(alias_present(&line));
    }

    #[test]
    fn a_fresh_file_gets_one_entry() {
        let (out, outcome) = merge_known_hosts("", KEY_A);
        assert_eq!(outcome, KnownHosts::Added);
        assert_eq!(out, format!("{}\n", alias_line(KEY_A)));
    }

    #[test]
    fn other_entries_survive_verbatim() {
        // The usbnet entry is what the admin key's ssh has been using all
        // along; pairing must not disturb it.
        let existing = "[192.168.15.244]:2222 ssh-ed25519 AAAAOTHER comment\n\
                        |1|hashed|hashed ssh-ed25519 AAAAHASHED\n\
                        # a comment\n";
        let (out, outcome) = merge_known_hosts(existing, KEY_A);
        assert_eq!(outcome, KnownHosts::Added);
        assert!(out.starts_with(existing), "{:?}", out);
        assert!(out.ends_with(&format!("{}\n", alias_line(KEY_A))));
    }

    #[test]
    fn re_pairing_with_the_same_key_changes_nothing() {
        let existing = format!("{}\n", alias_line(KEY_A));
        let (out, outcome) = merge_known_hosts(&existing, KEY_A);
        assert_eq!(outcome, KnownHosts::AlreadyTrusted);
        assert_eq!(out, existing);
    }

    #[test]
    fn a_changed_host_key_replaces_the_stale_line_rather_than_stacking() {
        // Two lines for one alias is how ssh starts refusing on the *first*
        // match; the peer that just proved it knows the code is the authority.
        let existing = format!("keep.example ssh-ed25519 AAAAKEEP\n{}\n",
                               alias_line(KEY_A));
        let (out, outcome) = merge_known_hosts(&existing, KEY_B);
        assert_eq!(outcome, KnownHosts::Replaced);
        assert!(out.contains("keep.example"));
        assert!(!out.contains("AAAAC3NzaC1lZDI1NTE5AAAAIAAAA"), "{}", out);
        assert_eq!(out.lines().filter(|l| alias_present(l)).count(), 1);
    }

    #[test]
    fn a_bare_alias_entry_counts_as_the_alias() {
        // ssh accepts `platokin` without the bracketed form; so must we, or a
        // hand-written entry survives as a duplicate.
        let (out, outcome) = merge_known_hosts("platokin ssh-ed25519 AAAAOLD\n", KEY_A);
        assert_eq!(outcome, KnownHosts::Replaced);
        assert!(!out.contains("AAAAOLD"));
    }

    #[test]
    fn a_file_without_a_final_newline_does_not_glue_two_entries_together() {
        let (out, _) = merge_known_hosts("other.example ssh-ed25519 AAAAOTHER", KEY_A);
        assert_eq!(out.lines().count(), 2);
    }

    // ---- key material

    #[test]
    fn a_multi_word_comment_is_replaced_rather_than_refused() {
        let line = format!("{} adriaan on the laptop", KEY_A);
        let out = normalize_public_key(&line, "platonic@studio").unwrap();
        assert_eq!(out, format!("{} platonic@studio", KEY_A));
        // and the result is what the wire will accept
        assert!(validate_ssh_public_key(&out).is_ok());
    }

    #[test]
    fn a_key_with_no_comment_gains_one() {
        assert_eq!(normalize_public_key(KEY_A, "platonic@m").unwrap(),
                   format!("{} platonic@m", KEY_A));
    }

    #[test]
    fn junk_is_refused_before_anything_is_sent() {
        assert!(normalize_public_key("", "c").is_err());
        assert!(normalize_public_key("ssh-ed25519", "c").is_err());
        assert!(normalize_public_key("rm -rf /", "c").is_err());
        // an rsa key is legitimate; only the shape is checked
        assert!(normalize_public_key(
            &format!("ssh-rsa {}", "A".repeat(300)), "c").is_ok());
    }

    #[test]
    fn a_hostile_comment_cannot_smuggle_a_second_line() {
        // The comment is derived from the Mac's hostname, which a determined
        // user controls; the device would write it into authorized_keys.
        assert!(normalize_public_key(KEY_A, "a\nssh-ed25519 AAAAEVIL evil").is_err());
        assert!(normalize_public_key(KEY_A, "two words").is_err());
    }

    #[test]
    fn the_comment_names_the_mac_and_is_always_one_token() {
        assert_eq!(key_comment("studio.local"), "platonic@studio");
        assert_eq!(key_comment("Adriaans-MacBook-Pro.local"),
                   "platonic@Adriaans-MacBook-Pro");
        assert_eq!(key_comment(""), "platonic@mac");
        assert_eq!(key_comment("."), "platonic@mac");
        // anything that could break the line is dropped, not escaped
        assert_eq!(key_comment("a b\nc"), "platonic@abc");
        for name in ["x", "", "a b", "…", "a\tb", "-"] {
            let c = key_comment(name);
            assert!(normalize_public_key(KEY_A, &c).is_ok(), "{:?} -> {:?}", name, c);
        }
    }

    #[test]
    fn keygen_asks_for_an_unencrypted_ed25519_key() {
        let argv = keygen_argv(Path::new("/home/u/.ssh/platonic_ed25519"),
                               "platonic@studio");
        assert_eq!(argv[0], "ssh-keygen");
        let t = argv.iter().position(|a| a == "-t").unwrap();
        assert_eq!(argv[t + 1], "ed25519");
        let n = argv.iter().position(|a| a == "-N").unwrap();
        assert_eq!(argv[n + 1], "", "the key must have no passphrase");
        let c = argv.iter().position(|a| a == "-C").unwrap();
        assert_eq!(argv[c + 1], "platonic@studio");
        let f = argv.iter().position(|a| a == "-f").unwrap();
        assert_eq!(argv[f + 1], "/home/u/.ssh/platonic_ed25519");
        // nothing here may name the private key to anything but ssh-keygen
        assert!(!argv.iter().any(|a| a == "-y"));
    }

    #[test]
    fn the_public_key_path_is_the_private_one_plus_pub() {
        assert_eq!(public_key_path(Path::new("/home/u/.ssh/platonic_ed25519")),
                   PathBuf::from("/home/u/.ssh/platonic_ed25519.pub"));
        // not `with_extension`, which would eat a dotted key name
        assert_eq!(public_key_path(Path::new("/tmp/id.test")),
                   PathBuf::from("/tmp/id.test.pub"));
    }

    // ---- discovery and messages

    fn offer(last: u8, label: &str) -> Offer {
        Offer {
            addr: IpAddr::from([192, 168, 178, last]),
            tcp_port: DEFAULT_PAIRING_PORT,
            label: label.to_string(),
        }
    }

    #[test]
    fn one_offer_is_the_reader() {
        let offers = vec![offer(30, "platokin")];
        assert_eq!(choose_offer(&offers).unwrap(), &offers[0]);
    }

    #[test]
    fn several_offers_are_refused_with_the_addresses_to_choose_from() {
        let offers = vec![offer(30, "platokin"), offer(31, "study")];
        let e = choose_offer(&offers).unwrap_err();
        assert!(e.contains("192.168.178.30"), "{}", e);
        assert!(e.contains("study"), "{}", e);
        assert!(e.contains("--host"), "{}", e);
    }

    #[test]
    fn no_offer_names_the_thing_to_tap() {
        let e = choose_offer(&[]).unwrap_err();
        assert!(e.contains("Pair a Mac"), "{}", e);
        assert!(e.contains("--host"), "{}", e);
    }

    #[test]
    fn a_refused_connection_says_the_window_is_shut() {
        let addr: SocketAddr = "192.168.178.30:30305".parse().unwrap();
        let msg = describe_connect_error(
            &addr, &std::io::Error::from(std::io::ErrorKind::ConnectionRefused));
        assert!(msg.contains("Pair a Mac"), "{}", msg);
        assert!(msg.contains("no window is open"), "{}", msg);
        let msg = describe_connect_error(
            &addr, &std::io::Error::from(std::io::ErrorKind::TimedOut));
        assert!(msg.contains("WiFi"), "{}", msg);
    }

    #[test]
    fn a_version_mismatch_names_both_builds() {
        let msg = describe_pair_error(&PairError::Version { ours: 1, theirs: 2 });
        assert!(msg.contains('1') && msg.contains('2'), "{}", msg);
        assert!(msg.contains("redeploy") || msg.contains("rebuild"), "{}", msg);
    }

    #[test]
    fn a_mid_handshake_eof_points_at_the_attempt_cap() {
        let msg = describe_pair_error(&PairError::Io(
            std::io::Error::from(std::io::ErrorKind::UnexpectedEof)));
        assert!(msg.contains("ten wrong codes"), "{}", msg);
        // a timeout is a different story and must not be conflated with it
        assert!(!describe_pair_error(&PairError::Timeout).contains("ten wrong"));
    }

    // ---- the wire, both ends, no device
    //
    // The reader half below is `attempt()` from crates/plato/src/pairing.rs
    // reduced to what crosses the wire: handshake as Role::Reader, receive the
    // hello, answer with the host key.  Everything device-specific (the
    // firewall, authorized_keys, the panel) is what makes that file untestable
    // here, and none of it touches a byte of the protocol.

    use std::net::TcpListener;
    use std::sync::mpsc;

    const HOST_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHOSTHOSTHOSTHOSTHOSTHOSTHOSTHOSTHOSTH";

    /// One reader window on loopback: `attempts` connections are served, the
    /// received hello of the successful one is sent back to the test.
    fn reader_window(code: &str, attempts: usize)
                     -> (SocketAddr, mpsc::Receiver<String>, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let code = Code::parse(code).unwrap();
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            for _ in 0..attempts {
                let Ok((stream, _)) = listener.accept() else { return };
                let mut session = match handshake(stream, Role::Reader, &code,
                                                  &Config::default()) {
                    Ok(s) => s,
                    // A wrong code: the device drops the stream and loops, and
                    // so does this.  That drop is what forces the Mac to
                    // reconnect, which is the behaviour being tested.
                    Err(_) => continue,
                };
                let hello = session.recv_mac_hello().unwrap();
                tx.send(hello.ssh_public_key).unwrap();
                session.send_reader_reply(&ReaderReply {
                    host_public_key: HOST_KEY.to_string(),
                    device_label: "test-kindle".to_string(),
                }).unwrap();
                return;
            }
        });
        (addr, rx, handle)
    }

    #[test]
    fn the_right_code_swaps_the_two_public_keys() {
        let (addr, rx, handle) = reader_window("abcd-2345", 1);
        let sent = format!("{} platonic@studio", KEY_A);
        let reply = exchange(addr, &sent,
                             &mut |_| Code::parse("ABCD 2345").unwrap()).unwrap();
        assert_eq!(reply.host_public_key, HOST_KEY);
        assert_eq!(reply.device_label, "test-kindle");
        // and the reader got exactly the line we meant to send -- public half,
        // one line, our comment
        assert_eq!(rx.recv().unwrap(), sent);
        handle.join().unwrap();
    }

    #[test]
    fn a_wrong_code_reconnects_and_the_next_one_pairs() {
        // The device closes the socket after every attempt, so this is the
        // path a mistyped code really takes.
        let (addr, rx, handle) = reader_window("abcd-2345", 2);
        let mut typed = 0;
        let reply = exchange(addr, KEY_A, &mut |attempt| {
            typed += 1;
            assert_eq!(typed, attempt);
            if attempt == 1 { Code::parse("zzzz-2345").unwrap() }
            else { Code::parse("abcd-2345").unwrap() }
        }).unwrap();
        assert_eq!(reply.host_public_key, HOST_KEY);
        assert_eq!(typed, 2);
        assert_eq!(rx.recv().unwrap(), KEY_A);
        handle.join().unwrap();
    }

    #[test]
    fn a_closed_window_is_reported_as_one_rather_than_as_a_crash() {
        // Bind, then drop: the port exists in the test's head and nowhere
        // else, which is exactly what a Mac sees after the window expires.
        let addr = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        };
        let e = exchange(addr, KEY_A, &mut |_| Code::parse("abcd-2345").unwrap())
            .unwrap_err();
        assert!(e.contains("Pair a Mac") || e.contains("could not reach"), "{}", e);
    }

    #[test]
    fn a_peer_that_hangs_up_never_sees_the_key() {
        // The security property, from this side: the key goes out only after
        // key confirmation, so a peer that never confirms gets nothing.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            // Read whatever arrives, answer nothing, close.
            let mut buf = [0u8; 64];
            use std::io::Read;
            let mut s = stream;
            let _ = s.read(&mut buf);
            drop(s);
            buf
        });
        let e = exchange(addr, KEY_A, &mut |_| Code::parse("abcd-2345").unwrap())
            .unwrap_err();
        assert!(!e.contains(KEY_A), "the key must not appear anywhere: {}", e);
        let seen = handle.join().unwrap();
        assert!(!seen.windows(8).any(|w| w == &KEY_A.as_bytes()[..8]),
                "the key reached an unconfirmed peer");
    }

    /// The ports are the shared crate's constants, never re-declared here --
    /// this asserts the values so a wire change shows up as a failing test on
    /// both ends rather than as a Mac that cannot find a reader.
    #[test]
    fn the_ports_come_from_the_shared_crate() {
        assert_eq!(DEFAULT_PAIRING_PORT, 30305);
        assert_eq!(DEFAULT_DISCOVERY_PORT, 30304);
        assert_ne!(Role::Mac, Role::Reader);
    }
}
