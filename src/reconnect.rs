//! Keeping a paired phone connected without the user opening Smart Connect on it.
//!
//! In the background the phone app runs no LAN listener and never starts a reconnect to a
//! Windows-class PC itself (only to Lenovo-cloud pairs, or once when its UI comes back to the
//! foreground). So the PC has to reach out: `discovery::connect_now` tries the phone's
//! `/readyfor/mdns` (app open) and then wakes it over Bluetooth LE (app in the background), as the
//! genuine Windows PC does. This module decides *when*:
//! - `run`: the reconnect loop. Backoff 2/5/15/30/60 s, then every 5 min — but at most a minute
//!   (for the first `EAGER_ATTEMPTS`) while the phone answers on the LAN, since then it's almost
//!   certainly in Bluetooth range too and only a missed advert stood in the way. Between attempts
//!   it probes the phone's last LAN address every 30 s and tries at once when the phone comes back
//!   on Wi-Fi. `Daemon::nudge_reconnect` also cuts the wait short.
//! - `watch_system`: nudges the loop after a resume from suspend or a LAN address change, and
//!   keeps the invite addresses current.
//! - `ble_connect::watch_adapter`: nudges it when the PC's Bluetooth is turned on.

use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::net::TcpStream;
use tokio::time::{Instant, sleep_until, timeout};
use tracing::{debug, info};

use crate::server::Daemon;

/// Waits before attempt N (0-based). The last value repeats forever.
const BACKOFF_S: [u64; 6] = [2, 5, 15, 30, 60, 300];
/// While the phone is on the LAN, retry at least once a minute for this many attempts.
const EAGER_ATTEMPTS: usize = 15;
const EAGER_CAP: Duration = Duration::from_secs(60);
/// How often to check, between attempts, whether the phone is back on the LAN.
const PROBE_EVERY: Duration = Duration::from_secs(30);
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Any closed port will do: a present phone answers a SYN with a reset.
const PROBE_PORT: u16 = 11000;
const WATCH_EVERY: Duration = Duration::from_secs(5);
/// Suspend shows up as boot-time clock running ahead of the monotonic clock.
const SUSPEND_THRESHOLD: Duration = Duration::from_secs(10);

pub fn backoff(attempt: usize, phone_on_lan: bool) -> Duration {
    let wait = Duration::from_secs(BACKOFF_S[attempt.min(BACKOFF_S.len() - 1)]);
    if phone_on_lan && attempt < EAGER_ATTEMPTS { wait.min(EAGER_CAP) } else { wait }
}

/// Whether the phone is on the LAN, from a TCP connect to its last address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// It answered (connected, or refused: its kernel is there, the port just isn't open).
    Present,
    /// No answer, or no route: off Wi-Fi, out of range, or switched off.
    Absent,
    /// Couldn't tell (e.g. this PC has no network either).
    Unknown,
}

pub fn classify(result: &std::io::Result<()>) -> Presence {
    match result {
        Ok(()) => Presence::Present,
        Err(e) => match e.kind() {
            ErrorKind::ConnectionRefused | ErrorKind::ConnectionReset => Presence::Present,
            ErrorKind::TimedOut | ErrorKind::HostUnreachable => Presence::Absent,
            _ => Presence::Unknown,
        },
    }
}

/// One TCP SYN to the phone; costs nothing on the radio, unlike a Bluetooth scan.
pub async fn probe(ip: IpAddr) -> Presence {
    let addr = SocketAddr::new(ip, PROBE_PORT);
    match timeout(PROBE_TIMEOUT, TcpStream::connect(addr)).await {
        Ok(r) => classify(&r.map(drop)),
        Err(_) => Presence::Absent,
    }
}

/// Why the loop stopped waiting.
#[derive(Debug, PartialEq, Eq)]
enum Wake {
    Backoff,
    Nudged,
    PhoneBack,
}

/// The reconnect loop (one at a time, see `Daemon::spawn_auto_reconnect`). Runs until connected,
/// the user disconnects, or there's no paired phone.
pub async fn run(daemon: Arc<Daemon>, reason: &'static str) {
    info!("auto-reconnect ({reason})");
    let mut attempt = 0usize;
    let mut presence = Presence::Unknown;
    loop {
        if attempt > 0 {
            if let Some(ip) = daemon.phone_ip() {
                presence = probe(ip).await;
            }
        }
        let wake = wait(&daemon, backoff(attempt, presence == Presence::Present), &mut presence).await;
        if !daemon.should_dial_back() || daemon.pairing.lock().unwrap().is_empty() {
            break;
        }
        if wake != Wake::Backoff {
            debug!(?wake, "auto-reconnect: woken early; backoff restarts");
            attempt = 0;
        }
        let started = Instant::now();
        let (ok, msg) = crate::discovery::connect_now(&daemon).await;
        info!(attempt, ok, took_ms = started.elapsed().as_millis() as u64, "auto-reconnect: {msg}");
        if ok {
            // The phone dials back on its own; give it a moment before deciding.
            let until = Instant::now() + Duration::from_secs(5);
            while Instant::now() < until && !daemon.is_connected() {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            if daemon.is_connected() {
                break;
            }
        }
        attempt += 1;
    }
}

/// Sleeps for `delay`, probing the phone's LAN presence every `PROBE_EVERY`; returns early on a
/// nudge or when the phone goes from absent to present.
async fn wait(daemon: &Daemon, delay: Duration, presence: &mut Presence) -> Wake {
    let deadline = Instant::now() + delay;
    loop {
        let next = deadline.min(Instant::now() + PROBE_EVERY);
        tokio::select! {
            _ = sleep_until(next) => {}
            _ = daemon.reconnect_nudged() => return Wake::Nudged,
        }
        if Instant::now() >= deadline {
            return Wake::Backoff;
        }
        let Some(ip) = daemon.phone_ip() else { continue };
        let now = probe(ip).await;
        let was = std::mem::replace(presence, now);
        debug!(?was, ?now, "auto-reconnect: presence probe");
        if was == Presence::Absent && now == Presence::Present {
            info!("auto-reconnect: the phone is back on Wi-Fi");
            return Wake::PhoneBack;
        }
    }
}

/// Private IPv4 addresses on physical-looking interfaces. Container bridges and VPN tunnels are
/// skipped because the phone would try (and time out on) every listed IP.
pub fn detect_lan_ips() -> Result<Vec<IpAddr>> {
    const SKIP: &[&str] = &["lo", "docker", "br-", "veth", "virbr", "tun", "tap", "wg", "tailscale"];
    let mut ips: Vec<IpAddr> = if_addrs::get_if_addrs()?
        .into_iter()
        .filter(|i| !SKIP.iter().any(|p| i.name.starts_with(p)))
        .filter_map(|i| match i.ip() {
            IpAddr::V4(v4) if v4.is_private() => Some(IpAddr::V4(v4)),
            _ => None,
        })
        .collect();
    ips.sort();
    ips.dedup();
    Ok(ips)
}

/// Kernel drivers of a phone's USB tethering link (the PC end of a cable connection).
const USB_TETHER_DRIVERS: &[&str] = &["rndis_host", "cdc_ncm", "cdc_ether", "cdc_eem"];

/// Whether a network interface is a phone's USB tethering link, from its driver in sysfs.
pub fn is_usb_tether(ifname: &str) -> bool {
    std::fs::read_link(format!("/sys/class/net/{ifname}/device/driver"))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .is_some_and(|d| USB_TETHER_DRIVERS.contains(&d.as_str()))
}

/// The PC's IPv4 addresses on USB tethering links (a phone plugged in by cable).
pub fn usb_tether_ips() -> Vec<IpAddr> {
    let mut ips: Vec<IpAddr> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| matches!(i.ip(), IpAddr::V4(_)) && is_usb_tether(&i.name))
        .map(|i| i.ip())
        .collect();
    ips.sort();
    ips.dedup();
    ips
}

/// Whether `ip` is on one of this PC's USB tethering links (the phone's cable address).
pub fn is_on_usb_tether(ip: IpAddr) -> bool {
    let IpAddr::V4(ip) = ip else { return false };
    if_addrs::get_if_addrs().unwrap_or_default().into_iter().any(|i| match i.addr {
        if_addrs::IfAddr::V4(a) if is_usb_tether(&i.name) => {
            let m = u32::from(a.netmask);
            u32::from(a.ip) & m == u32::from(ip) & m
        }
        _ => false,
    })
}

/// Whether the LAN came up or gained an address since the last look (a reason to retry now).
pub fn gained_address(before: &[IpAddr], now: &[IpAddr]) -> bool {
    now.iter().any(|ip| !before.contains(ip))
}

/// How long the system was suspended between two readings of (boot-time, monotonic) clocks.
pub fn slept_for(before: (Duration, Duration), now: (Duration, Duration)) -> Duration {
    let boot = now.0.saturating_sub(before.0);
    let mono = now.1.saturating_sub(before.1);
    boot.saturating_sub(mono)
}

fn clocks() -> (Duration, Duration) {
    fn read(clock: libc::clockid_t) -> Duration {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: clock_gettime only writes the timespec we pass; both clocks exist on Linux.
        if unsafe { libc::clock_gettime(clock, &mut ts) } != 0 {
            return Duration::ZERO;
        }
        Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
    }
    (read(libc::CLOCK_BOOTTIME), read(libc::CLOCK_MONOTONIC))
}

/// Watches for resume from suspend and LAN address changes. With `refresh_ips` (no `--ip`), the
/// invite addresses follow the PC's current LAN addresses.
pub async fn watch_system(daemon: Arc<Daemon>, refresh_ips: bool) {
    let mut last_clocks = clocks();
    let mut last_ips = detect_lan_ips().unwrap_or_default();
    let mut tick = tokio::time::interval(WATCH_EVERY);
    loop {
        tick.tick().await;
        let now_clocks = clocks();
        let slept = slept_for(last_clocks, now_clocks);
        last_clocks = now_clocks;
        if slept > SUSPEND_THRESHOLD {
            info!(slept_s = slept.as_secs(), "this PC resumed from suspend");
            daemon.nudge_reconnect("resumed from suspend");
        }
        let Ok(ips) = detect_lan_ips() else { continue };
        if ips == last_ips {
            continue;
        }
        let gained = gained_address(&last_ips, &ips);
        let usb = usb_tether_ips();
        if usb.iter().any(|ip| ips.contains(ip) && !last_ips.contains(ip)) {
            info!(?usb, "cable: the phone's USB tethering link is up; offering it in invites");
        }
        last_ips = ips.clone();
        // Keep the old addresses while offline: an empty policy list makes the phone ignore invites.
        if refresh_ips && !ips.is_empty() {
            daemon.set_ips(ips);
        }
        if gained {
            daemon.nudge_reconnect("the network came up or changed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_then_settles_at_five_minutes() {
        let s: Vec<u64> = (0..8).map(|a| backoff(a, false).as_secs()).collect();
        assert_eq!(s, [2, 5, 15, 30, 60, 300, 300, 300]);
    }

    #[test]
    fn backoff_stays_eager_while_the_phone_is_on_the_lan() {
        let s: Vec<u64> = (0..8).map(|a| backoff(a, true).as_secs()).collect();
        assert_eq!(s, [2, 5, 15, 30, 60, 60, 60, 60]);
        assert_eq!(backoff(EAGER_ATTEMPTS, true).as_secs(), 300);
    }

    #[test]
    fn a_refusing_phone_is_present() {
        let e = |k| Err(std::io::Error::from(k));
        assert_eq!(classify(&Ok(())), Presence::Present);
        assert_eq!(classify(&e(ErrorKind::ConnectionRefused)), Presence::Present);
        assert_eq!(classify(&e(ErrorKind::TimedOut)), Presence::Absent);
        assert_eq!(classify(&e(ErrorKind::HostUnreachable)), Presence::Absent);
        assert_eq!(classify(&e(ErrorKind::NetworkUnreachable)), Presence::Unknown);
    }

    #[test]
    fn suspend_is_boot_time_running_ahead() {
        let s = |x| Duration::from_secs(x);
        assert_eq!(slept_for((s(100), s(100)), (s(105), s(105))), s(0));
        assert_eq!(slept_for((s(100), s(100)), (s(705), s(105))), s(600));
        assert_eq!(slept_for((s(100), s(100)), (s(90), s(105))), s(0));
    }

    #[test]
    fn only_new_addresses_count_as_the_network_coming_up() {
        let a: IpAddr = "192.168.1.20".parse().unwrap();
        let b: IpAddr = "10.0.0.5".parse().unwrap();
        assert!(gained_address(&[], &[a]));
        assert!(gained_address(&[a], &[a, b]));
        assert!(!gained_address(&[a, b], &[a]));
        assert!(!gained_address(&[a], &[]));
    }
}
