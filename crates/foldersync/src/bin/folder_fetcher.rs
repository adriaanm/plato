//! folder_fetcher -- a Plato fetcher hook that mirrors a folder from a
//! computer on the LAN into the hooked directory.
//!
//! Wire it up in `Settings.toml`:
//!
//!     [[libraries.hooks]]
//!     path = "Papers"
//!     program = "bin/folder_fetcher/folder_fetcher"
//!     sort-method = "added"
//!
//! Entering *Papers* in the Home view starts this program; leaving it sends
//! SIGTERM.  So the sync is exactly as long-lived as the user's attention, and
//! the radio is on only for that window -- which is the point on a device where
//! WiFi is off by default to save power.
//!
//! The flow:
//!
//!   1. If Plato says the network is down, ask it to bring WiFi up
//!      (`setWifi`) and wait for the `network up` message on stdin.
//!   2. Find the hub: last known address first, UDP broadcast if that fails.
//!   3. Fetch the manifest.  If our clock disagrees with the hub's by more
//!      than an hour, take the hub's -- see `set_time` below.
//!   4. Download every file we don't already have, newest first, announcing
//!      each one to Plato as it lands so it appears in the list immediately.
//!
//! **Additive only.**  A file removed on the computer is never removed here:
//! deleting it would also throw away reading position, and a sync that can
//! delete is a sync that can delete everything when the folder is mistyped.

use std::fs::{self, File};
use std::io::{self, BufRead, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use foldersync::*;

/// Read next to the binary, so a deployment is one directory.
const CONFIG_NAME: &str = "folder_fetcher.conf";
/// Where the last working hub address is remembered, also next to the binary.
const CACHE_NAME: &str = ".last-hub";

struct Config {
    disco_port: u16,
    token: String,
    /// Take the hub's clock when ours disagrees.  On a device whose RTC never
    /// survives a battery pull and whose CA store is empty, the sync peer is
    /// the only time source there is.
    set_time: bool,
    /// How long to wait for `network up` after asking for WiFi.
    wifi_wait: u64,
    timeout: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            disco_port: DEFAULT_DISCO_PORT,
            token: String::new(),
            set_time: true,
            wifi_wait: 60,
            timeout: 20,
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 5 {
        eprintln!("usage: folder_fetcher LIBRARY_PATH SAVE_PATH WIFI ONLINE");
        process::exit(1);
    }

    let library_path = PathBuf::from(&args[1]);
    let save_path = PathBuf::from(&args[2]);
    let wifi = args[3] == "true";
    let online = args[4] == "true";

    let config = load_config();

    if let Err(e) = run(&config, &library_path, &save_path, wifi, online) {
        // Plato shows a notification for a non-zero exit, but not the reason,
        // so say it ourselves first.
        notify(&format!("Sync failed: {}.", e));
        eprintln!("folder_fetcher: {:#}", e);
        process::exit(1);
    }
}

fn run(config: &Config, library_path: &Path, save_path: &Path,
       wifi: bool, online: bool) -> io::Result<()> {
    fs::create_dir_all(save_path)?;

    if !online {
        wait_for_network(config, wifi)?;
    }

    let timeout = Duration::from_secs(config.timeout);
    let (address, from_cache) = find_hub(config, timeout)?;

    let manifest = match http_get_string(address, "/manifest", &config.token, timeout) {
        Ok(text) => text,
        // A cached address that no longer answers is the common case -- the
        // computer moved networks, or DHCP moved it.  Fall back to a probe
        // once before giving up.
        Err(e) if from_cache => {
            eprintln!("cached hub {} did not answer ({}), broadcasting", address, e);
            let (address, _) = discover_and_cache(config, timeout)?;
            http_get_string(address, "/manifest", &config.token, timeout)?
        },
        Err(e) => return Err(e),
    };

    let manifest = parse_manifest(&manifest)?;
    remember_hub(address);

    if config.set_time {
        adopt_clock(&manifest);
    }

    let mut wanted: Vec<&Entry> = manifest.entries.iter()
                                          .filter(|e| needs_download(save_path, e))
                                          .collect();
    // Newest first: if the user backs out early, they got the things they most
    // likely just added.
    wanted.sort_by(|a, b| b.mtime.cmp(&a.mtime));

    if wanted.is_empty() {
        notify("Library is up to date.");
        return Ok(());
    }

    notify(&format!("Fetching {} file{}.", wanted.len(),
                    if wanted.len() == 1 { "" } else { "s" }));

    let mut fetched = 0;
    for entry in &wanted {
        match download(config, address, save_path, entry, timeout) {
            Ok(path) => {
                announce(library_path, &path, entry);
                fetched += 1;
            },
            Err(e) => {
                eprintln!("can't fetch {}: {}", entry.path, e);
                notify(&format!("Couldn't fetch {}.", file_name(&entry.path)));
            },
        }
    }

    notify(&format!("Fetched {} of {} file{}.", fetched, wanted.len(),
                    if wanted.len() == 1 { "" } else { "s" }));
    Ok(())
}

//
// ------------------------------------------------------------------ network
//

/// Ask Plato for WiFi if it isn't already on, then block until it reports the
/// network is up.
fn wait_for_network(config: &Config, wifi: bool) -> io::Result<()> {
    if !wifi {
        emit(r#"{"type":"setWifi","enable":true}"#);
        notify("Turning WiFi on…");
    }

    // Plato writes `{"type":"network","status":"up"}` on our stdin when the
    // link comes up.  Read it on a thread so the wait can time out: a blocking
    // read here with no network would hang until the user walked away.
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let stdin = io::stdin();
        for line in stdin.lock().lines().map_while(Result::ok) {
            if line.contains("\"network\"") && line.contains("\"up\"") {
                sender.send(()).ok();
                return;
            }
        }
    });

    receiver.recv_timeout(Duration::from_secs(config.wifi_wait))
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut,
                                        "the network did not come up"))
}

/// The cached address first -- on a stable home network that makes the whole
/// discovery step disappear -- then a broadcast probe.
fn find_hub(config: &Config, timeout: Duration) -> io::Result<(SocketAddr, bool)> {
    if let Some(address) = cached_hub() {
        return Ok((address, true));
    }
    discover_and_cache(config, timeout).map(|(address, _)| (address, false))
}

fn discover_and_cache(config: &Config, timeout: Duration) -> io::Result<(SocketAddr, u16)> {
    notify("Looking for the library…");
    let (from, port) = discover(config.disco_port, &config.token, 3,
                                Duration::from_millis(1500))?;
    let address = SocketAddr::new(from.ip(), port);
    let _ = timeout;
    remember_hub(address);
    Ok((address, port))
}

fn cached_hub() -> Option<SocketAddr> {
    read_to_string_or_empty(&beside_binary(CACHE_NAME)).trim().parse().ok()
}

fn remember_hub(address: SocketAddr) {
    if let Ok(mut file) = File::create(beside_binary(CACHE_NAME)) {
        writeln!(file, "{}", address).ok();
    }
}

//
// ----------------------------------------------------------------- transfer
//

fn needs_download(save_path: &Path, entry: &Entry) -> bool {
    // Identity is name plus size.  No hashing: the device is a 1 GHz A9 and
    // would spend longer digesting a PDF than downloading it, and a same-name
    // same-size mismatch is not a thing a personal document folder produces.
    match fs::metadata(save_path.join(&entry.path)) {
        Ok(metadata) => metadata.len() != entry.size,
        Err(..) => true,
    }
}

fn download(config: &Config, address: SocketAddr, save_path: &Path,
            entry: &Entry, timeout: Duration) -> io::Result<PathBuf> {
    let destination = save_path.join(&entry.path);
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }

    // Download beside the target and rename, so a SIGTERM mid-transfer -- which
    // is just the user leaving the folder, and therefore routine -- can never
    // leave a truncated file that looks complete.
    let partial = destination.with_extension(format!(
        "{}.part",
        destination.extension().and_then(|e| e.to_str()).unwrap_or("")));

    let url = format!("/file/{}", percent_encode(&entry.path));
    {
        let mut file = File::create(&partial)?;
        http_get(address, &url, &config.token, timeout, &mut file)?;
        file.flush()?;
    }

    let written = fs::metadata(&partial)?.len();
    if written != entry.size {
        fs::remove_file(&partial).ok();
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof,
                                  format!("got {} bytes, expected {}", written, entry.size)));
    }

    fs::rename(&partial, &destination)?;
    Ok(destination)
}

//
// -------------------------------------------------------- talking to Plato
//

fn emit(line: &str) {
    println!("{}", line);
    io::stdout().flush().ok();
}

fn notify(message: &str) {
    emit(&format!(r#"{{"type":"notify","message":"{}"}}"#, json_escape(message)));
}

/// Tell Plato about a file that just landed, so it appears in the list without
/// waiting for a rescan.  Paths in the event are relative to the library root.
fn announce(library_path: &Path, path: &Path, entry: &Entry) {
    let relative = match path.strip_prefix(library_path) {
        Ok(relative) => relative,
        Err(..) => path,
    };
    let kind = path.extension().and_then(|e| e.to_str())
                   .map(|e| e.to_lowercase()).unwrap_or_default();

    emit(&format!(
        r#"{{"type":"addDocument","info":{{"title":"{}","file":{{"path":"{}","kind":"{}","size":{}}}}}}}"#,
        json_escape(&title_from(&entry.path)),
        json_escape(&relative.to_string_lossy()),
        json_escape(&kind),
        entry.size));
}

/// The file stem, tidied just enough to be a readable title.  Plato reads real
/// metadata out of the document itself on first open; this is what shows in the
/// list until then.
fn title_from(path: &str) -> String {
    // Underscores are almost always stand-ins for spaces; hyphens are not, and
    // eating them turns "Smith - 2019 - Title" into mush.
    Path::new(path).file_stem()
                   .map(|s| s.to_string_lossy().replace('_', " ")
                             .split_whitespace().collect::<Vec<_>>().join(" "))
                   .unwrap_or_else(|| path.to_string())
}

fn file_name(path: &str) -> String {
    Path::new(path).file_name()
                   .map(|s| s.to_string_lossy().into_owned())
                   .unwrap_or_else(|| path.to_string())
}

//
// -------------------------------------------------------------------- clock
//

/// Adopt the hub's wall clock when ours is obviously wrong.
///
/// The device this was written for boots believing it is 2023, which breaks
/// every timestamp in the library and any future attempt at TLS.  The hub's
/// manifest carries its time preformatted precisely so this is one `date -s`
/// and no calendar arithmetic on the device.
fn adopt_clock(manifest: &Manifest) {
    if manifest.epoch == 0 || manifest.stamp.is_empty() {
        return;
    }

    let drift = (manifest.epoch - epoch_secs()).abs();
    if drift < 3600 {
        return;
    }

    // `-u`: the stamp is UTC, and the reader's own zone is not our business.
    match Command::new("date").arg("-u").arg("-s").arg(&manifest.stamp).status() {
        Ok(status) if status.success() => {
            eprintln!("clock was off by {} s, set to {}", drift, manifest.stamp);
            // hwclock is absent on some devices and unwritable on others;
            // either way a failure here costs only the next boot.
            Command::new("hwclock").arg("-w").status().ok();
        },
        Ok(status) => eprintln!("date -s exited {}", status),
        Err(e) => eprintln!("can't run date: {}", e),
    }
}

//
// ------------------------------------------------------------------ config
//

fn beside_binary(name: &str) -> PathBuf {
    // Plato runs a hook with its own directory as the working directory, so a
    // bare relative path is already the right thing; going through argv[0]
    // keeps it working when run by hand from elsewhere.
    std::env::current_exe().ok()
             .and_then(|p| p.parent().map(|p| p.join(name)))
             .unwrap_or_else(|| PathBuf::from(name))
}

fn load_config() -> Config {
    let mut config = Config::default();

    for (key, value) in parse_config(&read_to_string_or_empty(&beside_binary(CONFIG_NAME))) {
        match key.as_str() {
            "disco_port" => if let Ok(v) = value.parse() { config.disco_port = v },
            "token" => config.token = value,
            "set_time" => config.set_time = value == "true",
            "wifi_wait" => if let Ok(v) = value.parse() { config.wifi_wait = v },
            "timeout" => if let Ok(v) = value.parse() { config.timeout = v },
            other => eprintln!("{}: unknown key {}", CONFIG_NAME, other),
        }
    }

    config
}
