# Troubleshooting

Start with the basics:

```sh
systemctl --user status smartconnectd
journalctl --user -u smartconnectd -n 200      # add -f to follow
smartconnectd status                           # is the phone connected?
```

If a section below doesn't help, collect the logs listed in [Where the logs are](#where-the-logs-are)
and open an issue. Remove personal data first: phone numbers, names, message text, addresses.

- [The daemon doesn't start](#the-daemon-doesnt-start)
- [The phone can't pair](#the-phone-cant-pair)
- [Paired, but the PC is missing or offline on the phone](#paired-but-the-pc-is-missing-or-offline-on-the-phone)
- [Connect and reconnect don't work](#connect-and-reconnect-dont-work)
- [The phone isn't found over Bluetooth](#the-phone-isnt-found-over-bluetooth)
- [Feature tiles are missing or do nothing](#feature-tiles-are-missing-or-do-nothing)
- [Clipboard doesn't sync](#clipboard-doesnt-sync)
- [Notifications](#notifications)
- [Mirroring and app streaming](#mirroring-and-app-streaming)
- [Cross Control](#cross-control)
- [Phone files (~/Phone)](#phone-files-phone)
- [PC lock and Hotspot](#pc-lock-and-hotspot)
- [Phone as display](#phone-as-display)
- [Webcam](#webcam)
- [Phone-side debug logs](#phone-side-debug-logs)
- [Where the logs are](#where-the-logs-are)
- [Starting over](#starting-over)

## The daemon doesn't start

| Message or symptom | Fix |
|---|---|
| `Address already in use` | Another `smartconnectd` holds port 52020, often a copy started by hand next to the service. Stop one of them. |
| `no LAN IPv4 address found; pass --ip` | No private IPv4 address on a physical-looking interface. Connect to the LAN, or pass `--ip <address>`. |
| `mdns: not advertising …` / `starting avahi-browse` | avahi isn't installed or `avahi-daemon` isn't running. The phone's Connect button won't find the PC. |
| `notifications: no desktop notification server` | No notification daemon on the session bus. Phone notifications won't show. |
| `clipboard: no Wayland session` | `WAYLAND_DISPLAY` is missing from the service environment. See [Clipboard doesn't sync](#clipboard-doesnt-sync). |
| `helpers: no graphical session` | Neither `WAYLAND_DISPLAY` nor `DISPLAY` is set, so the stream host, Cross Control and the phone display won't start. Import them (see below). |
| The service never starts at login | Your session may never reach `graphical-session.target`. Check `systemctl --user is-active graphical-session.target`, and start the daemon from your compositor's autostart if needed. |

Importing the session environment into the user manager:

```sh
systemctl --user import-environment WAYLAND_DISPLAY DISPLAY
systemctl --user restart smartconnectd
```

## The phone can't pair

1. **The QR expired.** It is valid for `--token-ttl` seconds (180 by default, 600 in the systemd
   unit). Make a new one: `smartconnectd pair`, **Pair new phone…** in the GTK app's device menu, or the
   Noctalia QR button.
2. **The firewall.** The phone must reach TCP 52020 on the PC. Runtime-only firewalld rules are
   gone after a reboot, and the phone then logs "host unreachable". Add the rules with
   `--permanent` (see [Installation](installation.md#firewall)).
3. **The wrong address in the QR.** The QR lists every private IPv4 address of the PC, skipping
   Docker, libvirt, VPN and similar interfaces. With several networks, or a phone on a guest network
   that can't reach the PC, pass the right one: `smartconnectd --ip 192.168.1.20`.
4. **"Unable to pair. Please check that Bluetooth is turned on for both devices."** This dialog is
   the phone's catch-all for "couldn't reach the PC". Pairing doesn't use Bluetooth. Check items 2
   and 3.
5. Watch the journal while you scan. `TCP connection` and `TLS established` show that the phone
   reached the PC, `accepted … token_ok=true` shows pairing went through, and
   `no valid invite token and not a known device; dropping` means the token was stale or wrong.

## Paired, but the PC is missing or offline on the phone

- **Offline right after the first pairing.** The phone creates the device entry after it has
  already marked the PC online, so a brand-new entry can show offline with a Connect button. About
  5 s later the daemon asks the phone to reconnect, if it has seen the phone's mDNS advert.
  Otherwise tap **Connect** once.
- **The PC never shows up in the list.** The pairing only completes with a key exchange right after
  the connection. The journal shows `key exchange: sending our key`. If the phone doesn't start the
  exchange within 2 s (for example when you removed the PC on the phone earlier), the daemon sends it
  anyway, which re-creates the entry.
- **The entry still won't appear after pairing again**, especially if you used a very early build:
  the phone may hold a stale record for this PC's identity. [Start over](#starting-over) with a fresh
  identity, or clear Smart Connect's storage on the phone.

## Connect and reconnect don't work

- **Wi-Fi reconnect needs mDNS.** Allow inbound mDNS (UDP 5353) and make sure `avahi-daemon` is
  running. Without it the phone's Connect button can't find the PC, and the PC can't see the phone's
  advert.
- **The phone only listens on Wi-Fi while Smart Connect is open.** With the app in the background,
  the journal shows `connect: Wi-Fi dial failed (Connection refused (os error 111)); trying
  Bluetooth`. That line is expected, not an error: "refused" means the phone is on the network but
  its app isn't listening, and the daemon goes on to wake it over Bluetooth LE, as the Windows app
  does. See [the next section](#the-phone-isnt-found-over-bluetooth) if that fails too.
- **Bluetooth is off on this PC.** Then nothing on Wi-Fi can wake a backgrounded phone (it has no
  listener and doesn't reconnect by itself). Either open Smart Connect on the phone, which makes it
  reconnect within a few seconds, or switch Bluetooth on: the daemon notices and reconnects right
  away. The daemon never changes the adapter's power itself.
- **Nothing happens after a Disconnect.** That's intended: after a Disconnect from either side the
  daemon waits for an explicit Connect. Run `smartconnectd connect` or tap Connect on the phone.
- **Auto-reconnect.** The journal logs `auto-reconnect (session dropped)` (or `(startup)`) and one
  `auto-reconnect: … took_ms=…` line per attempt. Attempts back off to once every 5 minutes, but stay
  at once a minute (for 15 attempts) while the phone still answers on the LAN. Lines like
  `reconnect: resumed from suspend; trying now`, `… the network came up or changed …`,
  `… Bluetooth was turned on …` and `auto-reconnect: the phone is back on Wi-Fi` show an early retry.
- **The session dropped by itself after a minute of silence**: `nothing from the phone for 70 s`
  means the phone stopped answering pings (it left Wi-Fi, or this PC slept). The daemon reconnects
  once the phone is reachable again.
- **The PC's IP address changed** (new network, DHCP lease, after the Hotspot tile): the daemon
  re-reads its LAN addresses every 5 s and logs `LAN addresses changed`; new invites carry the new
  ones. With `--ip` the addresses are pinned and you need to restart it.
- `connect` answers `ok: true` once the invite is delivered. The phone then dials back by itself,
  usually within a second. `smartconnectd status` shows `"connected":true` when it has.

## The cable connection doesn't work

- **"Unable to connect: disconnect the USB cable…" after 30 to 45 s.** The phone found nothing on
  the cable. Check that the daemon runs and logs `mdns: advertising _readyfor._tcp`: the phone looks
  for that service on the cable. `avahi-publish` must be on the daemon's `PATH` (the AppImage ships
  it in `~/.local/libexec/smartconnect-linux`). Let mDNS (udp 5353) and the control port through the
  firewall on the cable interface too.
- **The phone keeps reconnecting the USB link** (the kernel log shows the phone re-enumerating every
  30 to 45 s): same cause; that is the phone retrying.
- **Connect on a plugged-in phone fails after a replug.** The PC's cable address changed (see
  [By cable](usage.md#by-cable) to keep it fixed), or the phone still has the old one: use the
  phone's cable mode, or scan a new QR once.

## The phone isn't found over Bluetooth

| Reply from `smartconnectd connect` | Meaning and fix |
|---|---|
| `Bluetooth is off on this computer` | The daemon never powers the adapter on by itself. Turn it on: `bluetoothctl power on`. |
| `opening BlueZ session (is bluetoothd running?)` | `sudo systemctl enable --now bluetooth`. |
| `no Bluetooth adapter` | No Bluetooth LE adapter is available to BlueZ. |
| `phone not found over Bluetooth (is Bluetooth on on the phone?)` | Nothing matching was heard in the 30 s scan. Turn Bluetooth on on the phone and keep it near the PC. The phone's background advert is weak (about -7 dBm on the test phone), so walls and distance matter more than for earbuds; the daemon keeps retrying. If Smart Connect was force-stopped, open it once. |
| `couldn't connect to the phone over Bluetooth` | The phone was found, but its LE connection didn't come up (two scans, two 12 s attempts each). The daemon keeps retrying; toggle Bluetooth on the phone if it persists. |
| `Bluetooth wake is off (--no-ble-wake)` | The daemon was started with `--no-ble-wake`. Open Smart Connect on the phone, or drop the flag. |
| `the phone has no 0x5246 GATT service` / `characteristic 0x0722 missing` | The device that matched doesn't offer Smart Connect's wake service. Please report it with your phone model and app version. |

Background: the phone advertises a salted identifier, and the daemon matches it against the phones
it has paired with. Only phones paired with this PC are recognised. The phone also changes its
Bluetooth address from time to time, so the scan only considers devices heard during the current
scan. The `--no-ble` flag doesn't affect any of this; it only turns off the separate, experimental
BLE peripheral.

## Feature tiles are missing or do nothing

- **Which tiles are expected:** App streaming, Cross Control, PC lock, Hotspot, Webcam, Screen
  share and Mobile desktop. A daemon from before Screen share and Mobile desktop were on by default
  needed `--extra-tiles 1` for those two; if they vanish after a restart, update the daemon.
- **No tiles at all:** the phone draws a tile only if the PC lists the service in its handshake and
  in a status message sent right after it. The phone keeps the last list across reconnects. Update to
  a current build and reconnect, so the phone re-reads the list.
- **App streaming tile does nothing:** the stream host isn't running. The daemon starts
  `smartconnect-mirror --host` after the phone connects, but only if it finds the binary and has a
  graphical environment. Look in `$XDG_RUNTIME_DIR/smartconnectd/smartconnect-mirror.log` and in
  the journal (`helpers: …`, which names the variable to set when the binary isn't found). Also
  check that `SMARTCONNECT_HELPERS` isn't `0`, and see the binary lookup order in
  [Installation](installation.md#how-the-programs-find-each-other).
- **Cross Control tile does nothing:** the same applies to `smartconnect-crosscontrol`. Its output
  goes to `$XDG_RUNTIME_DIR/smartconnectd/smartconnect-crosscontrol.log`.
- **Webcam tile says to install the PC camera driver:** the daemon couldn't start
  `smartconnect-webcam`. Install it next to `smartconnectd` (`packaging/install.sh` installs all
  helpers, see [Installation](installation.md#build-and-install)).
  The helper's log is `$XDG_RUNTIME_DIR/smartconnectd/smartconnect-webcam.log`.
- **Starting the PC screen from the phone does nothing,** or the phone leaves "Connecting…" at once:
  the daemon starts `smartconnect-display` for that request and tells the phone to stop waiting when
  the helper can't start or fails. The reason is in
  `$XDG_RUNTIME_DIR/smartconnectd/smartconnect-display.log`, and the daemon's journal shows
  `helpers: can't start smartconnect-display …` or `… failed to start (see its log)`. See also
  [Phone as display](#phone-as-display).
- **PC lock / Hotspot:** see [PC lock and Hotspot](#pc-lock-and-hotspot).

## Clipboard doesn't sync

- **Linux → phone:** the daemon must see `WAYLAND_DISPLAY`
  (`systemctl --user show-environment | grep WAYLAND`). With the data-control protocol (KDE,
  Hyprland, sway, niri), test with `wl-paste --watch echo changed`, then copy something: it should
  print `changed`. Without it (GNOME prints "Watch mode requires a compositor that supports the
  data-control protocol"), the daemon logs `following the clipboard through Xwayland` and needs
  `DISPLAY` in its environment and Xwayland not disabled; `xclip -o -selection clipboard` should
  print what you copied.
- **Phone → Linux:** needs `wl-copy` and `WAYLAND_DISPLAY` too. The journal logs
  `clipboard: phone → Linux` with the character count.
- **Never sent, by design:** copies marked sensitive by a password manager, and anything that isn't
  text (images, files).
- The phone only accepts clipboard updates after it has negotiated the clipboard with the PC, which
  it does at the start of each connection.

## Notifications

- **Nothing shows:** check that a notification daemon is running (the journal warns if there is
  none), that the app isn't muted (`echo '{"cmd":"notif_mute"}' | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/smartconnectd.sock`),
  and that the phone forwards that app. Notifications with custom layouts are skipped.
- **The Reply window doesn't open:** the journal says
  ``reply window failed to start (is quickshell `qs` on PATH?)``. Install Quickshell, or reply from
  the GTK app or the Noctalia panel. `reply window exited abnormally` is usually harmless: the window
  was closed together with the shell, for example when Noctalia restarted.
- **The reply says "phone not connected" or "empty reply":** the phone dropped, or the text was
  empty.
- **Action buttons do nothing** (not verified on a device yet): the phone carries them out through
  app streaming, so the stream host must be running (`smartconnect-mirror --sessions` answers when
  it is). Its log (`$XDG_RUNTIME_DIR/smartconnectd/smartconnect-mirror.log`) should show
  `notification click for <package>: action button: the next invite runs without a window`, then a
  session for that package and `the phone is running the action now`. If no session follows the
  click, the phone didn't ask to stream the app; please report that with your phone model.
- **An action button opens a window:** either `SMARTCONNECT_BACKGROUND_ACTIONS=0` is set in the
  daemon's environment, or the phone's request came more than 6 s after the click, so the host
  treated it as a normal app start.
- **"Open on phone" opens nothing:** it opens the app in a window through app streaming; see
  [Mirroring and app streaming](#mirroring-and-app-streaming).

## Mirroring and app streaming

**The window never appears, and nothing is logged for about 15 s.** The phone gave up silently.
Possible causes:

1. **The phone can't reach the address offered for the video.** The helper offers only the address
   the phone reached the daemon on (`pcAddress` in `smartconnectd status`). The host's log names the
   rule it used (`ICE: offering …`). With a VPN, a second network card or an unusual network, set
   `SMARTCONNECT_ICE_ADDRESSES` to the address the phone can reach (or `all`) in the daemon's
   environment and restart it.
2. **The phone rejected the request.** Turn on [phone-side debug logs](#phone-side-debug-logs) and
   try again. The phone's log says why.
3. **A stream host is already mirroring.** `smartconnect-mirror` hands the request to the host and
   exits (it prints `host: {…}`), and the host ignores a second mirror request. Check
   `smartconnect-mirror --sessions`.
4. **The stream host isn't running or crashed.** Its log is in
   `$XDG_RUNTIME_DIR/smartconnectd/smartconnect-mirror.log` when the daemon started it, or in
   `~/.cache/smartconnect/mirror-host.log` when the GTK app did.

Other problems:

| Symptom | Fix |
|---|---|
| `could not attach the decoder` or a black window | The H.264 decoder is missing or broken. Check `gst-inspect-1.0 --exists vah264dec` and `avdec_h264`. A broken VA-API driver can be bypassed with `MIRROR_DECODER=avdec_h264`. |
| `offer has no H.264; leaving codecs as offered` | The phone offered no H.264 stream. The viewer can only decode H.264. Please report your phone model. |
| Mouse works, keyboard doesn't | The window isn't running through XWayland. Make sure `DISPLAY` is set where the viewer runs (on niri, install `xwayland-satellite`), and don't set `MIRROR_GL_WAYLAND`. |
| The window closes with `ICE down > 5 s` | Phone and PC lost each other on the network (Wi-Fi roaming, sleep). Start it again. |
| `start of <pkg> timed out` | The phone didn't start that app within 20 s. Try again, or open the app on the phone first. |
| An app window flickers or the stream restarts after resizing it | Window resize is opt-in and not verified on a device. Unset `SMARTCONNECT_MIRROR_RESIZE` (or drop `--resize`) and restart the host. |
| `--list-apps` prints `the phone did not answer; showing the cached list` | The phone is busy or not connected. The cached list in `~/.cache/smartconnect/apps.json` is still usable. |
| The *Apps* page shows initials instead of the phone's icons | The daemon downloads them on each connection; look for `app icons:` in `journalctl --user -u smartconnectd`. A `fetch failed` with a connection or TLS error usually means the one-shot offer expired before the download started; run `smartconnect-fex fetch-app-icons` to try again now. Nothing logged: the phone didn't send its app list (it skips it for a secondary Android user), or `SMARTCONNECT_APP_ICONS=0` is set. |
| The Noctalia **Mirror screen** button flips back while the window stays open | The panel ran the viewer, the viewer handed the request to the stream host and exited, so the panel thinks mirroring stopped. Close the window to stop mirroring. |

For GStreamer-level detail, run the viewer by hand with
`GST_DEBUG='3,webrtc*:5' smartconnect-mirror`.

## Cross Control

| Message or symptom | Fix |
|---|---|
| `compositor lacks wlr-layer-shell` | You forced `--backend layer-shell` on a compositor without it (GNOME). Leave `--backend` at `auto`, which uses the InputCapture portal there. |
| `no InputCapture portal` | The portal backend needs xdg-desktop-portal with a backend that implements InputCapture (GNOME 45 or newer; KDE Plasma 6.1 or newer). Check `busctl --user get-property org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop org.freedesktop.portal.InputCapture version`. |
| `InputCapture.CreateSession (was the prompt declined?)` | The desktop's "allow input capture" prompt was declined or closed. Start Cross Control again and allow it. |
| `the desktop accepted no pointer barrier on the … edge` | That edge touches another monitor on every screen. Pick an outer edge with `--edge`. |
| `the desktop disabled input capture` | The permission was revoked or the portal session ended. Start Cross Control again. |
| `missing zwp_pointer_constraints_v1 …` / `no pointer-constraints: relying on relative motion only` | It still works, but the PC cursor isn't locked while you control the phone. |
| `no svc26 cmd 2 reply from the phone within 4 s` | The phone didn't answer. Check that it is connected (`smartconnectd status`), and restart the daemon if you just updated it. |
| `cross control DENIED by phone` | Allow Cross Control on the phone (it may show a permission prompt), then push into the edge again. |
| `another smartconnect-crosscontrol is running and didn't stop` | An old client ignored the takeover request. Stop it: `pkill -INT -f smartconnect-crosscontrol`. |
| The pointer won't come back | Press **Ctrl+Alt+Esc**. Ctrl+C in the client's terminal also releases everything (on GNOME the desktop also ends the capture when the client exits). |
| The Noctalia button shows "off" although Cross Control works | A client started by the daemon (from the phone's tile) is running. This is a known gap in the plugin; the GTK app's Display tab does show it. |
| The pointer doesn't cross over everywhere on the edge | Only the highlighted part of the edge (where the phone touches the screen under *Phone position* on the Display tab) leads to the phone. Drag the phone to change it, or pass `--span 0:1`. |
| The pointer is too fast or too slow on the phone | Use `--gain` (default 1.6). |

## Some keys move the pointer on the phone

If typing into the mirror, an app window, the virtual phone or Cross Control drops keys such as
**u i o p, j k l, m**, or moves or clicks the phone's pointer instead, Android's **Mouse keys**
accessibility setting is on: it turns those keys of a physical keyboard into pointer controls, and
the PC's keyboard reaches the phone as one. Turn it off on the phone (search Settings for "Mouse
keys"; usually under *System → Keyboard → Physical keyboard* or *Accessibility*).

## Phone files (~/Phone)

- **`ls ~/Phone` says "Transport endpoint is not connected"**, the panel/app showed the folder as
  mounted but **Open** did nothing: a *stale* mount. The `smartconnect-files mount` helper behind it
  died (killed, crashed, or the shell that started it exited) and left the FUSE mount behind.
  `mountpoint` still calls it a mount point, and plain `stat` can keep working for up to 30 s from
  the kernel's cache, so only a listing or `stat -f` shows the error. Clear it with:

  ```sh
  fusermount3 -uz ~/Phone
  ```

  The helper and the front-ends clear it themselves: the helper before mounting, the GTK app at startup and when
  you press Open, the Noctalia panel when it opens and on Open.
- **Files say "Input/output error" for a while, then work again**: the phone went away (Wi-Fi drop,
  sleep, `smartconnectd` restarting). The mount stays up and reconnects by itself once the phone is
  connected again; nothing to do. After 10 minutes without the phone the helper unmounts.
- **Unmount says "Device or resource busy"**: a shell or file manager is still inside the folder.
  `fusermount3 -uz ~/Phone` detaches it lazily (the helper and the apps fall back to that on their own).
- The helper's output: `$XDG_RUNTIME_DIR/smartconnect-files.log` when the GTK app started it; run
  `RUST_LOG=smartconnect_files=debug smartconnect-files mount --mountpoint ~/Phone` for more.

### Sharing a folder to the phone (`serve`)

- **The status says the phone hasn't connected / the share never appears on the phone: the
  firewall.** The phone connects to the PC on TCP 2049 (or your `--port`). Open it as in
  [Installation](installation.md#firewall), **in the zone your Wi-Fi interface is in**:
  `firewall-cmd --get-active-zones` shows it (no sudo needed), for example
  `sudo firewall-cmd --zone=home --add-port=2049/tcp`. Plain `--add-port` without `--zone` changes
  the *default* zone, which often isn't the Wi-Fi's. On the phone this shows up in logcat as
  "No route to host" for port 2049. `ss -ltn 'sport = :2049'` shows the listener.
- **Where is it on the phone?** Smart Connect → **Files** tab → *Files* → your PC. It's also in
  Android's system file browser as "<PC name> <PC IP>" (any app's "open file"/attach picker, side
  menu). The PC only appears in the Files tab while the share runs.
- **The PC is in the Files tab but *All files* is empty or shows an error:** make sure the share is
  the current build (the Files tab needs Lenovo's own NFS calls, added on 2026-10-25), then go back
  and open the PC again. `RUST_LOG=smartconnect_files=debug` logs each `READCATEGORYDIR` request
  and how many entries it returned.
- **A folder or file I expect is missing:** names starting with a dot are hidden from the phone
  unless *Include hidden files* (`--include-hidden`) is on. The category views (Recents, Images, …)
  stop after 200 000 entries or 16 folder levels, and Recents only covers the last 30 days.
- **Videos have no thumbnails:** install `ffmpeg`; the share makes video thumbnails with it.
- **Deleting on the phone does nothing:** the share is read-only (`--read-only`, or *Let the phone
  delete files* off), or `gio` isn't installed. Deleted files are in your desktop Trash.
- **The phone drops the folder after a Wi-Fi hiccup or a `serve` restart, then gets it back:**
  expected. It re-handshakes and remounts within about a second. If it stays gone, open the folder
  again from the file browser.
- **"Address already in use"**: another NFS server (`nfs-server.service`) holds 2049. Pass another
  port, for example `smartconnect-files serve --port 20490`; the phone learns the port from the
  handshake.
- **The PC's address.** The phone is told `pcAddress` from the daemon status. On a PC with several
  networks, pass the right one with `--ip`.
- `RUST_LOG=smartconnect_files=debug` shows the handshake replies and TLS connections, and
  `SMARTCONNECT_FILES_TRACE=1` hex-dumps the NFS traffic.

## PC lock and Hotspot

- **Nothing locks, and the PC shows "Couldn't lock this PC":** the notification says why.
  - *GNOME Shell only has a lock screen when GDM is the display manager:* GNOME started from SDDM or
    LightDM can't lock at all (Super+L doesn't work either). Log in through GDM.
  - *No screen locker reacted to logind's Lock signal:* test with `loginctl lock-session`. If nothing
    happens, set up your idle daemon to lock on logind's lock request (swayidle's `lock` event,
    hypridle's `lock_cmd`), or give the daemon your locker: `--lock-command 'swaylock -f'`.
- **Noctalia:** the daemon calls Noctalia's own lock IPC (`qs ipc --pid <pid> call lockScreen
  lock`) when it finds Noctalia running. Noctalia doesn't report the lock to logind, so the phone's
  tile stays "locked" after you unlock, until the next lock or reconnect.
- **The phone's tile stays "locked":** your locker doesn't set logind's `LockedHint` (Noctalia, or
  a `--lock-command` locker), so the daemon can't see the unlock.
- **Hotspot (untested):** if the PC is left on the phone's hotspot or without network, remove the
  profile and reconnect your usual network:

  ```sh
  nmcli connection show | grep SmartConnect-
  nmcli connection delete id SmartConnect-XXXX
  nmcli connection up id "<your usual network>"
  ```

## Phone as display

- **The phone shows "Connecting to device…" forever.** Check the helper's output: the GTK app's
  *Display* page and the Noctalia panel show the last error, the log is
  `$XDG_RUNTIME_DIR/smartconnect-display.log` (or `$XDG_RUNTIME_DIR/smartconnectd/smartconnect-display.log`
  when the phone started the session), and a terminal run prints it directly. As with mirroring,
  only the address the phone reached the daemon on is offered for the video;
  `SMARTCONNECT_ICE_ADDRESSES` overrides that.
- **`already running (pid N)`:** only one session runs at a time. Stop it from the GTK app or the
  Noctalia panel, or start the new one with `--replace`.
- **Settings seem ignored:** command-line options override `~/.config/smartconnect-linux/display.json`.
  A bad value in the file is skipped with a warning at the top of the output. *Apply Changes* in the
  GTK app restarts a running session with the new settings; the Noctalia panel uses them for the
  next start.
- **A virtual monitor stays on after a crash.** When the process is killed hard, it can't remove the
  phone's screen. niri: `niri msg output Virtual-1 off`; Hyprland: `hyprctl output remove
  SMARTCONNECT-1`; sway: `swaymsg output HEADLESS-N unplug`. GNOME and KDE clean up by themselves.
- **Which desktop and screens does the helper see?** Run `smartconnect-display --list-outputs`. A
  wrong guess (e.g. a nested session) can be overridden with `SMARTCONNECT_DESKTOP=gnome|kde|hyprland|sway|niri`.
- **Extend finds no output (niri).** Load vkms (`sudo modprobe vkms`) and check that `niri msg outputs`
  lists a `Virtual-1` connector, or name another spare output with `--output`.
- **"… has no known way to add a screen for the phone":** your compositor isn't one of GNOME, KDE
  Plasma, Hyprland, sway or niri. Use Duplicate, or switch on a spare monitor or dummy plug and pass
  its name with `--output`.
- **KDE: "KWin did not offer zkde_screencast_unstable_v1" (or `org_kde_kwin_fake_input`).** KWin
  only gives these to programs listed in a `.desktop` file with this binary's absolute path. Run
  `packaging/install.sh` (it installs `dev.smartconnect.Display.desktop` and runs `kbuildsycoca6`),
  then log out and back in. Run the installed `smartconnect-display`, not a build-directory copy.
- **GNOME: no mouse cursor on the phone at 200 %.** GNOME doesn't draw the cursor into a scaled
  virtual monitor's stream. Touches still land in the right place.
- **Hyprland asks before sharing the screen.** With `ecosystem:enforce_permissions` on, allow
  screencopy for `smartconnect-display`.
- **The picture lags behind while touches don't.** Try a lower *Stream quality* (`--bitrate`), a
  lower *Frame rate* (`--fps 30`) and *Phone default* resolution instead of *Phone native*, and
  turn *Send PC audio to the phone* off to compare (the phone delays the picture to keep it in sync
  with the sound).
- **Text on the phone display is tiny or huge.** Adjust `--display-scale` (default 2, i.e. 200 %).
- **The picture is letterboxed.** Use `--size native` (*Phone native*) to stream at the phone's own
  panel size. A phone held upright shows the landscape picture letterboxed unless you turn on
  `--follow-rotation` (*Rotate with the phone*, Extend only, not verified on a device).
- **`no more input formats` / `not-negotiated` errors from `pipewiresrc`:** niri only offers DMA-BUF
  frames, and the capture path needs VA-API's `vapostproc` (package `gst-plugin-va`) to read them.
  GNOME, KDE, Hyprland and sway don't need it for capture.
- **The keyboard button does nothing** and you get "No on-screen keyboard found": install `wvkbd`
  (AUR), or set `SMARTCONNECT_OSK` to the command that starts your on-screen keyboard. On GNOME the
  button turns on GNOME's own screen keyboard instead (it opens when a text field is focused).
- **The on-screen keyboard is too small or covers too much:** change *On-screen keyboard height*
  (`--osk-height`, 15 to 80 % of the shared screen, default 45). It applies to wvkbd only.
- **No sound on the phone with `--audio`:** the helper says at startup whether the PC's audio could
  be captured, and sends a notification if it couldn't. Check the audio switch in the phone's display
  toolbar, which mutes the stream. `SMARTCONNECT_DISPLAY_AUDIO_SRC` replaces the capture source.
  Audio is not verified on a device yet.
- **The screencast picker keeps appearing (portal fallback):** delete
  `~/.local/share/smartconnect-linux/screencast-token`, pick the screen once, and the choice is
  remembered.
- For debugging, the last WebRTC offer and answer are written to
  `$XDG_RUNTIME_DIR/smartconnect-display.offer.sdp` and `…answer.sdp`.

## Webcam

- **No camera shows up in your app:** start the webcam first, then the app. Many apps only list
  cameras at startup, and with `exclusive_caps=1` the loopback device only shows as a camera while a
  session streams.
- **Only PipeWire-aware apps see it:** no v4l2loopback device was found, so the helper offered a
  PipeWire camera instead. Load v4l2loopback (see [Installation](installation.md#v4l2loopback-for-the-phone-as-a-webcam)).
- **`smartconnect-webcam --check`** shows the chosen output and checks the GStreamer elements.
  `--test-pattern 10` tests the output without the phone.
- **`--device /dev/videoN` is refused:** the helper only writes to virtual loopback devices, never
  to a real camera.

## Noctalia plugin

- **The panel says "smartconnectd is not running" although the daemon runs**, typically after the
  daemon was restarted: older plugin versions never reconnected, because a Quickshell `Socket` can't
  reconnect once the daemon's socket file is replaced. Update the plugin, then restart Noctalia.
  Switching the plugin off and on reloads its panels, but not the shared service that holds the
  connection.
- **Quickshell crashes or misbehaves after a system update:** if `qs` logs that it "was built against
  Qt X but the system has updated to Qt Y", rebuild or reinstall the Quickshell package (for example
  `noctalia-qs`).

## Phone-side debug logs

Release builds of Smart Connect hide most of their debug logging. You can switch it on over adb.
That needs USB debugging enabled on the phone.

```sh
adb shell setprop log.tag.MotoDesktopLog DEBUG
adb shell am force-stop com.motorola.mobiledesktop   # Smart Connect only reads the setting at start
# open Smart Connect on the phone again, then:
adb logcat -c                                        # clear old entries
adb logcat -v time > phone.log                       # reproduce the problem, then press Ctrl+C
```

The property resets when the phone reboots. Phone logs contain personal data (contact names, message
text, network names), so read and redact them before you share anything.

## Where the logs are

| What | Where |
|---|---|
| Daemon log | `journalctl --user -u smartconnectd` when it runs as a service. `RUST_LOG=debug` gives more detail. |
| Session logs (every frame exchanged with the phone, one file per connection; only with `smartconnectd --session-log`) | `~/.local/share/smartconnect-linux/sessions/<unix-ms>-<phone-ip>.jsonl` (mode 0600) |
| Helpers started by the daemon | `$XDG_RUNTIME_DIR/smartconnectd/smartconnect-mirror.log`, `…/smartconnect-crosscontrol.log`, `…/smartconnect-display.log`, `…/smartconnect-webcam.log` |
| Stream host started by the GTK app | `~/.cache/smartconnect/mirror-host.log` |
| Phone display started by the GTK app or the Noctalia plugin | `$XDG_RUNTIME_DIR/smartconnect-display.log` |
| Phone files mount started by the GTK app | `$XDG_RUNTIME_DIR/smartconnect-files.log` |
| Viewer or helpers started by hand | Their terminal (stdout and stderr) |
| Phone | See [Phone-side debug logs](#phone-side-debug-logs). |

Session logs are off unless the daemon runs with `--session-log`. They leave out keys, tokens,
passwords, clipboard text, SMS and notification contents and the wallpaper, but they still hold the
phone's ids, its app list, network details and the input you sent to the phone. See
[Security](security.md#session-logs) before you share one. They are never rotated, so delete old
ones whenever you like.

Useful one-liners:

```sh
cd ~/.local/share/smartconnect-linux/sessions
f=$(ls -t | head -1)                                                 # newest session
jq -c 'select(.frame | type == "object") | [.ts_ms, .dir, .frame.serviceId, (.frame.data.cmd // .frame.data.msgId)]' "$f" | head -50
jq -c 'select(.frame | type == "object") | select(.frame.serviceId == 15)' "$f"   # one service only
```

The live equivalent is the control socket's `watch` command
(see [Control socket API](control-socket-api.md#watch)).

## Starting over

- **Pair again:** remove the PC in the Smart Connect app, then scan a fresh QR.
- **New PC identity** (the phone sees a brand-new PC):

  ```sh
  systemctl --user stop smartconnectd
  cd ~/.local/share/smartconnect-linux
  mkdir old-identity-$(date +%Y%m%d-%H%M%S)
  mv identity.json cert.pem key.pem paired.json old-identity-*/
  systemctl --user start smartconnectd
  ```

  Then remove the old entry on the phone and pair again.
- **Remove everything:** `./packaging/uninstall.sh --purge`.
