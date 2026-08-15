//! Does rustls actually *run* on the device?
//!
//! platokin's iroh spike settled that a rustls/ring stack cross-compiles clean
//! for `armv7-unknown-linux-musleabi` through `zig cc` and passes the ABI gate,
//! and said in the same breath: "Not yet run on hardware -- compiles and links,
//! execution untested" (docs/paper-sync.md). Everything downstream of that
//! sentence -- reading Hacker News on the device, a buildable crates/fetcher,
//! anything else that speaks HTTPS -- is waiting on it being false.
//!
//! So: one static binary, no arguments needed, run it over ssh.
//!
//!     net-smoke                 # the endpoints the HN reader will use
//!     net-smoke <url> [<url>…]  # or whatever you want to try
//!
//! It prints the wall clock first, because a device whose clock has not been
//! synced fails every certificate check for a reason that looks nothing like a
//! clock problem in the error text.

use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use plato_net::{diagnose, Http};

/// The two endpoints the reader is planned around, plus the site itself.
/// One request each -- the whole front page, and a whole comment tree.
const DEFAULT_URLS: [(&str, &str); 3] = [
    ("https://hn.algolia.com/api/v1/search?tags=front_page&hitsPerPage=30", "\"hits\""),
    ("https://hn.algolia.com/api/v1/items/49299605", "\"children\""),
    ("https://news.ycombinator.com/", "Hacker News"),
];

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let targets: Vec<(&str, &str)> = if args.is_empty() {
        DEFAULT_URLS.to_vec()
    } else {
        args.iter().map(|u| (u.as_str(), "")).collect()
    };

    print_clock();

    let http = Http::new();
    let mut failures = 0;

    for (url, expect) in targets {
        print!("GET {url}\n    ");
        match http.get(url) {
            Ok(res) => {
                let ms = res.elapsed.as_millis();
                let ok = res.status == 200 && (expect.is_empty() || res.text().contains(expect));
                println!("{} {} bytes in {} ms{}",
                         res.status, res.body.len(), ms,
                         if ok { "" } else { "  <-- unexpected" });
                if !expect.is_empty() && !res.text().contains(expect) {
                    println!("    body does not contain {expect:?}");
                }
                if !ok {
                    failures += 1;
                }
            }
            Err(err) => {
                println!("FAILED: {err:#}");
                if let Some(hint) = diagnose(&err) {
                    println!("    hint: {hint}");
                }
                failures += 1;
            }
        }
    }

    if failures == 0 {
        println!("\nall requests succeeded: TLS works on this device");
        ExitCode::SUCCESS
    } else {
        println!("\n{failures} request(s) failed");
        ExitCode::FAILURE
    }
}

fn print_clock() {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => {
            let (y, m, day, hh, mm, ss) = civil_from_epoch(d.as_secs() as i64);
            println!("clock: {y:04}-{m:02}-{day:02} {hh:02}:{mm:02}:{ss:02} UTC");
            // The unsynced device reads 2023 (platokin docs/device.md); every
            // certificate issued since then is then "not yet valid".
            if y < 2026 {
                println!("       ^ this looks unsynced -- run clock-sync.sh first, \
                          or certificate checks will fail for the wrong reason");
            }
        }
        Err(_) => println!("clock: before 1970 (!)"),
    }
}

/// Days-to-civil, Howard Hinnant's algorithm. Ten lines beats a dependency
/// when the only date this program ever formats is `now`.
fn civil_from_epoch(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, (rem / 3_600) as u32, (rem % 3_600 / 60) as u32, (rem % 60) as u32)
}

#[cfg(test)]
mod tests {
    use super::civil_from_epoch;

    #[test]
    fn epoch_and_a_known_instant() {
        assert_eq!(civil_from_epoch(0), (1970, 1, 1, 0, 0, 0));
        // 2026-08-15T13:21:32Z -- the Date header HN returned while this was
        // being written, which is as good a fixture as any.
        assert_eq!(civil_from_epoch(1_786_800_092), (2026, 8, 15, 13, 21, 32));
    }
}
