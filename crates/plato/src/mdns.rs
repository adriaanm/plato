//! The mDNS/DNS-SD responder: the reader answers to `platokin.local`.
//!
//! Why this exists.  Every other rung of platonic's discovery ladder either
//! needs the router to register DHCP client names (it does here; a public
//! user's may not) or needs code on the Mac.  A responder needs neither:
//! macOS resolves `.local` natively through mDNSResponder, so once the device
//! answers, `ssh platokin.local` works from any Mac with nothing installed.
//!
//! Two hard constraints, both measured, both load-bearing:
//!
//! * **The port must be opened in the firewall.**  Amazon's boot chain is
//!   `INPUT` policy DROP with accept-all scoped to `usb0`, so inbound UDP on
//!   `wlan0` is dropped unless a rule says otherwise -- E21's control run
//!   received *zero* datagrams on an unopened port while an opened one took
//!   everything.  `scripts/wifi-up.sh` inserts the udp/5353 `ACCEPT`; without
//!   it this daemon runs, logs happily, and is deaf.
//! * **The multicast join succeeds on `ath6kl` / 3.0.35** (E21), so there is no
//!   driver obstacle.  A join failure is still logged rather than swallowed:
//!   this is the one thing that could differ on someone else's hardware.
//!
//! Scoped to `wlan0` on purpose.  usb0 is a point-to-point cable to one Mac
//! that already knows the address, and answering there would put the wlan0
//! address on a link where it means nothing.
//!
//! Lifetime is the radio's lifetime, and not one second longer -- WiFi here
//! exists only to serve a sync (docs/wifi.md), so the responder starts when
//! `wifi-enable.sh` succeeds and is torn down *before* `wifi-disable.sh` runs,
//! which is the only order in which the goodbye packets can still leave.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceInfo};

/// The DNS-SD type.  Port 2222 is dropbear's: platonic's transport is ssh, so
/// what is worth advertising is the ssh endpoint, not a second listener.
const SERVICE_TYPE: &str = "_platonic._tcp.local.";
const SSH_PORT: u16 = 2222;
const IFACE: &str = "wlan0";

/// DHCP can hand out a different address on reassociation, and `wifi-events.sh`
/// re-associates without Plato hearing about it.  A stale A record is worse
/// than no A record, so poll.
const ADDR_POLL_INTERVAL: Duration = Duration::from_secs(20);

/// Unregister and shutdown are asynchronous; both are worth a short wait so the
/// goodbye packets are on the wire before the caller drops the radio.
const TEARDOWN_WAIT: Duration = Duration::from_secs(2);

struct Responder {
    daemon: ServiceDaemon,
    fullname: String,
    name: String,
    addr: Ipv4Addr,
    /// Which watcher thread owns this responder; a stale one exits on sight.
    generation: u64,
}

static STATE: Mutex<Option<Responder>> = Mutex::new(None);
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Start (or restart) the responder for `name`, advertising `wlan0`'s current
/// address.  An empty name disables the feature.  Idempotent and safe to call
/// when it is already running.
pub fn start(name: &str) {
    if name.is_empty() {
        return;
    }

    stop();

    let Some(addr) = iface_addr(IFACE) else {
        eprintln!("mDNS: no IPv4 address on {}; not advertising.", IFACE);
        return;
    };

    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    match register(name, addr, generation) {
        Ok(responder) => {
            println!("mDNS: advertising {}.local as {} on {}.",
                     name, addr, IFACE);
            *STATE.lock().unwrap() = Some(responder);
            spawn_watcher(generation);
        }
        Err(e) => eprintln!("mDNS: can't advertise {}.local: {}.", name, e),
    }
}

/// Withdraw the records and stop the daemon.  Must be called *before* the radio
/// goes down: a goodbye packet sent over a dead link is not sent at all.
pub fn stop() {
    let Some(responder) = STATE.lock().unwrap().take() else { return };
    // Invalidate the watcher without waiting for it: it checks on its next tick.
    GENERATION.fetch_add(1, Ordering::SeqCst);
    teardown(responder);
}

fn teardown(responder: Responder) {
    if let Ok(rx) = responder.daemon.unregister(&responder.fullname) {
        rx.recv_timeout(TEARDOWN_WAIT).ok();
    }
    if let Ok(rx) = responder.daemon.shutdown() {
        rx.recv_timeout(TEARDOWN_WAIT).ok();
    }
    println!("mDNS: withdrew {}.local.", responder.name);
}

fn register(name: &str, addr: Ipv4Addr, generation: u64)
            -> Result<Responder, mdns_sd::Error> {
    let daemon = ServiceDaemon::new()?;
    // Order matters: the selections are applied in the order they arrive, so
    // "everything off, then wlan0 on" leaves exactly wlan0 (loopback included
    // in the off, which is why it is spelled All rather than IPv6).
    daemon.disable_interface(mdns_sd::IfKind::All)?;
    daemon.enable_interface(mdns_sd::IfKind::Name(IFACE.to_string()))?;

    // TXT carries a label and nothing else.  Nothing device-identifying is
    // allowed on the wire (CLAUDE.md, Secrets): no serial, no MAC, no board id.
    let properties = [("label", name), ("proto", "ssh")];
    let info = ServiceInfo::new(SERVICE_TYPE, name, &format!("{}.local.", name),
                                IpAddr::V4(addr), SSH_PORT, &properties[..])?;
    let fullname = info.get_fullname().to_string();
    daemon.register(info)?;

    Ok(Responder { daemon, fullname, name: name.to_string(), addr, generation })
}

/// Re-register when `wlan0`'s address changes under us, and stand down when the
/// address disappears -- announcing a lease we no longer hold is the failure
/// this whole feature is supposed to prevent.
fn spawn_watcher(generation: u64) {
    thread::spawn(move || loop {
        thread::sleep(ADDR_POLL_INTERVAL);

        // The lock is held across the whole swap on purpose: a `stop()` that
        // interleaved with a re-registration would leave the freshly made
        // daemon behind, advertising on a radio the app has decided to drop.
        let mut guard = STATE.lock().unwrap();
        let (name, known_addr) = match guard.as_ref() {
            Some(responder) if responder.generation == generation =>
                (responder.name.clone(), responder.addr),
            _ => return,
        };

        match iface_addr(IFACE) {
            Some(addr) if addr == known_addr => {}
            Some(addr) => {
                teardown(guard.take().expect("checked above"));
                match register(&name, addr, generation) {
                    Ok(responder) => {
                        println!("mDNS: {}.local moved to {}.", name, addr);
                        *guard = Some(responder);
                    }
                    Err(e) => {
                        eprintln!("mDNS: can't re-advertise {}.local: {}.",
                                  name, e);
                        return;
                    }
                }
            }
            None => {
                eprintln!("mDNS: {} lost its address; withdrawing.", IFACE);
                teardown(guard.take().expect("checked above"));
                return;
            }
        }
    });
}

fn iface_addr(name: &str) -> Option<Ipv4Addr> {
    if_addrs::get_if_addrs().ok()?.into_iter()
        .find(|iface| iface.name == name && !iface.is_loopback())
        .and_then(|iface| match iface.ip() {
            IpAddr::V4(addr) => Some(addr),
            _ => None,
        })
}
