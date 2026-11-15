# TODO / backlog

Known limitations and planned work. See [README.md](README.md#known-issues) for user-facing notes
and [docs/](docs/) for details.

## Bugs / limitations

- **Only one phone at a time.** The daemon pairs and connects a single phone. While one phone is
  connected, pairing or connecting a second fails; the first must be disconnected first, and can
  only be reconnected after the second is disconnected. Investigate supporting multiple paired
  devices (per-device identity/state is already keyed by the phone's device id in several places;
  the server likely accepts a single active connection). Decide whether to allow several concurrent
  connections or just make switching between paired phones seamless.

## Features in progress

- **File access.** Phone→PC browsing **works** (`smartconnect-files mount`, read-only FUSE mount of
  `/sdcard` over NFSv3-on-mutual-TLS; verified on device). Remaining:
  - **Write support** — the NFS write path (WRITE/CREATE/MKDIR/REMOVE/RENAME) plus the
    `nfs_dependency` bit-16 "data channel" the phone opens for write-back.
  - **PC→phone** — the phone *mounts the PC* (it actively requests this with its own `type:0`).
    ~~PC-side NFS server over a rustls listener, replying `type:1`~~ (done: `smartconnect-files
    serve`), ~~verify it with a real phone~~ (done: works after the 64-byte handle fix), ~~GTK
    switch~~ (done: *Share with the phone*). ~~Files tab, thumbnails, delete~~ (done: Lenovo procedures 22/23/27,
    delete moves to the Trash). Left: a Noctalia panel entry; sharing without a front-end
    (daemon-supervised `serve`); creating/uploading files from the phone (Lenovo `CREATEPATH` + NFS
    writes); several shared folders.
  - **Read caching** — every directory and file read is an NFS round-trip over TLS, so large folders
    are slow; cache attributes and directory listings, and read ahead for big files.
  - **Lifecycle & UX** — ~~unmount on SIGINT/SIGTERM~~ (done: SIGINT/SIGTERM/SIGHUP unmount
    cleanly, lazily if busy), ~~a stable default mountpoint `~/Phone`~~ (done), ~~GUI/plugin
    button~~ (done), ~~re-handshake on reconnect~~ (done: the mount survives a phone reconnect or a
    daemon restart, gives up after 10 min or when unpaired), ~~stale-mount detection/cleanup~~ (done:
    `statfs` probe in the helper, the GTK app and the plugin). Left: the daemon spawning/supervising
    `smartconnect-files` itself (auto-mount without a front-end), and verifying the reconnect against
    a real phone (tested end to end against this crate's own NFS-over-TLS server).
- **Clipboard images and files.** Clipboard image transfer is implemented in both directions over
  fex (service 2), but still needs real-phone verification. The Linux watcher accepts PNG, JPEG,
  WebP, GIF and BMP. Non-image file clipboard transfers remain unsupported. Improve the UI and test
  both directions on supported phones/desktops; retain the 10 MiB cap and password-manager exclusion.
  The fex client pins the advertised TLS fingerprint and pulls a zip over HTTPS after its WebSocket
  action/ack exchange (see `docs/protocol/file-exchange.md` §3).

- **Incoming image metadata validation.** Reject malformed or incomplete file entries before PULL:
  require a regular-file attribute and a known, non-zero size within the phone's 10 MiB limit.
  Regression tests cover missing fields and empty images.

- **Send and receive single files (fex).** The fex client can pull an offer (`smartconnect-fex
  pull`), but there's no way to send a file to the phone or to accept one the phone sends, and no UI.

## App streaming

- ~~**App icons in the app list.**~~ Done: the daemon downloads the service 17 icon transfer over fex
  on every connection into `~/.cache/smartconnect-linux/app-icons/` (`smartconnect-fex
  fetch-app-icons` by hand); the GTK app reads them. Possible follow-ups: prune icons of uninstalled
  apps; skip the download when the app list is unchanged (it would then also skip the phone's
  recent-apps `cmd 10`, which only follows a completed pull).
- **Hand off an app from the phone's Recents.** Let the phone push an already-open app (from its
  recent-apps list) into an app-streaming window on the PC, not just launch from the PC side.

## Audio

- **Two-way audio passthrough + mutual volume control + audio-out selection.** Reverse-engineered —
  full spec in the author's private research notes. There are FOUR audio paths:
  - **Display-session audio (svc 202)** — PC→phone Opus in a phone-as-display session. Already done.
  - **Bluetooth A2DP sink** — the everyday "phone media plays on the PC speakers," over classic BT
    A2DP (no WS). On Linux this is nearly free: BlueZ A2DP-sink profile + PipeWire `module-bluez5`;
    the phone routes to it via its "Desktop" audio output. **Lowest-effort — build this first.**
  - **R4 Audio Stream = service 21** — bidirectional PC↔phone audio independent of a display session
    (Opus over WebRTC, or SRTP-over-superlink; `protocol:int` selects). Beans fully recovered
    (`R4AudioData.PeerStart/SetParameters/WebRTC/StateChange/StartSuccessful`), but the numeric cmd
    values are obfuscated — **needs one live capture (or Ghidra) to get the cmd numbers** before
    building. This is the general two-way passthrough.
  - **Desktop/RDP audio (svc 6/7)** — noted, not reverse-engineered.
  - **Volume (both ways)** rides Android `MediaRouter` (not the audio beans): phone→PC →
    `IAudioEndpointVolume` (WASAPI) / on Linux a PipeWire sink volume; PC→phone → `AudioManager`
    `setStreamVolume`/`adjustStreamVolume` on `STREAM_MUSIC`. The exact wire message carrying the
    volume *level* (min/max/step) is still unknown (needs a capture).
  - **Audio-out selection**: the phone's `AudioOutputRouteControl` (speaker / earpiece / BT headset /
    **Desktop = the PC via A2DP**); route-type enum unknown.
  - Build order: (1) A2DP sink (phone→PC media, free-ish); (2) capture svc-21 cmd numbers, then
    bidirectional WebRTC/SRTP audio; (3) MediaRoute-style variable volume both ways + CLI/GUI.

## Connectivity

- **Connect over the phone's hotspot (no shared Wi-Fi).** Today pairing/connect need the phone and PC
  on the same Wi-Fi (mDNS discovery on a shared subnet). Investigate the PC joining the *phone's*
  hotspot instead, so they talk directly with no router. On the phone's AP the two are on one subnet,
  so discovery/connect should work in principle — open questions: whether mDNS/avahi and the Wi-Fi
  dial-back behave over the phone's AP interface and the phone's own gateway IP, and whether the
  Smart Connect app advertises/accepts the link in that mode. Note this is the *reverse* of the
  existing Hotspot tile (service 13), where the PC starts a SoftAP the phone joins; here the phone is
  the AP and the PC the client. (The BLE wake + reconnect paths may also need the phone's AP address.)

## Smaller items

- **Background reconnect, still to verify on a device:** the early-retry triggers (phone back on
  Wi-Fi, resume from suspend, network change, Bluetooth switched on), the 70 s dead-link drop, and a
  wake with the phone's screen off. Consider also a Lenovo-account (cloud) channel for when the PC's
  Bluetooth is off; without it a backgrounded phone can't be reached then. Possible refinement: skip
  the periodic Bluetooth scan while a Bluetooth audio device is streaming, if scans turn out to
  disturb earbuds.

- Verify the phone-started feature paths that are currently implemented-but-unverified (notification
  background actions, phone-started app streaming / cross control / display, mute UI, SMS).
