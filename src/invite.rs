//! Pairing invite: the one-shot token and the QR the phone scans.

use std::net::IpAddr;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use qrcode::render::{svg, unicode};
use qrcode::{EcLevel, QrCode};
use rand::{Rng, RngCore};
use serde_json::{Value, json};

use crate::identity::Identity;
use crate::proto::SERVICE_DEVICE_PAIRED;

/// Literal prefix the phone's QR parser looks for (`MOTO_RDP_CONNECTION_HEADER`).
pub const QR_PREFIX: &str = "motorolardpconnection";
/// Version the current Windows Smart Connect app advertises. The phone requires a
/// PC-class peer (os=6/type=4) to report verCode >= 700000000 or it shows the
/// "upgrade your PC app" dialog and refuses to connect
/// (`VersionMatchingDialogActivity.getVersionMatchType`). The genuine app encodes its
/// dotted build `09.0.3.013.000` by concatenating the components -> 903013000; the
/// phone's own Android build `09.0.3.013.0` (1-digit tail) encodes to 9030130, which
/// is why the gate separates the two.
pub const VER_CODE: u64 = 903013000;
pub const VER_NAME: &str = "09.0.3.013.000";
const OS_WINDOWS: i64 = 6;
const TYPE_PC: i64 = 4;

/// The invite's `policies`: where the phone may dial us. Every address goes under `wifi`: the
/// phone dials a cable address listed there (OBSERVED on a device) but not one listed only under
/// `usb` (OBSERVED: pairing then fails). Cable addresses also go under `usb`, the key the phone
/// app reserves for a cable; the phone still labels such a connection Wi-Fi.
pub fn policies(ips: &[IpAddr]) -> serde_json::Value {
    policies_with(ips, &crate::reconnect::usb_tether_ips())
}

fn policies_with(ips: &[IpAddr], usb_tether: &[IpAddr]) -> serde_json::Value {
    let wifi: Vec<String> = ips.iter().map(ToString::to_string).collect();
    let usb: Vec<String> = ips.iter().filter(|ip| usb_tether.contains(ip)).map(ToString::to_string).collect();
    let mut out = vec![json!({ "wifi": wifi })];
    if !usb.is_empty() {
        out.push(json!({ "usb": usb }));
    }
    serde_json::Value::Array(out)
}

pub struct Invite {
    pub token: String,
    pub expires: Instant,
    pub ws_port: u16,
    pub ips: Vec<IpAddr>,
}

impl Invite {
    pub fn new(ws_port: u16, ips: Vec<IpAddr>, ttl: Duration) -> Self {
        Self { token: gen_token(), expires: Instant::now() + ttl, ws_port, ips }
    }

    pub fn is_valid(&self, token: &str) -> bool {
        Instant::now() < self.expires && constant_time_eq(self.token.as_bytes(), token.as_bytes())
    }

    /// `motorolardpconnection{"wsData":{…},"version":"3.0.0"}`, as the Windows UI builds it
    /// (`setHostInfo` → `prefix + JSON.stringify(hostInfo)`).
    ///
    /// `ble_address` is the PC's Bluetooth MAC; the phone drives the BLE pairing connection off
    /// `RemoteClient.bleAddress`, so without it the phone dials a stale stored address.
    pub fn qr_payload(&self, id: &Identity, ble_address: Option<&str>) -> String {
        let ws_data = json!({
            // cmd:1 tells the phone to create the WebSocket and connect as client immediately
            // (the Windows "success" QR). cmd:0 instead makes the phone try a policy exchange that
            // times out -> "Receive gatt data timeout" (code 128). ver:2 + appVer match the genuine app.
            "cmd": 1,
            "ver": 2,
            "appVer": VER_NAME,
            "i3": 1,
            "deviceId": id.device_id,
            "realDeviceId": id.real_device_id,
            "deviceName": id.device_name,
            "serviceId": SERVICE_DEVICE_PAIRED,
            "token": self.token,
            "fp": id.fp_base64(),
            "wsPort": self.ws_port,
            "policies": policies(&self.ips),
            "os": OS_WINDOWS,
            "type": TYPE_PC,
            "verCode": VER_CODE,
            "verName": VER_NAME,
            "bleAddress": ble_address.unwrap_or(""),
            "device": device_json(id, ble_address),
        });
        let host_info = json!({ "wsData": ws_data, "version": "3.0.0" });
        format!("{QR_PREFIX}{host_info}")
    }
}

/// The PC's `device.Device` as embedded in invites.
pub fn device_json(id: &Identity, ble_address: Option<&str>) -> Value {
    json!({
        "deviceId": id.device_id,
        "realDeviceId": id.real_device_id,
        "deviceName": id.device_name,
        "os": OS_WINDOWS,
        "type": TYPE_PC,
        "verCode": VER_CODE,
        "verName": VER_NAME,
        "bleAddress": ble_address.unwrap_or(""),
    })
}

/// `utils.GenRandomToken(256)`: base64 of 32 random bytes with `=`, `+`, `/` swapped
/// for random alphanumerics.
pub fn gen_token() -> String {
    const ALNUM: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let mut rng = rand::thread_rng();
    B64.encode(bytes)
        .chars()
        .map(|c| match c {
            '=' | '+' | '/' => ALNUM[rng.gen_range(0..ALNUM.len())] as char,
            c => c,
        })
        .collect()
}

/// `utils.GenRandomKey(256)`: base64 of 32 random bytes (standard alphabet kept).
pub fn gen_key() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    B64.encode(bytes)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Terminal rendering. `invert` draws light modules as blocks, which is what a
/// dark-background terminal needs for scanners that expect dark-on-light.
pub fn render_terminal(payload: &str, invert: bool) -> Result<String> {
    let code = QrCode::with_error_correction_level(payload.as_bytes(), EcLevel::L)?;
    let mut r = code.render::<unicode::Dense1x2>();
    if invert {
        r.dark_color(unicode::Dense1x2::Light).light_color(unicode::Dense1x2::Dark);
    }
    Ok(r.quiet_zone(true).build())
}

pub fn write_svg(payload: &str, path: &Path) -> Result<()> {
    let code = QrCode::with_error_correction_level(payload.as_bytes(), EcLevel::L)?;
    let image = code.render::<svg::Color>().min_dimensions(480, 480).quiet_zone(true).build();
    // Owner-only: while valid, the QR's token lets any phone pair with this PC.
    crate::identity::write_private(path, image.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cable_addresses_are_offered_under_usb_too() {
        let lan: IpAddr = "192.168.1.20".parse().unwrap();
        let cable: IpAddr = "192.168.201.10".parse().unwrap();
        assert_eq!(policies_with(&[lan], &[]), json!([{ "wifi": ["192.168.1.20"] }]));
        assert_eq!(
            policies_with(&[lan, cable], &[cable]),
            json!([{ "wifi": ["192.168.1.20", "192.168.201.10"] }, { "usb": ["192.168.201.10"] }])
        );
    }
}
