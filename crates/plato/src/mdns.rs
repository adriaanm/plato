//! The mDNS/DNS-SD responder: the reader answers to `platokin.local`.
//!
//! Why this exists.  Every other rung of platonic's discovery ladder either
//! needs the router to register DHCP client names (it does here; a public
//! user's may not) or needs code on the Mac.  A responder needs neither:
//! macOS resolves `.local` natively through mDNSResponder, so once the device
//! answers, `ssh platokin.local` works from any Mac with nothing installed.
//!
//! ## Who decides that the radio is up
//!
//! **The scripts do, and they say so.**  Every path that changes the radio goes
//! through the platokin repo's WiFi scripts -- Plato's own menu delegates to
//! `wifi-enable.sh`/`wifi-disable.sh`, `just wifi-up` runs them over ssh, and an
//! unattended reassociation reaches `wifi-l3.sh` through `wifi-events.sh`.  So
//! they poke the command FIFO (`plato_core::fifo`) with `wifi-up [ADDR]` and
//! `wifi-down`, alongside the existing `kick_clock` call, and this module acts
//! on the verb.  Nothing here guesses, and nothing here polls.
//!
//! Two things that design still has to get right, both learned the hard way:
//!
//! * **A session can BEGIN with the radio already up**, which is the normal
//!   case here -- WiFi is often brought up by the scripts over ssh long before
//!   Plato starts, and the radio-off policy has usually cleared
//!   `settings.wifi` besides.  There is no transition to observe then, so
//!   `init()` takes ONE measurement at startup.  An earlier version keyed
//!   everything off transitions and had exactly this hole.
//! * **The verb is a notification, not evidence.**  `wifi_up` re-measures
//!   `wlan0` rather than trusting the address on the line, because the address
//!   is what goes in the A record and a stale one is worse than none.  A
//!   disagreement between the two is logged, not silently resolved.
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
//! ## Logging
//!
//! Every branch logs -- the verb received, the action taken, and the reason
//! when the action is nothing.  That is the repo rule, and this is why it is
//! spelled out here: the first field test of this module produced total
//! silence, which is indistinguishable from a binary that does not contain it.
//! The first check on the device is `netstat -uln | grep 5353`: it says whether
//! the socket exists at all, and it depends on no query reaching us.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Mutex;
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceInfo};

/// The DNS-SD type.  Port 2222 is dropbear's: platonic's transport is ssh, so
/// what is worth advertising is the ssh endpoint, not a second listener.
const SERVICE_TYPE: &str = "_platonic._tcp.local.";
const SSH_PORT: u16 = 2222;
const IFACE: &str = "wlan0";

/// Unregister and shutdown are asynchronous; both are worth a short wait so the
/// goodbye packets are on the wire before the radio goes.
const TEARDOWN_WAIT: Duration = Duration::from_secs(2);

struct Responder {
    daemon: ServiceDaemon,
    fullname: String,
    addr: Ipv4Addr,
}

struct Shared {
    /// Empty until `init()` runs, and forever if the feature is switched off.
    name: String,
    responder: Option<Responder>,
}

static SHARED: Mutex<Shared> = Mutex::new(Shared {
    name: String::new(),
    responder: None,
});

/// Record the name and take the one startup measurement.  Called once at app
/// start, unconditionally and NOT gated on `settings.wifi`: whether the radio
/// is up right now is a question for `getifaddrs`, not for a saved setting.
pub fn init(name: &str) {
    if name.is_empty() {
        println!("mDNS: disabled (mdns-name is empty).");
        return;
    }

    let mut shared = SHARED.lock().unwrap();
    shared.name = name.to_string();
    println!("mDNS: enabled as {}.local; measuring {} at startup.", name, IFACE);
    advertise(&mut shared, "startup");
}

/// `wifi-up [ADDR]` from the FIFO, and Plato's own successful enable.  The
/// address on the line is advisory: `wlan0` is re-measured, and a disagreement
/// is reported rather than resolved silently.
pub fn wifi_up(hint: Option<&str>) {
    let mut shared = SHARED.lock().unwrap();
    if shared.name.is_empty() {
        println!("mDNS: wifi-up ignored -- the responder is disabled.");
        return;
    }
    if let (Some(hint), Some(measured)) = (hint, iface_addr(IFACE)) {
        if hint != measured.to_string() {
            println!("mDNS: wifi-up says {} but {} measures {}; using the \
                      measurement.", hint, IFACE, measured);
        }
    }
    advertise(&mut shared, "wifi-up");
}

/// `wifi-down` from the FIFO, and every in-process path that drops the radio
/// (suspend, share, exit, the menu toggle).  Idempotent, and deliberately
/// reachable from both: whichever arrives first does the work, and the second
/// says so rather than falling silent.
///
/// It must run BEFORE the link goes -- a goodbye packet sent over a dead link
/// is not sent at all, and this is a network we do not own.
pub fn stop() {
    let mut shared = SHARED.lock().unwrap();
    if shared.name.is_empty() {
        return;
    }
    let name = shared.name.clone();
    match shared.responder.take() {
        Some(responder) => {
            teardown(responder);
            println!("mDNS: wifi-down -- withdrew {}.local.", name);
        }
        None => println!("mDNS: wifi-down -- nothing was advertised."),
    }
}

/// Measure, then register if that changes anything.  `why` names the trigger,
/// so the log says which of the several callers actually did the work.
fn advertise(shared: &mut Shared, why: &str) {
    let name = shared.name.clone();

    let Some(addr) = iface_addr(IFACE) else {
        // Not an error: the radio is simply off, or the poke beat the address.
        println!("mDNS: {} -- {} has no IPv4 address; not advertising.",
                 why, IFACE);
        return;
    };

    if shared.responder.as_ref().map(|r| r.addr) == Some(addr) {
        println!("mDNS: {} -- already advertising {}.local as {}; no change.",
                 why, name, addr);
        return;
    }

    if let Some(responder) = shared.responder.take() {
        let old = responder.addr;
        teardown(responder);
        println!("mDNS: {} -- {}.local moved off {}.", why, name, old);
    }

    match register(&name, addr) {
        Ok(responder) => {
            shared.responder = Some(responder);
            println!("mDNS: {} -- advertising {}.local as {} on {}, and {} on \
                      port {} (udp/5353 must be open on {}).",
                     why, name, addr, IFACE, SERVICE_TYPE, SSH_PORT, IFACE);
        }
        Err(e) => eprintln!("mDNS: {} -- can't advertise {}.local as {}: {}.",
                            why, name, addr, e),
    }
}

fn teardown(responder: Responder) {
    if let Ok(rx) = responder.daemon.unregister(&responder.fullname) {
        rx.recv_timeout(TEARDOWN_WAIT).ok();
    }
    if let Ok(rx) = responder.daemon.shutdown() {
        rx.recv_timeout(TEARDOWN_WAIT).ok();
    }
}

fn register(name: &str, addr: Ipv4Addr) -> Result<Responder, mdns_sd::Error> {
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

    Ok(Responder { daemon, fullname, addr })
}

/// The interface's IPv4 address.
///
/// `filter().find_map()`, NOT `find().and_then()` -- `getifaddrs` returns one
/// entry per address, so on a dual-stack interface the FIRST `wlan0` entry is
/// its IPv6 link-local one.  Matching the name and then asking that one entry
/// for v4 reports "no address" on an interface that plainly has one.  Caught on
/// the Mac, where `en1`'s v4 address sits sixth in the list behind five v6 ones.
fn iface_addr(name: &str) -> Option<Ipv4Addr> {
    if_addrs::get_if_addrs().ok()?.into_iter()
        .filter(|iface| iface.name == name && !iface.is_loopback())
        .find_map(|iface| match iface.ip() {
            IpAddr::V4(addr) => Some(addr),
            _ => None,
        })
}
