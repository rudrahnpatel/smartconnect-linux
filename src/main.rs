//! smartconnectd: a Linux peer for Motorola Smart Connect that the unmodified phone
//! app treats as a Windows PC. Rung 4: QR pairing + the control WebSocket handshake.

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::Parser;
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

use smartconnect::identity::Identity;
use smartconnect::pairing::PairingStore;
use smartconnect::server::{self, Daemon, QrOutput};

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// TCP port for the control WebSocket (must be open in the firewall).
    #[arg(long, default_value_t = 52020)]
    port: u16,
    /// LAN address(es) to put in the QR's `wifi` policy; auto-detected when omitted.
    #[arg(long)]
    ip: Vec<IpAddr>,
    /// Name shown on the phone; defaults to the hostname.
    #[arg(long)]
    name: Option<String>,
    /// Where identity, cert, pairings and session logs live.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Seconds an invite token stays valid (Windows uses 60).
    #[arg(long, default_value_t = 180)]
    token_ttl: u64,
    /// Draw the terminal QR for a light-background terminal.
    #[arg(long)]
    light_terminal: bool,
    /// Disable the legacy BLE pairing peripheral (advertiser + GATT server). Pairing doesn't need
    /// it; this does NOT affect the Bluetooth wake used to reconnect (see --no-ble-wake).
    #[arg(long)]
    no_ble: bool,
    /// Never use Bluetooth LE to wake the phone for a reconnect. Without it, a phone whose Smart
    /// Connect app is in the background only reconnects when the app is opened. The wake never
    /// changes the adapter's power state: it's skipped while Bluetooth is off.
    #[arg(long)]
    no_ble_wake: bool,
    /// Don't create or use the NetworkManager profile `smartconnect-phone-usb`, which keeps this
    /// PC's address on the phone's USB cable fixed across plug-ins.
    #[arg(long)]
    no_cable_profile: bool,
    /// Experimental: extra phone feature tiles to advertise in the svc 25 `cap` map, as service
    /// ids (`SVC` or `SVC=FLAGS`), e.g. `--extra-tiles 20` for the File explorer tile.
    /// Untested on a device; the service must also be in `serviceVersions`.
    #[arg(long, value_delimiter = ',', value_name = "SVC[=FLAGS]")]
    extra_tiles: Vec<String>,
    /// Record every control-channel frame under `<state dir>/sessions/` for protocol debugging
    /// (keys, tokens, passwords, clipboard, SMS and notification contents are redacted). Off by default.
    #[arg(long)]
    session_log: bool,
    /// Command (run with `sh -c`) that locks the screen when the phone's PC lock tile is tapped.
    /// Default: Noctalia's lock screen when Noctalia is running, else `loginctl lock-session` on
    /// the graphical session (GNOME under GDM, KDE, and lockers started by swayidle/hypridle).
    #[arg(long, value_name = "CMD")]
    lock_command: Option<String>,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Ask the running daemon to connect to the paired phone now (desktop-side "Connect").
    Connect,
    /// Close the current session.
    Disconnect,
    /// Print the phone status as JSON.
    Status,
    /// Print status changes as JSON lines until interrupted.
    Subscribe,
    /// Show a fresh pairing QR (for a new phone).
    Pair,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    let args = Args::parse();
    if let Some(cmd) = &args.command {
        let cmd = match cmd {
            Cmd::Connect => "connect",
            Cmd::Disconnect => "disconnect",
            Cmd::Status => "status",
            Cmd::Subscribe => "subscribe",
            Cmd::Pair => "pair",
        };
        return smartconnect::control::request(cmd).await;
    }

    if args.session_log {
        smartconnect::server::enable_session_logs();
    }
    let extra_tiles = smartconnect::proto::parse_extra_tiles(&args.extra_tiles).map_err(anyhow::Error::msg)?;
    if !extra_tiles.is_empty() {
        let versions = smartconnect::proto::service_versions();
        for (svc, _) in &extra_tiles {
            if versions.get(svc).is_none() {
                tracing::warn!(%svc, "--extra-tiles: service {svc} is not in serviceVersions, so its tile won't show");
            }
        }
        info!(tiles = ?extra_tiles, "advertising extra feature tiles (experimental)");
        smartconnect::proto::set_extra_tiles(extra_tiles);
    }

    let state_dir = args.state_dir.unwrap_or_else(default_state_dir);
    let identity = Identity::load_or_create(&state_dir, args.name)?;
    info!(
        name = %identity.device_name,
        device_id = %identity.device_id,
        fp = %identity.fp_hex(),
        state = %state_dir.display(),
        "identity ready"
    );

    let refresh_ips = args.ip.is_empty();
    let ips = if refresh_ips { smartconnect::reconnect::detect_lan_ips()? } else { args.ip };
    if ips.is_empty() {
        bail!("no LAN IPv4 address found; pass --ip");
    }

    let acceptor = server::tls_acceptor(&identity)?;

    // BLE pairing peripheral (required for first-time pairing). Kept alive for the program's life.
    let _ble = if args.no_ble {
        None
    } else {
        match smartconnect::ble::start(&identity).await {
            Ok(h) => Some(h),
            Err(e) => {
                tracing::warn!("BLE peripheral disabled: {e:#} (first-time pairing needs it; Wi-Fi reconnect may still work)");
                None
            }
        }
    };

    let ble_address = _ble.as_ref().map(|h| h.address.clone());

    let listener = TcpListener::bind(("0.0.0.0", args.port)).await?;
    info!(port = args.port, ips = ?ips, ble = ?ble_address, "control WebSocket listening on wss://<ip>:{}/readyfor/", args.port);

    let daemon = Arc::new(Daemon::new(
        identity,
        PairingStore::load(&state_dir)?,
        args.port,
        ips,
        Duration::from_secs(args.token_ttl),
        QrOutput { invert: !args.light_terminal, svg_path: state_dir.join("pairing-qr.svg") },
        state_dir.join("sessions"),
        ble_address,
    ));
    daemon.set_ble_wake(!args.no_ble_wake);
    // Clipboard images to the phone are served over fex on this same port and certificate.
    daemon.clipboard.set_fex_endpoint(args.port, daemon.identity.fp_hex());
    tokio::spawn(daemon.clone().invite_loop());
    // Reconnect promptly after a resume, a network change, or Bluetooth being switched on.
    tokio::spawn(smartconnect::reconnect::watch_system(daemon.clone(), refresh_ips));
    if !args.no_cable_profile {
        tokio::spawn(smartconnect::cable_link::run());
    }
    if !args.no_ble_wake {
        let daemon = daemon.clone();
        tokio::spawn(async move {
            if let Err(e) = smartconnect::ble_connect::watch_adapter(daemon).await {
                tracing::debug!("ble: not watching the adapter: {e:#}");
            }
        });
    }
    tokio::spawn(smartconnect::pc_lock::run(daemon.clone(), args.lock_command.clone()));
    // Persistence: if the paired phone isn't back shortly after startup, reconnect on our own
    // (Bluetooth works even when the phone app is in the background).
    {
        let daemon = daemon.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(8)).await;
            if daemon.should_dial_back() {
                daemon.spawn_auto_reconnect("startup");
            }
        });
    }

    match smartconnect::notifications::Notifier::new(daemon.to_phone.clone(), daemon.from_phone.clone()).await {
        Ok(n) => {
            let _ = daemon.notifier.set(n);
        }
        Err(e) => tracing::warn!("notifications: no desktop notification server, phone notifications won't show: {e:#}"),
    }

    // Wi-Fi reconnect: advertise ourselves (phone's Connect button) and watch for the phone's own
    // advert to dial it back (daemon restart). Needs inbound mDNS (udp 5353) through the firewall.
    let _advert = match smartconnect::discovery::advertise(&daemon.identity, args.port) {
        Ok(child) => Some(child),
        Err(e) => {
            tracing::warn!("mdns: not advertising, the phone's Connect button won't reach us: {e:#}");
            None
        }
    };
    // Cable: the phone's cable mode looks for `_readyfor._tcp` on the USB tethering link.
    tokio::spawn(smartconnect::discovery::keep_cable_advert(daemon.clone(), args.port));
    {
        let daemon = daemon.clone();
        tokio::spawn(async move {
            if let Err(e) = smartconnect::control::serve(daemon).await {
                tracing::warn!("control socket stopped: {e:#}");
            }
        });
    }
    {
        let daemon = daemon.clone();
        tokio::spawn(async move {
            if let Err(e) = smartconnect::discovery::watch_phones(daemon).await {
                tracing::warn!("mdns: phone watcher stopped: {e:#}");
            }
        });
    }

    tokio::spawn(smartconnect::hotspot::run(daemon.clone()));

    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        let clipboard = daemon.clipboard.clone();
        tokio::spawn(async move {
            if let Err(e) = clipboard.watch().await {
                tracing::warn!("clipboard: Linux → phone sync stopped: {e:#}");
            }
        });
    } else {
        tracing::warn!("clipboard: no Wayland session, Linux → phone sync is off (phone → Linux needs wl-copy too)");
    }

    // Stream host + cross control client, so phone-initiated requests always get an answer.
    let helpers = Arc::new(smartconnect::helpers::Helpers::default());
    helpers.start(daemon.clone());

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let r = tokio::select! {
        r = server::serve(daemon, listener, acceptor) => r,
        _ = tokio::signal::ctrl_c() => Ok(()),
        _ = sigterm.recv() => Ok(()),
    };
    info!("shutting down");
    helpers.shutdown().await;
    r
}

fn default_state_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("smartconnect-linux")
}
