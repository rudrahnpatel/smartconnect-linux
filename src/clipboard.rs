//! Clipboard service (serviceId 9) and the Linux clipboard behind it.
//!
//! Payloads are `ClipboardData{msgId, data}` — the clipboard message type is `data.msgId`, not
//! `cmd`. The phone opens with NEGO once the PC advertises `serviceVersions["9"] >= 2`, and only
//! pushes its copies after the PC's CONFIRM. Text travels inline in UPDATE_NOTIFY in both
//! directions; neither side acks or PULLs text. Spec: `evidence-2026-10-20/clipboard-spec.md`.
//! Never send PULL for text, msgIds 13–17, or `shareHubMountInfo` — the phone mishandles them.
//!
//! **Images, phone → PC.** An image clip arrives as an UPDATE_NOTIFY with a one-entry `filelist`
//! (no `text`). We answer PULL (msgId 4) with its `clipId`; the phone then offers the bytes over
//! "fex": a service 2 `FileExSendData` with `serviceId` 9, a `port` and a `fingerprint`. We pull that
//! with `smartconnect_fex::client::pull` (a zip with the one image) and hand the image to
//! `wl-copy --type image/…` ([`on_file_ex`]). Other file clips are only logged.
//!
//! **Images, PC → phone.** The watchers also report image copies (`image/png`, …, up to the phone's
//! 10 MB). We announce one as a one-file UPDATE_NOTIFY with an **empty `path`** (a non-empty path
//! would make the phone try a Share Hub mount instead), the phone answers PULL, and we offer the
//! image over fex: a zip served by `smartconnect_fex::server` on the daemon's own port
//! (`crate::fex_host`), announced with a service 2 `cmd 1` carrying that port and the daemon
//! certificate's fingerprint ([`LocalClipboard::offer_current_image`]).
//! See `docs/protocol/clipboard.md` and `docs/protocol/file-exchange.md`.

use std::net::IpAddr;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

pub const SERVICE_CLIPBOARD: i64 = 9;

const CLIP_NEGO: i64 = 1;
const CLIP_CONFIRM: i64 = 2;
const CLIP_UPDATE_NOTIFY: i64 = 3;
const CLIP_PULL: i64 = 4;
const CLIP_PULL_RESPONSE: i64 = 5;
const CLIP_CLOSE: i64 = 7;
/// fex (file exchange) service id; the phone offers a pulled clip's bytes there.
pub const SERVICE_FILE_EX: i64 = 2;
/// The phone drops image clips larger than this, so a bigger offer isn't a clipboard image.
const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
/// Version the CONFIRM agrees to; the phone only checks that it is non-zero.
const CLIP_VERSION: i64 = 1;
/// The phone truncates longer text, so don't send more.
const MAX_TEXT_CHARS: usize = 100_000;

/// fex `cmd` for "download from the PC" (the phone's `FileExReceiveData.cmd`).
const FEX_CMD_DOWNLOAD_FROM_PC: i64 = 1;

/// A copy made on Linux, numbered once so every session sends the same `clipId`. Either text, or
/// (with `image` set and `text` empty) an image.
#[derive(Clone, Debug)]
pub struct LocalClip {
    pub id: i64,
    pub text: String,
    pub image: Option<Arc<ClipImage>>,
}

/// An image copied on Linux, as offered to the phone.
#[derive(Debug)]
pub struct ClipImage {
    /// The file name the phone stores it under, e.g. `clipboard_123.png`.
    pub name: String,
    pub mime: String,
    pub data: Vec<u8>,
}

/// What a watcher saw on the Linux clipboard.
#[derive(Debug)]
enum Seen {
    Text(String),
    Image { mime: String, data: Vec<u8> },
}

/// Image types we pick up from the Linux clipboard, best first (all are images the phone accepts).
const IMAGE_TYPES: [&str; 5] = ["image/png", "image/jpeg", "image/webp", "image/gif", "image/bmp"];

/// The file extension for an image MIME type.
fn image_ext(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/bmp" => "bmp",
        _ => "png",
    }
}

/// Per-connection state: after a CLOSE the phone ignores us until it negotiates again.
#[derive(Default)]
pub struct Session {
    negotiated: bool,
}

impl Session {
    /// Handles one clipboard message from the phone and returns the reply, if it needs one, as
    /// `(service id, data)`: usually service 9, but a PULL of our image is answered on fex (2).
    pub fn handle(&mut self, data: &Value, local: &LocalClipboard) -> Option<(i64, Value)> {
        self.handle_clipboard(data, local).map(|d| (SERVICE_CLIPBOARD, d)).or_else(|| {
            (data.get("msgId").and_then(Value::as_i64) == Some(CLIP_PULL))
                .then(|| data.pointer("/data/clipId").and_then(Value::as_i64))
                .flatten()
                .and_then(|clip_id| local.offer_current_image(clip_id))
                .map(|d| (SERVICE_FILE_EX, d))
        })
    }

    fn handle_clipboard(&mut self, data: &Value, local: &LocalClipboard) -> Option<Value> {
        match data.get("msgId").and_then(Value::as_i64)? {
            CLIP_NEGO => {
                let version = data.pointer("/data/version").and_then(Value::as_i64).unwrap_or(CLIP_VERSION);
                let version = version.min(CLIP_VERSION);
                self.negotiated = version > 0;
                Some(json!({ "msgId": CLIP_CONFIRM, "data": { "version": version } }))
            }
            CLIP_UPDATE_NOTIFY => {
                let inner = data.pointer("/data").cloned().unwrap_or(Value::Null);
                if let Some(text) = inner.get("text").and_then(Value::as_str) {
                    local.set_from_phone(text);
                    return None;
                }
                // An image clip: PULL it, and the phone offers the bytes over fex (service 2).
                // Only ever PULL a clip with a file list: pulling a text clip crashes the phone's
                // handler. Other file clips are logged only.
                match image_clip_id(&inner) {
                    Some(clip_id) => {
                        info!(clip_id, "clipboard: phone copied an image; pulling it");
                        Some(json!({ "msgId": CLIP_PULL, "data": { "clipId": clip_id } }))
                    }
                    None => {
                        describe_file_clip(&inner);
                        None
                    }
                }
            }
            CLIP_PULL_RESPONSE => {
                // Sent only on errors: 1 = invalid request, 2 = the clip is no longer current.
                let error = data.pointer("/data/error").and_then(Value::as_i64);
                warn!(?error, "clipboard: the phone refused our pull");
                None
            }
            CLIP_CLOSE => {
                self.negotiated = false;
                None
            }
            _ => None,
        }
    }

    /// The UPDATE_NOTIFY that carries a Linux copy to the phone, once the phone is listening. An
    /// image goes out as a one-file list with an empty `path` and `storage`: the phone then PULLs
    /// it (over fex) instead of trying to mount the path over Share Hub.
    pub fn outgoing(&self, clip: &LocalClip) -> Option<Value> {
        self.negotiated.then(|| match &clip.image {
            Some(img) => json!({ "msgId": CLIP_UPDATE_NOTIFY, "data": {
                "clipId": clip.id,
                "filelist": [ { "attr": 128, "name": img.name, "path": "", "size": img.data.len() } ],
                "storage": "",
            } }),
            None => {
                let text: String = clip.text.chars().take(MAX_TEXT_CHARS).collect();
                json!({ "msgId": CLIP_UPDATE_NOTIFY, "data": { "clipId": clip.id, "text": text } })
            }
        })
    }
}

/// The Linux clipboard: written with `wl-copy`; followed with `wl-paste --watch` where the
/// compositor has the data-control protocol (KDE, Hyprland, sway, niri, …), else (GNOME) through
/// Xwayland, which GNOME keeps in step with the Wayland clipboard.
pub struct LocalClipboard {
    /// Last text that crossed the link in either direction; a watcher event with the same text
    /// is our own `wl-copy` (or a clipboard manager re-owning it) and must not go back out.
    last_synced: Mutex<Option<String>>,
    /// SHA-256 of the last image that crossed the link, for the same echo check on images.
    last_image: Mutex<Option<[u8; 32]>>,
    /// The image copied on Linux most recently, by `clipId`, until the phone pulls it.
    current_image: Mutex<Option<(i64, Arc<ClipImage>)>>,
    /// Our fex offers (served on the daemon's port by `crate::fex_host`).
    fex: smartconnect_fex::server::Registry,
    /// Where the phone fetches our offers: the daemon's TLS port and its certificate's fingerprint
    /// (lowercase hex SHA-256), set once at startup.
    fex_endpoint: OnceLock<(u16, String)>,
    changes: broadcast::Sender<LocalClip>,
    next_id: AtomicI64,
}

impl LocalClipboard {
    pub fn new() -> Arc<Self> {
        // The phone keeps the clipId as a Java int; seed from the clock so ids keep rising across
        // daemon restarts without overflowing 31 bits.
        let seed = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64 & 0x3fff_ffff).unwrap_or(1);
        Arc::new(Self {
            last_synced: Mutex::new(None),
            last_image: Mutex::new(None),
            current_image: Mutex::new(None),
            fex: smartconnect_fex::server::Registry::default(),
            fex_endpoint: OnceLock::new(),
            changes: broadcast::channel(16).0,
            next_id: AtomicI64::new(seed),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LocalClip> {
        self.changes.subscribe()
    }

    /// Our fex offers, for `crate::fex_host` to serve.
    pub fn fex(&self) -> &smartconnect_fex::server::Registry {
        &self.fex
    }

    /// Where the phone fetches our offers: the daemon's TLS `port` and its certificate's
    /// `fingerprint` (lowercase hex SHA-256). Call once at startup.
    pub fn set_fex_endpoint(&self, port: u16, fingerprint: String) {
        let _ = self.fex_endpoint.set((port, fingerprint));
    }

    /// The phone PULLed `clip_id`: if that is our current image, offer it over fex and return the
    /// service 2 message that tells the phone where to fetch it.
    fn offer_current_image(&self, clip_id: i64) -> Option<Value> {
        let image = match &*self.current_image.lock().unwrap() {
            Some((id, img)) if *id == clip_id => img.clone(),
            _ => {
                warn!(clip_id, "clipboard: the phone pulled a clip that isn't our current image");
                return None;
            }
        };
        let Some((port, fingerprint)) = self.fex_endpoint.get() else {
            warn!("clipboard: no fex endpoint configured; can't send the image");
            return None;
        };
        let file = smartconnect_fex::server::OutFile { name: image.name.clone(), mime_type: image.mime.clone(), data: image.data.clone() };
        let offer = self.fex.offer(&[file], "", "");
        info!(clip_id, bytes = image.data.len(), "clipboard: the phone pulled our image; offering it over fex");
        Some(json!({
            "ver": "1.0",
            "cmd": FEX_CMD_DOWNLOAD_FROM_PC,
            "serviceId": SERVICE_CLIPBOARD,
            "requestId": offer.task_id,
            "port": port,
            "fingerprint": fingerprint,
            "token": offer.token,
            "fexId": 0,
            "autoDownload": false,
        }))
    }

    /// Records an image copied on Linux and hands it to the sessions, unless it is the image we
    /// just put there ourselves or is over the phone's limit.
    fn local_image_changed(&self, mime: String, data: Vec<u8>) {
        if data.is_empty() || data.len() > MAX_IMAGE_BYTES {
            info!(bytes = data.len(), "clipboard: not sending an image over the phone's 10 MB limit");
            return;
        }
        {
            let hash = digest(&data);
            let mut last = self.last_image.lock().unwrap();
            if *last == Some(hash) {
                return;
            }
            *last = Some(hash);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let name = format!("clipboard_{id}.{}", image_ext(&mime));
        let image = Arc::new(ClipImage { name, mime, data });
        *self.current_image.lock().unwrap() = Some((id, image.clone()));
        info!(clip_id = id, bytes = image.data.len(), mime = %image.mime, "clipboard: Linux → phone (image)");
        let _ = self.changes.send(LocalClip { id, text: String::new(), image: Some(image) });
    }

    fn seen(&self, seen: Seen) {
        match seen {
            Seen::Text(text) => self.local_changed(text),
            Seen::Image { mime, data } => self.local_image_changed(mime, data),
        }
    }

    /// Puts an image from the phone on the Linux clipboard.
    async fn set_image_from_phone(&self, name: &str, bytes: Vec<u8>) {
        let mime = image_mime(name);
        *self.last_image.lock().unwrap() = Some(digest(&bytes));
        match wl_copy_bytes(&bytes, Some(mime)).await {
            Ok(()) => info!(bytes = bytes.len(), mime, "clipboard: phone → Linux (image)"),
            Err(e) => warn!("clipboard: wl-copy of the image failed: {e:#}"),
        }
    }

    fn set_from_phone(&self, text: &str) {
        *self.last_synced.lock().unwrap() = Some(text.to_owned());
        let text = text.to_owned();
        tokio::spawn(async move {
            match wl_copy(&text).await {
                Ok(()) => info!(chars = text.chars().count(), "clipboard: phone → Linux"),
                Err(e) => warn!("clipboard: wl-copy failed: {e:#}"),
            }
        });
    }

    /// Records a Linux copy and hands it to the sessions, unless it is the text we just synced.
    fn local_changed(&self, text: String) {
        {
            let mut last = self.last_synced.lock().unwrap();
            if last.as_deref() == Some(text.as_str()) {
                return;
            }
            *last = Some(text.clone());
        }
        let clip = LocalClip { id: self.next_id.fetch_add(1, Ordering::Relaxed), text, image: None };
        info!(clip_id = clip.id, chars = clip.text.chars().count(), "clipboard: Linux → phone");
        let _ = self.changes.send(clip); // no session connected = nothing to do
    }

    /// Follows the Wayland clipboard with `wl-paste --watch`, which runs the helper on every
    /// selection change with the content on stdin. The helper frames each event as
    /// `<CLIPBOARD_STATE>\n<text>\0` on our pipe. Password-manager entries arrive with state
    /// `sensitive` and are never forwarded.
    /// When the compositor refuses watch mode (no data-control, e.g. GNOME), it falls back to
    /// `watch_x11`.
    pub async fn watch(self: Arc<Self>) -> Result<()> {
        let images = tokio::spawn(self.clone().watch_wayland_images());
        let result = match self.clone().watch_wayland().await {
            Err(e) if format!("{e:#}").contains("data-control") => {
                images.abort();
                info!("clipboard: the compositor has no data-control protocol; following the clipboard through Xwayland");
                self.watch_x11().await
            }
            r => r,
        };
        images.abort();
        result
    }

    /// Follows image copies on Wayland: on every selection change, list the offered types, and if
    /// it is an image without text (text is the other watcher's), read the best image type.
    async fn watch_wayland_images(self: Arc<Self>) -> Result<()> {
        let mut child = Command::new("wl-paste")
            .args(["--watch", "sh", "-c", r#"printf '%s\n' "$CLIPBOARD_STATE"; wl-paste --list-types 2>/dev/null; printf '\0'"#])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("starting wl-paste --watch for images")?;
        let mut events = BufReader::new(child.stdout.take().context("wl-paste stdout")?);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            if events.read_until(b'\0', &mut buf).await? == 0 {
                anyhow::bail!("wl-paste --watch (images) exited: {:?}", child.wait().await?);
            }
            let event = String::from_utf8_lossy(&buf);
            let mut lines = event.trim_end_matches('\0').lines();
            let state = lines.next().unwrap_or_default();
            let types: Vec<&str> = lines.map(str::trim).collect();
            if state == "sensitive" || types.iter().any(|t| t.starts_with("text/") || *t == "UTF8_STRING" || *t == "STRING") {
                continue;
            }
            let Some(mime) = IMAGE_TYPES.iter().find(|m| types.contains(m)) else { continue };
            match Command::new("wl-paste").args(["--no-newline", "--type", mime]).stdin(Stdio::null()).output().await {
                Ok(out) if out.status.success() => self.local_image_changed((*mime).to_owned(), out.stdout),
                Ok(out) => debug!("clipboard: wl-paste --type {mime} exited with {}", out.status),
                Err(e) => warn!("clipboard: wl-paste --type {mime}: {e}"),
            }
        }
    }

    async fn watch_wayland(self: Arc<Self>) -> Result<()> {
        let mut child = Command::new("wl-paste")
            .args(["--type", "text", "--watch", "sh", "-c", r#"printf '%s\n' "$CLIPBOARD_STATE"; cat; printf '\0'"#])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("starting wl-paste --watch (is wl-clipboard installed?)")?;
        let mut events = BufReader::new(child.stdout.take().context("wl-paste stdout")?);
        let mut stderr = child.stderr.take().context("wl-paste stderr")?;
        let mut buf = Vec::new();
        loop {
            buf.clear();
            if events.read_until(b'\0', &mut buf).await? == 0 {
                let mut why = String::new();
                let _ = tokio::io::AsyncReadExt::read_to_string(&mut stderr, &mut why).await;
                anyhow::bail!("wl-paste --watch exited ({:?}): {}", child.wait().await?, why.trim());
            }
            if buf.last() == Some(&b'\0') {
                buf.pop();
            }
            let event = String::from_utf8_lossy(&buf);
            let (state, text) = event.split_once('\n').unwrap_or((event.as_ref(), ""));
            match state {
                "sensitive" => debug!("clipboard: skipping a sensitive (password manager) copy"),
                "nil" | "clear" => {}
                _ if text.is_empty() => {}
                _ => self.local_changed(text.to_owned()),
            }
        }
    }
}

impl LocalClipboard {
    /// Follows the X11 CLIPBOARD selection (XFixes owner-change events) in a thread. GNOME mirrors
    /// every Wayland copy into Xwayland, and an X11 client may read the selection without focus,
    /// so this sees copies from Wayland and X11 apps alike. Starting the connection also starts an
    /// on-demand Xwayland.
    async fn watch_x11(self: Arc<Self>) -> Result<()> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Seen>();
        let thread = std::thread::Builder::new()
            .name("clipboard-x11".into())
            .spawn(move || x11::watch(&tx))
            .context("starting the X11 clipboard thread")?;
        while let Some(seen) = rx.recv().await {
            self.seen(seen);
        }
        match thread.join() {
            Ok(r) => r.context("X11 clipboard (is Xwayland available? DISPLAY must be set)"),
            Err(_) => anyhow::bail!("the X11 clipboard thread panicked"),
        }
    }
}

mod x11 {
    use anyhow::{Context, Result};
    use x11rb::connection::Connection;
    use x11rb::protocol::Event;
    use x11rb::protocol::xfixes::{self, ConnectionExt as _};
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, Property, WindowClass};

    use super::{IMAGE_TYPES, MAX_IMAGE_BYTES, Seen};

    /// Longest text we read; clipboard text sync stops well before this anyway.
    const MAX_TEXT_BYTES: usize = 4 << 20;

    /// An image arriving in pieces (INCR): its type and the bytes so far.
    struct Incr {
        mime: String,
        data: Vec<u8>,
    }

    pub fn watch(tx: &tokio::sync::mpsc::UnboundedSender<Seen>) -> Result<()> {
        let (conn, screen) = x11rb::connect(None).context("connecting to the X server")?;
        let root = conn.setup().roots[screen].root;
        let win = conn.generate_id()?;
        // PropertyChange events carry the pieces of a large (INCR) image.
        let aux = CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE);
        conn.create_window(0, win, root, 0, 0, 1, 1, 0, WindowClass::INPUT_ONLY, 0, &aux)?;
        conn.xfixes_query_version(5, 0)?.reply().context("XFixes")?;
        let atom = |name: &str| -> Result<u32> { Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom) };
        let (clipboard, targets, utf8, incr) = (atom("CLIPBOARD")?, atom("TARGETS")?, atom("UTF8_STRING")?, atom("INCR")?);
        let images: Vec<(u32, &str)> = IMAGE_TYPES.iter().map(|m| Ok((atom(m)?, *m))).collect::<Result<_>>()?;
        // Set by KeePassXC, KDE and others on secrets; such copies never leave the PC.
        let secret = atom("x-kde-passwordManagerHint")?;
        let prop = atom("SMARTCONNECT_CLIP")?;
        conn.xfixes_select_selection_input(win, clipboard, xfixes::SelectionEventMask::SET_SELECTION_OWNER)?;
        conn.flush()?;
        let mut pending: Option<Incr> = None;
        // Reads the whole property (in steps, so it isn't capped at one request) and deletes it.
        let read_all = |max: usize| -> Result<(u32, Vec<u8>)> {
            let mut data = Vec::new();
            let mut type_ = x11rb::NONE;
            loop {
                let reply = conn.get_property(false, win, prop, AtomEnum::ANY, (data.len() / 4) as u32, 1 << 20)?.reply()?;
                type_ = if type_ == x11rb::NONE { reply.type_ } else { type_ };
                data.extend_from_slice(&reply.value);
                if reply.bytes_after == 0 || data.len() > max {
                    break;
                }
            }
            conn.delete_property(win, prop)?;
            conn.flush()?;
            Ok((type_, data))
        };
        loop {
            match conn.wait_for_event()? {
                // A new owner: ask what it offers first.
                Event::XfixesSelectionNotify(e) if e.owner != x11rb::NONE => {
                    pending = None;
                    conn.convert_selection(win, clipboard, targets, prop, e.selection_timestamp)?;
                    conn.flush()?;
                }
                Event::SelectionNotify(e) if e.property != x11rb::NONE => {
                    if e.target == targets {
                        let (_, raw) = read_all(64 * 1024)?;
                        let offered: Vec<u32> = raw.as_chunks::<4>().0.iter().map(|c| u32::from_ne_bytes(*c)).collect();
                        if offered.contains(&secret) {
                            tracing::debug!("clipboard: skipping a sensitive (password manager) copy");
                        } else if offered.contains(&utf8) {
                            conn.convert_selection(win, clipboard, utf8, prop, e.time)?;
                            conn.flush()?;
                        } else if let Some((image, _)) = images.iter().find(|(a, _)| offered.contains(a)) {
                            conn.convert_selection(win, clipboard, *image, prop, e.time)?;
                            conn.flush()?;
                        }
                    } else if e.target == utf8 {
                        let (type_, data) = read_all(MAX_TEXT_BYTES)?;
                        if type_ == incr || data.len() > MAX_TEXT_BYTES {
                            continue; // too big for a clipboard sync
                        }
                        let text = String::from_utf8_lossy(&data).into_owned();
                        if !text.is_empty() && tx.send(Seen::Text(text)).is_err() {
                            return Ok(());
                        }
                    } else if let Some((_, mime)) = images.iter().find(|(a, _)| *a == e.target) {
                        let (type_, data) = read_all(MAX_IMAGE_BYTES)?;
                        if type_ == incr {
                            // Deleting the INCR property (done) asks the owner for the first piece.
                            pending = Some(Incr { mime: (*mime).to_owned(), data: Vec::new() });
                        } else if data.len() <= MAX_IMAGE_BYTES && tx.send(Seen::Image { mime: (*mime).to_owned(), data }).is_err() {
                            return Ok(());
                        }
                    }
                }
                // The next piece of an INCR image; an empty piece ends it.
                Event::PropertyNotify(e) if e.atom == prop && e.state == Property::NEW_VALUE && pending.is_some() => {
                    let (_, piece) = read_all(MAX_IMAGE_BYTES)?;
                    let done = piece.is_empty();
                    let incr_img = pending.as_mut().expect("checked above");
                    incr_img.data.extend_from_slice(&piece);
                    if incr_img.data.len() > MAX_IMAGE_BYTES {
                        tracing::info!("clipboard: not sending an image over the phone's 10 MB limit");
                        pending = None;
                    } else if done {
                        let img = pending.take().expect("checked above");
                        if tx.send(Seen::Image { mime: img.mime, data: img.data }).is_err() {
                            return Ok(());
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// The `clipId` of an image clip worth pulling: exactly one regular file (`attr` 128), an image by
/// name, within the phone's own 10 MB limit.
fn image_clip_id(data: &Value) -> Option<i64> {
    let list = data.get("filelist").or_else(|| data.get("fileList"))?.as_array()?;
    let [file] = list.as_slice() else { return None };
    let name = file.get("name").and_then(Value::as_str)?;
    // Incomplete or malformed metadata must never turn an unknown-size transfer into an image.
    let size = usize::try_from(file.get("size").and_then(Value::as_u64)?).ok()?;
    let regular = file.get("attr").and_then(Value::as_i64) == Some(128);
    (regular && is_image_name(name) && (1..=MAX_IMAGE_BYTES).contains(&size))
        .then(|| data.get("clipId").and_then(Value::as_i64))
        .flatten()
        .filter(|id| *id > 0)
}

/// The clipboard MIME type for an image file name.
fn image_mime(name: &str) -> &'static str {
    let lower = name.to_ascii_lowercase();
    match lower.rsplit_once('.').map(|(_, e)| e) {
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        Some("heic" | "heif") => "image/heic",
        Some("tif" | "tiff") => "image/tiff",
        _ => "image/jpeg",
    }
}

/// A service 2 (fex) frame from the phone at `phone`. A `FileExSendData` for the clipboard
/// (`serviceId` 9) is the bytes of the image we pulled: fetch it and put it on the clipboard.
pub fn on_file_ex(phone: IpAddr, data: &Value, local: Arc<LocalClipboard>) {
    if data.get("serviceId").and_then(Value::as_i64) != Some(SERVICE_CLIPBOARD) {
        return;
    }
    let port = data.get("port").and_then(Value::as_u64).and_then(|p| u16::try_from(p).ok()).filter(|p| *p > 0);
    let fingerprint = data.get("fingerprint").and_then(Value::as_str).filter(|f| !f.is_empty());
    let (Some(port), Some(fingerprint)) = (port, fingerprint) else {
        warn!("clipboard: the phone's image offer has no port or fingerprint");
        return;
    };
    let offer = smartconnect_fex::client::Offer {
        host: phone,
        port,
        fingerprint: fingerprint.to_owned(),
        token: data.get("token").and_then(Value::as_str).unwrap_or_default().to_owned(),
    };
    tokio::spawn(async move {
        match fetch_image(&offer).await {
            Ok((name, bytes)) => local.set_image_from_phone(&name, bytes).await,
            Err(e) => warn!("clipboard: fetching the phone's image failed: {e:#}"),
        }
    });
}

/// Pulls a clipboard transfer and returns its one image (name and bytes).
async fn fetch_image(offer: &smartconnect_fex::client::Offer) -> Result<(String, Vec<u8>)> {
    // The zip adds a little to the image; the phone caps images at 10 MB.
    let pulled = smartconnect_fex::client::pull(offer, MAX_IMAGE_BYTES + 1024 * 1024).await?;
    let entries = smartconnect_fex::zip::read_all(&pulled.archive)?;
    let entry = entries.into_iter().find(|e| is_image_name(&e.name)).context("the transfer holds no image")?;
    Ok((entry.name, entry.data))
}

/// Logs what an image/file `UPDATE_NOTIFY` from the phone contains, without fetching anything.
/// `data` is the inner `{clipId, filelist, storage}` object. We cannot receive the bytes yet
/// (that needs a fex client; see docs/protocol/file-exchange.md), so this is informational only.
fn describe_file_clip(data: &Value) {
    // Casing is seen as both `filelist` and `fileList` on the wire.
    let list = data
        .get("filelist")
        .or_else(|| data.get("fileList"))
        .and_then(Value::as_array);
    let Some(list) = list else {
        info!("phone sent a non-text clip with no file list; ignoring");
        return;
    };
    let count = list.len();
    let total: i64 = list
        .iter()
        .filter_map(|f| f.get("size").and_then(Value::as_i64))
        .sum();
    // attr: 16 = directory, 128 = normal file.
    let dirs = list
        .iter()
        .filter(|f| f.get("attr").and_then(Value::as_i64) == Some(16))
        .count();
    let kind = if count == 1 && dirs == 0 {
        match list[0].get("name").and_then(Value::as_str) {
            Some(name) if is_image_name(name) => "an image",
            _ => "a file",
        }
    } else if dirs == count {
        "folders"
    } else {
        "files"
    };
    info!(
        files = count,
        bytes = total,
        "phone copied {kind} to its clipboard; file/image clipboard is not supported yet \
         (needs the fex transport, see docs/protocol/file-exchange.md)"
    );
}

/// Whether a file name looks like an image, by extension.
fn is_image_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [
        ".jpg", ".jpeg", ".png", ".gif", ".webp", ".bmp", ".heic", ".heif", ".tif", ".tiff",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext))
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).into()
}

async fn wl_copy(text: &str) -> Result<()> {
    wl_copy_bytes(text.as_bytes(), None).await
}

/// `wl-copy` (with `--type MIME` when given) fed `bytes` on stdin.
async fn wl_copy_bytes(bytes: &[u8], mime: Option<&str>) -> Result<()> {
    let mut cmd = Command::new("wl-copy");
    if let Some(mime) = mime {
        cmd.args(["--type", mime]);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("starting wl-copy")?;
    let mut stdin = child.stdin.take().context("wl-copy stdin")?;
    stdin.write_all(bytes).await?;
    drop(stdin);
    // wl-copy forks a child that serves the selection; this waits only for the parent.
    let status = child.wait().await?;
    anyhow::ensure!(status.success(), "wl-copy exited with {status}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live (any desktop with Xwayland, e.g. GNOME): a Wayland copy shows up through Xwayland.
    /// Replaces the clipboard. `cargo test --release -p smartconnect-linux x11_clipboard -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn x11_clipboard_sees_a_wayland_copy() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || x11::watch(&tx));
        std::thread::sleep(std::time::Duration::from_millis(500));
        let want = format!("smartconnect clipboard test {}", std::process::id());
        assert!(std::process::Command::new("wl-copy").arg(&want).status().unwrap().success());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match rx.try_recv() {
                Ok(Seen::Text(text)) if text == want => break,
                Ok(other) => eprintln!("saw {other:?}"),
                Err(_) if std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(50)),
                Err(e) => panic!("no X11 clipboard event for the copy: {e}"),
            }
        }
        eprintln!("seen through Xwayland: {want:?}");
    }

    #[test]
    fn nego_is_confirmed_and_enables_sending() {
        let local = LocalClipboard::new();
        let mut s = Session::default();
        let clip = LocalClip { id: 5, text: "hi".into(), image: None };
        assert!(s.outgoing(&clip).is_none(), "nothing goes out before the phone negotiates");

        let confirm = s.handle(&json!({ "msgId": 1, "data": { "version": 1 } }), &local).unwrap();
        assert_eq!(confirm, (9, json!({ "msgId": 2, "data": { "version": 1 } })));
        assert_eq!(s.outgoing(&clip).unwrap(), json!({ "msgId": 3, "data": { "clipId": 5, "text": "hi" } }));

        assert!(s.handle(&json!({ "msgId": 7, "data": { "code": 0, "reason": "" } }), &local).is_none());
        assert!(s.outgoing(&clip).is_none(), "CLOSE stops sending until the next NEGO");
    }

    #[test]
    fn own_copies_are_not_echoed() {
        let local = LocalClipboard::new();
        let mut rx = local.subscribe();
        *local.last_synced.lock().unwrap() = Some("from phone".into());
        local.local_changed("from phone".into());
        assert!(rx.try_recv().is_err());
        local.local_changed("typed on linux".into());
        assert_eq!(rx.try_recv().unwrap().text, "typed on linux");
    }

    #[test]
    fn a_linux_image_is_announced_then_offered_on_pull() {
        let local = LocalClipboard::new();
        local.set_fex_endpoint(52020, "ab".repeat(32));
        let mut rx = local.subscribe();
        local.local_image_changed("image/png".into(), b"\x89PNG fake".to_vec());
        let clip = rx.try_recv().unwrap();
        let img = clip.image.clone().expect("an image clip");
        assert_eq!(img.name, format!("clipboard_{}.png", clip.id));

        let mut s = Session { negotiated: true };
        let out = s.outgoing(&clip).unwrap();
        assert_eq!(out["data"]["filelist"][0], json!({ "attr": 128, "name": img.name, "path": "", "size": 9 }));
        assert_eq!(out["data"]["storage"], "");
        assert!(out["data"].get("text").is_none());

        // The phone pulls it: we answer on fex (service 2) with our endpoint and a task.
        let pull = json!({ "msgId": 4, "data": { "clipId": clip.id } });
        let (svc, fex) = s.handle(&pull, &local).expect("an offer");
        assert_eq!(svc, 2);
        assert_eq!((fex["cmd"].as_i64(), fex["serviceId"].as_i64(), fex["port"].as_i64()), (Some(1), Some(9), Some(52020)));
        assert_eq!(fex["fingerprint"], "ab".repeat(32));
        assert!(local.fex().has_offers());
        // A pull of some other clip gets nothing.
        assert!(s.handle(&json!({ "msgId": 4, "data": { "clipId": 1 } }), &local).is_none());
        // The same image again (e.g. our own wl-copy echo) isn't re-announced.
        local.local_image_changed("image/png".into(), b"\x89PNG fake".to_vec());
        assert!(rx.try_recv().is_err());
    }

    fn file_clip(clip_id: i64, files: Value) -> Value {
        json!({ "msgId": 3, "data": { "clipId": clip_id, "filelist": files, "storage": "/storage/emulated/0" } })
    }

    #[test]
    fn an_image_clip_is_pulled() {
        let local = LocalClipboard::new();
        let mut s = Session { negotiated: true };
        let img = file_clip(9, json!([{ "attr": 128, "name": "a.jpg", "path": "/storage/emulated/0/a.jpg", "size": 1234 }]));
        assert_eq!(s.handle(&img, &local), Some((9, json!({ "msgId": 4, "data": { "clipId": 9 } }))));
        // The phone's text is untouched by a file clip.
        assert!(local.last_synced.lock().unwrap().is_none());
    }

    #[test]
    fn other_file_clips_are_never_pulled() {
        let local = LocalClipboard::new();
        let mut s = Session::default();
        let doc = file_clip(9, json!([{ "attr": 128, "name": "a.pdf", "size": 10 }]));
        assert!(s.handle(&doc, &local).is_none(), "not an image");
        let two = file_clip(9, json!([{ "attr": 128, "name": "a.jpg", "size": 1 }, { "attr": 128, "name": "b.jpg", "size": 1 }]));
        assert!(s.handle(&two, &local).is_none(), "several files");
        let dir = file_clip(9, json!([{ "attr": 16, "name": "Pictures.png", "size": 0 }]));
        assert!(s.handle(&dir, &local).is_none(), "a folder");
        let huge = file_clip(9, json!([{ "attr": 128, "name": "a.png", "size": 11 * 1024 * 1024 }]));
        assert!(s.handle(&huge, &local).is_none(), "over the phone's 10 MB limit");
        let unknown_size = file_clip(9, json!([{ "attr": 128, "name": "a.png" }]));
        assert!(s.handle(&unknown_size, &local).is_none(), "missing size");
        let missing_type = file_clip(9, json!([{ "name": "a.png", "size": 1 }]));
        assert!(s.handle(&missing_type, &local).is_none(), "missing file type");
        let zero_size = file_clip(9, json!([{ "attr": 128, "name": "a.png", "size": 0 }]));
        assert!(s.handle(&zero_size, &local).is_none(), "empty image");
        let empty = file_clip(0, json!([{ "attr": 128, "name": "a.png", "size": 1 }]));
        assert!(s.handle(&empty, &local).is_none(), "clipId 0 is an empty clip");
        // (A text clip returns before the file-list check; it isn't run here because applying it
        // calls wl-copy, which would replace the clipboard of whoever runs the tests.)
    }

    #[test]
    fn image_mime_types() {
        assert_eq!(image_mime("Screenshot_2026.PNG"), "image/png");
        assert_eq!(image_mime("IMG_1.jpg"), "image/jpeg");
        assert_eq!(image_mime("x.webp"), "image/webp");
        assert_eq!(image_mime("noext"), "image/jpeg");
    }

    #[test]
    fn image_names_are_detected() {
        assert!(is_image_name("photo.JPG"));
        assert!(is_image_name("x.heic"));
        assert!(!is_image_name("notes.txt"));
        assert!(!is_image_name("archive.zip"));
    }

    #[test]
    fn long_text_is_truncated_to_the_phone_limit() {
        let s = Session { negotiated: true };
        let clip = LocalClip { id: 1, text: "é".repeat(MAX_TEXT_CHARS + 10), image: None };
        let out = s.outgoing(&clip).unwrap();
        assert_eq!(out["data"]["text"].as_str().unwrap().chars().count(), MAX_TEXT_CHARS);
    }
}
