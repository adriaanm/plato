//! `platonic` -- push a document to the Kindle and start reading it.
//!
//! Spec: `docs/platonic.md` in the platokin repo.  The premise that shapes
//! everything: the reader is awake with WiFi on BEFORE this runs (or cabled
//! over usbnet), so a path exists by construction and the only job is finding
//! the address, moving the bytes, and poking Plato's `/tmp/plato.cmd` FIFO so
//! the document opens.
//!
//! Ported from `scripts/platonic.py` (2026-08-12), which it replaces: the tool
//! moves to Rust so pairing can share one protocol crate with the device side,
//! and so another Mac gets a single binary instead of a Python install.
//!
//! Identity is the HostKeyAlias trick: every candidate address is verified
//! against the `platokin` alias in the known_hosts file, so any discovery
//! mechanism is safe by construction -- ssh itself refuses anything that is
//! not our device.
//!
//! Clocks: the device's clock reads 2023 and drifts, so age is NEVER computed
//! against it.  Every pushed file gets `touch -t` stamped with the Mac's
//! current time (rendered in UTC -- the device's TZ is UTC, and busybox
//! `touch` interprets `-t` in the device's local zone), and the expiry sweep
//! compares those mtimes against the Mac's clock.  Files that reached `inbox/`
//! some other way carry 2023-ish mtimes and will look ancient; the sweep is
//! scoped to `inbox/` alone, which only platonic feeds, so that is acceptable.
//!
//! busybox rule (CLAUDE.md, 2026-08-11): the device's busybox silently accepts
//! options it does not implement, so remote commands stick to idioms verified
//! against `device-facts/bin-listing.txt` (`find`, `stat`, `touch`, `timeout`
//! all exist as applets) and every check trusts OUTPUT, not exit codes.

mod link;
mod pair;
mod pure;
mod session;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use link::{open_link, Link, Transport};
use platonic_recv::proto::Entry;
use pure::*;
use session::{arp_sweep, probe, resolve_all, Probe, Session};

const USAGE: &str = "\
usage: platonic [FILE ...] [--to NAME] [--quiet] [--title T] [--open FILE]
                [--list] [--pair] [--days N] [--host H] [--dry-run]

push a document to the Kindle and start reading it (docs/platonic.md)

  FILE          documents to push ('-' reads stdin)
  --to NAME     destination folder under documents/ (default: inbox, which
                expires; named folders do not)
  --quiet       deliver and refresh the library, don't open
  --title T     library title (heading) for text/stdin input
  --open FILE   with several files, the one to open
  --list        what is on the reader, and when inbox items expire
  --pair        one-time per Mac: tap Applications > Pair a Mac on the
                reader, then type the code it shows
  --days N      inbox expiry age (default 14)
  --host H      reader address; skips discovery, no fallback
  --dry-run     print every ssh command instead of running it

environment:
  PLATONIC_TRANSPORT=recv|shell   force one transport (default: auto, which
                prefers the restricted receiver and degrades loudly)
";

fn die(msg: impl AsRef<str>) -> ! {
    eprintln!("{}", msg.as_ref());
    std::process::exit(1)
}

fn arg_error(msg: &str) -> ! {
    eprint!("{}", USAGE);
    eprintln!("platonic: error: {}", msg);
    std::process::exit(2)
}

//
// ---------------------------------------------------------------- arguments
//
// Hand-rolled: the workspace has no argument parser in its lockfile, and ten
// flags do not justify making one a dependency of a binary that other Macs
// have to build.

struct Args {
    files: Vec<String>,
    to: String,
    quiet: bool,
    title: Option<String>,
    open: Option<String>,
    list: bool,
    pair: bool,
    days: i64,
    host: Option<String>,
    dry_run: bool,
}

fn parse_args(argv: Vec<String>) -> Args {
    let mut args = Args {
        files: Vec::new(),
        to: "inbox".to_string(),
        quiet: false,
        title: None,
        open: None,
        list: false,
        pair: false,
        days: DEFAULT_DAYS,
        host: None,
        dry_run: false,
    };

    let mut it = argv.into_iter().peekable();
    while let Some(arg) = it.next() {
        // `--flag=value` and `--flag value` both work, as argparse does.
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        let mut value = |what: &str| -> String {
            match inline.clone() {
                Some(v) => v,
                None => match it.next() {
                    Some(v) => v,
                    None => arg_error(&format!("{} expects a value", what)),
                },
            }
        };
        match name.as_str() {
            "-h" | "--help" => {
                print!("{}", USAGE);
                std::process::exit(0);
            }
            "--to" => args.to = value("--to"),
            "--title" => args.title = Some(value("--title")),
            "--open" => args.open = Some(value("--open")),
            "--days" => {
                let raw = value("--days");
                args.days = raw.parse().unwrap_or_else(|_| {
                    arg_error(&format!("--days: invalid int value: {:?}", raw))
                });
            }
            "--host" => args.host = Some(value("--host")),
            "--quiet" => args.quiet = true,
            "--list" => args.list = true,
            "--pair" => args.pair = true,
            "--dry-run" => args.dry_run = true,
            "-" => args.files.push("-".to_string()),
            other if other.starts_with('-') && other.len() > 1 => {
                arg_error(&format!("unrecognized argument: {}", other))
            }
            _ => args.files.push(arg),
        }
    }

    if args.list && args.pair {
        arg_error("--list and --pair are mutually exclusive");
    }
    if (args.list || args.pair) && !args.files.is_empty() {
        arg_error("--list/--pair take no FILE arguments");
    }
    if !args.list && !args.pair && args.files.is_empty() {
        arg_error("nothing to do: give FILEs, '-', --list or --pair");
    }
    args
}

//
// ------------------------------------------------------------------ context
//

struct Ctx {
    key: PathBuf,
    /// Which key was chosen.  It decides whether a fallback exists at all: a
    /// paired key's ssh session runs the receiver and nothing else.
    identity: Identity,
    known: PathBuf,
    cache: PathBuf,
    config: PathBuf,
    dry_run: bool,
    transport: Transport,
}

impl Ctx {
    fn new(dry_run: bool) -> Ctx {
        let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
        let (key, identity) = choose_key(
            std::env::var("PLATONIC_KEY").ok().as_deref(), &home,
            &|p: &Path| p.exists());
        if !identity.is_default() {
            // So a confusing auth failure names the key it actually used.
            match identity {
                Identity::Env =>
                    eprintln!("note: using the key from PLATONIC_KEY ({})",
                              key.display()),
                _ =>
                    eprintln!("note: no {} yet; using the shared admin key {}",
                              home.join(".ssh/platonic_ed25519").display(),
                              key.display()),
            }
        }
        let transport = Transport::from_env(
            std::env::var("PLATONIC_TRANSPORT").ok().as_deref())
            .unwrap_or_else(|e| die(e));
        Ctx {
            key,
            identity,
            transport,
            known: choose_known_hosts(
                std::env::var("PLATONIC_KNOWN_HOSTS").ok().as_deref(), &home),
            cache: home.join(".cache/platonic/last-host"),
            config: home.join(".config/platonic/config"),
            dry_run,
        }
    }

    fn session(&self, host: &str) -> Session {
        Session::new(host, self.dry_run, "yes", self.key.clone(),
                     self.known.clone())
    }

    fn probe(&self, host: &str) -> Probe {
        probe(host, &self.key, &self.known, "yes")
    }

    /// A paired key may run exactly one program, so there is no shell to fall
    /// back to.  `--pair` mints that key; every Mac set up before it keeps the
    /// admin key, and keeps the fallback with it.
    fn restricted_key(&self) -> bool {
        self.identity == Identity::PerMac
    }

    fn read_cache(&self) -> Option<String> {
        let text = std::fs::read_to_string(&self.cache).ok()?;
        let host = text.trim().to_string();
        if host.is_empty() { None } else { Some(host) }
    }

    fn write_cache(&self, host: &str) {
        if let Some(parent) = self.cache.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&self.cache, format!("{}\n", host));
    }

    /// The resolution ladder from docs/platonic.md, cheapest first, every rung
    /// verified by actually connecting under the HostKeyAlias.
    ///
    /// The probe that finds the reader is also the probe that establishes
    /// whether the receiver is deployed, so choosing the transport costs no
    /// extra connection.
    fn discover(&self, host_opt: &Option<String>) -> (String, Probe) {
        let explicit = host_opt.clone()
            .or_else(|| std::env::var("PLATONIC_HOST").ok())
            .filter(|s| !s.is_empty());

        if self.dry_run {
            let host = explicit
                .or_else(|| self.read_cache())
                .unwrap_or_else(|| USB_ADDR.to_string());
            println!("[dry-run] using host {} without probing", host);
            return (host, Probe { reachable: true, receiver: true,
                                  note: String::new() });
        }

        if let Some(host) = explicit {
            // The escape hatch: no fallback past it.
            let probe = self.probe(&host);
            if probe.reachable {
                return (host, probe);
            }
            die(format!("no reader at {} (from --host/PLATONIC_HOST); not \
                         falling back. {}", host, NO_READER_MSG));
        }

        if let Some(cached) = self.read_cache() {
            let probe = self.probe(&cached);
            if probe.reachable {
                return (cached, probe);
            }
        }

        // Two name rungs, cheapest first -- see `pure::dns_names`.  The
        // `.local` one is answered by the reader's own mDNS responder
        // (crates/plato/src/mdns.rs), so it works on a router that registers
        // no client names at all.
        for name in dns_names(ALIAS) {
            for addr in resolve_all(&name) {
                let probe = self.probe(&addr);
                if probe.reachable {
                    self.write_cache(&addr);
                    return (addr, probe);
                }
            }
        }

        let probe = self.probe(USB_ADDR);
        if probe.reachable {
            self.write_cache(USB_ADDR);
            return (USB_ADDR.to_string(), probe);
        }

        // The device MAC is device-identifying and must never enter the
        // tracked repo (CLAUDE.md, Secrets).  It lives in
        // kindle-secrets-untracked.md; the user copies it into the config once
        // as `mac=aa:bb:cc:dd:ee:ff`.
        let cfg = std::fs::read_to_string(&self.config)
            .map(|t| parse_config(&t))
            .unwrap_or_default();
        match config_get(&cfg, "mac") {
            Some(mac) => match norm_mac(mac) {
                Ok(mac) => {
                    if let Some(addr) = arp_sweep(&mac) {
                        let probe = self.probe(&addr);
                        if probe.reachable {
                            self.write_cache(&addr);
                            return (addr, probe);
                        }
                    }
                }
                Err(e) => eprintln!("note: {} in {}", e, self.config.display()),
            },
            None => eprintln!(
                "note: no mac= in {}; skipping the ARP sweep. The MAC is in \
                 kindle-secrets-untracked.md — add `mac=…` to enable it.",
                self.config.display()),
        }

        die(NO_READER_MSG)
    }
}

//
// ------------------------------------------------------ preparing documents
//

struct Doc {
    /// what the user typed (for `--open` matching)
    source: String,
    /// destination filename
    name: String,
    data: Vec<u8>,
}

fn git_repo_name(directory: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C").arg(directory)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let top = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if top.is_empty() {
        return None;
    }
    Path::new(&top).file_name().map(|n| n.to_string_lossy().into_owned())
}

/// The repo containing the source file; fall back to the cwd's repo, then no
/// prefix.  Two projects both having a `docs/plan.md` is the normal case.
fn repo_prefix_for(path: &Path) -> Option<String> {
    let dir = std::fs::canonicalize(path).ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    git_repo_name(&dir).or_else(|| git_repo_name(Path::new(".")))
}

/// One input -> a Doc, or a refusal.  Renderable kinds ship as-is; other UTF-8
/// text ships as fenced `.md`; binary is refused -- silently shipping
/// something the reader cannot open is the failure docs/platonic.md exists to
/// avoid.
fn prepare(source: &str, title_opt: &Option<String>) -> Result<Doc, String> {
    if source == "-" {
        let mut data = Vec::new();
        std::io::stdin().read_to_end(&mut data)
            .map_err(|e| format!("stdin: {}", e))?;
        if !is_text(&data) {
            return Err("stdin: binary input refused (not UTF-8 text)".to_string());
        }
        let title = title_opt.clone().unwrap_or_else(|| "stdin".to_string());
        let text = wrap_fenced(std::str::from_utf8(&data).unwrap(), &title);
        let repo = git_repo_name(Path::new("."));
        let name = dest_name(repo.as_deref(), &title, "md");
        return Ok(Doc { source: source.to_string(), name, data: text.into_bytes() });
    }

    let p = Path::new(source);
    let data = std::fs::read(p).map_err(|e| format!("{}: {}", source, e))?;

    let ext = p.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
    let filename = p.file_name().map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
    let repo = repo_prefix_for(p);

    if RENDERABLE.contains(&ext.as_str()) {
        let mut data = data;
        if ext == "md" && is_text(&data) {
            // The library shows the first heading, not the filename.
            let title = title_opt.clone().unwrap_or_else(|| stem.clone());
            data = ensure_md_heading(std::str::from_utf8(&data).unwrap(), &title)
                .into_bytes();
        }
        return Ok(Doc {
            source: source.to_string(),
            name: dest_name(repo.as_deref(), &stem, &ext),
            data,
        });
    }

    if is_text(&data) {
        let title = title_opt.clone().unwrap_or_else(|| filename.clone());
        let text = wrap_fenced(std::str::from_utf8(&data).unwrap(), &title);
        return Ok(Doc {
            source: source.to_string(),
            name: dest_name(repo.as_deref(), &filename, "md"),
            data: text.into_bytes(),
        });
    }

    Err(format!("{}: refused — binary, and .{} is not a kind Plato can render",
                source, if ext.is_empty() { "(none)".to_string() } else { ext }))
}

//
// ---------------------------------------------------- push / poke  /  sweep
//

fn push_doc(link: &mut dyn Link, doc: &Doc, to: &str, now: f64) {
    let written = link.put(to, &doc.name, &doc.data, now as i64)
        .unwrap_or_else(|e| die(format!("push of {} failed: {}", doc.source, e)));
    // The byte count is the push's only proof, whichever transport moved it.
    if written != doc.data.len() as u64 {
        die(format!("push of {} failed: the reader wrote {} bytes, not {}",
                    doc.source, written, doc.data.len()));
    }
    println!("pushed {} -> {}/{} ({} bytes)",
             doc.source, to, doc.name, doc.data.len());
}

/// Best-effort: the push already succeeded, and a missing listener only means
/// the document appears after a restart.
fn poke(link: &mut dyn Link, target: Option<(&str, &str)>) {
    let result = match target {
        Some((folder, name)) => link.open(folder, name),
        None => link.import(),
    };
    if let Err(e) = result {
        eprintln!("warning: {} — the document will appear after a restart", e);
    }
}

/// Expire `inbox/` by age, decided on the Mac's clock against mtimes this tool
/// stamped.  Scoped to `inbox/` alone: a named folder means "keep this".
fn sweep_inbox(link: &mut dyn Link, now: f64, days: i64) {
    match link.sweep(cutoff_epoch(now, days) as i64) {
        Ok(names) => for name in names {
            println!("expired (>{}d): {}", days, name);
        },
        Err(e) => eprintln!("warning: sweep failed: {}", e),
    }
}

fn pick_open_target<'a>(docs: &'a [Doc], open_arg: &str) -> &'a Doc {
    let matches: Vec<&Doc> = docs.iter().filter(|d| {
        d.source == open_arg
            || Path::new(&d.source).file_name()
                   .map(|n| n == open_arg).unwrap_or(false)
    }).collect();
    if matches.len() != 1 {
        die(format!("--open '{}' does not name exactly one of the pushed files",
                    open_arg));
    }
    matches[0]
}

//
// -------------------------------------------------------- subcommand bodies
//

fn cmd_push(ctx: &Ctx, args: &Args, now: f64) {
    let to = match validate_to(&args.to) {
        Ok(to) => to,
        Err(e) => die(e),
    };
    if to == "plans" {
        eprintln!("note: plans/ is mirrored from the repo; the next sync-docs \
                   will remove this unless it is committed");
    }

    let mut docs = Vec::new();
    let mut errors = Vec::new();
    for source in &args.files {
        match prepare(source, &args.title) {
            Ok(doc) => docs.push(doc),
            Err(e) => errors.push(e),
        }
    }
    if !errors.is_empty() {
        // Refuse before pushing anything: all-or-nothing beats a partial batch
        // whose failures scrolled past.
        die(errors.join("\n"));
    }

    let (host, probe) = ctx.discover(&args.host);
    let sess = ctx.session(&host);
    let mut link = match open_link(&sess, ctx.transport, &probe,
                                   ctx.restricted_key()) {
        Ok(link) => link,
        Err(e) => die(e),
    };

    // The whole push is one session on the receiver path: every PUT, the
    // open, and the sweep travel over the connection opened above.
    for doc in &docs {
        push_doc(link.as_mut(), doc, to, now);
    }

    // Opening: default for one file; several are delivered and none opened
    // ("which one did you mean" has no good default) unless --open names one.
    // --quiet still pokes import so the library refreshes.
    let target: Option<&Doc> = if args.quiet {
        None
    } else if docs.len() == 1 {
        Some(&docs[0])
    } else {
        args.open.as_ref().map(|open| pick_open_target(&docs, open))
    };
    poke(link.as_mut(), target.map(|d| (to, d.name.as_str())));

    // Opportunistic: we are already connected, so the sweep costs one op.
    sweep_inbox(link.as_mut(), now, args.days);
    link.finish();
}

fn cmd_list(ctx: &Ctx, args: &Args, now: f64) {
    let (host, probe) = ctx.discover(&args.host);
    let sess = ctx.session(&host);
    let mut link = match open_link(&sess, ctx.transport, &probe,
                                   ctx.restricted_key()) {
        Ok(link) => link,
        Err(e) => die(e),
    };
    let entries = match link.list() {
        Ok(entries) => entries,
        Err(e) => die(format!("could not list the library: {}", e)),
    };
    link.finish();

    if entries.is_empty() && !sess.dry_run {
        println!("nothing under {}", DOCROOT);
        return;
    }

    let mut folders: Vec<(String, Vec<Entry>)> = Vec::new();
    for entry in entries {
        match folders.iter_mut().find(|(f, _)| *f == entry.folder) {
            Some((_, items)) => items.push(entry),
            None => folders.push((entry.folder.clone(), vec![entry])),
        }
    }
    folders.sort_by(|a, b| a.0.cmp(&b.0));

    for (folder, items) in &mut folders {
        println!("{}/", folder);
        items.sort_by(|a, b| b.mtime.cmp(&a.mtime));
        for entry in items.iter() {
            let age_d = (now - entry.mtime as f64) / 86_400.0;
            let mut note = format!("{}, {:5.1}d old", human_size(entry.size), age_d);
            if folder == "inbox" {
                let left = args.days as f64 - age_d;
                if left <= 0.0 {
                    note.push_str("  expired");
                } else {
                    note.push_str(&format!("  expires in {:.1}d", left));
                }
            }
            println!("  {}  ({})", entry.name, note);
        }
    }
}

fn main() {
    let args = parse_args(std::env::args().skip(1).collect());
    let now = SystemTime::now().duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let ctx = Ctx::new(args.dry_run);

    if args.list {
        cmd_list(&ctx, &args, now);
    } else if args.pair {
        pair::cmd_pair(&ctx, &args);
    } else {
        cmd_push(&ctx, &args, now);
    }
}
