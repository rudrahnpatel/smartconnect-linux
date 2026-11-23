# Security

What smartconnect-linux exposes, how the link to the phone is protected, what it stores and logs,
and the known gaps. If you find a vulnerability, see [Reporting](#reporting-a-vulnerability).

- [Threat model](#threat-model)
- [Network exposure](#network-exposure)
- [Pairing, keys and encryption](#pairing-keys-and-encryption)
- [What a paired phone can do](#what-a-paired-phone-can-do)
- [Local exposure](#local-exposure)
- [Data on disk](#data-on-disk)
- [Logging and redaction](#logging-and-redaction)
- [Known gaps](#known-gaps)
- [Hardening checklist](#hardening-checklist)

## Threat model

smartconnect-linux is built for a single-user desktop on a home or office LAN, with a phone the same
person owns and has paired by scanning a QR code on their own screen.

- **Trusted:** your user account and its processes, and the paired phone.
- **Partly trusted:** other devices on the LAN. They can reach the daemon's port and see its mDNS
  advert, but shouldn't be able to pair, read your data or impersonate the phone. See
  [Known gaps](#known-gaps) for where this is weaker than it should be.
- **Untrusted:** everything beyond the LAN. Nothing should be reachable from there; the daemon is
  meant to be firewalled to your local network.

The protocol is Motorola's, and the phone app is unmodified. The daemon has to work with whatever
security the phone offers, which limits what this project can change.

## Network exposure

| Direction | What | Protection |
|---|---|---|
| Inbound TCP 52020 (`--port`), **all interfaces** | TLS WebSocket for pairing, sessions (`/readyfor/`) and Connect-button triggers (`/readyfor/mdns`). | TLS 1.2/1.3 (rustls). A session needs a valid pairing token or a known phone id (see below). |
| mDNS adverts (`_readyformdns._tcp` and, for the cable, `_readyfor._tcp` via avahi; same TXT) | TXT records: this run's salted device id, the PC name (`--name`), the certificate fingerprint (hex), static type fields, a timestamp. | Public on the LAN by design. The fingerprint is stable, so the PC can be recognised on the network across restarts. |
| Outbound TCP to the phone's discovery port | One sealed invite when reconnecting over Wi-Fi. | AES-256-GCM sealing with the per-pair key. The phone's TLS certificate is throwaway and isn't checked. |
| Bluetooth LE (Connect wake) | The PC scans, connects to the phone as a GATT central, writes one sealed invite and disconnects. | The same AES-GCM sealing. The adapter's power state is never changed. |
| Bluetooth LE peripheral (only without `--no-ble`) | Advertises service 0x5246 with this run's device id. The GATT server only logs writes. Also powers the adapter on at startup. | Experimental and not needed. The systemd unit passes `--no-ble`. |
| Outbound TCP to the phone's fex port (app icons) | On every app list (service 17) the daemon connects to the port the phone advertised, on the phone's session address, and downloads the app icons. | TLS 1.3, the phone's throwaway certificate pinned by the SHA-256 fingerprint from the (authenticated) app list. Icons are size-capped, must be PNGs and are written only under `~/.cache/smartconnect-linux/app-icons/` with plain file names (no paths). On the phone's side the server does not authenticate the client: its `token` is not checked by the download route, so another device on the LAN that reaches the port in time could fetch the same icons (a phone-side property; the icons reveal which apps are installed). |
| Outbound TCP to the phone's NFS ports (`smartconnect-files list`/`mount`) | NFSv3 to the ports the phone names in its File Station handshake reply (service 168, `cmd=1001`), which travels over the authenticated session. | Mutual TLS. The phone's certificate is pinned by the SHA-256 fingerprint from that reply; it is a minimal X.509 v1 certificate that webpki rejects, so the chain check is bypassed and only the pin and the handshake signature (checked against the pinned certificate's key) are verified. The phone checks this PC's client certificate by the fingerprint the PC sent in the handshake. |
| Inbound TCP 2049 (`smartconnect-files serve` only, `--port`), **all interfaces** | NFSv3 export of `--sharedir` (default: your home folder) for the phone, hidden (dot) entries excluded unless `--include-hidden`. Only listens while `serve` runs. | Mutual TLS. A client certificate is accepted only when its SHA-256 fingerprint is one the connected phone sent on service 168, during this run or (the last 4, kept in `files-identity/phone-fps`) an earlier one; anything else is refused in the TLS handshake. Standard NFS writes are refused; the only change the phone can make is Lenovo's batch delete, which moves files to the Trash (`--read-only` refuses it too). |
| UDP, WebRTC (mirroring, app streaming, phone display, webcam) | Media between the phone and the helper. Only UDP candidates on the address the phone already reached the daemon on (`pcAddress`) are offered; without it, private IPv4 addresses on non-virtual interfaces. `SMARTCONNECT_ICE_ADDRESSES` overrides this. The webcam offers addresses on the phone's own subnet. | DTLS-SRTP (standard WebRTC encryption), negotiated over the TLS control channel. |

## Pairing, keys and encryption

- **The PC's identity** is a self-signed RSA-2048 certificate (`cert.pem`/`key.pem`), valid
  2025–2045. The phone pins the SHA-256 fingerprint of its DER encoding, taken from the QR code (and
  later from invites and the mDNS advert). Nothing else in the certificate is checked, and there are
  no client certificates.
- **The pairing QR** carries a random token (32 random bytes, base64), the PC's addresses, the port
  and the fingerprint. The token is valid for `--token-ttl` seconds (180 by default, 600 in the
  systemd unit) and can be used several times within that window, because the phone connects more
  than once while pairing. **Anyone who can read the QR (your screen, or the SVG file) while it is
  valid, and who can reach port 52020, can pair a device.** A new token replaces the old one.
- **Per-pair keys:** when a phone pairs, the daemon generates a random 256-bit key for it. The key
  goes to the phone in the handshake reply and the key exchange, and the phone stores it for this
  PC. The phone sends its own key too. Both are stored in `paired.json` (mode 0600). The key seals
  the discovery frames used for reconnecting: AES-256-GCM with a random 96-bit IV per frame, and
  the sender id plus frame type as associated data. Frames that don't decrypt with a paired phone's
  key are ignored.
- **Reconnecting:** a session is accepted when the phone's hello carries a valid pairing token,
  **or** names the stable id (`realDeviceId`) of a phone that is already paired. The phone also
  sends a reconnect credential, but how it works hasn't been worked out, so the daemon doesn't check
  it yet. See [Known gaps](#known-gaps).
- **Session traffic** is JSON over the TLS WebSocket. Video uses WebRTC's DTLS-SRTP.

## What a paired phone can do

These are the kinds of things the Windows app allows too. A paired phone can:

- push clipboard text into your Linux clipboard, and receive what you copy as text (password-manager
  copies marked sensitive are never sent);
- show notifications on your desktop and receive the replies you type;
- lock your session (the PC lock tile). It can't unlock it;
- make the PC join a Wi-Fi hotspot whose name and password the PC generated (Hotspot tile);
- start a webcam session that sends its camera picture to the PC (Webcam tile);
- open stream windows on your desktop (app streaming, and the Screen share and Mobile desktop views), and ask for Cross Control. Control only moves to the phone when **you**
  push the pointer into the screen edge, and while it is there everything you type, passwords
  included, goes to the phone;
- **read every visible file under the shared folder while `smartconnect-files serve` runs**
  (default: your **home folder**; the GTK app's *Share with the phone* switch runs it whenever the
  phone is connected), and **move those files to your Trash** unless the share is read-only
  (`--read-only`, or *Let the phone delete files* off). It can't create or change files. Hidden
  (dot) files and folders, such as `~/.ssh`, browser profiles and this project's own keys, are
  invisible to it unless you turn on *Include hidden files* (`--include-hidden`): don't, unless
  the folder you share holds nothing secret. Without `serve` running, nothing on the PC is
  exported. Browsing the phone's own files (`mount`) gives the phone nothing on the PC;
- **start a phone-display session by itself** (when the PC's screen is started from the phone), as
  it can with the Windows app: the daemon then starts `smartconnect-display`, and the phone sees the
  duplicated or extended screen, can move the pointer and type on the PC, and hears the PC's audio
  if `audio` is on in `display.json`. The same applies to sessions you start yourself. Today the
  only way to stop the daemon from starting it is `SMARTCONNECT_HELPERS=0`, which also turns off the
  other automatic helpers.

## Local exposure

- **Control socket** (`$XDG_RUNTIME_DIR/smartconnectd.sock`): there is no authentication. Any
  process running as your user can connect or disconnect the phone, read status, notifications and
  SMS, reply to notifications, send SMS from your phone, and send raw protocol messages to it. This
  is the usual trust model for per-user desktop daemons. `$XDG_RUNTIME_DIR` is private to your user
  (mode 0700). If `XDG_RUNTIME_DIR` is unset, the socket goes to `/tmp` with the daemon's umask; a
  umask of 022 still keeps other users out, because connecting needs write permission. Run the
  daemon in a normal login session, where systemd sets `XDG_RUNTIME_DIR`.
- **Stream host socket** (`$XDG_RUNTIME_DIR/smartconnect-mirror.sock`): same model. It can start and
  close stream windows.
- **Reply window:** the notification's title and body reach `qs` through environment variables,
  which are readable only by your user.
- **Helpers** inherit the daemon's environment and run as your user.
- **Phone display:** while a session runs, the phone's touches move a virtual pointer and keyboard
  in your session, pinned to the shared output.
- **Webcam:** while a session runs, the phone camera's picture is available to every app that can
  open the v4l2loopback device (device permissions decide who that is), or the PipeWire camera node
  in your session. The helper only writes to virtual loopback devices.

## Data on disk

State directory: `$XDG_DATA_HOME/smartconnect-linux/`, usually `~/.local/share/smartconnect-linux/`.
The daemon creates it with mode 0700 and sets 0700 again on every start, so other local users can't
read anything in it. Files with secrets or a live token are also 0600 themselves, and the daemon
tightens them when it rewrites files left by older versions.

| File | Mode | Contents | Sensitivity |
|---|---|---|---|
| `key.pem` | 0600 | TLS private key | **Secret.** Together with network access it lets someone impersonate this PC to your phone. |
| `paired.json` | 0600 | Per paired phone: its name, its stable id, our per-pair key and the phone's key | **Secret.** |
| `pairing-qr.svg` | 0600 | The current pairing QR, with a live token | **Sensitive while the token is valid.** |
| `identity.json` | 0600 | This PC's ids and display name | Identifying, not secret. |
| `phone-endpoints.json` | 0600 | The phone's last LAN address and discovery port | Personal (network info). |
| `sessions/*.jsonl` | 0600, in a 0700 directory | Frame logs, only written with `--session-log` (see [below](#session-logs)) | **Personal**, with secrets redacted. |
| `cert.pem` | umask (0644) | The public certificate | Public. |
| `notif-mute.json` | umask (0644) | Package names you muted | Low. |
| `screencast-token` | umask (0644) | Screen-picker restore token for the phone display's portal fallback | Low. |
| `files-identity/key.der` | 0600, in a 0700 directory | `smartconnect-files`' own TLS private key for File Station (separate from `key.pem`) | **Secret.** Lets someone present this PC's client certificate to the phone's NFS server, or serve the PC's share to it. |
| `files-identity/phone-fps` | 0600, in a 0700 directory | The last 4 certificate fingerprints the phone presented for a PC folder share, so a restarted `serve` accepts the phone at once | Not secret, but it decides which client certificates the share accepts: only add fingerprints you trust. |
| `files-identity/cert.der` | 0600 | Its self-signed certificate | Public. |

Elsewhere:

| Location | Contents | Lifetime |
|---|---|---|
| `$XDG_RUNTIME_DIR/smartconnectd/icons/` | App icons, contact photos and pictures from notifications (JPEG) | tmpfs, cleared at logout/reboot |
| `$XDG_RUNTIME_DIR/smartconnectd/phone-wallpaper.*` | The phone's wallpaper | same |
| `$XDG_RUNTIME_DIR/smartconnectd/*.log` | Output of helpers the daemon started | same |
| `$XDG_RUNTIME_DIR/smartconnect-display.log` | Output of the phone display session the GTK app or the plugin started | same |
| `$XDG_RUNTIME_DIR/smartconnect-files.log` | Output of the phone-files mount the GTK app started | same |
| `$XDG_RUNTIME_DIR/smartconnect-display.{offer,answer}.sdp` | The last WebRTC offer and answer of the phone display (contains LAN addresses) | same |
| `~/.config/smartconnect-linux/display.json`, `gui.json` | Phone-as-display settings and GTK app preferences | until deleted |
| `~/.cache/smartconnect/apps.json` | The list of apps installed on your phone, work profile included | until deleted |
| `~/.cache/smartconnect/mirror-host.log` | Stream host output when the GTK app started it | until deleted |
| NetworkManager | A `SmartConnect-XXXX` Wi-Fi profile with its password while you use the Hotspot tile. Deleted when the phone stops the hotspot. | It stays if the daemon stops first. |
| Memory only | Phone status, the last 30 notifications, the SMS cache | daemon lifetime |

## Logging and redaction

### Journal

With the default `RUST_LOG=info`, `journalctl --user -u smartconnectd` contains:

- at startup: the PC name, this run's device id, the certificate fingerprint and the state
  directory;
- per connection: the phone's IP address and port, the first (at most 32) bytes of the connection in
  hex (the start of the TLS handshake), the TLS version and cipher, the phone's name and stable id;
- per message: one summary line (service, command, size). **Payloads aren't logged.**
- clipboard: direction and character count only;
- Hotspot: the network name. **The passphrase never appears**: it is hidden from debug output and
  removed from `nmcli` error messages;
- Bluetooth wake: the phone's current (rotating) Bluetooth address and its stable id. With the
  optional peripheral (no `--no-ble`): the adapter's address and every GATT write in hex. Those
  writes are encrypted frames.

`RUST_LOG=debug` adds detail such as muted package names, but no message contents.

### Session logs

Session logs are **off by default**. Started with `--session-log`, the daemon records every frame
of every session in `sessions/<unix-ms>-<phone-ip>.jsonl` (mode 0600, in a 0700 directory) for
protocol debugging. Command numbers and payload shapes survive; before writing, the daemon removes:

- the value of every field named `key`, `token`, `pwd`, `password`, `passwd`, `secret` or `psk`, at
  any depth, replaced by its length: per-pair keys in the handshake reply and the key exchange, the
  pairing token in the phone's hello, the Hotspot passphrase;
- notification payloads (text, pictures and action lists, including the replies you send);
- SMS and Messages frames (services 100, 204, 205, 206), down to their command number;
- clipboard text and file names;
- the phone's wallpaper, replaced by its size.

**Still in the logs:** the phone's name and stable id (`realDeviceId`, which the daemon currently
accepts for reconnects; see [Known gaps](#known-gaps)), the Hotspot network name, the phone's app
list, WebRTC session descriptions with LAN addresses, and the input you send to the phone (pointer
positions, and key presses as scancodes from the mirror window and Cross Control). Logs written by
versions before the redaction existed contain keys and personal text: delete them.

Session logs are never rotated or deleted by the daemon. Delete them whenever you like; nothing
reads them back:

```sh
find ~/.local/share/smartconnect-linux/sessions -name '*.jsonl' -mtime +7 -delete
```

Never attach a session log to an issue without reading it and removing personal data first.

### Helper output

The stream host prints package and app names, connection state and short excerpts of unrecognised
stream messages (up to 300 characters). Cross Control prints its service messages (screen sizes,
state). The phone display writes its SDP files as listed above. None of them record media.

## Known gaps

These are open issues, listed so you can judge the risk:

1. **Reconnects are accepted on the phone's stable id alone.** The id is never broadcast in clear
   (the phone's adverts carry a salted hash), but it isn't a cryptographic secret, and every PC the
   phone has paired with has seen it. A device on your LAN that knows it could open a session as
   that phone and get what a phone gets ([see above](#what-a-paired-phone-can-do)), including the
   per-pair key from the handshake reply. The phone also sends a reconnect credential, but it
   isn't known yet whether that proves possession of the pair key, and the daemon can't change what
   the phone sends. Mitigation: firewall port 52020 to your LAN, and remove phones you no longer
   use from `paired.json`.
2. **The daemon listens on every interface**, VPN and container networks included. Mitigation: the
   firewall.
3. **No authentication on the local sockets** beyond file permissions. This is by design; see
   [Local exposure](#local-exposure).
4. **A Hotspot profile can be left behind** in NetworkManager if the daemon stops while you are on
   the phone's hotspot.
5. **A paired phone can start a phone-display session** without anything happening on the PC first
   (see [What a paired phone can do](#what-a-paired-phone-can-do)); there is no separate switch to
   refuse it yet.

Fixed:

- **Session logs** used to be written for every session and held per-pair keys, pairing tokens, the
  Hotspot passphrase, clipboard text and SMS. They are now opt-in (`--session-log`) and redacted
  (see [Session logs](#session-logs)).
- **Permissions:** the state directory used to follow the umask (usually 0755), and
  `identity.json`, `phone-endpoints.json` and `pairing-qr.svg` were 0644, so another local user
  could read a live pairing QR and pair a phone while it was valid. The directory is now 0700 and
  those files 0600.
- **WebRTC candidates:** the media helpers offered every `192.168.` address, which broke other
  private ranges and could include virtual bridges in that range. They now offer the address the
  phone already reached (`pcAddress`), with a filtered fallback and `SMARTCONNECT_ICE_ADDRESSES` as
  an override.

## Hardening checklist

- Open TCP 52020 and mDNS only for your home network, and TCP 2049 only while you share a folder
  with `smartconnect-files serve` (a firewall zone or a source range; see
  [Installation](installation.md#firewall)). Stop the service on untrusted networks.
- Leave `--session-log` off unless you are debugging the protocol, and delete the logs afterwards,
  including any written by older versions (`~/.local/share/smartconnect-linux/sessions/`).
- Don't leave a pairing QR on screen in public. If you do, make a new one with
  `smartconnectd pair`, which invalidates the old token. A shorter `--token-ttl` narrows the window.
- Keep the experimental Bluetooth peripheral off (`--no-ble`, the systemd default).
- **Lost phone:** remove it in its own Smart Connect app if you still can. On the PC, stop the
  daemon, delete its entry from `paired.json`, and start the daemon again.
- **Leaked `key.pem` or `paired.json`:** create a new identity (see
  [Starting over](troubleshooting.md#starting-over)) and pair again.

## Reporting a vulnerability

Please don't open a public issue for security problems. Use GitHub's private vulnerability
reporting for this repository (*Security* tab, then *Report a vulnerability*), and include the
version or commit, your setup, and steps to reproduce. Remove personal data and keys from any logs.
