//! The pure half of `platonic`: naming, filetype gates, the remote command
//! strings, the clock arithmetic and the small parsers.
//!
//! Nothing here touches the network, the filesystem or the clock, which is the
//! shape the Python original had and the reason its 32 tests could run without
//! a device.  `main.rs` and `session.rs` do the I/O and call in here for every
//! decision.

use std::path::{Path, PathBuf};

use platonic_recv::proto::Entry;

/// What Plato can render (docs/platonic.md "Any filetype").  Anything else
/// that is UTF-8 text ships as fenced `.md`; binary is refused.
///
/// Kept sorted so `corrected_kinds_line` is deterministic.
pub const RENDERABLE: &[&str] = &[
    "cbz", "djvu", "epub", "fb2", "html", "md", "mobi", "oxps", "pdf", "txt",
    "xps",
];

pub const SSH_PORT: &str = "2222";
/// HostKeyAlias: identity independent of address.
pub const ALIAS: &str = "platokin";
/// volumd's configured usbnet address.
pub const USB_ADDR: &str = "192.168.15.244";
pub const DOCROOT: &str = "/mnt/us/documents";
pub const FIFO: &str = "/tmp/plato.cmd";
#[allow(dead_code)] // dormant since --pair landed; see DORMANT below
pub const SETTINGS_TOML: &str = "/mnt/us/plato/Settings.toml";
pub const DEFAULT_DAYS: i64 = 14;

pub const NO_READER_MSG: &str = "no reader found — is it awake with WiFi on?";

/// The name rungs of the discovery ladder, cheapest first.
///
/// 1. The bare name, answered by the router *if* it registers DHCP client
///    names.  Ours does; a public user's may not, which is why this is a fast
///    path rather than a mechanism (docs/pairing-candidates.md).
/// 2. `<name>.local`, answered by the reader itself over mDNS — no router
///    cooperation at all.  macOS resolves it natively through mDNSResponder,
///    so this rung costs the Mac nothing but a name.
///
/// The order is load-bearing: rung 1 is one unicast query to a resolver that
/// usually has the answer cached, rung 2 is a multicast round trip that waits
/// for the reader to be awake and associated.  On a network where both work,
/// paying for the cheap one first is free.
///
/// Both rungs are still verified by connecting under the HostKeyAlias, so a
/// name hijacked by anything else on the LAN fails the probe rather than
/// receiving a document.
pub fn dns_names(alias: &str) -> Vec<String> {
    vec![alias.to_string(), format!("{}.local", alias)]
}

//
// ------------------------------------------------------------------ identity
//
// The ssh key is NOT fixed (Adriaan, 2026-08-12).  `platokin_ed25519` is the
// *admin* key -- the justfile, the backups, the WiFi scripts and the Plato
// deploys all use it, and it stays exactly as it is.  Pairing will mint a
// per-Mac, individually revocable document-push key; until it exists on this
// Mac we fall back to the admin key, so every already-set-up Mac keeps working
// with no migration at all.
//
// The host-key file does not layer the same way: it holds the *device's* key,
// which has nothing to do with which client key we authenticate with, so it
// keeps its name.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identity {
    /// `PLATONIC_KEY` was set.
    Env,
    /// `~/.ssh/platonic_ed25519`, the per-Mac key pairing mints.
    PerMac,
    /// `~/.ssh/platokin_ed25519`, the shared admin key.
    Admin,
}

impl Identity {
    /// The per-Mac key is what a paired Mac is meant to use; anything else is
    /// worth naming out loud, so an auth failure names the key it really used.
    pub fn is_default(self) -> bool {
        self == Identity::PerMac
    }
}

/// `PLATONIC_KEY`, else the per-Mac key if it exists, else the admin key.
///
/// `exists` is injected so the order can be tested without laying down files.
pub fn choose_key(env: Option<&str>, home: &Path,
                  exists: &dyn Fn(&Path) -> bool) -> (PathBuf, Identity) {
    if let Some(path) = env.filter(|s| !s.is_empty()) {
        return (PathBuf::from(path), Identity::Env);
    }
    let per_mac = home.join(".ssh/platonic_ed25519");
    if exists(&per_mac) {
        return (per_mac, Identity::PerMac);
    }
    (home.join(".ssh/platokin_ed25519"), Identity::Admin)
}

/// `PLATONIC_KNOWN_HOSTS`, else `~/.ssh/platokin_known_hosts`.
pub fn choose_known_hosts(env: Option<&str>, home: &Path) -> PathBuf {
    match env.filter(|s| !s.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => home.join(".ssh/platokin_known_hosts"),
    }
}

//
// ------------------------------------------------------------------- quoting
//
// Filenames on this library really do contain spaces (`Cutting Through
// Spiritual Materialism - Chogyam Trungpa.epub`), and every remote path is
// interpolated into a command that runs through the device's shell.  This is
// Python's `shlex.quote`, byte for byte, so the emitted commands can be diffed
// against the original tool's.

pub fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return String::from("''");
    }
    let safe = s.chars().all(|c| {
        c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c)
    });
    if safe {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

//
// ------------------------------------------------------------- naming, kinds
//

/// One path component, nothing else.  This value is interpolated into a remote
/// path that runs through a shell, so it is an injection site as much as a
/// correctness one.
pub fn validate_to(name: &str) -> Result<&str, String> {
    if name.is_empty() {
        return Err("--to must not be empty".to_string());
    }
    if name.contains('/') || name.contains('\\') {
        return Err(format!("--to must be a single folder name, got '{}'", name));
    }
    if name == "." || name == ".." {
        return Err(format!("--to must not be '{}'", name));
    }
    if name.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7F) {
        return Err("--to contains control characters".to_string());
    }
    Ok(name)
}

/// Text means: decodes as UTF-8 and contains no NULs.  Detection is by
/// content, not extension -- the interesting cases (`justfile`, `.envrc`, a
/// scratch file with no suffix) have no extension to go on.
pub fn is_text(data: &[u8]) -> bool {
    !data.contains(&0) && std::str::from_utf8(data).is_ok()
}

pub fn slugify(stem: &str) -> String {
    let mut out = String::new();
    let mut in_run = false;
    for c in stem.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
            out.push(c);
            in_run = false;
        } else if !in_run {
            out.push('-');
            in_run = true;
        }
    }
    let slug = out.trim_matches(|c| c == '-' || c == '.');
    if slug.is_empty() { "doc".to_string() } else { slug.to_string() }
}

/// `<repo>-<slug>.<ext>`.  The prefix disambiguates two projects both having a
/// `docs/plan.md`; the library shows the first heading, not this name.
pub fn dest_name(repo: Option<&str>, stem: &str, ext: &str) -> String {
    let slug = slugify(stem);
    match repo {
        Some(repo) if !repo.is_empty() => format!("{}-{}.{}", repo, slug, ext),
        _ => format!("{}.{}", slug, ext),
    }
}

/// The library entry is the first heading; without one it reads like a path.
/// Only the leading heading matters.
pub fn ensure_md_heading(text: &str, title: &str) -> String {
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if line.trim_start().starts_with('#') {
            return text.to_string();
        }
        break;
    }
    format!("# {}\n\n{}", title, text)
}

/// Ship arbitrary text as `.md` in a fenced code block, so it lands in
/// monospace with `css/md.css` doing the work.  The fence must be longer than
/// any backtick run in the content.
pub fn wrap_fenced(text: &str, title: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let fence = "`".repeat(std::cmp::max(3, longest + 1));
    let mut body = text.to_string();
    if !body.ends_with('\n') {
        body.push('\n');
    }
    format!("# {}\n\n{}\n{}{}\n", title, fence, body, fence)
}

//
// --------------------------------------------------------------------- clock
//
// The device's clock reads 2023 and drifts, so age is NEVER computed against
// it.  Every pushed file is stamped with the Mac's current time, and the
// expiry sweep compares those mtimes against the Mac's clock.

/// busybox `touch -t YYYYMMDDhhmm.SS` interprets the stamp in the device's
/// local time zone.  ASSUMPTION: the device's TZ is UTC (no TZ set, glibc
/// defaults to UTC), so the stamp is rendered from the Mac's clock in UTC.
pub fn touch_stamp(epoch: f64) -> String {
    let secs = epoch as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{:04}{:02}{:02}{:02}{:02}.{:02}",
            y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// Howard Hinnant's `civil_from_days`: days since the Unix epoch -> (y, m, d).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn cutoff_epoch(now: f64, days: i64) -> f64 {
    now - (days as f64) * 86_400.0
}

//
// ------------------------------------------------------------------- parsers
//

/// `KEY=VALUE` lines; blank lines and `#` comments ignored.
pub fn parse_config(text: &str) -> Vec<(String, String)> {
    let mut cfg = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || !line.contains('=') {
            continue;
        }
        let (k, v) = line.split_once('=').unwrap();
        cfg.push((k.trim().to_string(), v.trim().to_string()));
    }
    cfg
}

pub fn config_get<'a>(cfg: &'a [(String, String)], key: &str) -> Option<&'a str> {
    cfg.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// macOS `arp -an` drops leading zeros (`a:b:c:…`); normalise both sides
/// before comparing.
pub fn norm_mac(mac: &str) -> Result<String, String> {
    let parts: Vec<&str> = mac.trim().split(':').collect();
    if parts.len() != 6 {
        return Err(format!("not a MAC: {:?}", mac));
    }
    let mut out = Vec::with_capacity(6);
    for p in parts {
        let n = u8::from_str_radix(p, 16)
            .map_err(|_| format!("not a MAC: {:?}", mac))?;
        out.push(format!("{:02x}", n));
    }
    Ok(out.join(":"))
}

/// `arp -an` lines -> (normalised mac, ip) pairs.
pub fn parse_arp(output: &str) -> Vec<(String, String)> {
    let mut table = Vec::new();
    for line in output.lines() {
        let bytes: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] != '(' {
                i += 1;
                continue;
            }
            // (ip) at mac
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == '.') {
                j += 1;
            }
            if j == i + 1 || j >= bytes.len() || bytes[j] != ')' {
                i += 1;
                continue;
            }
            let ip: String = bytes[i + 1..j].iter().collect();
            let rest: String = bytes[j + 1..].iter().collect();
            if let Some(tail) = rest.strip_prefix(" at ") {
                let mac: String = tail.chars()
                    .take_while(|c| c.is_ascii_hexdigit() || *c == ':')
                    .collect();
                if !mac.is_empty() {
                    if let Ok(norm) = norm_mac(&mac) {
                        table.retain(|(m, _): &(String, String)| *m != norm);
                        table.push((norm, ip));
                    }
                }
            }
            i = j + 1;
        }
    }
    table
}

pub fn arp_lookup(table: &[(String, String)], mac: &str) -> Option<String> {
    table.iter().find(|(m, _)| m == mac).map(|(_, ip)| ip.clone())
}

// DORMANT, and deliberately kept.  `--pair` (src/pair.rs) landed 2026-08-12
// and made two of these unnecessary rather than wrong: the reader now SENDS its
// host key over the confirmed channel, so the alias entry is written from that
// instead of derived from an address-keyed line somebody else wrote
// (`derive_alias_line`, `find_source_line`).  The `allowed-kinds` trio is the
// check docs/platonic.md asks --pair to make, which cannot be made yet: a
// paired key may run only `platonic-recv`, and the receiver has no op that
// reads `Settings.toml`.  Both are cheap to keep and expensive to re-derive.

fn known_hosts_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines().map(|l| l.trim()).filter(|l| {
        !l.is_empty() && !l.starts_with('#') && !l.starts_with('|')
    })
}

/// Does any known_hosts line already answer for the alias?  ssh looks up
/// `[platokin]:2222` for a non-default port; accept the bare name too.
#[allow(dead_code)] // dormant since --pair landed; see DORMANT below
pub fn alias_present(known_text: &str) -> bool {
    let bracketed = format!("[{}]:{}", ALIAS, SSH_PORT);
    known_hosts_lines(known_text).any(|line| {
        line.split_whitespace().next().unwrap_or("").split(',')
            .any(|h| h == ALIAS || h == bracketed)
    })
}

/// Rewrite an existing known_hosts line's host field to answer for the alias,
/// keeping the key material verbatim.  Hashed lines (`|1|…`) cannot be derived
/// from.
#[allow(dead_code)] // dormant since --pair landed; see DORMANT below
pub fn derive_alias_line(line: &str) -> Result<String, String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') || line.starts_with('|') {
        return Err("cannot derive an alias from this line".to_string());
    }
    match line.split_once(char::is_whitespace) {
        Some((_, rest)) if !rest.trim_start().is_empty() => {
            Ok(format!("{},[{}]:{} {}", ALIAS, ALIAS, SSH_PORT, rest.trim_start()))
        }
        _ => Err("malformed known_hosts line".to_string()),
    }
}

/// The line to derive the alias from: the entry keyed to the usbnet address on
/// our port.
#[allow(dead_code)] // dormant since --pair landed; see DORMANT below
pub fn find_source_line(known_text: &str) -> Option<String> {
    let wanted = format!("[{}]:{}", USB_ADDR, SSH_PORT);
    known_hosts_lines(known_text)
        .find(|line| {
            line.split_whitespace().next().unwrap_or("").split(',')
                .any(|h| h == wanted)
        })
        .map(|l| l.to_string())
}

/// Extract the `allowed-kinds` array from Plato's `Settings.toml`.  Returns
/// (kinds, the matched line).
#[allow(dead_code)] // dormant since --pair landed; see DORMANT below
pub fn parse_allowed_kinds(toml_text: &str) -> Option<(Vec<String>, String)> {
    for line in toml_text.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("allowed-kinds") else { continue };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=') else { continue };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('[') else { continue };
        let Some(inner) = rest.rsplit_once(']') else { continue };
        if !inner.1.trim().is_empty() {
            continue;
        }
        let mut kinds = Vec::new();
        let mut chars = inner.0.chars();
        while let Some(c) = chars.next() {
            if c == '"' {
                let s: String = chars.by_ref().take_while(|c| *c != '"').collect();
                if !s.is_empty() {
                    kinds.push(s);
                }
            }
        }
        return Some((kinds, line.to_string()));
    }
    None
}

#[allow(dead_code)] // dormant since --pair landed; see DORMANT below
pub fn corrected_kinds_line(kinds: &[String]) -> String {
    let mut ordered: Vec<String> = kinds.to_vec();
    for k in RENDERABLE {
        if !kinds.iter().any(|have| have == k) {
            ordered.push((*k).to_string());
        }
    }
    let inner: Vec<String> = ordered.iter().map(|k| format!("\"{}\"", k)).collect();
    format!("allowed-kinds = [{}]", inner.join(", "))
}

#[allow(dead_code)] // dormant since --pair landed; see DORMANT below
pub fn missing_kinds(kinds: &[String]) -> Vec<&'static str> {
    RENDERABLE.iter().copied()
        .filter(|k| !kinds.iter().any(|have| have == k))
        .collect()
}

/// (mtime, size, path), as the shell path's `stat` prints them.
pub type StatEntry = (i64, u64, String);

/// busybox `stat -c '%Y %s %n'` lines -> (mtime, size, path).  Paths contain
/// spaces on this library, so only the two numeric fields are split off.
pub fn parse_stat_lines(output: &str) -> Vec<StatEntry> {
    let mut entries = Vec::new();
    for line in output.lines() {
        let line = line.trim_start();
        let Some((mtime, rest)) = line.split_once(char::is_whitespace) else { continue };
        let Some((size, path)) = rest.trim_start().split_once(char::is_whitespace)
            else { continue };
        let path = path.trim_start();
        if path.is_empty() {
            continue;
        }
        let (Ok(mtime), Ok(size)) = (mtime.parse::<i64>(), size.parse::<u64>())
            else { continue };
        entries.push((mtime, size, path.to_string()));
    }
    entries
}

pub fn basename(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((_, name)) if !name.is_empty() => name.to_string(),
        _ => path.to_string(),
    }
}

/// Shell-path listings carry absolute paths; the receiver reports folder and
/// name separately.  Convert, so both links answer with the same type and
/// `--list` has one implementation.
pub fn entries_from_stat(entries: &[StatEntry]) -> Vec<Entry> {
    let prefix = format!("{}/", DOCROOT);
    entries.iter().map(|(mtime, size, path)| {
        let rel = path.strip_prefix(&prefix).unwrap_or(path.as_str());
        let (folder, name) = match rel.rsplit_once('/') {
            Some((f, n)) => (f.to_string(), n.to_string()),
            None => (".".to_string(), rel.to_string()),
        };
        Entry { folder, name, size: *size, mtime: *mtime }
    }).collect()
}

/// A size a person reads at a glance.  `--list` gained sizes when the
/// receiver started reporting them; a raw byte count for a 34 MB paper is
/// noise.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [(&str, u64); 3] = [("M", 1024 * 1024), ("K", 1024), ("B", 1)];
    for (suffix, scale) in UNITS {
        if bytes >= scale * 10 || (scale == 1) {
            return format!("{}{}", bytes / scale, suffix);
        }
        if bytes >= scale {
            return format!("{:.1}{}", bytes as f64 / scale as f64, suffix);
        }
    }
    format!("{}B", bytes)
}

pub fn expired_paths(entries: &[StatEntry], cutoff: f64) -> Vec<String> {
    entries.iter()
        .filter(|(mtime, _, _)| (*mtime as f64) <= cutoff)
        .map(|(_, _, p)| p.clone())
        .collect()
}

//
// ---------------------------------------------------------- remote  commands
//

/// Every ssh invocation goes through this: HostKeyAlias makes the host-key
/// check independent of the address, so a candidate that is not our reader is
/// refused by ssh itself.
pub fn ssh_argv(host: &str, remote_cmd: &str, key: &Path, known: &Path,
                strict: &str) -> Vec<String> {
    vec![
        "ssh".to_string(),
        "-p".to_string(), SSH_PORT.to_string(),
        "-i".to_string(), key.display().to_string(),
        "-o".to_string(), format!("HostKeyAlias={}", ALIAS),
        "-o".to_string(), format!("UserKnownHostsFile={}", known.display()),
        "-o".to_string(), format!("StrictHostKeyChecking={}", strict),
        "-o".to_string(), "BatchMode=yes".to_string(),
        "-o".to_string(), "ConnectTimeout=3".to_string(),
        // The receiver session holds one connection open for the whole push,
        // and a pipe has no timeout of its own: this is what bounds a reader
        // that goes to sleep mid-transfer.  30 s of silence and ssh gives up.
        "-o".to_string(), "ServerAliveInterval=5".to_string(),
        "-o".to_string(), "ServerAliveCountMax=6".to_string(),
        format!("root@{}", host),
        remote_cmd.to_string(),
    ]
}

/// One round trip: create the folder, stream the bytes, stamp the mtime with
/// the Mac's clock, and echo back the size so the caller can verify by OUTPUT
/// (busybox exit codes are not to be trusted).
pub fn push_command(remote_dir: &str, remote_path: &str, stamp: &str) -> String {
    format!("mkdir -p {} && cat > {} && touch -t {} {} && wc -c < {}",
            shell_quote(remote_dir), shell_quote(remote_path), stamp,
            shell_quote(remote_path), shell_quote(remote_path))
}

/// Write one line to Plato's FIFO, guarded against blocking forever (a blocked
/// write means nothing is reading).  `timeout` exists as a busybox applet
/// (bin-listing), but its option spelling differs across busybox generations:
/// old is `timeout -t SECS CMD`, new is `timeout SECS CMD`.  Try old-first; a
/// wrong spelling errors immediately and falls through.
pub fn fifo_write_command(line: &str) -> String {
    let inner = format!("printf '%s\\n' {} > {}", shell_quote(line), FIFO);
    let q = shell_quote(&inner);
    format!("timeout -t 3 sh -c {} 2>/dev/null || timeout 3 sh -c {}", q, q)
}

pub fn sweep_list_command() -> String {
    format!("find {}/inbox -type f -exec stat -c '%Y %s %n' {{}} \\; 2>/dev/null",
            DOCROOT)
}

pub fn list_command() -> String {
    format!("find {} -mindepth 1 -maxdepth 2 -type f \
             -exec stat -c '%Y %s %n' {{}} \\; 2>/dev/null", DOCROOT)
}

pub fn rm_command(paths: &[String]) -> String {
    let quoted: Vec<String> = paths.iter().map(|p| shell_quote(p)).collect();
    format!("rm -f -- {}", quoted.join(" "))
}

//
// --------------------------------------------------------------------- tests
//

#[cfg(test)]
mod tests {
    use super::*;

    // ---- the discovery ladder's name rungs

    #[test]
    fn dns_names_puts_the_router_name_before_the_mdns_one() {
        assert_eq!(dns_names(ALIAS), vec!["platokin", "platokin.local"]);
    }

    #[test]
    fn dns_names_follows_a_renamed_reader() {
        // The responder's name is a Settings key; rename it there and the
        // `.local` rung has to follow, or the ladder probes a name nothing
        // answers to.
        assert_eq!(dns_names("study"), vec!["study", "study.local"]);
    }

    #[test]
    fn the_mdns_rung_is_not_the_usb_address() {
        // Ordering guard for the ladder as a whole: the `.local` name is tried
        // while still on WiFi, BEFORE falling back to the usbnet address --
        // which only answers with the cable in.
        let names = dns_names(ALIAS);
        assert!(!names.iter().any(|n| n == USB_ADDR));
        assert_eq!(names.last().unwrap(), "platokin.local");
    }

    // ---- --to validation

    #[test]
    fn validate_to_accepts_plain_names() {
        for name in ["inbox", "papers", "to-read", "a.b", "notes_2026"] {
            assert_eq!(validate_to(name).unwrap(), name);
        }
    }

    #[test]
    fn validate_to_rejects_traversal_and_absolute() {
        for bad in ["", "/", "/etc", "..", ".", "a/b", "../x", "a\\b",
                    "inbox/", "x\n", "x\0y", "\ta"] {
            assert!(validate_to(bad).is_err(), "{:?} should be refused", bad);
        }
    }

    // ---- text detection

    #[test]
    fn utf8_is_text() {
        assert!(is_text("héllo wörld\n".as_bytes()));
        assert!(is_text(b""));
    }

    #[test]
    fn nul_and_invalid_utf8_are_binary() {
        assert!(!is_text(b"abc\x00def"));
        assert!(!is_text(b"\xff\xfe\x00\x01"));
        assert!(!is_text(b"%PDF-1.4 \xc3"));
    }

    // ---- naming

    #[test]
    fn slug_sanitises() {
        assert_eq!(slugify("auth rework (v2)"), "auth-rework-v2");
        assert_eq!(slugify("Chögyam!"), "Ch-gyam");
        assert_eq!(slugify("???"), "doc");
    }

    #[test]
    fn repo_prefix() {
        assert_eq!(dest_name(Some("platokin"), "plan", "md"), "platokin-plan.md");
        assert_eq!(dest_name(None, "plan", "md"), "plan.md");
    }

    #[test]
    fn full_name_kept_for_wrapped_text() {
        // a .py wrapped as md keeps its identity: stem is the whole filename
        assert_eq!(dest_name(Some("repo"), "foo.py", "md"), "repo-foo.py.md");
    }

    // ---- markdown heading

    #[test]
    fn leading_heading_untouched() {
        let text = "# Title\n\nbody\n";
        assert_eq!(ensure_md_heading(text, "x"), text);
    }

    #[test]
    fn heading_after_blank_lines_untouched() {
        let text = "\n\n## Sub\nbody\n";
        assert_eq!(ensure_md_heading(text, "x"), text);
    }

    #[test]
    fn heading_injected() {
        let out = ensure_md_heading("just prose\n", "My Title");
        assert!(out.starts_with("# My Title\n\n"));
        assert!(out.contains("just prose"));
    }

    #[test]
    fn body_heading_does_not_count() {
        let out = ensure_md_heading("prose\n# late heading\n", "T");
        assert!(out.starts_with("# T\n"));
    }

    // ---- fenced wrapping

    #[test]
    fn fence_basic() {
        let out = wrap_fenced("print('hi')\n", "foo.py");
        assert!(out.starts_with("# foo.py\n\n```\n"));
        assert!(out.ends_with("```\n"));
        assert!(out.contains("print('hi')\n"));
    }

    #[test]
    fn fence_outgrows_content_backticks() {
        let out = wrap_fenced("a ```` b\n", "t");
        // the content holds a 4-run, so the fence must be at least 5
        assert!(out.contains("\n`````\n"));
    }

    #[test]
    fn fence_adds_missing_trailing_newline() {
        assert!(wrap_fenced("no newline", "t").contains("no newline\n"));
    }

    // ---- commands

    #[test]
    fn ssh_argv_shape() {
        let key = PathBuf::from("/home/u/.ssh/platokin_ed25519");
        let known = PathBuf::from("/home/u/.ssh/platokin_known_hosts");
        let argv = ssh_argv("192.168.178.30", "echo hi", &key, &known, "yes");
        assert_eq!(argv[0], "ssh");
        assert!(argv.iter().any(|a| a == "HostKeyAlias=platokin"));
        let p = argv.iter().position(|a| a == "-p").unwrap();
        assert_eq!(argv[p + 1], "2222");
        assert!(argv.iter().any(|a| a == "StrictHostKeyChecking=yes"));
        assert!(argv.iter().any(|a| a == "BatchMode=yes"));
        assert!(argv.iter().any(|a| a == "ConnectTimeout=3"));
        let i = argv.iter().position(|a| a == "-i").unwrap();
        assert_eq!(argv[i + 1], key.display().to_string());
        assert_eq!(argv[argv.len() - 2], "root@192.168.178.30");
        assert_eq!(argv[argv.len() - 1], "echo hi");
    }

    #[test]
    fn push_command_quotes_spaces() {
        let cmd = push_command("/mnt/us/documents/inbox",
                               "/mnt/us/documents/inbox/repo-A File - Name.pdf",
                               "202608121200.00");
        assert!(cmd.contains("'/mnt/us/documents/inbox/repo-A File - Name.pdf'"));
        assert!(cmd.contains("mkdir -p /mnt/us/documents/inbox"));
        assert!(cmd.contains("touch -t 202608121200.00"));
        assert!(cmd.contains("wc -c <"));
    }

    #[test]
    fn push_command_defuses_injection() {
        let evil = "/mnt/us/documents/inbox/a;rm -rf $HOME`x`.md";
        let cmd = push_command("/mnt/us/documents/inbox", evil, "0");
        let quoted = shell_quote(evil);
        assert!(cmd.contains(&quoted));
        // the dangerous chars must only appear inside the quoted region
        assert!(!cmd.replace(&quoted, "").contains(";rm"));
    }

    #[test]
    fn fifo_write_command_quotes_line() {
        let cmd = fifo_write_command("open /mnt/us/documents/inbox/has space.md");
        assert!(cmd.contains("timeout"));
        assert!(cmd.contains("/tmp/plato.cmd"));
        // the whole payload rides inside one quoted sh -c argument
        let inner = shell_quote(concat!(
            "printf '%s\\n' 'open /mnt/us/documents/inbox/has space.md'",
            " > /tmp/plato.cmd"));
        assert!(cmd.contains(&inner), "{}", cmd);
    }

    #[test]
    fn shell_quote_matches_python_shlex() {
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("plain-name_1.md"), "plain-name_1.md");
        assert_eq!(shell_quote("/mnt/us/a b"), "'/mnt/us/a b'");
        assert_eq!(shell_quote("it's"), "'it'\"'\"'s'");
    }

    // ---- clock

    #[test]
    fn cutoff() {
        let now = 1_770_000_000.0;
        assert_eq!(cutoff_epoch(now, 14), now - 14.0 * 86_400.0);
    }

    #[test]
    fn touch_stamp_is_utc() {
        // 2026-08-12 09:30:05 UTC
        assert_eq!(touch_stamp(1_786_527_005.0), "202608120930.05");
        assert_eq!(touch_stamp(0.0), "197001010000.00");
        // leap day, and the last second of a year
        assert_eq!(touch_stamp(1_709_164_800.0), "202402290000.00");
        assert_eq!(touch_stamp(1_767_225_599.0), "202512312359.59");
    }

    #[test]
    fn expired_selection() {
        let entries = vec![(100, 1, "/i/old.md".to_string()),
                           (200, 1, "/i/edge.md".to_string()),
                           (201, 1, "/i/fresh.md".to_string())];
        assert_eq!(expired_paths(&entries, 200.0),
                   vec!["/i/old.md".to_string(), "/i/edge.md".to_string()]);
    }

    // ---- known_hosts

    const SAMPLE: &str =
        "[192.168.15.244]:2222 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIabc comment\n";

    #[test]
    fn derive_alias() {
        assert_eq!(derive_alias_line(SAMPLE).unwrap(),
                   "platokin,[platokin]:2222 ssh-ed25519 \
                    AAAAC3NzaC1lZDI1NTE5AAAAIabc comment");
    }

    #[test]
    fn derive_refuses_hashed() {
        assert!(derive_alias_line("|1|hash|hash ssh-ed25519 AAAA").is_err());
    }

    #[test]
    fn alias_present_detects_both_spellings() {
        assert!(!alias_present(SAMPLE));
        let with = format!("{}{}\n", SAMPLE, derive_alias_line(SAMPLE).unwrap());
        assert!(alias_present(&with));
        assert!(alias_present("platokin ssh-ed25519 AAAA\n"));
    }

    #[test]
    fn find_source_line_prefers_2222() {
        let text = format!("[192.168.15.244]:2223 ssh-ed25519 OTHERKEY\n{}", SAMPLE);
        assert_eq!(find_source_line(&text).unwrap(), SAMPLE.trim());
        assert!(find_source_line("[192.168.15.244]:2223 ssh-ed25519 OTHERKEY\n")
                .is_none());
    }

    // ---- config / arp

    #[test]
    fn config_parse() {
        let cfg = parse_config("# the reader\nmac = aa:bb:cc:dd:ee:ff\n\nx=1=2\n");
        assert_eq!(config_get(&cfg, "mac"), Some("aa:bb:cc:dd:ee:ff"));
        assert_eq!(config_get(&cfg, "x"), Some("1=2"));
    }

    #[test]
    fn mac_normalisation() {
        assert_eq!(norm_mac("A:B:0C:DD:EE:F").unwrap(), "0a:0b:0c:dd:ee:0f");
        assert!(norm_mac("not-a-mac").is_err());
    }

    #[test]
    fn arp_parsing() {
        let out = "? (192.168.178.30) at a:b:c:dd:ee:f on en0 ifscope [ethernet]\n\
                   ? (192.168.178.1) at 11:22:33:44:55:66 on en0\n\
                   ? (192.168.178.255) at (incomplete) on en0\n";
        let table = parse_arp(out);
        assert_eq!(arp_lookup(&table, "0a:0b:0c:dd:ee:0f").unwrap(),
                   "192.168.178.30");
        assert_eq!(arp_lookup(&table, "11:22:33:44:55:66").unwrap(),
                   "192.168.178.1");
        assert_eq!(table.len(), 2);
    }

    // ---- stat output

    #[test]
    fn stat_paths_with_spaces_survive() {
        let out = "1786000000 4096 /mnt/us/documents/inbox/repo-plan.md\n\
                   1786000001 12 /mnt/us/documents/books/Cutting Through Spiritual \
                   Materialism - Chogyam Trungpa.epub\n\
                   garbage line\n";
        let entries = parse_stat_lines(out);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].1, 4096);
        assert_eq!(entries[1].2,
                   "/mnt/us/documents/books/Cutting Through Spiritual \
                    Materialism - Chogyam Trungpa.epub");
    }

    #[test]
    fn shell_listings_become_the_same_entries_the_receiver_returns() {
        let entries = entries_from_stat(&parse_stat_lines(
            "1786000000 5 /mnt/us/documents/inbox/a.md\n"));
        assert_eq!(entries, vec![Entry {
            folder: "inbox".to_string(), name: "a.md".to_string(),
            size: 5, mtime: 1_786_000_000,
        }]);
    }

    #[test]
    fn sizes_read_at_a_glance() {
        assert_eq!(human_size(0), "0B");
        assert_eq!(human_size(999), "999B");
        assert_eq!(human_size(2048), "2.0K");
        assert_eq!(human_size(34 * 1024 * 1024), "34M");
    }

    #[test]
    fn basename_of_a_library_path() {
        assert_eq!(basename("/mnt/us/documents/inbox/a b.md"), "a b.md");
        assert_eq!(basename("bare.md"), "bare.md");
    }

    // ---- Settings.toml

    #[test]
    fn allowed_kinds_parse_and_fix() {
        let toml = "library-path = \"/mnt/us/documents\"\n\
                    allowed-kinds = [\"pdf\", \"epub\"]\n";
        let (kinds, _line) = parse_allowed_kinds(toml).unwrap();
        assert_eq!(kinds, vec!["pdf".to_string(), "epub".to_string()]);
        let fixed = corrected_kinds_line(&kinds);
        // existing order preserved, missing kinds appended sorted
        assert!(fixed.starts_with("allowed-kinds = [\"pdf\", \"epub\", "));
        for k in RENDERABLE {
            assert!(fixed.contains(&format!("\"{}\"", k)));
        }
        assert!(!missing_kinds(&kinds).is_empty());
    }

    #[test]
    fn allowed_kinds_absent_key() {
        assert!(parse_allowed_kinds("foo = 1\n").is_none());
    }

    // ---- identity resolution (Adriaan, 2026-08-12)

    #[test]
    fn key_env_override_wins() {
        let home = Path::new("/home/u");
        let (path, id) = choose_key(Some("/tmp/other_key"), home, &|_| true);
        assert_eq!(path, PathBuf::from("/tmp/other_key"));
        assert_eq!(id, Identity::Env);
    }

    #[test]
    fn key_per_mac_preferred_when_present() {
        let home = Path::new("/home/u");
        let (path, id) = choose_key(None, home, &|p: &Path| {
            p.ends_with(".ssh/platonic_ed25519")
        });
        assert_eq!(path, home.join(".ssh/platonic_ed25519"));
        assert_eq!(id, Identity::PerMac);
        assert!(id.is_default());
    }

    #[test]
    fn key_falls_back_to_admin_key() {
        let home = Path::new("/home/u");
        let (path, id) = choose_key(None, home, &|_| false);
        assert_eq!(path, home.join(".ssh/platokin_ed25519"));
        assert_eq!(id, Identity::Admin);
        assert!(!id.is_default());
    }

    #[test]
    fn known_hosts_keeps_its_name() {
        let home = Path::new("/home/u");
        assert_eq!(choose_known_hosts(None, home),
                   home.join(".ssh/platokin_known_hosts"));
        assert_eq!(choose_known_hosts(Some("/tmp/kh"), home),
                   PathBuf::from("/tmp/kh"));
    }
}
