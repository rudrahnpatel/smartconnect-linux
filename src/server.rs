//! Control WebSocket server: TLS, `/readyfor/` upgrade, MANAGER handshake, then a
//! logging loop for everything the phone sends.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::{self, ServerConfig};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tracing::{debug, info, warn};

use crate::clipboard::{self, LocalClipboard};
use crate::discovery;
use crate::notifications::{self, Notifier, PhoneCommand};
use crate::status::StatusHub;
use crate::identity::{self, Identity};
use crate::invite::{self, Invite};
use crate::pairing::PairingStore;
use crate::proto::{self, Hello, SERVICE_MANAGER, WsMsg};

const CONTROL_PATH: &str = "/readyfor/";
/// Windows sets a 40 s read deadline for the first frame after the upgrade.
const FIRST_MESSAGE_TIMEOUT: Duration = Duration::from_secs(40);
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const PING_INTERVAL: Duration = Duration::from_secs(20);
/// A session that hasn't produced a single frame (the phone answers our pings) for this long is
/// dead — the phone left Wi-Fi, or this PC slept — and is dropped so the reconnect loop can run.
/// Without it a dead TCP link looks "connected" until the kernel gives up (~15 min).
const PEER_SILENCE_LIMIT: Duration = Duration::from_secs(70);

pub struct QrOutput {
    pub invert: bool,
    pub svg_path: PathBuf,
}

pub struct Daemon {
    pub identity: Identity,
    pub pairing: Mutex<PairingStore>,
    pub invite: Mutex<Option<Invite>>,
    pub ws_port: u16,
    /// LAN addresses put in invites; refreshed by `reconnect::watch_system` when not pinned by `--ip`.
    ips: Mutex<Vec<IpAddr>>,
    pub token_ttl: Duration,
    pub qr: QrOutput,
    pub session_dir: PathBuf,
    /// The PC's Bluetooth MAC to advertise in the QR's `bleAddress` (None if BLE is off).
    pub ble_address: Option<String>,
    pub clipboard: Arc<LocalClipboard>,
    active_sessions: AtomicUsize,
    /// Last seen `/readyfor/mdns` endpoint per paired phone (from its mDNS advert).
    phone_mdns: Mutex<HashMap<String, SocketAddr>>,
    endpoints_path: PathBuf,
    /// When the phone last closed a session itself (e.g. the user tapped Disconnect).
    /// Set by an explicit Disconnect (phone `cmd 6` or desktop); cleared by any Connect. While set,
    /// the daemon doesn't reconnect on its own.
    user_disconnected: std::sync::atomic::AtomicBool,
    /// The phone sent svc 0 `cmd 6` ("I'm disconnecting") on the current session.
    phone_said_bye: std::sync::atomic::AtomicBool,
    reconnecting: std::sync::atomic::AtomicBool,
    /// Wakes the reconnect loop early (network change, resume, Bluetooth on, phone back on Wi-Fi).
    reconnect_nudge: tokio::sync::Notify,
    /// One connect attempt at a time (desktop button, CLI and the loop share BlueZ).
    pub(crate) connect_lock: tokio::sync::Mutex<()>,
    /// Whether Connect may wake a backgrounded phone over Bluetooth LE (`--no-ble-wake` turns it off).
    ble_wake: std::sync::atomic::AtomicBool,
    /// The paired phone's last LAN address (from its session or mDNS advert), for presence probes.
    phone_ip: Mutex<Option<IpAddr>>,
    pub status: StatusHub,
    /// Desktop-side Disconnect: ends the active session.
    kick: tokio::sync::Notify,
    /// Messages for the phone from desktop-side features (notification clicks…).
    pub to_phone: broadcast::Sender<PhoneCommand>,
    /// Every frame from the phone as (serviceId, data), for desktop clients (`watch`), e.g. the
    /// mirror viewer relaying WebRTC signalling.
    pub from_phone: broadcast::Sender<PhoneCommand>,
    pub notifier: std::sync::OnceLock<Arc<Notifier>>,
    /// Per-service svc 25 `cap` flags set by helpers (e.g. cross control's in-use bit), merged over
    /// `proto::device_status_caps()` so the phone always gets the full tile map.
    pub cap_overrides: Mutex<serde_json::Map<String, Value>>,
}

impl Daemon {
    pub fn new(
        identity: Identity,
        pairing: PairingStore,
        ws_port: u16,
        ips: Vec<IpAddr>,
        token_ttl: Duration,
        qr: QrOutput,
        session_dir: PathBuf,
        ble_address: Option<String>,
    ) -> Self {
        // Last known /readyfor/mdns endpoint per phone, kept across restarts.
        let endpoints_path = session_dir.with_file_name("phone-endpoints.json");
        let endpoints: HashMap<String, SocketAddr> =
            std::fs::read(&endpoints_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let status = StatusHub::new(!pairing.is_empty(), Some(qr.svg_path.display().to_string()));
        let phone_ip = endpoints.values().next().map(SocketAddr::ip);
        Self {
            identity,
            pairing: Mutex::new(pairing),
            invite: Mutex::new(None),
            ws_port,
            ips: Mutex::new(ips),
            token_ttl,
            qr,
            session_dir,
            ble_address,
            clipboard: LocalClipboard::new(),
            active_sessions: AtomicUsize::new(0),
            phone_mdns: Mutex::new(endpoints),
            endpoints_path,
            user_disconnected: std::sync::atomic::AtomicBool::new(false),
            phone_said_bye: std::sync::atomic::AtomicBool::new(false),
            reconnecting: std::sync::atomic::AtomicBool::new(false),
            reconnect_nudge: tokio::sync::Notify::new(),
            connect_lock: tokio::sync::Mutex::new(()),
            ble_wake: std::sync::atomic::AtomicBool::new(true),
            phone_ip: Mutex::new(phone_ip),
            status,
            kick: tokio::sync::Notify::new(),
            to_phone: broadcast::channel(32).0,
            from_phone: broadcast::channel(256).0,
            notifier: std::sync::OnceLock::new(),
            cap_overrides: Mutex::default(),
        }
    }

    /// Svc 25 `cmd 1` carrying the full tile map (base caps + helper overrides).
    pub fn device_status(&self) -> Value {
        let overrides = self.cap_overrides.lock().unwrap().clone();
        proto::device_status_msg(&self.identity.device_name, pc_battery(), &overrides)
    }

    /// Keeps a valid invite on screen while no phone is connected. Windows uses a 60 s
    /// token; the TTL here is configurable because only the PC enforces it.
    pub async fn invite_loop(self: Arc<Self>) {
        loop {
            {
                let mut slot = self.invite.lock().unwrap();
                let idle = self.active_sessions.load(Ordering::SeqCst) == 0;
                let needs_new = match slot.as_ref() {
                    None => idle,
                    Some(inv) => Instant::now() >= inv.expires,
                };
                if needs_new {
                    let inv = Invite::new(self.ws_port, self.ips(), self.token_ttl);
                    if let Err(e) = self.show_qr(&inv) {
                        warn!("rendering QR failed: {e:#}");
                    }
                    *slot = Some(inv);
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    fn show_qr(&self, inv: &Invite) -> Result<()> {
        let payload = inv.qr_payload(&self.identity, self.ble_address.as_deref());
        invite::write_svg(&payload, &self.qr.svg_path)?;
        println!("{}", invite::render_terminal(&payload, self.qr.invert)?);
        info!(
            ttl_s = self.token_ttl.as_secs(),
            svg = %self.qr.svg_path.display(),
            bytes = payload.len(),
            "scan this QR in Smart Connect on the phone"
        );
        Ok(())
    }

    /// Whether any phone currently holds a control session.
    pub fn is_connected(&self) -> bool {
        self.active_sessions.load(Ordering::SeqCst) > 0
    }

    /// Whether the daemon may reconnect on its own: not connected, and the user didn't disconnect.
    pub fn should_dial_back(&self) -> bool {
        !self.is_connected() && !self.user_disconnected.load(Ordering::SeqCst)
    }

    /// Desktop-side Disconnect: close the session; stay disconnected until the user connects again.
    pub fn disconnect(&self) -> bool {
        if !self.is_connected() {
            return false;
        }
        self.user_disconnected.store(true, Ordering::SeqCst);
        self.kick.notify_waiters();
        true
    }

    /// Keeps the link up: after an unexpected drop (or at startup) retry Connect — Wi-Fi, then
    /// Bluetooth so it works with the phone app in the background — with backoff, until connected
    /// or the user disconnects. Only one loop runs at a time; see `reconnect::run`.
    pub fn spawn_auto_reconnect(self: &Arc<Self>, reason: &'static str) {
        if self.reconnecting.swap(true, Ordering::SeqCst) {
            return;
        }
        let daemon = self.clone();
        tokio::spawn(async move {
            crate::reconnect::run(daemon.clone(), reason).await;
            daemon.reconnecting.store(false, Ordering::SeqCst);
        });
    }

    /// Something changed that makes a reconnect worth trying now (network up, resume, Bluetooth on):
    /// cut the running loop's backoff short, or start the loop.
    pub fn nudge_reconnect(self: &Arc<Self>, why: &'static str) {
        if !self.should_dial_back() || self.pairing.lock().unwrap().is_empty() {
            return;
        }
        info!("reconnect: {why}; trying now");
        if self.reconnecting.load(Ordering::SeqCst) {
            self.reconnect_nudge.notify_one();
        } else {
            self.spawn_auto_reconnect(why);
        }
    }

    /// Resolves when `nudge_reconnect` asks the running loop to try now.
    pub(crate) async fn reconnect_nudged(&self) {
        self.reconnect_nudge.notified().await
    }

    /// LAN addresses for invites.
    pub fn ips(&self) -> Vec<IpAddr> {
        self.ips.lock().unwrap().clone()
    }

    /// Replaces the invite addresses (the PC moved networks); returns whether they changed. A
    /// pending QR invite with the old addresses is dropped so the next one carries the new ones.
    pub fn set_ips(&self, ips: Vec<IpAddr>) -> bool {
        let mut cur = self.ips.lock().unwrap();
        if *cur == ips {
            return false;
        }
        info!(old = ?*cur, new = ?ips, "LAN addresses changed");
        *cur = ips;
        drop(cur);
        if !self.is_connected() {
            *self.invite.lock().unwrap() = None;
        }
        true
    }

    pub fn set_ble_wake(&self, on: bool) {
        self.ble_wake.store(on, Ordering::SeqCst);
    }

    pub fn ble_wake(&self) -> bool {
        self.ble_wake.load(Ordering::SeqCst)
    }

    /// The paired phone's last known LAN address.
    pub fn phone_ip(&self) -> Option<IpAddr> {
        *self.phone_ip.lock().unwrap()
    }

    /// Records where the phone was last seen. A remembered `/readyfor/mdns` endpoint on another
    /// address (DHCP gave the phone a new lease) moves with it, keeping its port.
    ///
    /// A cable (USB tethering) address is not remembered: the phone's `/readyfor/mdns` server
    /// listens on Wi-Fi only (OBSERVED: refused on the cable address while the app was open).
    pub fn remember_phone_ip(&self, phone: &str, ip: IpAddr) {
        if crate::reconnect::is_on_usb_tether(ip) {
            return;
        }
        *self.phone_ip.lock().unwrap() = Some(ip);
        let moved = self.phone_mdns.lock().unwrap().get(phone).filter(|a| a.ip() != ip).map(|a| SocketAddr::new(ip, a.port()));
        if let Some(addr) = moved {
            self.remember_phone_mdns(phone, addr);
        }
    }

    /// Fresh invite + QR for pairing (another) phone; returns the SVG path.
    pub fn new_pairing_qr(&self) -> Result<String> {
        let inv = Invite::new(self.ws_port, self.ips(), self.token_ttl);
        self.show_qr(&inv)?;
        *self.invite.lock().unwrap() = Some(inv);
        let path = self.qr.svg_path.display().to_string();
        self.status.update(|s| s.pairing_qr = Some(path.clone()));
        Ok(path)
    }

    /// An explicit Connect (desktop button or the phone dialing in) ends a user Disconnect.
    pub fn clear_phone_close(&self) {
        self.user_disconnected.store(false, Ordering::SeqCst);
    }

    pub fn known_phone_endpoints(&self) -> Vec<(String, SocketAddr)> {
        self.phone_mdns.lock().unwrap().iter().map(|(p, a)| (p.clone(), *a)).collect()
    }

    pub fn remember_phone_mdns(&self, phone: &str, addr: SocketAddr) {
        *self.phone_ip.lock().unwrap() = Some(addr.ip());
        let mut map = self.phone_mdns.lock().unwrap();
        if map.insert(phone.to_owned(), addr) != Some(addr) {
            if let Ok(json) = serde_json::to_vec_pretty(&*map) {
                let _ = crate::identity::write_private(&self.endpoints_path, &json);
            }
        }
    }

    /// A token valid for at least a minute, for discovery invites (the phone sends it back in its hello).
    pub fn current_token(&self) -> String {
        let mut slot = self.invite.lock().unwrap();
        let fresh = matches!(slot.as_ref(), Some(inv) if inv.expires > Instant::now() + Duration::from_secs(60));
        if !fresh {
            *slot = Some(Invite::new(self.ws_port, self.ips(), self.token_ttl.max(Duration::from_secs(120))));
        }
        slot.as_ref().unwrap().token.clone()
    }

    /// Validates the invite token without consuming it — the token stays usable for its whole TTL,
    /// so the phone's pairing-then-session reconnects within the window all pass.
    fn token_valid(&self, token: &str) -> bool {
        matches!(self.invite.lock().unwrap().as_ref(), Some(inv) if inv.is_valid(token))
    }
}

pub fn tls_acceptor(id: &Identity) -> Result<TlsAcceptor> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])?
        .with_no_client_auth()
        .with_single_cert(vec![id.cert_der.clone()], id.key_der.clone_key())?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

pub async fn serve(daemon: Arc<Daemon>, listener: TcpListener, acceptor: TlsAcceptor) -> Result<()> {
    loop {
        let (tcp, peer) = listener.accept().await?;
        let daemon = daemon.clone();
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(daemon, acceptor, tcp, peer).await {
                warn!(%peer, "connection closed: {e:#}");
            }
        });
    }
}

async fn handle(daemon: Arc<Daemon>, acceptor: TlsAcceptor, tcp: TcpStream, peer: SocketAddr) -> Result<()> {
    info!(%peer, "TCP connection");
    // The PC address the phone reached us on: helpers offer WebRTC candidates on it only.
    let local_ip = tcp.local_addr().ok().map(|a| a.ip().to_string());
    let tls_local_ip = tcp.local_addr().ok().map(|a| a.ip());
    // Peek the first bytes (non-destructive, MSG_PEEK) so we can see what protocol the
    // client actually speaks: a TLS record starts 0x16 0x03; "GET " (0x47) is a plaintext
    // WebSocket/HTTP handshake. The phone is sending us non-TLS first bytes.
    {
        let mut buf = [0u8; 32];
        match timeout(Duration::from_secs(3), tcp.peek(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => info!(
                %peer, n,
                hex = %hex::encode(&buf[..n]),
                ascii = %String::from_utf8_lossy(&buf[..n]).escape_default().to_string(),
                "first bytes from client"
            ),
            Ok(Ok(_)) => info!(%peer, "client sent EOF before any data"),
            Ok(Err(e)) => warn!(%peer, "peek error: {e}"),
            Err(_) => info!(%peer, "client sent no data within 3s"),
        }
    }
    let tls = timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await.context("TLS handshake timed out")??;
    {
        let (_, conn) = tls.get_ref();
        info!(
            %peer,
            version = ?conn.protocol_version(),
            suite = ?conn.negotiated_cipher_suite().map(|s| s.suite()),
            sni = ?conn.server_name(),
            "TLS established"
        );
    }

    // Our fex offers (clipboard images to the phone) share this port: route their two paths there,
    // and replay the request head into the control WebSocket for everything else.
    let mut tls = tls;
    let head = timeout(FIRST_MESSAGE_TIMEOUT, crate::fex_host::read_head(&mut tls)).await.context("no HTTP request")??;
    if let Some(target) = crate::fex_host::target(&head).filter(|t| crate::fex_host::is_fex(t)).map(str::to_owned) {
        info!(%peer, "fex request");
        return crate::fex_host::serve(daemon.clipboard.fex(), crate::fex_host::Prefixed::new(head, tls), &target, peer).await;
    }
    let tls = crate::fex_host::Prefixed::new(head, tls);

    let mut uri = String::new();
    let ws = tokio_tungstenite::accept_hdr_async(tls, |req: &Request, resp: Response| {
        uri = req.uri().to_string();
        // Go's ServeMux pattern "/readyfor/" matches the whole subtree.
        if req.uri().path().starts_with(CONTROL_PATH) {
            Ok(resp)
        } else {
            let mut err = ErrorResponse::new(Some("not found".into()));
            *err.status_mut() = StatusCode::NOT_FOUND;
            Err(err)
        }
    })
    .await
    .context("WebSocket upgrade")?;
    info!(%peer, %uri, "WebSocket upgraded");
    if uri.split('?').next() == Some(discovery::MDNS_PATH) {
        return discovery::serve(daemon, ws, peer).await;
    }
    if uri.split('?').next() == Some(discovery::CABLE_PATH) {
        let local = tls_local_ip.unwrap_or_else(|| daemon.ips().into_iter().next().unwrap_or(IpAddr::from([0, 0, 0, 0])));
        return serve_cable(&daemon, ws, peer, local).await;
    }

    let mut log = SessionLog::create(&daemon.session_dir, peer)?;
    let (mut tx, mut rx) = ws.split();

    let first = timeout(FIRST_MESSAGE_TIMEOUT, rx.next()).await.context("no first message within 40 s")?;
    let text = match first {
        Some(Ok(Message::Text(t))) => t.to_string(),
        other => bail!("expected a text frame first, got {other:?}"),
    };
    log.record("in", &text);
    let hello_msg: WsMsg = serde_json::from_str(&text).context("first frame is not a WsMsg")?;
    if hello_msg.service_id != SERVICE_MANAGER {
        // Unknown first frames (e.g. the phone's cable mode on /readyfor/rdp): log them to learn the protocol.
        info!(%peer, %uri, frame = %text.chars().take(4000).collect::<String>(), "unexpected first frame");
        bail!("first frame is for serviceId {}, expected 0", hello_msg.service_id);
    }
    let data = hello_msg.data.unwrap_or(Value::Null);
    let hello = Hello::parse(&data);
    info!(
        %peer,
        phone = %hello_msg.device_id,
        name = ?hello.device_name,
        cmd = ?hello.cmd,
        paired = ?hello.paired,
        "handshake received"
    );

    // Stable identity: the phone's realDeviceId (the envelope deviceId is a per-connection nonce).
    let stable_id = hello.real_device_id.unwrap_or(hello_msg.device_id.as_str()).to_owned();
    let already_paired = daemon.pairing.lock().unwrap().get(&stable_id).is_some();
    let token_ok = daemon.token_valid(hello.token.unwrap_or_default());
    // Accept a fresh pair (valid invite token) OR a reconnect from a device we've already paired.
    if !token_ok && !already_paired {
        warn!(%peer, phone = %stable_id, "no valid invite token and not a known device; dropping");
        return Ok(());
    }

    let record = daemon
        .pairing
        .lock()
        .unwrap()
        .pair(&stable_id, hello.device_name, hello.real_device_id)?;
    info!(%peer, phone = %stable_id, reconnect = already_paired, token_ok, "accepted");
    let id = &daemon.identity;
    let sku = identity::product_name();
    // Shaped like the genuine Windows reply (logged by the phone in a real pair). The `device` must
    // carry the full version info: on connect the phone re-runs getVersionMatchType against it, and
    // a missing (null) verName makes it crash with an NPE. `serviceVersions`/`caps` decide which
    // services the phone brings up for this PC.
    let reply = json!({
        "ver": 3,
        "cmd": 0,
        "token": "OK",
        "paired": 1,
        "connType": 1,
        "device": {
            "deviceId": id.device_id,
            "realDeviceId": id.real_device_id,
            "deviceName": id.device_name,
            "os": 6,
            "type": 4,
            "scType": 2,
            "verCode": invite::VER_CODE,
            "verName": invite::VER_NAME,
            "appVer": invite::VER_NAME,
            "minVerCode": { "1": 7000000, "2": 700000000, "3": 7000000, "4": 7000000 },
            "sn": id.serial_number(),
            "Sku": sku,
            "sku": sku,
            "cap": 0,
            "caps": proto::service_caps(),
            "serviceVersions": proto::service_versions(),
            "key": record.key,
            "isPaired": true,
        },
        "serviceVersions": proto::service_versions(),
    });
    let out = serde_json::to_string(&WsMsg::outgoing(&id.device_id, SERVICE_MANAGER, reply))?;
    tx.send(Message::text(out.clone())).await?;
    log.record("out", &out);
    info!(%peer, phone = %hello_msg.device_id, "phone verified; session log: {}", log.describe());
    // The phone only draws a feature tile when the service is also in this svc 25 `cap` map.
    let status = daemon.device_status();
    let out = serde_json::to_string(&WsMsg::outgoing(&id.device_id, proto::SERVICE_DEVICE_STATUS, status))?;
    tx.send(Message::text(out.clone())).await?;
    log.record("out", &out);

    daemon.active_sessions.fetch_add(1, Ordering::SeqCst);
    let name = hello.device_name.map(str::to_owned);
    daemon.status.update(|s| {
        s.connected = true;
        s.paired = true;
        s.name = name.or(s.name.take());
        s.pc_address = local_ip.or(s.pc_address.take());
        s.phone_address = Some(peer.ip().to_canonical().to_string());
    });
    daemon.remember_phone_ip(&stable_id, peer.ip().to_canonical());
    // A verified session (phone- or desktop-initiated) means the user wants to be connected.
    daemon.clear_phone_close();
    daemon.phone_said_bye.store(false, Ordering::SeqCst);
    let result = session_loop(&mut tx, &mut rx, &mut log, peer, &daemon, &stable_id).await;
    if daemon.active_sessions.fetch_sub(1, Ordering::SeqCst) == 1 {
        daemon.status.update(|s| s.connected = false);
        if daemon.phone_said_bye.load(Ordering::SeqCst) {
            // svc 0 cmd 6: the user tapped Disconnect on the phone.
            daemon.user_disconnected.store(true, Ordering::SeqCst);
            info!(%peer, "phone disconnected on the user's request; not reconnecting");
        } else if daemon.should_dial_back() {
            daemon.spawn_auto_reconnect("session dropped");
        }
    }
    result
}

async fn session_loop<S, R>(
    tx: &mut S,
    rx: &mut R,
    log: &mut SessionLog,
    peer: SocketAddr,
    daemon: &Daemon,
    phone_id: &str,
) -> Result<()>
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
    R: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let device_id = daemon.identity.device_id.as_str();
    let clipboard = &*daemon.clipboard;
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await;
    let mut clip = clipboard::Session::default();
    let mut local_clips = clipboard.subscribe();
    let mut to_phone = daemon.to_phone.subscribe();
    // If the phone doesn't start the key exchange itself (it still believes it's paired, e.g. after
    // the user removed our tile), send EXCHANGE_KEY unprompted: the phone's handler is
    // insert-or-update, so this re-creates the tile and is harmless when it already exists.
    let key_exchange_deadline = tokio::time::sleep(Duration::from_secs(2));
    tokio::pin!(key_exchange_deadline);
    let mut key_exchanged = false;
    let mut last_heard = Instant::now();
    let mut pong_seen = false;
    loop {
        tokio::select! {
            msg = rx.next() => match heard(&mut last_heard, msg) {
                Some(Ok(Message::Text(t))) => {
                    log.record("in", &t);
                    info!(%peer, "{}", summarize(&t));
                    if let Ok(msg) = serde_json::from_str::<WsMsg>(&t) {
                        if msg.service_id == SERVICE_MANAGER && msg.data.as_ref().and_then(|d| d.get("cmd")).and_then(Value::as_i64) == Some(6) {
                            daemon.phone_said_bye.store(true, Ordering::SeqCst);
                        }
                        if let Some(d) = msg.data.as_ref() {
                            let _ = daemon.from_phone.send((msg.service_id, d.clone()));
                            daemon.status.apply(msg.service_id, d);
                            if msg.service_id == crate::app_icons::SERVICE_APP_LIST {
                                crate::app_icons::on_app_list(peer.ip().to_canonical(), d);
                            }
                            if msg.service_id == clipboard::SERVICE_FILE_EX {
                                clipboard::on_file_ex(peer.ip().to_canonical(), d, daemon.clipboard.clone());
                            }
                            if matches!(msg.service_id, notifications::SERVICE_NOTIFY | notifications::SERVICE_NOTIFY_WIDGET) {
                                if let Some(n) = daemon.notifier.get() {
                                    n.handle(msg.service_id, d).await;
                                }
                                continue;
                            }
                        }
                        let resp = match (msg.service_id, msg.data.as_ref()) {
                            (SERVICE_MANAGER, Some(d)) if d.get("cmd").and_then(Value::as_i64) == Some(1) => {
                                key_exchanged = true;
                                exchange_key(daemon, phone_id, d, true).map(|d| WsMsg::outgoing(device_id, SERVICE_MANAGER, d))
                            }
                            (clipboard::SERVICE_CLIPBOARD, Some(d)) => clip
                                .handle(d, clipboard)
                                .map(|(svc, d)| WsMsg::outgoing(device_id, svc, d)),
                            _ => respond(&msg, device_id),
                        };
                        if let Some(resp) = resp {
                            send(tx, log, peer, &resp).await?;
                        }
                    }
                }
                Some(Ok(Message::Binary(b))) => {
                    log.record("in-binary-len", &b.len().to_string());
                    info!(%peer, len = b.len(), "binary frame");
                }
                Some(Ok(Message::Close(frame))) => {
                    info!(%peer, ?frame, "phone closed the WebSocket");
                    return Ok(());
                }
                Some(Ok(Message::Pong(_))) if !pong_seen => {
                    pong_seen = true;
                    info!(%peer, "phone answers WebSocket pings; dead-link detection active");
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
                None => return Ok(()),
            },
            local = local_clips.recv() => match local {
                Ok(c) => {
                    if let Some(d) = clip.outgoing(&c) {
                        send(tx, log, peer, &WsMsg::outgoing(device_id, clipboard::SERVICE_CLIPBOARD, d)).await?;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => warn!(%peer, n, "clipboard: skipped local copies"),
                Err(broadcast::error::RecvError::Closed) => unreachable!("the daemon's LocalClipboard outlives sessions"),
            },
            _ = &mut key_exchange_deadline, if !key_exchanged => {
                key_exchanged = true;
                info!(%peer, "phone didn't start the key exchange; sending ours unprompted");
                if let Some(d) = exchange_key(daemon, phone_id, &Value::Null, false) {
                    send(tx, log, peer, &WsMsg::outgoing(device_id, SERVICE_MANAGER, d)).await?;
                }
            }
            cmd = to_phone.recv() => {
                if let Ok((service_id, data)) = cmd {
                    send(tx, log, peer, &WsMsg::outgoing(device_id, service_id, data)).await?;
                }
            }
            _ = daemon.kick.notified() => {
                info!(%peer, "desktop asked to disconnect");
                let _ = tx.send(Message::Close(None)).await;
                return Ok(());
            }
            _ = ping.tick() => {
                let silent = last_heard.elapsed();
                if silent > PEER_SILENCE_LIMIT {
                    bail!("nothing from the phone for {} s (left Wi-Fi, or this PC slept); dropping the dead link", silent.as_secs());
                }
                tx.send(Message::Ping(Vec::new().into())).await?
            }
        }
    }
}

/// Notes when the phone last sent anything (pongs included), for dead-link detection.
fn heard<T>(at: &mut Instant, msg: T) -> T {
    *at = Instant::now();
    msg
}

async fn send<S>(tx: &mut S, log: &mut SessionLog, peer: SocketAddr, msg: &WsMsg) -> Result<()>
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let out = serde_json::to_string(msg)?;
    tx.send(Message::text(out.clone())).await?;
    log.record("out", &out);
    let data = msg.data.as_ref();
    let cmd = data.and_then(|d| d.get("cmd").or_else(|| d.get("msgId"))).map(ToString::to_string).unwrap_or_default();
    info!(%peer, "-> svc {} cmd {}", msg.service_id, cmd);
    Ok(())
}

/// The PC's half of the key exchange. Right after the verify reply the phone sends svc 0
/// `cmd:1` with its own key; it commits the pair — `paired_device` row, PC shown in its device
/// list, EXCHANGE_KEY → handleDevicePaired in logcat — only when it receives the PC's `cmd:1`
/// with a key. Without it PairServiceHelper gives up ~7 s later (`handleFailure code:1`).
/// Spec: `evidence-2026-10-20/pairing-commit-spec.md`.
fn exchange_key(daemon: &Daemon, phone_id: &str, data: &Value, first_pair: bool) -> Option<Value> {
    let mut pairing = daemon.pairing.lock().unwrap();
    let key = pairing.get(phone_id)?.key.clone();
    if let Some(phone_key) = data.pointer("/device/key").and_then(Value::as_str) {
        if let Err(e) = pairing.save_phone_key(phone_id, phone_key) {
            warn!("saving the phone's key: {e:#}");
        }
    }
    drop(pairing);
    info!(phone = %phone_id, "key exchange: sending our key");
    if first_pair {
        schedule_post_pair_dial(daemon, phone_id);
    }
    let id = &daemon.identity;
    // `device.deviceId` is the id the phone files the pair under (`paired_device.deviceId`); the
    // genuine PC sends its realDeviceId here (handleDevicePaired logs it), unlike the verify reply.
    Some(json!({
        "ver": 3,
        "cmd": 1,
        "paired": 1,
        "device": {
            "deviceId": id.real_device_id,
            "realDeviceId": id.real_device_id,
            "deviceName": id.device_name,
            "key": key,
            "sn": id.serial_number(),
            "cap": 0,
            "os": 6,
            "type": 4,
            "i3": 1,
            "verCode": invite::VER_CODE,
            "verName": invite::VER_NAME,
        },
    }))
}

/// The phone's cable mode (OBSERVED on a device; the genuine PC's replies were not captured, these
/// are the ones the phone accepts): it finds `_readyfor._tcp` on the USB tethering link, dials
/// `/readyfor/rdp` and opens with `{action: 1, serviceId: 1, status: 1, ver: 3, device}`. We answer
/// in the same shape with our device record. It then sends a service 19 `cmd 0` trigger; we answer
/// with an invite whose only address is the one the phone reached us on, and the phone dials
/// `/readyfor/` over the cable. It keeps this socket open for the session, repeating its `action`
/// frame every 20 s.
async fn serve_cable<S>(daemon: &Daemon, ws: tokio_tungstenite::WebSocketStream<S>, peer: SocketAddr, local: IpAddr) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (mut tx, mut rx) = ws.split();
    let mut replied = false;
    while let Some(msg) = timeout(Duration::from_secs(60), rx.next()).await.context("cable: phone silent for 60 s")? {
        let msg = msg?;
        let text = match &msg {
            Message::Text(t) => t.to_string(),
            Message::Binary(b) => format!("<binary {} B: {}>", b.len(), b.iter().take(64).map(|x| format!("{x:02x}")).collect::<String>()),
            Message::Close(c) => {
                info!(%peer, close = ?c, "cable: phone closed");
                break;
            }
            _ => continue,
        };
        let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        // After the first, the phone repeats its `action` frame every 20 s as a heartbeat; it
        // keeps the session without a reply to each (OBSERVED for several minutes).
        if replied && parsed.get("action").is_some() {
            debug!(%peer, "cable: heartbeat");
            continue;
        }
        info!(%peer, frame = %text.chars().take(4000).collect::<String>(), "cable: <- phone");
        // Service 19 cmd 0 is the phone's trigger ("send me an invite"): answer with the invite,
        // echoing reqId, as on /readyfor/mdns but in plain JSON.
        if parsed.get("serviceId").and_then(Value::as_i64) == Some(19) && parsed.get("cmd").and_then(Value::as_i64) == Some(0) {
            // Offer the address the phone reached us on (the cable): the LAN list may lag a fresh link.
            let mut inv = discovery::invite_json(&daemon.identity, &daemon.current_token(), &[local], daemon.ws_port);
            inv["policies"] = json!([{ "wifi": [local.to_string()] }, { "usb": [local.to_string()] }]);
            inv["serviceId"] = json!(19);
            inv["reqId"] = parsed.get("reqId").cloned().unwrap_or(json!(1));
            inv["errorCode"] = json!(0);
            let reply = inv.to_string();
            info!(%peer, frame = %reply.replace(&daemon.current_token(), "<token>"), "cable: -> phone (invite)");
            tx.send(Message::text(reply)).await?;
            continue;
        }
        if !replied {
            let reply = json!({
                "action": parsed.get("action").cloned().unwrap_or(json!(1)),
                "deviceId": daemon.identity.device_id,
                "serviceId": parsed.get("serviceId").cloned().unwrap_or(json!(1)),
                "status": 1,
                "ver": 3,
                "device": invite::device_json(&daemon.identity, daemon.ble_address.as_deref()),
            })
            .to_string();
            info!(%peer, frame = %reply, "cable: -> phone");
            tx.send(Message::text(reply)).await?;
            replied = true;
        }
    }
    Ok(())
}

/// The genuine PC makes the phone reconnect ~5 s after a first pair: the phone shows a brand-new
/// tile offline until its next connection. Dial the phone's `/readyfor/mdns` if we know it.
fn schedule_post_pair_dial(daemon: &Daemon, phone_id: &str) {
    let Some(addr) = daemon.phone_mdns.lock().unwrap().get(phone_id).copied() else {
        info!(phone = %phone_id, "no /readyfor/mdns endpoint known for the phone; tile refresh waits for the next connection");
        return;
    };
    let (id, key, token) = {
        let key = daemon.pairing.lock().unwrap().get(phone_id).map(|p| p.key.clone());
        (daemon.identity.device_id.clone(), key, daemon.current_token())
    };
    let Some(key) = key else { return };
    let invite = discovery::invite_json(&daemon.identity, &token, &daemon.ips(), daemon.ws_port).to_string();
    let phone = phone_id.to_owned();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        if let Err(e) = discovery::push_invite(addr, &id, &key, &invite).await {
            warn!(%addr, %phone, "post-pair dial-back failed: {e:#}");
        }
    });
}

/// Minimal service responses to keep the session alive after the handshake. The phone sends a few
/// request-type messages right after pairing and closes if the PC stays silent. We don't run the
/// services yet, so we just acknowledge: reply on the same service id, echoing the request's msgId
/// and marking `reply:1`. This is empirical — enough to hold the connection, not a real service.
fn respond(msg: &WsMsg, device_id: &str) -> Option<WsMsg> {
    let data = msg.data.as_ref()?;
    let cmd = data.get("cmd").and_then(Value::as_i64);
    let reply = |mut d: Value| -> Option<WsMsg> {
        if let Value::Object(ref mut m) = d {
            m.insert("reply".into(), json!(1));
        }
        Some(WsMsg {
            device_id: device_id.to_owned(),
            src_service_id: msg.service_id,
            service_id: msg.service_id,
            msg_id: msg.msg_id, // echo for request/response correlation
            data: Some(d),
            ver: None,
            sender_id: None,
        })
    };
    match (msg.service_id, cmd) {
        (1, Some(31)) => reply(data.clone()),  // RDP/mirror state query
        (16, Some(14)) => reply(data.clone()), // input-mode query
        _ => None,
    }
}

/// One console line per inbound frame: service id plus whichever command field the
/// payload uses (`cmd` for most services, `msgId` inside clipboard data).
fn summarize(text: &str) -> String {
    let Ok(msg) = serde_json::from_str::<WsMsg>(text) else {
        return format!("unparsed frame ({} bytes)", text.len());
    };
    let data = msg.data.as_ref();
    let field = |k: &str| data.and_then(|d| d.get(k)).map(|v| v.to_string()).unwrap_or_else(|| "-".into());
    format!(
        "<- svc {} (src {}) cmd {} data.msgId {} ({} bytes)",
        msg.service_id,
        msg.src_service_id,
        field("cmd"),
        field("msgId"),
        text.len()
    )
}

static SESSION_LOGS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Turns on per-session frame logs (`--session-log`). Off by default: they are a protocol
/// debugging aid, not something every user should accumulate.
pub fn enable_session_logs() {
    SESSION_LOGS.store(true, Ordering::Relaxed);
}

/// JSONL record of every frame in one session (redacted, see `redact`), for working out command
/// numbers. Only written with `--session-log`; lives in the private (0700) state dir.
struct SessionLog {
    path: Option<PathBuf>,
    file: Option<File>,
}

impl SessionLog {
    fn create(dir: &Path, peer: SocketAddr) -> Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        if !SESSION_LOGS.load(Ordering::Relaxed) {
            return Ok(Self { path: None, file: None });
        }
        crate::identity::create_private_dir(dir)?;
        let path = dir.join(format!("{}-{}.jsonl", unix_ms(), peer.ip()));
        let file = OpenOptions::new().create(true).append(true).mode(0o600).open(&path)?;
        Ok(Self { path: Some(path), file: Some(file) })
    }

    fn describe(&self) -> String {
        self.path.as_ref().map_or_else(|| "off (enable with --session-log)".to_owned(), |p| p.display().to_string())
    }

    fn record(&mut self, dir: &str, text: &str) {
        let Some(file) = self.file.as_mut() else { return };
        let mut body = serde_json::from_str::<Value>(text).unwrap_or_else(|_| Value::String(text.to_owned()));
        redact(&mut body);
        let line = json!({ "ts_ms": unix_ms(), "dir": dir, "frame": body });
        if let Err(e) = writeln!(file, "{line}") {
            warn!("writing session log: {e}");
        }
    }
}

/// Field names whose string values never reach the session log (pairing keys in the verify
/// reply and EXCHANGE_KEY, QR tokens, the hotspot passphrase, …).
const SECRET_FIELDS: [&str; 7] = ["key", "token", "pwd", "password", "passwd", "secret", "psk"];

/// Keeps secrets and personal content out of the on-disk session log: secret fields anywhere,
/// notification and SMS contents, clipboard text, and the wallpaper.
fn redact(frame: &mut Value) {
    let svc = frame.get("serviceId").and_then(Value::as_i64);
    let src = frame.get("srcServiceId").and_then(Value::as_i64);
    let Some(data) = frame.get_mut("data").and_then(Value::as_object_mut) else { return };
    let sms = |s: Option<i64>| matches!(s, Some(100 | 204 | 205 | 206));
    match svc {
        Some(notifications::SERVICE_NOTIFY | notifications::SERVICE_NOTIFY_WIDGET) => {
            data.remove("data");
            data.remove("actions");
            data.insert("redacted".into(), json!(true));
        }
        _ if sms(svc) || sms(src) => {
            data.retain(|k, _| k == "cmd" || k == "ver" || k == "pId");
            data.insert("redacted".into(), json!(true));
        }
        Some(crate::clipboard::SERVICE_CLIPBOARD) => {
            redact_where(data, &|k| k == "text" || k == "name" || k == "path");
        }
        Some(25) => {
            if let Some(w) = data.get_mut("wallpaper") {
                *w = json!(format!("<{} bytes>", w.as_str().map_or(0, str::len)));
            }
        }
        _ => {}
    }
    redact_where(data, &|k| SECRET_FIELDS.contains(&k.to_ascii_lowercase().as_str()));
}

/// Replaces every string value (at any depth) whose field name matches with a length marker.
fn redact_where(map: &mut serde_json::Map<String, Value>, matches: &dyn Fn(&str) -> bool) {
    for (k, v) in map.iter_mut() {
        match v {
            Value::String(s) if matches(k) => *v = json!(format!("<redacted {} chars>", s.chars().count())),
            Value::Object(inner) => redact_where(inner, matches),
            Value::Array(items) => {
                for item in items.iter_mut() {
                    if let Value::Object(inner) = item {
                        redact_where(inner, matches);
                    }
                }
            }
            _ => {}
        }
    }
}

fn unix_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

/// This PC's battery percentage for the phone's device page (100 on machines without a battery).
fn pc_battery() -> i64 {
    std::fs::read_dir("/sys/class/power_supply")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("BAT"))
        .find_map(|e| std::fs::read_to_string(e.path().join("capacity")).ok()?.trim().parse().ok())
        .unwrap_or(100)
}

#[cfg(test)]
mod redact_tests {
    use super::*;

    #[test]
    fn session_log_drops_keys_tokens_passwords_and_personal_text() {
        let mut verify = json!({ "serviceId": 0, "data": { "token": "OK", "device": { "key": "AAAA", "deviceName": "PC" } } });
        redact(&mut verify);
        assert_eq!(verify["data"]["device"]["key"], "<redacted 4 chars>");
        assert_eq!(verify["data"]["token"], "<redacted 2 chars>");
        assert_eq!(verify["data"]["device"]["deviceName"], "PC");

        let mut hotspot = json!({ "serviceId": 13, "data": { "cmd": 1, "ssid": "x", "pwd": "secret123" } });
        redact(&mut hotspot);
        assert_eq!(hotspot["data"]["pwd"], "<redacted 9 chars>");

        let mut clip = json!({ "serviceId": 9, "data": { "cmd": 3, "data": { "clipId": 1, "text": "hello" } } });
        redact(&mut clip);
        assert_eq!(clip["data"]["data"]["text"], "<redacted 5 chars>");

        let mut sms = json!({ "serviceId": 100, "srcServiceId": 204, "data": { "cmd": 29, "data": [{ "body": "otp" }] } });
        redact(&mut sms);
        assert_eq!(sms["data"], json!({ "cmd": 29, "redacted": true }));
    }
}
