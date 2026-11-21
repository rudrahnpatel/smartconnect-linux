//! Wi-Fi reconnect over `/readyfor/mdns` (spec: `evidence-2026-10-20/reconnect-spec.md`).
//!
//! Both ends advertise `_readyformdns._tcp` and serve a TLS WebSocket at `/readyfor/mdns`. Either
//! side pushes one encrypted *discovery frame* to the other:
//! `[16 B sender nonce id][1 B type][12 B IV][AES-256-GCM ct || tag]`, AAD = the first 17 bytes,
//! key = the PC's per-pair key (the one sent in EXCHANGE_KEY, `paired_device.secretKey`).
//! - Connect button: the phone finds us by mDNS, dials our `/readyfor/mdns` and sends a `cmd:0`
//!   trigger; we answer on the same socket with the `cmd:1` invite.
//! - Daemon start / post-pair: we find the phone's own advert, dial its `/readyfor/mdns` and send
//!   the invite. The phone sends nothing back there; it dials `wss://pc:wsPort/readyfor/`.
//! Type 2 (plaintext) makes the phone UNPAIR — never send it.

use std::net::{IpAddr, SocketAddr};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use futures_util::{SinkExt, StreamExt};
use rand::RngCore;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{self, ClientConfig, DigitallySignedStruct, SignatureScheme};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use crate::identity::{self, Identity};
use crate::invite::{VER_CODE, VER_NAME};
use crate::server::Daemon;

pub const MDNS_PATH: &str = "/readyfor/mdns";
const SERVICE_TYPE: &str = "_readyformdns._tcp";
/// What the phone's cable mode browses for on the USB tethering link (OBSERVED on a device).
const CABLE_SERVICE_TYPE: &str = "_readyfor._tcp";
/// Where the phone's cable mode dials once it resolves [`CABLE_SERVICE_TYPE`].
pub const CABLE_PATH: &str = "/readyfor/rdp";
const TYPE_ENCRYPTED: u8 = 1;
const HEADER_LEN: usize = 17;
const IV_LEN: usize = 12;
const TAG_LEN: usize = 16;

/// Encrypts `plaintext` into a type-1 discovery frame from `sender_nonce` (16 raw bytes).
pub fn seal(key_b64: &str, sender_nonce: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    ensure!(sender_nonce.len() == 16, "nonce id must be 16 bytes");
    let mut iv = [0u8; IV_LEN];
    rand::rngs::OsRng.fill_bytes(&mut iv);
    seal_with_iv(key_b64, sender_nonce, &iv, plaintext)
}

fn seal_with_iv(key_b64: &str, sender_nonce: &[u8], iv: &[u8; IV_LEN], plaintext: &[u8]) -> Result<Vec<u8>> {
    let mut frame = Vec::with_capacity(HEADER_LEN + IV_LEN + plaintext.len() + TAG_LEN);
    frame.extend_from_slice(sender_nonce);
    frame.push(TYPE_ENCRYPTED);
    let ct = cipher(key_b64)?
        .encrypt(Nonce::from_slice(iv), Payload { msg: plaintext, aad: &frame[..HEADER_LEN] })
        .map_err(|_| anyhow!("AES-GCM encrypt failed"))?;
    frame.extend_from_slice(iv);
    frame.extend_from_slice(&ct);
    Ok(frame)
}

/// Splits a discovery frame into (sender nonce, type, plaintext), decrypting type 1 with `key_b64`.
pub fn open(key_b64: &str, frame: &[u8]) -> Result<(Vec<u8>, u8, Vec<u8>)> {
    ensure!(frame.len() >= HEADER_LEN + IV_LEN + TAG_LEN, "discovery frame too short ({} B)", frame.len());
    let (nonce, kind) = (frame[..16].to_vec(), frame[16]);
    ensure!(kind == TYPE_ENCRYPTED, "unexpected discovery frame type {kind}");
    let iv = &frame[HEADER_LEN..HEADER_LEN + IV_LEN];
    let pt = cipher(key_b64)?
        .decrypt(Nonce::from_slice(iv), Payload { msg: &frame[HEADER_LEN + IV_LEN..], aad: &frame[..HEADER_LEN] })
        .map_err(|_| anyhow!("AES-GCM decrypt failed (wrong key?)"))?;
    Ok((nonce, kind, pt))
}

fn cipher(key_b64: &str) -> Result<Aes256Gcm> {
    let key = B64.decode(key_b64).context("per-pair key is not base64")?;
    Aes256Gcm::new_from_slice(&key).map_err(|_| anyhow!("per-pair key is {} bytes, need 32", key.len()))
}

/// The `cmd:1` invite: where and how to dial us. The phone checks token, fp, policies and wsPort,
/// then dials `wss://<ip>:<wsPort>/readyfor/` pinning our cert by `fp`.
pub fn invite_json(id: &Identity, token: &str, ips: &[IpAddr], ws_port: u16) -> Value {
    json!({
        "ver": 2,
        "cmd": 1,
        "deviceId": id.device_id,
        "deviceName": id.device_name,
        "fp": id.fp_base64(),
        "token": token,
        "policies": crate::invite::policies(ips),
        "wsPort": ws_port,
        "appVer": VER_NAME,
        "verCode": VER_CODE,
        "os": 6,
        "scType": 2,
        "type": 4,
    })
}

/// The paired phone a nonce id belongs to, with the keys to try (ours first).
fn phone_for_nonce(daemon: &Daemon, nonce: &[u8]) -> Option<(String, Vec<String>)> {
    let pairing = daemon.pairing.lock().unwrap();
    pairing.iter().find_map(|(real_id, p)| {
        identity::is_nonce_of(nonce, real_id).then(|| {
            let keys = std::iter::once(p.key.clone()).chain(p.phone_key.clone()).collect();
            (real_id.clone(), keys)
        })
    })
}

pub fn sealed_invite(daemon: &Daemon, key: &str) -> Result<Vec<u8>> {
    let invite = invite_json(&daemon.identity, &daemon.current_token(), &daemon.ips(), daemon.ws_port);
    let nonce = B64.decode(&daemon.identity.device_id)?;
    seal(key, &nonce, invite.to_string().as_bytes())
}

/// The phone dialed our `/readyfor/mdns` (Connect button): answer its `cmd:0` trigger with the invite.
pub async fn serve<S>(daemon: Arc<Daemon>, mut ws: WebSocketStream<S>, peer: SocketAddr) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    while let Some(msg) = ws.next().await {
        let frame = match msg? {
            Message::Binary(b) => b,
            Message::Close(_) => break,
            Message::Text(t) => {
                debug!(%peer, "mdns: ignoring text frame ({} B)", t.len());
                continue;
            }
            _ => continue,
        };
        let Some((phone, keys)) = frame.get(..16).and_then(|n| phone_for_nonce(&daemon, n)) else {
            warn!(%peer, "mdns: frame from an unknown device; ignoring");
            continue;
        };
        let Some((key, pt)) = keys.iter().find_map(|k| open(k, &frame).ok().map(|(_, _, pt)| (k.clone(), pt))) else {
            warn!(%peer, %phone, "mdns: could not decrypt the phone's frame with any pair key");
            continue;
        };
        let msg: Value = serde_json::from_slice(&pt).unwrap_or(Value::Null);
        info!(%peer, %phone, cmd = ?msg.get("cmd"), "mdns: phone trigger");
        if msg.get("cmd").and_then(Value::as_i64) == Some(0) {
            ws.send(Message::binary(sealed_invite(&daemon, &key)?)).await?;
            info!(%peer, %phone, "mdns: sent invite; the phone should dial /readyfor/ now");
        }
    }
    Ok(())
}

/// Dials a phone's `/readyfor/mdns` and pushes our invite (reconnect without a QR).
pub async fn dial_phone(daemon: &Daemon, phone_real_id: &str, addr: SocketAddr) -> Result<()> {
    let key = daemon.pairing.lock().unwrap().get(phone_real_id).map(|p| p.key.clone()).context("phone not paired")?;
    let invite = invite_json(&daemon.identity, &daemon.current_token(), &daemon.ips(), daemon.ws_port).to_string();
    push_invite(addr, &daemon.identity.device_id, &key, &invite).await
}

/// Opens `wss://addr/readyfor/mdns`, sends the sealed invite as one binary frame, then lingers
/// briefly: the phone answers by dialing our control port, not on this socket.
pub async fn push_invite(addr: SocketAddr, our_nonce_b64: &str, key: &str, invite: &str) -> Result<()> {
    let frame = seal(key, &B64.decode(our_nonce_b64)?, invite.as_bytes())?;
    let tcp = timeout(Duration::from_secs(5), TcpStream::connect(addr)).await??;
    let config = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .dangerous()
        // The phone presents a throwaway self-signed cert; the payload itself is AES-GCM sealed.
        .with_custom_certificate_verifier(Arc::new(AcceptAny))
        .with_no_client_auth();
    let tls = TlsConnector::from(Arc::new(config)).connect(ServerName::IpAddress(addr.ip().into()), tcp).await?;
    let url = format!("wss://{addr}{MDNS_PATH}");
    let (mut ws, _) = timeout(Duration::from_secs(5), tokio_tungstenite::client_async(url, tls)).await??;
    ws.send(Message::binary(frame)).await?;
    info!(%addr, "mdns: invite pushed to the phone");
    tokio::time::sleep(Duration::from_secs(5)).await;
    let _ = ws.close(None).await;
    Ok(())
}

/// Desktop-side Connect: Wi-Fi first (instant, but the phone only listens while its app is open),
/// then Bluetooth LE (works with the app in the background, takes a few seconds).
///
/// Why Bluetooth: in the background the phone app has no LAN listener at all (its `/readyfor/mdns`
/// server and mDNS advert stop when the app leaves the foreground), and it never starts a
/// reconnect to a Windows-class PC by itself unless the pair is a Lenovo-cloud one. The genuine
/// Windows PC wakes it the same way: a GATT write as central (see `ble_connect`).
pub async fn connect_now(daemon: &Daemon) -> (bool, String) {
    // One attempt at a time: a second caller (button during the auto loop) waits for the first,
    // instead of both scanning and connecting over BlueZ at once ("Operation already in progress").
    let _one_at_a_time = daemon.connect_lock.lock().await;
    if daemon.is_connected() {
        return (true, "already connected".into());
    }
    daemon.clear_phone_close();
    let mut wifi_err = None;
    for (phone, addr) in daemon.known_phone_endpoints() {
        match dial_phone(daemon, &phone, addr).await {
            Ok(()) => return (true, "invite sent over Wi-Fi; the phone should connect in a moment".into()),
            Err(e) => {
                info!(%addr, "connect: Wi-Fi dial failed ({e:#}); trying Bluetooth");
                wifi_err = Some(e);
            }
        }
    }
    let wifi_hint = match &wifi_err {
        Some(e) if is_refused(e) => "the phone's Smart Connect app is in the background",
        _ => "the phone isn't reachable over Wi-Fi",
    };
    if !daemon.ble_wake() {
        return (false, format!("{wifi_hint} and Bluetooth wake is off (--no-ble-wake): open Smart Connect on the phone"));
    }
    match crate::ble_connect::connect(daemon).await {
        Ok(msg) => (true, msg),
        Err(e) => {
            warn!("connect: Bluetooth failed: {e:#}");
            (false, format!("{wifi_hint}, and Bluetooth didn't reach it ({e:#}): open Smart Connect on the phone"))
        }
    }
}

/// Whether a dial failed because the phone actively refused it (it's on the LAN, app not open).
fn is_refused(e: &anyhow::Error) -> bool {
    e.chain()
        .filter_map(|c| c.downcast_ref::<std::io::Error>())
        .any(|io| io.kind() == std::io::ErrorKind::ConnectionRefused)
}

/// Advertises `_readyformdns._tcp` via avahi so the phone can find us (Connect button).
/// TXT matches the Windows PC: deviceId, deviceName, appType=1, type=4, os=6, cap, timestamp (s), fp (hex).
pub fn advertise(id: &Identity, port: u16) -> Result<Child> {
    publish(id, port, SERVICE_TYPE)
}

/// Advertises `_readyfor._tcp` (same TXT, same port) for the phone's cable mode: after the user
/// picks Smart Connect in the phone's USB popup, the phone turns on USB tethering, browses for
/// this service on the cable and dials [`CABLE_PATH`] (OBSERVED on a device).
pub fn advertise_cable(id: &Identity, port: u16) -> Result<Child> {
    publish(id, port, CABLE_SERVICE_TYPE)
}

fn publish(id: &Identity, port: u16, service: &str) -> Result<Child> {
    let ts = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let txt = [
        format!("deviceId={}", id.device_id),
        format!("deviceName={}", id.device_name),
        "appType=1".into(),
        "type=4".into(),
        "os=6".into(),
        "cap=0".into(),
        format!("timestamp={ts}"),
        format!("fp={}", id.fp_hex()),
    ];
    let child = Command::new("avahi-publish")
        .arg("-s")
        .arg(&id.device_name)
        .arg(service)
        .arg(port.to_string())
        .args(&txt)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("starting avahi-publish (is avahi running?)")?;
    info!(port, "mdns: advertising {service}");
    Ok(child)
}

/// Follows phones' own `_readyformdns._tcp` adverts (up only while Smart Connect is in the foreground)
/// and dials any paired phone that isn't connected, so it reconnects without a QR.
pub async fn watch_phones(daemon: Arc<Daemon>) -> Result<()> {
    let mut child = Command::new("avahi-browse")
        .args(["-r", "-p", "-k", SERVICE_TYPE])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("starting avahi-browse")?;
    let mut lines = BufReader::new(child.stdout.take().context("avahi-browse stdout")?).lines();
    while let Some(line) = lines.next_line().await? {
        let Some((addr, txt_device_id)) = parse_resolved(&line) else { continue };
        let Ok(nonce) = B64.decode(&txt_device_id) else { continue };
        let Some((phone, _)) = phone_for_nonce(&daemon, &nonce) else { continue };
        daemon.remember_phone_mdns(&phone, addr);
        if !daemon.should_dial_back() {
            info!(%addr, %phone, "mdns: phone is advertising but just disconnected or is connected; not dialing");
            continue;
        }
        info!(%addr, %phone, "mdns: paired phone is advertising; dialing it back");
        let d = daemon.clone();
        tokio::spawn(async move {
            if let Err(e) = dial_phone(&d, &phone, addr).await {
                warn!(%addr, "mdns: dial-back failed: {e:#}");
            }
        });
    }
    bail!("avahi-browse exited: {:?}", child.wait().await?)
}

/// `=;iface;IPv4;name;type;domain;host;address;port;"k=v" "k=v"` → (address:port, TXT deviceId).
fn parse_resolved(line: &str) -> Option<(SocketAddr, String)> {
    let f: Vec<&str> = line.splitn(10, ';').collect();
    if f.len() < 10 || f[0] != "=" || f[2] != "IPv4" {
        return None;
    }
    let addr = SocketAddr::new(f[7].parse().ok()?, f[8].parse().ok()?);
    let device_id = f[9]
        .split('"')
        .find_map(|kv| kv.strip_prefix("deviceId="))
        .map(str::to_owned)?;
    Some((addr, device_id))
}

#[derive(Debug)]
struct AcceptAny;

impl ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider().signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same dummy inputs as the RE agent's Python vector (`scratchpad/tools/kat.py`).
    #[test]
    fn frame_round_trips_with_the_phone_layout() {
        let key = B64.encode((0u8..32).collect::<Vec<_>>());
        let nonce: Vec<u8> = (0xA0u8..0xB0).collect();
        let iv: [u8; 12] = core::array::from_fn(|i| i as u8);
        let frame = seal_with_iv(&key, &nonce, &iv, b"{\"cmd\":1}").unwrap();
        assert_eq!(frame.len(), 29 + 9 + 16);
        assert_eq!(&frame[..16], &nonce[..]);
        assert_eq!(frame[16], 1);
        assert_eq!(&frame[17..29], &iv);
        let (n, kind, pt) = open(&key, &frame).unwrap();
        assert_eq!((n, kind, pt.as_slice()), (nonce, 1, &b"{\"cmd\":1}"[..]));
        let mut tampered = frame.clone();
        tampered[16] = 2; // AAD covers the type byte
        assert!(open(&key, &tampered).is_err());
    }

    #[test]
    fn refused_dials_mean_the_app_is_in_the_background() {
        let refused = anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::ConnectionRefused)).context("dialing");
        assert!(is_refused(&refused));
        let timed_out = anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::TimedOut));
        assert!(!is_refused(&timed_out));
    }

    #[test]
    fn parses_avahi_resolved_lines() {
        let line = r#"=;wlan0;IPv4;phone;_readyformdns._tcp;local;phone.local;192.168.1.20;11234;"fp=ab" "deviceId=AAECAwQFBgcICQoLDA0ODw==" "os=1""#;
        let (addr, id) = parse_resolved(line).unwrap();
        assert_eq!(addr, "192.168.1.20:11234".parse().unwrap());
        assert_eq!(id, "AAECAwQFBgcICQoLDA0ODw==");
        assert!(parse_resolved("+;wlan0;IPv4;phone;_readyformdns._tcp;local").is_none());
    }
}
