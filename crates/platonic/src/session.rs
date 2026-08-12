//! The I/O half: one ssh helper every invocation goes through, and the
//! discovery ladder from docs/platonic.md.
//!
//! Transport is `cat` over ssh, never scp/sftp -- dropbear's sftp-server path
//! is compiled in as `/mnt/us/koreader/sftp-server` on a userstore that keeps
//! going stale.  We shell out to the system `ssh` binary rather than linking a
//! library: dropbear compatibility and the HostKeyAlias trick are proven with
//! the real client.

use std::io::{Read, Write};
use std::net::{IpAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::pure::{self, shell_quote, ssh_argv};

pub struct Output {
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Output {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
    pub fn err_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_string()
    }
}

#[derive(Debug)]
pub enum RunError {
    Timeout,
    Spawn(std::io::Error),
}

pub struct Session {
    pub host: String,
    pub dry_run: bool,
    pub strict: String,
    pub key: PathBuf,
    pub known: PathBuf,
}

impl Session {
    pub fn new(host: &str, dry_run: bool, strict: &str, key: PathBuf,
               known: PathBuf) -> Session {
        Session {
            host: host.to_string(),
            dry_run,
            strict: strict.to_string(),
            key,
            known,
        }
    }

    pub fn run(&self, remote_cmd: &str, input: Option<&[u8]>, timeout: Duration)
               -> Result<Output, RunError> {
        let argv = ssh_argv(&self.host, remote_cmd, &self.key, &self.known,
                            &self.strict);
        if self.dry_run {
            let line: Vec<String> = argv.iter().map(|a| shell_quote(a)).collect();
            let suffix = match input {
                Some(bytes) => format!("  # (+{} bytes on stdin)", bytes.len()),
                None => String::new(),
            };
            println!("+ {}{}", line.join(" "), suffix);
            return Ok(Output { status: 0, stdout: Vec::new(), stderr: Vec::new() });
        }
        run_with_timeout(&argv, input, timeout)
    }

    /// stdout as text; errors and timeouts read as "nothing came back", which
    /// is what every caller of this wants (the checks trust OUTPUT, never exit
    /// codes -- busybox, CLAUDE.md 2026-08-11).
    pub fn out(&self, remote_cmd: &str, timeout: Duration) -> String {
        match self.run(remote_cmd, None, timeout) {
            Ok(o) => o.text(),
            Err(_) => String::new(),
        }
    }
}

/// `std::process::Command` has no timeout, so: spawn, pump the three pipes on
/// their own threads (a large stdin would otherwise deadlock against a full
/// pipe buffer), and poll for exit until the deadline.
fn run_with_timeout(argv: &[String], input: Option<&[u8]>, timeout: Duration)
                    -> Result<Output, RunError> {
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(RunError::Spawn)?;

    let mut stdin = child.stdin.take().expect("piped");
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let payload: Vec<u8> = input.unwrap_or(&[]).to_vec();

    let deadline = Instant::now() + timeout;

    let result = std::thread::scope(|scope| {
        scope.spawn(move || {
            // A closed remote stdin is not our problem to report: the size
            // check on the way back is what decides whether the push worked.
            let _ = stdin.write_all(&payload);
            let _ = stdin.flush();
            drop(stdin);
        });
        let out_handle = scope.spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf);
            buf
        });
        let err_handle = scope.spawn(move || {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf);
            buf
        });

        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {}
                Err(e) => return Err(RunError::Spawn(e)),
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };

        let stdout = out_handle.join().unwrap_or_default();
        let stderr = err_handle.join().unwrap_or_default();
        match status {
            Some(status) => Ok(Output {
                status: status.code().unwrap_or(-1),
                stdout,
                stderr,
            }),
            None => Err(RunError::Timeout),
        }
    });

    result
}

//
// ----------------------------------------------------------------- discovery
//

/// Verified by connecting: the host key must match the alias entry AND the
/// marker must come back -- output, not exit code.
pub fn probe(host: &str, key: &PathBuf, known: &PathBuf, strict: &str) -> bool {
    let sess = Session::new(host, false, strict, key.clone(), known.clone());
    match sess.run("echo PLATONIC_OK", None, Duration::from_secs(8)) {
        Ok(out) => out.text().contains("PLATONIC_OK"),
        Err(_) => false,
    }
}

/// The router answers with several addresses for one name, some of them stale
/// leases (docs/paper-sync.md) -- try them all.
pub fn resolve_all(name: &str) -> Vec<String> {
    let mut addrs = Vec::new();
    let Ok(iter) = (name, 0u16).to_socket_addrs() else { return addrs };
    for sock in iter {
        if let IpAddr::V4(v4) = sock.ip() {
            let s = v4.to_string();
            if !addrs.contains(&s) {
                addrs.push(s);
            }
        }
    }
    addrs
}

/// `/24` prefixes of the Mac's own IPv4 addresses (hot-plugged interfaces are
/// invisible to `networksetup`; `ifconfig` sees everything).
pub fn local_prefixes() -> Vec<String> {
    let Ok(out) = Command::new("ifconfig").output() else { return Vec::new() };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut prefixes: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        let Some(rest) = line.strip_prefix("inet ") else { continue };
        let addr: String = rest.chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        let octets: Vec<&str> = addr.split('.').collect();
        if octets.len() != 4 || octets.iter().any(|o| o.is_empty()) {
            continue;
        }
        let prefix = octets[..3].join(".");
        // rung 4 covers usbnet, and loopback is nobody's reader
        if prefix.starts_with("127.") || prefix == "192.168.15" {
            continue;
        }
        if !prefixes.contains(&prefix) {
            prefixes.push(prefix);
        }
    }
    prefixes
}

/// Populate the ARP cache with a parallel unicast ping sweep (~1 s per `/24`),
/// then match the device MAC in `arp -an`.  L2-only by nature -- but if the
/// reader were on another segment, ssh would not work either.
pub fn arp_sweep(mac: &str) -> Option<String> {
    for prefix in local_prefixes() {
        println!("sweeping {}.0/24 for the reader's MAC …", prefix);
        ping_sweep(&prefix);
        let out = Command::new("arp").arg("-an").output().ok()?;
        let table = pure::parse_arp(&String::from_utf8_lossy(&out.stdout));
        if let Some(addr) = pure::arp_lookup(&table, mac) {
            return Some(addr);
        }
    }
    None
}

/// A bounded pool of plain threads -- 128 of them, each walking its own slice
/// of the `/24`.  No async runtime for one second of pings.
fn ping_sweep(prefix: &str) {
    const WORKERS: usize = 128;
    let hosts: Vec<u32> = (1..255).collect();
    std::thread::scope(|scope| {
        for worker in 0..WORKERS {
            let hosts = &hosts;
            scope.spawn(move || {
                for host in hosts.iter().skip(worker).step_by(WORKERS) {
                    let addr = format!("{}.{}", prefix, host);
                    let _ = Command::new("ping")
                        .args(["-c", "1", "-t", "1", "-n", &addr])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                }
            });
        }
    });
}
