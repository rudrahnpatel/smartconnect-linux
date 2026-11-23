//! Keeps the PC's address on the phone's USB cable fixed, through NetworkManager.
//!
//! The phone gives its RNDIS tethering link a new random MAC address on every plug-in, so DHCP on
//! the phone hands the PC a new address each time, and the phone's Connect button and any QR
//! scanned while plugged in remember a stale one. A NetworkManager profile that matches the link by
//! driver and clones one fixed MAC onto it keeps the address the same.
//!
//! `run` creates that profile (owned by this user, so no admin rights are needed) the first time it
//! sees a phone's tethering link, and switches a link NetworkManager already brought up with its
//! default profile over to it. It does nothing without `nmcli`, and leaves an existing profile of
//! the same name alone, so a hand-made one wins. Only `rndis_host` is matched: `cdc_ether` and
//! `cdc_ncm` also drive USB docks and Ethernet dongles, which must keep their own MAC and routes.

use std::collections::HashSet;
use std::time::Duration;

use tokio::process::Command;
use tracing::{debug, info, warn};

pub const PROFILE: &str = "smartconnect-phone-usb";
/// The phone's USB tethering driver (Motorola phones use RNDIS).
const DRIVER: &str = "rndis_host";
/// Locally administered, unicast; the bytes spell "SCL".
const CLONED_MAC: &str = "02:53:43:4c:00:01";
const WATCH_EVERY: Duration = Duration::from_secs(3);

/// `nmcli connection add` arguments for the profile, owned by `user`.
pub fn add_args(user: &str) -> Vec<String> {
    [
        "connection", "add", "type", "ethernet", "con-name", PROFILE,
        "match.driver", DRIVER,
        "ethernet.cloned-mac-address", CLONED_MAC,
        "ipv4.method", "auto", "ipv4.never-default", "yes",
        "ipv6.method", "auto", "ipv6.never-default", "yes",
        "connection.autoconnect-priority", "50",
    ]
    .into_iter()
    .map(String::from)
    .chain(["connection.permissions".to_string(), format!("user:{user}")])
    .collect()
}

/// Network interfaces driven by the phone's tethering driver.
fn tether_ifaces() -> Vec<String> {
    let Ok(dir) = std::fs::read_dir("/sys/class/net") else { return Vec::new() };
    dir.filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            std::fs::read_link(format!("/sys/class/net/{name}/device/driver"))
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .is_some_and(|d| d == DRIVER)
        })
        .collect()
}

async fn nmcli(args: &[&str]) -> Option<std::process::Output> {
    Command::new("nmcli").args(args).output().await.ok()
}

async fn profile_exists() -> Option<bool> {
    let out = nmcli(&["-t", "-f", "NAME", "connection", "show"]).await?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).lines().any(|l| l == PROFILE))
}

/// The profile NetworkManager has active on `iface`, if any.
async fn active_profile(iface: &str) -> Option<String> {
    let out = nmcli(&["-g", "GENERAL.CONNECTION", "device", "show", iface]).await?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !name.is_empty()).then_some(name)
}

fn current_user() -> Option<String> {
    std::env::var("USER").ok().filter(|u| !u.is_empty()).or_else(|| {
        // SAFETY: getpwuid returns a pointer into static storage or null; we copy the name out.
        unsafe {
            let pw = libc::getpwuid(libc::getuid());
            (!pw.is_null()).then(|| std::ffi::CStr::from_ptr((*pw).pw_name).to_string_lossy().into_owned())
        }
    })
}

async fn create_profile() -> bool {
    let Some(user) = current_user() else {
        warn!("cable: can't tell the user name; not creating the {PROFILE} profile");
        return false;
    };
    let args = add_args(&user);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match nmcli(&args).await {
        Some(out) if out.status.success() => {
            info!("cable: created the NetworkManager profile {PROFILE}, which keeps this PC's cable address fixed");
            true
        }
        Some(out) => {
            warn!(
                "cable: couldn't create the NetworkManager profile {PROFILE}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
            false
        }
        None => false,
    }
}

/// Watches for the phone's tethering link and puts it on the fixed-MAC profile.
pub async fn run() {
    if nmcli(&["--version"]).await.is_none() {
        debug!("cable: no nmcli; the PC's cable address will change on each plug-in");
        return;
    }
    // Links already handled (switched, or failed to switch): one try each. The phone's random MAC
    // makes the interface name new on each plug-in on most systems, so a replug tries again.
    let mut handled: HashSet<String> = HashSet::new();
    let mut tick = tokio::time::interval(WATCH_EVERY);
    loop {
        tick.tick().await;
        let ifaces = tether_ifaces();
        handled.retain(|i| ifaces.contains(i));
        for iface in ifaces {
            if handled.contains(&iface) {
                continue;
            }
            let Some(active) = active_profile(&iface).await else { continue }; // not up yet
            handled.insert(iface.clone());
            if active == PROFILE {
                continue;
            }
            match profile_exists().await {
                Some(true) => {}
                Some(false) => {
                    if !create_profile().await {
                        continue;
                    }
                }
                None => continue,
            }
            info!(iface, from = active, "cable: switching the phone's link to {PROFILE}");
            match nmcli(&["connection", "up", PROFILE, "ifname", &iface]).await {
                Some(out) if out.status.success() => {}
                Some(out) => warn!(
                    iface,
                    "cable: couldn't switch the link to {PROFILE}: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
                None => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_profile_is_owned_by_the_user_and_never_takes_the_default_route() {
        let args = add_args("alice").join(" ");
        assert!(args.contains("con-name smartconnect-phone-usb"));
        assert!(args.contains("match.driver rndis_host"));
        assert!(args.contains("ethernet.cloned-mac-address 02:53:43:4c:00:01"));
        assert!(args.contains("ipv4.never-default yes"));
        assert!(args.contains("ipv6.never-default yes"));
        assert!(args.ends_with("connection.permissions user:alice"));
    }
}
