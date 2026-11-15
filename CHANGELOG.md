# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The README's
[feature matrix](README.md#feature-status) shows what is verified on a real phone.

## [Unreleased]

### Added

- GTK **Messages** page: fetches the daemon's in-memory SMS conversations only on refresh, opens threads, sends through phone service 204, and marks received messages read. It never writes SMS content to disk.

### Fixed

- Clipboard Linux → phone on GNOME: GNOME has no data-control protocol, so `wl-paste --watch` refused to run and copies on the PC never reached the phone. The daemon now follows the clipboard through Xwayland (XFixes) there; password-manager copies are still skipped.
- Phone display lag: frames are now converted and scaled on the GPU (VA-API `vapostproc`; multi-threaded on the CPU otherwise) and the encoder runs in its own thread. Before, 60 fps did not fit the per-frame time and frames queued up (measured on the PC: 53 ms → 21 ms per frame, no more dropped frames).
- GTK Display tab: the settings groups no longer leave empty space below them.
- Phone display with PC audio on: the audio is captured with a 40 ms buffer instead of 200 ms (plus a queue of up to 200 ms), which held the phone's picture back to keep it in sync.
- Sharing a PC folder with the phone (`smartconnect-files serve`) now works on a real phone. The phone only accepts 64-byte NFS file handles and silently retried every folder listing until it timed out; handles are now 64 bytes. The share also answers the phone's network check (`cmd 2006` → `2007`), so a Wi-Fi hiccup or a restart no longer unmounts it, and it remembers the phone's certificate across restarts.
- PC lock tile: the phone is told "locked" only once the lock is actually seen (logind's `LockedHint`), instead of "locked" followed 3 s later by "unlocked" when nothing had locked. When nothing locks, the tile resets and the PC shows a notification saying why. Noctalia is now locked through its own IPC. GNOME started from SDDM or LightDM can't be locked at all (GNOME Shell only creates its lock screen under GDM); the notification says so.
- Cross Control tile on GNOME: tapping it no longer starts a client that exits at once with `compositor lacks wlr-layer-shell`; the client now uses the InputCapture portal there (see Added).
- Cross Control tile: a tap always starts Cross Control (and brings the GTK app's three-mode page forward), never Extend on its own. The phone merges Cross Control and the PC display into one tile that repeats whichever connected last when the PC lists `"202"` in its device-status capability map; the daemon keeps `"202"` out of it.
- Reconnect with Smart Connect in the background on the phone is reliable: Connect attempts no longer run concurrently (the GTK button during the auto-reconnect loop used to make BlueZ fail with "operation already in progress"); a Bluetooth LE connect that times out is cancelled and the phone rescanned (its address can go stale); scans last 30 s instead of 20 s; and a desktop Connect that fails keeps retrying in the background. Verified: CLI Connect in 5 to 6 s and daemon restart in about 16 s, app in the background.
- A session whose phone went silent (left Wi-Fi, or this PC slept) is dropped after 70 s and reconnected, instead of looking connected until the kernel gave up on the socket.
- Invites follow the PC's current LAN addresses after a network change instead of the ones read at startup (unless `--ip` is given).
- Phone files: a stale mount no longer looks mounted. When the `smartconnect-files mount` helper died (for example after a daemon restart, when the phone's per-session NFS ports changed), it left a FUSE mount behind ("Transport endpoint is not connected") that `mountpoint` still reported as mounted, so the Noctalia tile showed *Unmount files* and **Open** silently did nothing. Now:
  - `smartconnect-files mount` **survives a phone reconnect or a daemon restart**: on a lost connection files calls fail fast with an I/O error, and once the phone is back it redoes the handshake and carries on (file handles from the old session are re-resolved). It unmounts only after 10 minutes without the phone or when the phone is unpaired. SIGINT/SIGTERM/SIGHUP unmount cleanly (lazily if busy), and a stale mount at the folder is cleared before mounting.
  - The GTK *Files* page and the Noctalia *Phone files* tile check that the mount is alive (`statfs` and a listing, with timeouts), not just that the folder is a mount point. They clear a stale mount, remount it (on **Open**, or with auto-mount), and say when the phone is offline or the folder couldn't be opened instead of failing silently. The panel also adopts a live mount it didn't start.
- Notifications: WhatsApp (and other messaging-app) notifications no longer show your own profile picture. The phone's `person_icon` for a chat is the phone owner, not the sender; it is now used only when it is the headline person, and otherwise the app icon is shown (the phone's launcher icon from the app-icon cache when present, then a themed icon, then the phone's glyph).
- Webcam: switching between the front and back camera on the phone no longer stops the webcam. The phone re-sends its unchanged offer after a switch; the helper now keeps the running session instead of answering it again.
- Noctalia plugin: the panel reconnects after the daemon restarts instead of staying on "smartconnectd is not running" (a fresh Quickshell socket per attempt).
- Noctalia panel layout: compact phone header, 2×2 action tiles with readable option chips (Cross Control edge, display mode and scale), and a panel height that follows its content.

### Changed

- The Screen share and Mobile desktop tiles are on by default (service 1 in the device-status map); `--extra-tiles 1` is no longer needed, so they no longer disappear after a daemon restart without it.
- GTK app: the window was restructured to a shared design. A header **device menu** (left of the title) shows the connected device and holds *Pair new phone…*, *Preferences* (the *Start at login* switch) and *Connect/Disconnect* — moved out of the pages. The view switcher now has five pages: *Phone* (a compact phone card with wallpaper, name, an inline battery/Wi-Fi/mobile/Bluetooth row and **Mirror screen**, an unread-messages shortcut to Notifications, and the Cross Control switch), *Apps* (search + Recent chips + an icon grid), *Display* (a phone-as-display card over Layout / Experimental / Advanced groups), *Files* (a storage card with a *Read-only* badge, **Open in Files**, quick-folder shortcuts, and a *Mount automatically when connected* switch) and *Notifications* (grouped under *Today* with **Clear all** and a per-notification **⋯** menu for *Open on phone* / *Dismiss* / *Mute app*). Pairing is now a dialog opened from the device menu. All controls follow the system light/dark theme and accent colour. No change to the control-socket wiring, mirror/app-streaming, display sessions, mount lifecycle, tray or autostart behaviour.

### Added

- AppImage for any distribution (`packaging/appimage/make-appimage.sh`, built on Arch with quick-sharun, everything bundled): opening it shows a welcome window whose **Set up** installs to `~/.local/opt/smartconnect-linux` with the launcher and the user service; `--install`, `--uninstall`, and running any bundled program through the file.
- Pairing window: warns when firewalld's default zone blocks port 52020 and, on GNOME and KDE, opens it with one click (firewalld asks for the password); shows the commands for ufw and other desktops.
- Sharing a PC folder with the phone now works in Smart Connect's **Files** tab, the way it does with Windows: the PC is listed under Files, with *All files* and the *Recents / Images / Videos / Documents / Audio / Downloads* views, image and video thumbnails (videos need `ffmpeg`), and delete, which moves files to your Trash (*Let the phone delete files* / `--read-only` to turn it off). The share now defaults to your home folder, with hidden (dot) files kept invisible to the phone unless *Include hidden files* / `--include-hidden`.
- Phone display: *Stream quality* and *Frame rate* settings in the GTK app (`--bitrate`, new `--fps`, and `bitrate`/`fps` in `display.json`; the Noctalia plugin keeps them).
- GTK app: *Share with the phone* on the Files page — a switch that shares a PC folder (default `~/PhoneShare`, changeable) read-only with the phone whenever it's connected, with a status line that points at the firewall when the phone can't reach the PC. The phone opens it from Android's file browser, or the File explorer tile in Smart Connect's home-screen widget, which `serve` now switches on while it runs.
- Phone as display (Extend and Duplicate) on GNOME (Mutter virtual monitor + RemoteDesktop input), KDE Plasma (KWin virtual output + fake input; untested), Hyprland and sway (headless output + wlr-screencopy; sway untested), not only niri. The desktop is detected automatically (`SMARTCONNECT_DESKTOP` overrides it).
- `smartconnect-display --list-outputs`: the desktop, how Extend works there and its screens as JSON; the GTK *Display* page uses it instead of `niri msg`.
- `packaging/dev.smartconnect.Display.desktop`, installed by `install.sh`, so KWin trusts the display helper.
- On GNOME the phone display's keyboard button turns on GNOME's own screen keyboard.
- Extend puts the phone's screen on the side of the main screen chosen under *Phone position* on the Display tab (`crosscontrol.json`; right by default), follows a change while it runs, and takes `--side left|right|top|bottom`. Not on KDE yet.
- GTK app: the **Cross Control** modes at the top of the Display tab (one tab with the display settings instead of two pages): the three modes the Windows app offers, side by side: *Shared mouse and keyboard*, *Duplicate display* and *Extend display* (the last two run a phone-as-display session with the Display page's settings). Starting one stops the other. It replaces the Cross Control switch and edge selector on the Phone page, which now links to it. Tapping Cross Control on the phone brings the app forward on that page.
- Cross Control phone position: drag the phone to any side of the screen on the Display tab (drawn landscape while a display session runs with the phone held that way); only the highlighted part of that edge crosses over to the phone, mapped onto the phone's whole side. Saved in `~/.config/smartconnect-linux/crosscontrol.json`, which `smartconnect-crosscontrol` reads at startup (so the phone's tile uses it too); `--span START:END` overrides it. The daemon no longer forces `--edge right` on clients it starts for the tile.
- Cross Control on GNOME: `smartconnect-crosscontrol --backend portal` grabs the mouse and keyboard through the xdg-desktop-portal InputCapture interface (a pointer barrier on the chosen edge, events over libei) instead of wlr-layer-shell. `--backend auto` (the default) picks layer-shell when the compositor has it, else the portal. The first start asks to allow input capture; the answer is remembered. Verified on GNOME 51 with a phone.
- `smartconnectd --lock-command <CMD>`: lock the screen with your own command for the PC lock tile.
- Reconnect triggers: the daemon retries at once when the phone answers on Wi-Fi again (a TCP probe every 30 s, no Bluetooth), after a resume from suspend, when the PC gains a LAN address, and when the PC's Bluetooth is switched on. While the phone is on the LAN the retry interval stays at a minute. Clearer messages when a backgrounded phone can't be reached (`Bluetooth is off`, `--no-ble-wake`).
- `smartconnectd --no-ble-wake`: never use Bluetooth LE to wake the phone (the wake never changes the adapter's power either way).
- App icons from the phone: on every connection `smartconnectd` now downloads the icons the phone offers with its app list (service 17) over the phone's "fex" file-transfer server and writes them to `~/.cache/smartconnect-linux/app-icons/`, named as the phone's `iconName` (atomic writes; unchanged icons are not rewritten). Completing the download also makes the phone send its recent-apps list. Turn it off with `SMARTCONNECT_APP_ICONS=0`.
- `smartconnect-fex`, a new helper: `fetch-app-icons [--out DIR]` asks the phone for its app list and downloads the icons now; `pull --host --port --fingerprint` pulls any fex offer. The reusable client (`fex/`) is what clipboard images will use.
- Control socket: `status` now includes `phoneAddress`, the connected phone's IP address.
- GTK app: real phone app icons on the *Apps* page, Recent chips, running streams and notification rows, read from `~/.cache/smartconnect-linux/app-icons/` (the phone's `iconName`, then `<pkg>.png`). `smartconnect-mirror` now saves the icon the phone sends with a stream invite or focus change (svc 15 `cmd 2` / `cmd 13`) there, and `--list-apps --json` keeps each app's `iconName`/`userId`. Without a cached icon the app uses a desktop icon only on an exact match, else the app's initials — no more generic placeholder glyph or wrong look-alike icons. New icons appear without a restart.
- GTK app: the Phone page and device menu remember the last connected phone (name, model, wallpaper, last-known battery/signal, when it was last connected) in `~/.local/state/smartconnect-linux/last-device.json`, and show it while the phone is away, while the service is stopped, and after an app restart. Disconnected: a **Connect** button with a *Connecting…* spinner and a clear failure toast with the daemon's reason; no phone paired: **Pair a phone**; service not running: a *Smart Connect service isn't running* page with the last phone and **Start service** (also in the device menu).
- GTK app: a system-tray icon (StatusNotifierItem) that reflects the connection state, with a menu to show/hide the window, Connect, Disconnect and Quit. Closing the window now hides it to the tray instead of quitting.
- GTK app: opening it starts `smartconnectd` if it isn't running (the `systemctl --user` unit if installed, otherwise as a child process), showing a *Starting…* state until the control socket answers; Quit stops the daemon it manages.
- GTK app: a *Start at login* switch that adds or removes the XDG autostart entry, and a `smartconnect-gui --background` flag that starts the app hidden to the tray.
- GTK app: a *Files* page that mounts the phone's storage (via `smartconnect-files`) as a read-only folder — **Mount**, then **Open in Files** / **Unmount** — with a configurable mount folder (default `~/Phone`, saved in `gui.json`) and errors surfaced in a banner. The Mount button is disabled until the phone is connected; closing the window to the tray keeps the mount, and Quit unmounts it so no stale FUSE mount is left behind.
- Noctalia plugin: a *Phone files* action tile in the panel that mounts the phone's storage (via `smartconnect-files`) as a read-only folder — tap to **Mount** (default `~/Phone`), an option chip shows *Mounting…* then a clickable **Open** once mounted, and the tile becomes **Unmount**. Available only while the phone is connected (or a mount is live, so it can still be unmounted); a fast exit (phone not connected / handshake failed) falls back to unmounted and shows the helper's error in the status line. The mount folder and helper path are configurable in the plugin settings. The mount persists when the panel closes; a stale mount left by a shell exit is cleared the next time the panel opens.
- Webcam quality presets (`--quality low|medium|high|max`, or `quality` in `~/.config/smartconnect-linux/webcam.json`): the bitrate the phone may encode at.
- Protocol docs: [docs/protocol/file-exchange.md](docs/protocol/file-exchange.md) covers fex file transfer (service 2), File Station NFSv3 (service 168) and Share Hub (service 30); the clipboard protocol page adds the image/file clip control plane.
- Browse the phone's files: `smartconnect-files mount --mountpoint DIR` presents the phone's storage as a read-only FUSE filesystem, using the phone's own File Station protocol (NFSv3 over fingerprint-pinned mutual TLS, no root/adb). It handshakes with the phone through the daemon. Verified on a device. Needs `fuse3`.
- Share a PC folder to the phone: `smartconnect-files serve [--sharedir DIR]` exports a local folder (default `~/PhoneShare`) **read-only** as an NFSv3 server over the same fingerprint-pinned mutual TLS, so the connected phone can mount and browse it (the mirror of the mount above). It answers the phone's `cmd=1001` File Station handshake over the daemon control socket, forcing SSL transport (`dt2type:2`). The full server + mutual-TLS + NFS path is covered by loopback tests; **live-phone validation is pending** (see [file-exchange.md](docs/protocol/file-exchange.md)).
- Clipboard: the daemon now recognises an incoming image/file clip from the phone (and logs it) instead of treating it as unknown; the bytes are not fetched yet (the fex client is not wired to clipboard clips), and it never mistakenly PULLs.
- `smartconnectd` daemon: the unmodified Motorola Smart Connect app sees this Linux PC as a Windows PC.
- QR pairing (`smartconnectd pair`, the GTK app's pairing dialog, the Noctalia QR button), including the key exchange that lists the PC in the phone's device list.
- Reconnect without a QR over Wi-Fi (mDNS via avahi): the phone's Connect button, and the daemon dialing the phone back.
- Bluetooth LE wake: a desktop-side Connect reaches the phone even with Smart Connect in the background.
- Persistent auto-reconnect with backoff; a Disconnect from the phone or the desktop is respected.
- Clipboard text sync in both directions through wl-clipboard; password-manager copies are never sent.
- Phone notifications as desktop notifications, updated and closed together with the phone.
- Quick reply to phone notifications from a reply window, the GTK app or the Noctalia panel.
- Multi-line chat notifications showing recent messages and, in group chats, their senders.
- Notification dedupe and coalescing of rapid updates.
- Themed desktop icons for phone notifications.
- Notification action buttons run on the phone through a short, windowless app-streaming session (`SMARTCONNECT_BACKGROUND_ACTIONS=0` opens a window instead); clicking a notification opens the app in a window (not verified on a device).
- Per-app notification mute: a toggle on each notification and a *Muted apps* list in the GTK app and the Noctalia plugin, and the `notif_mute` control command.
- Phone status for front-ends: battery, charging, signal, Bluetooth, unread count and wallpaper.
- SMS backend with the `sms_threads`, `sms_thread`, `sms_send`, `sms_mark_read` and `sms_state` control commands (not verified on a device).
- Control socket `$XDG_RUNTIME_DIR/smartconnectd.sock` with `status`, `subscribe`, `connect`, `disconnect`, `pair`, `watch`, `send`, `reply` and `notif_mute`, plus the matching CLI subcommands.
- The status object carries `pcAddress` (the address the phone reached this PC on) and each notification's `pkg`.
- `watch` can stream the daemon's local events (service id `-1`); the first one is `notification_action`.
- Feature tiles for this PC on the phone: App streaming, Cross Control, PC lock, Hotspot and Webcam.
- `--extra-tiles` to advertise more tiles; `--extra-tiles 1` adds Screen share and Mobile desktop, answered by the stream host (experimental).
- PC lock tile, which locks the session through logind.
- Hotspot tile: the phone starts a hotspot with PC-chosen credentials and the PC joins it through NetworkManager (not verified on a device).
- `smartconnect-mirror`: phone screen mirroring over WebRTC (H.264, VA-API or software decoding) with mouse input.
- Keyboard input, phone keys (Esc = Back, Ctrl+Alt+H = Home, Ctrl+Alt+R = Recents) and scrolling in the mirror window.
- App streaming: phone apps in their own windows (`--host`, `--app`, `--list-apps`, `--sessions`, `--close`), including apps picked from the phone's App streaming tile.
- Opt-in app window resize: the phone resizes an app's virtual display to its window (`SMARTCONNECT_MIRROR_RESIZE=1` or `--host --resize`).
- `smartconnect-mirror --desktop` and the stream host's `desktop` command ask the phone for Ready For's Mobile desktop (experimental).
- The video helpers offer the phone WebRTC candidates only on the address it reached the daemon on (`pcAddress`), on any private network; `SMARTCONNECT_ICE_ADDRESSES` overrides it.
- `smartconnect-crosscontrol`: Cross Control on Wayland, moving the PC's mouse and keyboard onto the phone across a screen edge (`--edge`, `--output`, `--gain`, `--exit-px`; Ctrl+Alt+Esc returns).
- Cross Control can be started from the phone's tile; the daemon starts the client on demand.
- The daemon supervises the stream host, and starts Cross Control, the phone display and the webcam helper when the phone asks for them (`SMARTCONNECT_HELPERS`, `SMARTCONNECT_CROSS_EDGE`).
- Programs find each other through `SMARTCONNECT_<NAME>_BIN` (`SMARTCONNECT_MIRROR_BIN`, `SMARTCONNECT_CROSSCONTROL_BIN`, `SMARTCONNECT_DISPLAY_BIN`, `SMARTCONNECT_WEBCAM_BIN`, and for the GTK app also `SMARTCONNECT_FILES_BIN` and `SMARTCONNECT_SMARTCONNECTD_BIN`), then next to the running program, then on `PATH`.
- `smartconnect-gui`: GTK4/libadwaita app with Phone, Apps, Display, Files and Notifications pages, a pairing dialog, a mirror button and a Cross Control switch.
- Noctalia Shell plugin: bar pill, control-center button, panel, settings, and the IPC target `plugin:smart-connect` (`toggle`, `connect`, `disconnect`, `mirror`, `display …`).
- `smartconnect-display`: the phone as an external display, either extending the desktop onto a `vkms` virtual output (`--mode extend`, `--size native`, `--output`) or duplicating the focused screen.
- `--display-scale` for the phone's monitor in Extend, 200 % by default.
- Touch input from the phone display is injected on the PC: tap, drag, long-press for right click, two-finger scroll.
- The keyboard button on the phone display shows an on-screen keyboard on the shared screen (`wvkbd` or `SMARTCONNECT_OSK`), sized with `--osk-height`.
- Phone display controls in the GTK app (*Display* page) and the Noctalia plugin (panel button, settings, `display` IPC), sharing `~/.config/smartconnect-linux/display.json` with the helper (not yet verified with a phone).
- The phone display can be started from the phone; the daemon starts `smartconnect-display` and tells the phone to stop waiting if it fails (not verified on a device).
- One phone display session at a time, with `--replace` to take over.
- Opt-in `--follow-rotation`: the extended monitor turns portrait with the phone (not verified on a device).
- Opt-in `--audio`: the PC's sound plays on the phone display, muted from the phone's audio switch (not verified on a device).
- Webcam tile and the `smartconnect-webcam` helper: the phone camera as a Linux webcam through v4l2loopback or a PipeWire camera node (verified with a phone, including the front/back camera switch).
- One Cargo workspace for all eight programs, `packaging/install.sh` (installs everything, including the systemd user unit, the GTK app's launcher and icon and the Noctalia plugin; checks the build libraries with pkg-config first) and `packaging/uninstall.sh`, plus an Arch `PKGBUILD` for a local checkout (it turns off makepkg's `lto` option, which breaks the link).
- Documentation: README with feature matrix, *Notification actions* and *Known issues* (one phone at a time, no file/image clipboard yet), `docs/` (installation, usage, troubleshooting, architecture, control socket API, development, security), `docs/protocol/`, and a [TODO.md](TODO.md) backlog.

### Security

- Session logs are opt-in (`smartconnectd --session-log`) and redact keys, tokens, passwords, clipboard text, SMS and notification contents and the wallpaper.
- The state directory is created with mode 0700; `key.pem`, `paired.json`, `identity.json`, `phone-endpoints.json`, `pairing-qr.svg` and session logs are 0600.
- The Hotspot password is kept out of the daemon log and `nmcli` error messages.
