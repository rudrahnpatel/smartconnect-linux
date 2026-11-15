//! The main window: a header with a device menu + a Phone / Apps / Display (Cross Control and the display) / Files / Notifications
//! view switcher, driven by daemon status events. Pairing and Preferences live in dialogs opened
//! from the device menu.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::rc::{Rc, Weak};
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use crate::apps::{self, AppsPage};
use crate::crosscontrol::CrossControlPage;
use crate::icons::{IconSlot, Query};
use crate::daemon::{self, Event, Notification, Reply, Status};
use crate::display::DisplayPage;
use crate::files::FilesPage;
use crate::lastdevice::{self, LastDevice};
use crate::lifecycle::{self, Daemon};
use crate::messages::MessagesPage;
use crate::mirror::{Exit, Mirror};
use crate::tray;

const SIGNAL_WORDS: [&str; 6] = ["None", "Very weak", "Weak", "Fair", "Good", "Excellent"];
/// Adwaita's `network-{wireless,cellular}-signal-<x>-symbolic` names for levels 0–5.
const SIGNAL_ICONS: [&str; 6] = ["none", "weak", "weak", "ok", "good", "excellent"];

fn signal_level(level: Option<i64>) -> Option<usize> {
    level.map(|l| l.clamp(0, 5) as usize)
}

fn signal_text(level: Option<i64>) -> &'static str {
    signal_level(level).map_or("—", |l| SIGNAL_WORDS[l])
}

pub struct Ui {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    root: gtk::Stack,
    /// Label under the spinner on the "connecting" page ("Connecting…" / "Starting smartconnectd…").
    connecting_label: gtk::Label,
    /// The five-view stack, kept so pages can be switched programmatically (the unread shortcut).
    views: adw::ViewStack,
    switcher: adw::ViewSwitcher,
    switcher_bar: adw::ViewSwitcherBar,
    notif_page: adw::ViewStackPage,
    device: DeviceMenu,
    /// The "service isn't running" page (with the last device and a Start button).
    down: DownPage,
    phone: PhonePage,
    notifs: NotifPage,
    pair: PairDialog,
    apps: Rc<AppsPage>,
    display: Rc<DisplayPage>,
    cross: Rc<CrossControlPage>,
    files: Rc<FilesPage>,
    messages: Rc<MessagesPage>,
    /// The system-tray item, when the desktop has a StatusNotifierHost.
    tray: RefCell<Option<tray::Tray>>,
    /// Tracks how the daemon is running, so Quit stops the right thing.
    daemon_ctl: RefCell<Daemon>,
    state: RefCell<State>,
}

#[derive(Default)]
struct State {
    status: Option<Status>,
    wallpaper: Option<String>,
    busy: bool,
    /// The daemon was just launched; keep the "Starting…" spinner until the socket answers.
    starting: bool,
    mirror: Option<Mirror>,
    /// A whole-screen mirror request to the stream host is in flight.
    mirror_pending: bool,
    show_qr: bool,
    qr_path: Option<String>,
    rows: HashMap<String, NotifRow>,
    /// Notifications cleared from this PC's list (Dismiss / Clear all). Pruned when the key is gone
    /// from the phone. Local only — the phone keeps the notification (no dismiss command exists yet).
    dismissed: HashSet<String>,
    logged_first: bool,
    /// Packages whose desktop popups the daemon suppresses (`notif_mute`); `None` until asked.
    muted: Option<Vec<String>>,
    muted_loading: bool,
    muted_rows: Vec<adw::ActionRow>,
    /// App names seen in notifications, by package (for the muted list).
    app_names: HashMap<String, String>,
    /// The last connected phone (persisted; shown while it is away or the daemon is down).
    last: LastDevice,
    /// The daemon's wallpaper value (path + `?md5`) last copied into the cache.
    copied_wallpaper: Option<String>,
    /// Connected at the previous status, to stamp and save the cache on a disconnect.
    was_connected: bool,
    /// Unix seconds of the last cache save (re-saved at most every few minutes while connected).
    last_saved: u64,
    /// Wallpaper shown on the "service isn't running" page.
    down_wallpaper: Option<String>,
    /// "Start service" was pressed and `Daemon::ensure_running` is in flight.
    service_starting: bool,
}

/// What the Phone page shows: the live phone, the last one while it is away, or the pairing prompt.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PhoneState {
    Connected,
    Disconnected,
    Unpaired,
}

/// The left-of-header device button and its popover (current device, Pair, Preferences, Disconnect).
struct DeviceMenu {
    button: gtk::MenuButton,
    popover: gtk::Popover,
    dot: gtk::Image,
    label: gtk::Label,
    menu_name: gtk::Label,
    menu_sub: gtk::Label,
    check: gtk::Image,
    pair: gtk::Button,
    prefs: gtk::Button,
    /// Connect / Disconnect, or "Start service" while the daemon is down.
    connect: gtk::Button,
}

/// One inline stat in the phone card: a symbolic icon and a short value, full text in the tooltip.
struct Stat {
    row: gtk::Box,
    icon: gtk::Image,
    value: gtk::Label,
}

impl Stat {
    fn new() -> Self {
        let icon = gtk::Image::new();
        let value = gtk::Label::builder().css_classes(["dim-label"]).build();
        let row = gtk::Box::builder().spacing(4).build();
        row.append(&icon);
        row.append(&value);
        Self { row, icon, value }
    }

    fn set(&self, icon: &str, value: &str, tooltip: &str) {
        self.icon.set_icon_name(Some(icon));
        self.value.set_label(value);
        self.row.set_tooltip_text(Some(tooltip));
    }
}

struct PhonePage {
    /// The thumbnail's outer widget (dimmed while the phone is away).
    thumb: gtk::Widget,
    wallpaper: gtk::Picture,
    wallpaper_stack: gtk::Stack,
    name: gtk::Label,
    subtitle: gtk::Label,
    /// Inline battery / Wi-Fi / mobile / Bluetooth row under the name.
    stats: gtk::FlowBox,
    battery: Stat,
    wifi: Stat,
    cell: Stat,
    bluetooth: Stat,
    mirror: gtk::Button,
    /// Shown instead of Mirror screen while the paired phone is away.
    connect: gtk::Button,
    connect_spinner: adw::Spinner,
    connect_label: gtk::Label,
    /// Shown when no phone is paired.
    pair: gtk::Button,
    unread_card: gtk::Button,
    unread_label: gtk::Label,
    cross_group: adw::PreferencesGroup,
    /// Opens the Cross Control page.
    cross: adw::ActionRow,
}

/// The root "Smart Connect service isn't running" page.
struct DownPage {
    /// The last device (thumbnail + name + when it was last connected); hidden when none is known.
    card: gtk::Box,
    wallpaper: gtk::Picture,
    wallpaper_stack: gtk::Stack,
    name: gtk::Label,
    sub: gtk::Label,
    start: gtk::Button,
}

struct NotifPage {
    empty: adw::StatusPage,
    today: gtk::Box,
    clear_all: gtk::Button,
    list: gtk::ListBox,
    muted_group: adw::PreferencesGroup,
    muted: adw::ExpanderRow,
}

struct PairDialog {
    dialog: adw::Dialog,
    qr: gtk::Picture,
    qr_box: gtk::Box,
    status: gtk::Label,
    spinner: adw::Spinner,
    button: gtk::Button,
    /// Firewall hint (kept alive with the dialog).
    #[allow(dead_code)]
    firewall: std::rc::Rc<crate::setup::FirewallHint>,
}

struct NotifRow {
    row: gtk::ListBoxRow,
    icon: IconSlot,
    header: gtk::Label,
    sub: gtk::Label,
    body: gtk::Label,
    reply_box: gtk::Box,
    entry: gtk::Entry,
    send: gtk::Button,
    menu: gtk::MenuButton,
    /// "Open on phone": forwards the notification click. Needs a live connection.
    open_item: gtk::Button,
    /// Mutes/unmutes the app's desktop popups (hidden without a package name).
    mute_item: gtk::Button,
    data: Notification,
}

fn page_scroller(child: &impl IsA<gtk::Widget>, max: i32) -> gtk::ScrolledWindow {
    // A generous clamp: content fills a wide/maximized window instead of sitting in a thin strip.
    // threshold == maximum_size: the content simply fills the width up to `max` (like CSS max-width),
    // instead of AdwClamp's default halving of growth past the threshold.
    let clamp = adw::Clamp::builder().maximum_size(max).tightening_threshold(max).child(child).build();
    gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&clamp).build()
}

/// A wallpaper thumbnail of exactly `w`×`h` (cropped to fill, aspect kept), with a phone placeholder.
/// A `GtkPicture`'s natural size is the image's own size, so it is clamped in both directions —
/// otherwise a full-resolution wallpaper inflates the card.
fn wallpaper_thumb(w: i32, h: i32) -> (gtk::Widget, gtk::Picture, gtk::Stack) {
    let picture = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Cover)
        .can_shrink(true)
        .width_request(w)
        .height_request(h)
        .css_classes(["wallpaper"])
        .overflow(gtk::Overflow::Hidden)
        .build();
    let placeholder = gtk::Image::builder().icon_name("phone-symbolic").pixel_size(40).css_classes(["dim-label"]).build();
    let stack = gtk::Stack::new();
    stack.add_named(&placeholder, Some("placeholder"));
    stack.add_named(&picture, Some("picture"));
    let tall = adw::Clamp::builder().orientation(gtk::Orientation::Vertical).maximum_size(h).tightening_threshold(h).child(&stack).build();
    let thumb = adw::Clamp::builder()
        .orientation(gtk::Orientation::Horizontal)
        .maximum_size(w)
        .tightening_threshold(w)
        .valign(gtk::Align::Center)
        .child(&tall)
        .build();
    (thumb.upcast(), picture, stack)
}

/// Shows the image at `path` (a `?md5` suffix is ignored) or the placeholder. A fresh texture each
/// time: the daemon rewrites the same file for a new wallpaper.
fn show_wallpaper(picture: &gtk::Picture, stack: &gtk::Stack, path: Option<&str>) {
    match path.map(|p| p.split('?').next().unwrap_or(p)).map(gdk::Texture::from_filename) {
        Some(Ok(texture)) => {
            picture.set_paintable(Some(&texture));
            stack.set_visible_child_name("picture");
        }
        _ => {
            picture.set_paintable(None::<&gdk::Paintable>);
            stack.set_visible_child_name("placeholder");
        }
    }
}

/// "last connected 5 min ago" → "Last connected 5 min ago".
fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or_else(String::new, |f| f.to_uppercase().chain(c).collect())
}

fn set_dimmed(w: &impl IsA<gtk::Widget>, dimmed: bool) {
    if dimmed {
        w.add_css_class("sc-dimmed");
    } else {
        w.remove_css_class("sc-dimmed");
    }
}

/// A flat, full-width, left-aligned button for a popover menu.
fn menu_item(label: &str) -> gtk::Button {
    let l = gtk::Label::builder().label(label).xalign(0.0).hexpand(true).build();
    gtk::Button::builder().css_classes(["flat"]).child(&l).build()
}

impl DeviceMenu {
    fn build() -> Self {
        let dot = gtk::Image::builder().icon_name("media-record-symbolic").pixel_size(10).css_classes(["dim-label"]).build();
        let label = gtk::Label::builder().label("Smart Connect").ellipsize(gtk::pango::EllipsizeMode::End).max_width_chars(18).build();
        let arrow = gtk::Image::from_icon_name("pan-down-symbolic");
        let content = gtk::Box::builder().spacing(6).build();
        content.append(&dot);
        content.append(&label);
        content.append(&arrow);

        // Popover: the current device (name + "Wi-Fi · N%" + a check), then the actions.
        let menu_name = gtk::Label::builder().xalign(0.0).hexpand(true).css_classes(["heading"]).ellipsize(gtk::pango::EllipsizeMode::End).build();
        let menu_sub = gtk::Label::builder().xalign(0.0).css_classes(["caption", "dim-label"]).build();
        let names = gtk::Box::builder().orientation(gtk::Orientation::Vertical).hexpand(true).build();
        names.append(&menu_name);
        names.append(&menu_sub);
        let check = gtk::Image::builder().icon_name("object-select-symbolic").valign(gtk::Align::Center).build();
        let device_row = gtk::Box::builder().spacing(8).margin_start(6).margin_end(6).margin_top(4).margin_bottom(4).build();
        device_row.append(&names);
        device_row.append(&check);

        let pair = menu_item("Pair new phone…");
        let prefs = menu_item("Preferences");
        let connect = menu_item("Disconnect");

        let pop_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).build();
        pop_box.append(&device_row);
        pop_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        pop_box.append(&pair);
        pop_box.append(&prefs);
        pop_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        pop_box.append(&connect);
        let popover = gtk::Popover::builder().child(&pop_box).width_request(240).build();
        let button = gtk::MenuButton::builder().child(&content).popover(&popover).build();
        Self { button, popover, dot, label, menu_name, menu_sub, check, pair, prefs, connect }
    }
}

impl DownPage {
    fn build() -> (Self, gtk::Widget) {
        let (thumb, wallpaper, wallpaper_stack) = wallpaper_thumb(48, 100);
        thumb.add_css_class("sc-dimmed");
        let name = gtk::Label::builder().xalign(0.0).css_classes(["heading"]).wrap(true).build();
        let sub = gtk::Label::builder().xalign(0.0).css_classes(["dim-label"]).wrap(true).build();
        let text = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).valign(gtk::Align::Center).build();
        text.append(&name);
        text.append(&sub);
        let card = gtk::Box::builder().spacing(12).halign(gtk::Align::Center).css_classes(["card", "sc-card"]).visible(false).build();
        card.append(&thumb);
        card.append(&text);
        let start = gtk::Button::builder()
            .label("Start service")
            .css_classes(["pill", "suggested-action"])
            .halign(gtk::Align::Center)
            .build();
        let column = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(24).build();
        column.append(&card);
        column.append(&start);
        let page = adw::StatusPage::builder()
            .icon_name("network-offline-symbolic")
            .title("Smart Connect service isn't running")
            .description("Start it to reach your phone. This window reconnects on its own once it is up.")
            .child(&column)
            .build();
        (Self { card, wallpaper, wallpaper_stack, name, sub, start }, page.upcast())
    }
}

impl PhonePage {
    fn build() -> (Self, gtk::Widget) {
        // ── Phone card (hero): thumbnail · name / state / inline stats · action ──
        let (thumb, wallpaper, wallpaper_stack) = wallpaper_thumb(68, 144);

        let name = gtk::Label::builder().xalign(0.0).css_classes(["title-3"]).wrap(true).build();
        let subtitle = gtk::Label::builder().xalign(0.0).css_classes(["dim-label"]).wrap(true).build();
        let (battery, wifi, cell, bluetooth) = (Stat::new(), Stat::new(), Stat::new(), Stat::new());
        // A FlowBox so the stats wrap instead of forcing a minimum width on a narrow window.
        let stats = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .min_children_per_line(1)
            .max_children_per_line(4)
            .column_spacing(16)
            .row_spacing(4)
            .halign(gtk::Align::Start)
            .margin_top(6)
            .build();
        for s in [&battery, &wifi, &cell, &bluetooth] {
            stats.append(&s.row);
            if let Some(cell) = s.row.parent() {
                cell.set_focusable(false);
            }
        }
        let info = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).hexpand(true).valign(gtk::Align::Center).build();
        info.append(&name);
        info.append(&subtitle);
        info.append(&stats);

        let mirror = gtk::Button::builder().css_classes(["suggested-action"]).label("Mirror screen").build();
        let connect_spinner = adw::Spinner::builder().visible(false).build();
        let connect_label = gtk::Label::new(Some("Connect"));
        let connect_box = gtk::Box::builder().spacing(8).halign(gtk::Align::Center).build();
        connect_box.append(&connect_spinner);
        connect_box.append(&connect_label);
        let connect = gtk::Button::builder().css_classes(["suggested-action"]).child(&connect_box).visible(false).build();
        let pair = gtk::Button::builder().css_classes(["suggested-action"]).label("Pair a phone").visible(false).build();
        let actions = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).valign(gtk::Align::Center).build();
        actions.append(&mirror);
        actions.append(&connect);
        actions.append(&pair);

        let hero = gtk::Box::builder().spacing(16).valign(gtk::Align::Start).css_classes(["card", "sc-card"]).build();
        hero.append(&thumb);
        hero.append(&info);
        hero.append(&actions);

        // ── Unread shortcut ──────────────────────────────────────────────────
        let unread_label = gtk::Label::builder().xalign(0.0).hexpand(true).build();
        let unread_link = gtk::Label::builder().label("Open notifications ›").css_classes(["accent"]).build();
        let unread_row = gtk::Box::builder().spacing(8).build();
        unread_row.append(&unread_label);
        unread_row.append(&unread_link);
        let unread_card = gtk::Button::builder().css_classes(["card", "sc-card"]).visible(false).child(&unread_row).build();

        // ── Cross Control ─────────────────────────────────────────────────────
        let cross_group = adw::PreferencesGroup::builder().title("Cross Control").build();
        let cross = adw::ActionRow::builder()
            .title("Cross Control")
            .subtitle("Shared mouse and keyboard, duplicate display or extend display")
            .activatable(true)
            .build();
        cross.add_prefix(&gtk::Image::from_icon_name("input-mouse-symbolic"));
        cross.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        cross_group.add(&cross);

        // Nothing here expands vertically: the unread row and Cross Control sit right under the card.
        let column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .valign(gtk::Align::Start)
            .margin_top(24)
            .margin_bottom(24)
            .margin_start(12)
            .margin_end(12)
            .build();
        column.append(&hero);
        column.append(&unread_card);
        column.append(&cross_group);

        let page = Self {
            thumb,
            wallpaper,
            wallpaper_stack,
            name,
            subtitle,
            stats,
            battery,
            wifi,
            cell,
            bluetooth,
            mirror,
            connect,
            connect_spinner,
            connect_label,
            pair,
            unread_card,
            unread_label,
            cross_group,
            cross,
        };
        (page, page_scroller(&column, 1400).upcast())
    }
}

impl NotifPage {
    fn build() -> (Self, gtk::Widget) {
        // "Muted apps" sits above the list (or the empty state) so it is reachable either way.
        let muted = adw::ExpanderRow::builder().title("Muted apps").build();
        let muted_group = adw::PreferencesGroup::builder().visible(false).build();
        muted_group.add(&muted);
        let empty = adw::StatusPage::builder()
            .icon_name("preferences-system-notifications-symbolic")
            .title("No Notifications")
            .description("Notifications from the phone show up here while it is connected.")
            .vexpand(true)
            .build();

        // The day header: "Today" + Clear all (we have no timestamps, so everything is Today).
        let today_label = gtk::Label::builder().label("Today").xalign(0.0).hexpand(true).css_classes(["heading"]).build();
        let clear_all = gtk::Button::builder().label("Clear all").css_classes(["flat"]).build();
        let today = gtk::Box::builder().spacing(8).visible(false).build();
        today.append(&today_label);
        today.append(&clear_all);

        let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).visible(false).build();
        let column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(24)
            .margin_bottom(24)
            .margin_start(12)
            .margin_end(12)
            .build();
        column.append(&muted_group);
        column.append(&empty);
        column.append(&today);
        column.append(&list);
        (Self { empty, today, clear_all, list, muted_group, muted }, page_scroller(&column, 1400).upcast())
    }
}

impl PairDialog {
    fn build() -> Self {
        let qr = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain)
            .width_request(200)
            .height_request(200)
            .css_classes(["qr"])
            .build();
        let qr_box = gtk::Box::builder().halign(gtk::Align::Center).valign(gtk::Align::Center).build();
        qr_box.append(&qr);

        let steps = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(10).hexpand(true).valign(gtk::Align::Center).build();
        for (i, text) in ["Open Smart Connect on your phone", "Tap + and choose “Scan QR code”", "Point the camera at this code"].iter().enumerate() {
            let row = gtk::Box::builder().spacing(10).build();
            row.append(&gtk::Label::builder().label(&format!("{}", i + 1)).css_classes(["caption", "dim-label"]).valign(gtk::Align::Start).build());
            row.append(&gtk::Label::builder().label(*text).xalign(0.0).wrap(true).build());
            steps.append(&row);
        }
        let spinner = adw::Spinner::new();
        let status = gtk::Label::builder().label("Waiting for phone…").xalign(0.0).hexpand(true).wrap(true).css_classes(["dim-label"]).build();
        let status_row = gtk::Box::builder().spacing(8).margin_top(4).build();
        status_row.append(&spinner);
        status_row.append(&status);
        steps.append(&status_row);

        let top = gtk::Box::builder().spacing(18).build();
        top.append(&qr_box);
        top.append(&steps);
        let button = gtk::Button::builder().label("New code").halign(gtk::Align::End).build();
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(18)
            .margin_bottom(18)
            .margin_start(18)
            .margin_end(18)
            .build();
        content.append(&top);
        // Shown only when a firewall may drop the phone's connection (checked when the QR shows).
        let firewall = crate::setup::FirewallHint::build();
        content.append(&firewall.widget);
        content.append(&button);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&content));
        let dialog = adw::Dialog::builder().title("Pair a new phone").content_width(520).child(&toolbar).build();
        let fw = firewall.clone();
        dialog.connect_map(move |_| fw.check());
        Self { dialog, qr, qr_box, status, spinner, button, firewall }
    }
}

impl Ui {
    pub fn new(app: &adw::Application) -> Rc<Self> {
        let views = adw::ViewStack::new();
        let (phone, phone_w) = PhonePage::build();
        let (notifs, notifs_w) = NotifPage::build();
        let toasts = adw::ToastOverlay::new();
        let (apps, apps_w) = AppsPage::build(&toasts);
        let (display, display_w) = DisplayPage::build(&toasts);
        let (cross, cross_modes, cross_position) = CrossControlPage::build(&toasts, display.clone());
        // One Display tab: the Cross Control modes and the phone's position on top, then the
        // display session and its settings.
        let display_column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(24)
            .margin_top(24)
            .margin_bottom(24)
            .margin_start(12)
            .margin_end(12)
            .build();
        display_column.append(&cross_modes);
        display_column.append(&cross_position);
        display_column.append(&display_w);
        let display_clamp = adw::Clamp::builder().maximum_size(1100).tightening_threshold(1100).child(&display_column).build();
        let display_w = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&display_clamp).build();
        let (files, files_w) = FilesPage::build(&toasts);
        let (messages, messages_w) = MessagesPage::build();
        views.add_titled_with_icon(&phone_w, Some("phone"), "Phone", "phone-symbolic");
        views.add_titled_with_icon(&apps_w, Some("apps"), "Apps", "view-app-grid-symbolic");
        views.add_titled_with_icon(&display_w, Some("display"), "Display", "video-display-symbolic");
        views.add_titled_with_icon(&files_w, Some("files"), "Files", "folder-symbolic");
        views.add_titled_with_icon(&messages_w, Some("messages"), "Messages", "mail-message-new-symbolic");
        let notif_page = views.add_titled_with_icon(&notifs_w, Some("notifications"), "Notifications", "preferences-system-notifications-symbolic");

        let (down_page, down) = DownPage::build();
        let spinner = adw::Spinner::builder().width_request(48).height_request(48).build();
        let connecting_label = gtk::Label::builder().label("Connecting…").css_classes(["dim-label"]).build();
        let connecting = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .build();
        connecting.append(&spinner);
        connecting.append(&connecting_label);
        let root = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).build();
        root.add_named(&connecting, Some("connecting"));
        root.add_named(&down, Some("down"));
        root.add_named(&views, Some("main"));

        let device = DeviceMenu::build();
        let switcher = adw::ViewSwitcher::builder().stack(&views).policy(adw::ViewSwitcherPolicy::Wide).build();
        let header = adw::HeaderBar::builder().title_widget(&switcher).build();
        header.pack_start(&device.button);
        let switcher_bar = adw::ViewSwitcherBar::builder().stack(&views).build();
        toasts.set_child(Some(&root));
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.add_bottom_bar(&switcher_bar);
        toolbar.set_content(Some(&toasts));

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Smart Connect")
            .default_width(600)
            .default_height(760)
            .content(&toolbar)
            .build();
        // Six page names plus the device menu don't fit a narrow header: then the switcher moves to
        // a bar at the bottom and the header keeps the window title and the device menu.
        let narrow = adw::Breakpoint::new(adw::BreakpointCondition::parse("max-width: 780sp").expect("valid breakpoint condition"));
        narrow.add_setter(&header, "title-widget", Some(&None::<gtk::Widget>.to_value()));
        narrow.add_setter(&switcher_bar, "reveal", Some(&true.to_value()));
        window.add_breakpoint(narrow);

        let pair = PairDialog::build();

        let ui = Rc::new(Self {
            window,
            toasts,
            root,
            connecting_label,
            views: views.clone(),
            switcher,
            switcher_bar,
            notif_page,
            device,
            down: down_page,
            phone,
            notifs,
            pair,
            apps,
            display,
            cross,
            files,
            messages,
            tray: RefCell::default(),
            daemon_ctl: RefCell::default(),
            state: RefCell::new(State { last: lastdevice::load(), ..State::default() }),
        });
        ui.show_switcher(false);
        // Until the daemon answers, the header and the "not running" page show the cached phone.
        ui.refresh_device_offline();
        ui.refresh_down();
        ui.connect_signals();
        let weak = Rc::downgrade(&ui);
        views.connect_visible_child_name_notify(move |views| {
            let Some(ui) = weak.upgrade() else { return };
            let name = views.visible_child_name();
            ui.display.set_visible(name.as_deref() == Some("display"));
            match name.as_deref() {
                Some("apps") => ui.apps.shown(),
                Some("display") => ui.cross.refresh(),
                Some("messages") => ui.messages.refresh(),
                Some("notifications") => ui.load_muted(),
                _ => {}
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.messages.refresh.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.messages.refresh();
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.apps.connect_sessions_changed(move || {
            if let Some(ui) = weak.upgrade() {
                ui.refresh_phone();
            }
        });
        ui
    }

    pub fn present(&self) {
        self.window.present();
        self.refresh_tray();
    }

    /// Stops the mirror viewer and Cross Control (if any) when the app quits, and unmounts the
    /// phone's storage so no stale FUSE mount is left behind. A phone-as-display session keeps
    /// running on purpose (like the stream host): it is a monitor with windows on it, the phone can
    /// stop it, and the Display page picks it up again when the app is reopened.
    pub fn shutdown(&self) {
        {
            let mut st = self.state.borrow_mut();
            let st = &mut *st;
            if let Some(mirror) = st.mirror.as_mut() {
                mirror.stop();
            }
            // Still connected at quit: that is when the phone was last seen.
            if st.status.as_ref().is_some_and(|s| s.connected) {
                st.last.last_connected = Some(lastdevice::now_secs());
                if let Err(e) = lastdevice::save(&st.last) {
                    eprintln!("smartconnect-gui: could not save the last device: {e}");
                }
            }
        }
        self.files.shutdown();
        self.cross.shutdown();
    }

    fn connect_signals(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.phone.mirror.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.toggle_mirror();
            }
        });
        let weak = Rc::downgrade(self);
        self.phone.cross.connect_activated(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.views.set_visible_child_name("display");
            }
        });
        let weak = Rc::downgrade(self);
        self.phone.unread_card.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.views.set_visible_child_name("notifications");
            }
        });
        // Phone card: Connect (phone away) and Pair a phone (none paired).
        let weak = Rc::downgrade(self);
        self.phone.connect.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.toggle_connection();
            }
        });
        let weak = Rc::downgrade(self);
        self.phone.pair.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.open_pair_dialog();
            }
        });
        // "Service isn't running" page: Start.
        let weak = Rc::downgrade(self);
        self.down.start.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.start_service();
            }
        });
        // Device menu: Connect/Disconnect (or Start service while the daemon is down), Pair new
        // phone…, Preferences.
        let weak = Rc::downgrade(self);
        self.device.connect.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.device.popover.popdown();
                if ui.state.borrow().status.is_none() {
                    ui.start_service();
                } else {
                    ui.toggle_connection();
                }
            }
        });
        let weak = Rc::downgrade(self);
        self.device.pair.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.device.popover.popdown();
                ui.open_pair_dialog();
            }
        });
        let weak = Rc::downgrade(self);
        self.device.prefs.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.device.popover.popdown();
                ui.open_preferences();
            }
        });
        // Pair dialog's "New code" asks for a fresh QR.
        let weak = Rc::downgrade(self);
        self.pair.button.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.request_pairing();
            }
        });
        // Clear all notifications from this PC's list.
        let weak = Rc::downgrade(self);
        self.notifs.clear_all.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.clear_all_notifications();
            }
        });
        // Closing the window only hides it to the tray; Quit (from the tray) is the only way out.
        let weak = Rc::downgrade(self);
        self.window.connect_close_request(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.window.set_visible(false);
                ui.refresh_tray();
            }
            glib::Propagation::Stop
        });
    }

    /// Spawns the tray, makes sure the daemon is running, and starts listening for daemon status
    /// and tray-menu commands. Called once, right after construction.
    pub fn start(self: &Rc<Self>) {
        let (daemon, started) = Daemon::ensure_running();
        *self.daemon_ctl.borrow_mut() = daemon;

        if let Some((tray, commands)) = tray::Tray::spawn() {
            *self.tray.borrow_mut() = Some(tray);
            self.listen_tray(commands);
        }
        self.listen(daemon::subscribe());
        if started {
            self.begin_starting();
        }
        self.refresh_tray();
        // Keep "last connected 5 min ago" current.
        let weak = Rc::downgrade(self);
        glib::timeout_add_seconds_local(60, move || {
            let Some(ui) = weak.upgrade() else { return glib::ControlFlow::Break };
            if ui.state.borrow().status.is_some() {
                ui.refresh_phone();
            } else {
                ui.refresh_down();
                ui.refresh_device_offline();
            }
            glib::ControlFlow::Continue
        });
    }

    /// "Start service" (the not-running page or the device menu): the same path as opening the app —
    /// the `systemctl --user` unit if installed, else `smartconnectd` as a child — off the main thread.
    fn start_service(self: &Rc<Self>) {
        {
            let mut st = self.state.borrow_mut();
            if st.service_starting || st.starting {
                return;
            }
            st.service_starting = true;
        }
        self.down.start.set_sensitive(false);
        self.down.start.set_label("Starting…");
        self.refresh_device_offline();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(Daemon::ensure_running).await;
            let Some(ui) = weak.upgrade() else { return };
            ui.state.borrow_mut().service_starting = false;
            ui.down.start.set_sensitive(true);
            ui.down.start.set_label("Start service");
            match result {
                Ok((daemon, true)) => {
                    *ui.daemon_ctl.borrow_mut() = daemon;
                    ui.begin_starting();
                }
                // Already answering (someone else started it): the subscriber picks it up shortly.
                Ok(_) if lifecycle::socket_alive() => {}
                Ok(_) => ui.toast(
                    "Could not start smartconnectd. Is it installed? Run it in a terminal, or check `journalctl --user -u smartconnectd`.",
                ),
                Err(_) => ui.toast("Could not start smartconnectd (worker thread failed)"),
            }
            ui.refresh_device_offline();
        });
    }

    /// Drains tray-menu commands on the GTK main loop.
    fn listen_tray(self: &Rc<Self>, commands: async_channel::Receiver<tray::Command>) {
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(command) = commands.recv().await {
                let Some(ui) = weak.upgrade() else { break };
                match command {
                    tray::Command::ToggleWindow => ui.toggle_window(),
                    tray::Command::Connect => ui.tray_connect(true),
                    tray::Command::Disconnect => ui.tray_connect(false),
                    tray::Command::Quit => ui.quit(),
                }
            }
        });
    }

    /// Show the window if hidden, hide it if shown (the tray's Show/Hide and left click).
    fn toggle_window(self: &Rc<Self>) {
        if self.window.is_visible() {
            self.window.set_visible(false);
        } else {
            self.window.present();
        }
        self.refresh_tray();
    }

    /// Connect/Disconnect from the tray: only acts when it would actually change the state.
    fn tray_connect(self: &Rc<Self>, connect: bool) {
        let act = {
            let st = self.state.borrow();
            !st.busy && st.status.as_ref().is_some_and(|s| s.connected != connect)
        };
        if act {
            self.toggle_connection();
        }
    }

    /// Quit: stop our helpers and the daemon we manage, then leave. The only exit path.
    fn quit(self: &Rc<Self>) {
        self.shutdown();
        self.daemon_ctl.borrow_mut().stop();
        if let Some(app) = self.window.application() {
            app.quit();
        }
    }

    /// Opens the Preferences dialog (built fresh so the Start-at-login switch reflects the disk).
    fn open_preferences(self: &Rc<Self>) {
        let start_at_login = adw::SwitchRow::builder()
            .title("Start at login")
            .subtitle("Launch Smart Connect in the background (tray only) when you log in")
            .build();
        start_at_login.set_active(lifecycle::autostart_enabled());
        let weak = Rc::downgrade(self);
        start_at_login.connect_active_notify(move |row| {
            if let Some(ui) = weak.upgrade() {
                ui.set_start_at_login(row, row.is_active());
            }
        });
        let group = adw::PreferencesGroup::new();
        group.add(&start_at_login);
        let page = adw::PreferencesPage::new();
        page.add(&group);
        let dialog = adw::PreferencesDialog::new();
        dialog.add(&page);
        dialog.present(Some(&self.window));
    }

    /// Adds or removes the XDG autostart entry behind the "Start at login" switch.
    fn set_start_at_login(&self, row: &adw::SwitchRow, on: bool) {
        match lifecycle::set_autostart(on) {
            Ok(()) => self.toast(if on {
                "Smart Connect will start at login"
            } else {
                "Smart Connect will no longer start at login"
            }),
            Err(e) => {
                self.toast(&format!("Could not change the login setting: {e}"));
                // Put the switch back to what's actually on disk.
                row.set_active(lifecycle::autostart_enabled());
            }
        }
    }

    /// The daemon was just launched: show a "Starting…" spinner until the socket answers, or fall
    /// back to the "not running" page after a grace period.
    fn begin_starting(self: &Rc<Self>) {
        self.state.borrow_mut().starting = true;
        self.connecting_label.set_label("Starting smartconnectd…");
        self.root.set_visible_child_name("connecting");
        self.show_switcher(false);
        self.refresh_device_offline();
        self.refresh_tray();
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_secs(20), move || {
            let Some(ui) = weak.upgrade() else { return };
            let was_starting = std::mem::take(&mut ui.state.borrow_mut().starting);
            ui.connecting_label.set_label("Connecting…");
            if was_starting && ui.state.borrow().status.is_none() {
                ui.set_down();
            }
        });
    }

    /// The coarse state the tray icon should show.
    fn tray_state(&self) -> tray::State {
        let st = self.state.borrow();
        if st.starting {
            return tray::State::Starting;
        }
        match &st.status {
            Some(s) if s.connected => tray::State::Connected,
            Some(_) => tray::State::Disconnected,
            None => tray::State::DaemonDown,
        }
    }

    /// Pushes the current state and window visibility to the tray.
    fn refresh_tray(&self) {
        let state = self.tray_state();
        let visible = self.window.is_visible();
        if let Some(tray) = self.tray.borrow().as_ref() {
            tray.set(state, visible);
        }
    }

    /// Consumes daemon events on the GTK main loop.
    pub fn listen(self: &Rc<Self>, events: async_channel::Receiver<Event>) {
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(event) = events.recv().await {
                let Some(ui) = weak.upgrade() else { break };
                match event {
                    Event::Status(status) => ui.apply_status(*status),
                    Event::Down => ui.set_down(),
                    Event::CrossControlRequested => {
                        // The phone's tile: show the three modes (the daemon answers the phone itself).
                        ui.present();
                        ui.views.set_visible_child_name("display");
                        ui.cross.refresh();
                    }
                }
            }
        });
    }

    fn toast(&self, message: &str) {
        if !message.is_empty() {
            self.toasts.add_toast(adw::Toast::new(message));
        }
    }

    /// The page switcher only makes sense while the daemon is up. The device menu also stays while it
    /// is down if a phone is remembered (it then shows that phone and offers "Start service").
    fn show_switcher(&self, on: bool) {
        self.switcher.set_visible(on);
        self.switcher_bar.set_visible(on);
        let known = self.state.borrow().last.known();
        self.device.button.set_visible(on || known);
    }

    /// Runs one daemon command on a worker thread and hands the reply back on the main loop.
    fn command(self: &Rc<Self>, line: String, done: impl FnOnce(&Rc<Self>, Result<Reply, String>) + 'static) {
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || daemon::command(&line))
                .await
                .unwrap_or_else(|_| Err("command thread failed".into()));
            if let Some(ui) = weak.upgrade() {
                done(&ui, result);
            }
        });
    }

    fn toast_reply(&self, result: &Result<Reply, String>) {
        match result {
            Ok(r) if !r.message.is_empty() => self.toast(&r.message),
            Ok(r) => self.toast(if r.ok { "Done" } else { "Failed" }),
            Err(e) => self.toast(e),
        }
    }

    fn set_down(self: &Rc<Self>) {
        // While the daemon is still coming up (we just started it), keep the "Starting…" spinner
        // rather than flashing the "not running" page on each failed reconnect.
        if self.state.borrow().starting {
            self.refresh_tray();
            return;
        }
        self.apps.set_connected(false);
        self.display.set_connected(false);
        self.cross.set_connected(false);
        self.files.set_connected(false);
        {
            let mut st = self.state.borrow_mut();
            st.status = None;
            st.busy = false;
            st.muted = None; // ask again once the daemon is back
            // The phone can't be connected without the daemon: it was last seen now.
            if std::mem::take(&mut st.was_connected) {
                st.last.last_connected = Some(lastdevice::now_secs());
                if let Err(e) = lastdevice::save(&st.last) {
                    eprintln!("smartconnect-gui: could not save the last device: {e}");
                }
            }
        }
        self.root.set_visible_child_name("down");
        self.show_switcher(false);
        self.refresh_down();
        self.refresh_device_offline();
        self.refresh_tray();
    }

    fn apply_status(self: &Rc<Self>, status: Status) {
        {
            let mut st = self.state.borrow_mut();
            if !st.logged_first {
                st.logged_first = true;
                eprintln!(
                    "smartconnect-gui: first status from smartconnectd (connected={}, paired={}, {} notifications)",
                    status.connected,
                    status.paired,
                    status.notifications.len()
                );
            }
            if st.show_qr {
                if let Some(qr) = status.pairing_qr.clone() {
                    st.qr_path = Some(qr);
                }
            }
            st.starting = false; // the socket answered
            st.status = Some(status);
        }
        self.remember_device();
        self.root.set_visible_child_name("main");
        self.show_switcher(true);
        let (connected, muted_known) = {
            let st = self.state.borrow();
            (st.status.as_ref().is_some_and(|s| s.connected), st.muted.is_some())
        };
        self.apps.set_connected(connected);
        self.display.set_connected(connected);
        self.cross.set_connected(connected);
        self.files.set_connected(connected);
        self.refresh_phone();
        self.refresh_notifications();
        self.refresh_pair();
        self.refresh_tray();
        if !muted_known {
            self.load_muted();
        }
    }

    // ── Phone page + device menu ────────────────────────────────────────────

    /// Folds the current status into the persisted last-device cache: absorbs a connected phone's
    /// name/stats (copying its wallpaper when it changes), and stamps + saves on a disconnect.
    fn remember_device(&self) {
        let mut guard = self.state.borrow_mut();
        let st = &mut *guard;
        let Some(s) = st.status.as_ref() else { return };
        let now = lastdevice::now_secs();
        let save = if s.connected {
            let mut next = st.last.clone();
            next.absorb(s);
            if s.wallpaper.is_some() && s.wallpaper != st.copied_wallpaper {
                if let Some(copy) = s.wallpaper_path().and_then(|p| lastdevice::copy_wallpaper(Path::new(p))) {
                    next.wallpaper = Some(copy);
                }
                st.copied_wallpaper = s.wallpaper.clone();
            }
            let save = !next.same_fields(&st.last) || !st.was_connected || now.saturating_sub(st.last_saved) >= 300;
            next.last_connected = Some(now);
            st.last = next;
            st.was_connected = true;
            save
        } else if st.was_connected {
            st.last.last_connected = Some(now);
            st.was_connected = false;
            true
        } else {
            false
        };
        if save {
            st.last_saved = now;
            if let Err(e) = lastdevice::save(&st.last) {
                eprintln!("smartconnect-gui: could not save the last device: {e}");
            }
        }
    }

    fn refresh_phone(&self) {
        let mut st = self.state.borrow_mut();
        let busy = st.busy;
        let pending = st.mirror_pending;
        let mirroring = st.mirror.as_ref().is_some_and(Mirror::running) || self.apps.mirror_session().is_some();
        let Some(s) = st.status.clone() else { return };
        let last = st.last.clone();
        let p = &self.phone;
        let state = if s.connected {
            PhoneState::Connected
        } else if s.paired {
            PhoneState::Disconnected
        } else {
            PhoneState::Unpaired
        };

        // Wallpaper: the daemon's file while it exists, else our cached copy of the last one.
        let wall = s
            .wallpaper
            .clone()
            .filter(|_| s.wallpaper_path().is_some_and(|p| Path::new(p).is_file()))
            .or_else(|| last.wallpaper_path().map(str::to_owned))
            .filter(|_| state != PhoneState::Unpaired);
        if wall != st.wallpaper {
            st.wallpaper = wall.clone();
            show_wallpaper(&p.wallpaper, &p.wallpaper_stack, wall.as_deref());
        }
        drop(st);
        set_dimmed(&p.thumb, state != PhoneState::Connected);

        // Name and state line. A daemon restart forgets the name until the phone reconnects: use ours.
        let name = s.name.clone().filter(|n| !n.is_empty()).or_else(|| last.name.clone());
        let model = s.model.clone().filter(|m| !m.is_empty()).or_else(|| last.model.clone());
        let display_name = match state {
            PhoneState::Unpaired => "No phone paired".to_owned(),
            _ => name.clone().or_else(|| model.clone()).unwrap_or_else(|| "Phone".to_owned()),
        };
        p.name.set_label(&display_name);
        let seen = last.last_seen();
        let line = match state {
            PhoneState::Connected => "Connected over Wi-Fi".to_owned(),
            PhoneState::Disconnected if busy => "Connecting…".to_owned(),
            PhoneState::Disconnected => seen.as_ref().map_or_else(|| "Disconnected".to_owned(), |t| format!("Disconnected · {t}")),
            PhoneState::Unpaired => "Pair your Motorola phone with Smart Connect to get started".to_owned(),
        };
        p.subtitle.set_label(&match model.as_deref().filter(|m| state != PhoneState::Unpaired && Some(*m) != name.as_deref()) {
            Some(model) => format!("{model} · {line}"),
            None => line,
        });

        // Inline stats: live while connected, the last known values (dimmed) while away.
        let values = if state == PhoneState::Connected {
            let mut v = LastDevice::default();
            v.absorb(&s);
            Some(v)
        } else {
            Some(last.clone()).filter(|l| state == PhoneState::Disconnected && l.known())
        };
        p.stats.set_visible(values.is_some());
        if let Some(v) = values {
            self.fill_stats(&v, state == PhoneState::Connected);
        }
        set_dimmed(&p.stats, state != PhoneState::Connected);

        // Unread shortcut.
        let unread = s.unread.unwrap_or(0).max(0);
        p.unread_card.set_visible(state == PhoneState::Connected && unread > 0);
        p.unread_label.set_label(&match unread {
            1 => "1 unread message".to_owned(),
            n => format!("{n} unread messages"),
        });

        // The card's action: Mirror screen (connected, or a mirror still open), Connect (phone
        // away), or Pair a phone (none paired).
        p.mirror.set_visible(state == PhoneState::Connected || mirroring);
        p.mirror.set_sensitive(!pending && (s.connected || mirroring));
        p.mirror.set_label(if pending {
            "Starting…"
        } else if mirroring {
            "Stop mirroring"
        } else {
            "Mirror screen"
        });
        if mirroring {
            p.mirror.add_css_class("destructive-action");
            p.mirror.remove_css_class("suggested-action");
        } else {
            p.mirror.remove_css_class("destructive-action");
            p.mirror.add_css_class("suggested-action");
        }
        p.connect.set_visible(state == PhoneState::Disconnected && !mirroring);
        p.connect.set_sensitive(!busy);
        p.connect_spinner.set_visible(busy);
        p.connect_label.set_label(if busy { "Connecting…" } else { "Connect" });
        p.pair.set_visible(state == PhoneState::Unpaired);

        // Cross Control stays visible for a paired phone (its page says when to connect).
        p.cross_group.set_visible(s.paired);

        // Device menu button + popover.
        let d = &self.device;
        d.label.set_label(&display_name);
        d.dot.set_css_classes(if s.connected { &["success"] } else { &["dim-label"] });
        d.menu_name.set_label(&display_name);
        d.menu_sub.set_label(&match state {
            PhoneState::Connected => match s.battery {
                Some(b) => format!("Wi-Fi · {b}%"),
                None => "Wi-Fi".to_owned(),
            },
            PhoneState::Disconnected if busy => "Connecting…".to_owned(),
            PhoneState::Disconnected => seen.map_or_else(|| "Disconnected".to_owned(), |t| format!("Disconnected · {t}")),
            PhoneState::Unpaired => "No phone paired".to_owned(),
        });
        d.check.set_visible(s.connected);
        d.pair.set_sensitive(true);
        d.connect.set_visible(s.paired);
        d.connect.set_sensitive(!busy);
        if let Some(label) = d.connect.child().and_downcast::<gtk::Label>() {
            label.set_label(if busy {
                "Working…"
            } else if s.connected {
                "Disconnect"
            } else {
                "Connect"
            });
        }
    }

    /// Fills the inline battery / Wi-Fi / mobile / Bluetooth row from `v`.
    fn fill_stats(&self, v: &LastDevice, live: bool) {
        let p = &self.phone;
        let pre = if live { "" } else { "Last known · " };
        match v.battery {
            Some(b) => {
                let level = (b.clamp(0, 100) + 5) / 10 * 10;
                let charging = v.charging && live;
                // Adwaita names: battery-level-N[-charging]-symbolic, and -100-charged when full.
                let icon = match (level, charging) {
                    (100, true) => "battery-level-100-charged-symbolic".to_owned(),
                    (l, true) => format!("battery-level-{l}-charging-symbolic"),
                    (l, false) => format!("battery-level-{l}-symbolic"),
                };
                let tip = format!("{pre}Battery: {b}%{}", if charging { ", charging" } else { "" });
                p.battery.set(&icon, &format!("{b}%"), &tip);
            }
            None => p.battery.set("battery-missing-symbolic", "—", &format!("{pre}Battery: unknown")),
        }
        let wifi_word = signal_text(v.wifi_signal);
        match signal_level(v.wifi_signal).filter(|_| v.wifi) {
            Some(l) => p.wifi.set(
                &format!("network-wireless-signal-{}-symbolic", SIGNAL_ICONS[l]),
                wifi_word,
                &format!("{pre}Wi-Fi: {}", wifi_word.to_lowercase()),
            ),
            None if v.wifi => p.wifi.set("network-wireless-symbolic", "On", &format!("{pre}Wi-Fi: on")),
            None => p.wifi.set("network-wireless-offline-symbolic", "Off", &format!("{pre}Wi-Fi: off")),
        }
        let cell_word = signal_text(v.cell_signal);
        let cell_icon = signal_level(v.cell_signal).map_or("none", |l| SIGNAL_ICONS[l]);
        p.cell.set(
            &format!("network-cellular-signal-{cell_icon}-symbolic"),
            cell_word,
            &format!("{pre}Mobile signal: {}", cell_word.to_lowercase().replace('—', "unknown")),
        );
        if v.bluetooth {
            p.bluetooth.set("bluetooth-active-symbolic", "On", &format!("{pre}Bluetooth: on"));
        } else {
            p.bluetooth.set("bluetooth-disabled-symbolic", "Off", &format!("{pre}Bluetooth: off"));
        }
    }

    /// The header device menu while the daemon is down (or starting): the remembered phone with a
    /// grey dot, and "Start service" in place of Connect/Disconnect.
    fn refresh_device_offline(&self) {
        let st = self.state.borrow();
        let d = &self.device;
        let name = st.last.name.clone().or_else(|| st.last.model.clone()).unwrap_or_else(|| "Smart Connect".to_owned());
        let starting = st.starting || st.service_starting;
        d.label.set_label(&name);
        d.menu_name.set_label(&name);
        d.dot.set_css_classes(&["dim-label"]);
        d.menu_sub.set_label(if starting { "Starting the Smart Connect service…" } else { "Smart Connect service isn't running" });
        d.check.set_visible(false);
        d.pair.set_sensitive(false);
        d.connect.set_visible(true);
        d.connect.set_sensitive(!starting);
        if let Some(label) = d.connect.child().and_downcast::<gtk::Label>() {
            label.set_label(if starting { "Starting…" } else { "Start service" });
        }
        d.button.set_visible(st.status.is_some() || st.last.known());
    }

    /// The "service isn't running" page's last-device card.
    fn refresh_down(&self) {
        let mut st = self.state.borrow_mut();
        let last = st.last.clone();
        self.down.card.set_visible(last.known());
        self.down.name.set_label(last.name.as_deref().or(last.model.as_deref()).unwrap_or("Your phone"));
        self.down.sub.set_label(&last.last_seen().map_or_else(|| "Not connected yet".to_owned(), |t| capitalize(&t)));
        let wall = last.wallpaper_path().map(str::to_owned);
        if wall != st.down_wallpaper {
            st.down_wallpaper = wall.clone();
            show_wallpaper(&self.down.wallpaper, &self.down.wallpaper_stack, wall.as_deref());
        }
    }

    fn toggle_connection(self: &Rc<Self>) {
        let connected = {
            let mut st = self.state.borrow_mut();
            if st.busy {
                return;
            }
            st.busy = true;
            st.status.as_ref().is_some_and(|s| s.connected)
        };
        self.refresh_phone();
        let cmd = if connected { "disconnect" } else { "connect" };
        self.command(cmd.into(), move |ui, result| {
            ui.state.borrow_mut().busy = false;
            match &result {
                // Say plainly that it failed, with the daemon's reason (e.g. the phone isn't reachable).
                Ok(r) if !r.ok => {
                    let why = if r.message.is_empty() { "no reason given" } else { r.message.as_str() };
                    ui.toast(&format!("Couldn't {cmd}: {why}"));
                }
                _ => ui.toast_reply(&result),
            }
            ui.refresh_phone();
        });
    }

    /// Mirror Screen goes through the stream host (started on demand), which opens the mirror as
    /// one more session window; a standalone viewer child is only the fallback when the host cannot
    /// start. (A viewer spawned while a host runs just hands off and exits, which used to look like
    /// a failed/stopped mirror.)
    fn toggle_mirror(self: &Rc<Self>) {
        if let Some(sid) = self.apps.mirror_session() {
            let weak = Rc::downgrade(self);
            glib::spawn_future_local(async move {
                let result = gio::spawn_blocking(move || apps::close(sid)).await.unwrap_or_else(|_| Err("close thread failed".into()));
                let Some(ui) = weak.upgrade() else { return };
                if let Err(e) = result {
                    ui.toast(&e);
                }
                ui.apps.poll_soon();
            });
            return;
        }
        {
            let mut st = self.state.borrow_mut();
            if st.mirror_pending {
                return;
            }
            if let Some(mirror) = st.mirror.as_mut().filter(|m| m.running()) {
                mirror.stop();
                drop(st);
                self.refresh_phone();
                return;
            }
            st.mirror_pending = true;
        }
        self.refresh_phone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(apps::start_mirror).await.unwrap_or_else(|_| Err("mirror thread failed".into()));
            let Some(ui) = weak.upgrade() else { return };
            ui.state.borrow_mut().mirror_pending = false;
            match result {
                Ok(_) => {
                    ui.toast("Mirroring opens in its own window");
                    ui.apps.poll_soon();
                }
                Err(e) => {
                    eprintln!("smartconnect-gui: stream host unavailable ({e}); running the viewer standalone");
                    ui.start_viewer();
                }
            }
            ui.refresh_phone();
        });
    }

    /// Fallback: run `smartconnect-mirror` (no args) as our own child.
    fn start_viewer(self: &Rc<Self>) {
        let started = {
            let mut st = self.state.borrow_mut();
            let mirror = st.mirror.get_or_insert_with(Mirror::viewer);
            match mirror.start(&[]) {
                Ok(()) => true,
                Err(e) => {
                    drop(st);
                    self.toast(&e);
                    false
                }
            }
        };
        if started {
            self.watch_mirror();
        }
    }

    /// Polls the viewer process until it exits, then resets the button.
    fn watch_mirror(self: &Rc<Self>) {
        let weak: Weak<Self> = Rc::downgrade(self);
        glib::timeout_add_local(Duration::from_millis(500), move || {
            let Some(ui) = weak.upgrade() else { return glib::ControlFlow::Break };
            let exit = ui.state.borrow_mut().mirror.as_mut().and_then(Mirror::poll);
            match exit {
                None => glib::ControlFlow::Continue,
                Some(exit) => {
                    if let Exit::Failed(msg) = exit {
                        ui.toast(&msg);
                    }
                    // A clean exit may be a hand-off to a host that appeared meanwhile.
                    ui.apps.poll_soon();
                    ui.refresh_phone();
                    glib::ControlFlow::Break
                }
            }
        });
    }

    // ── Notifications page ──────────────────────────────────────────────────

    fn refresh_notifications(self: &Rc<Self>) {
        let raw = match &self.state.borrow().status {
            Some(s) => s.notifications.clone(),
            None => return,
        };
        let connected = self.state.borrow().status.as_ref().is_some_and(|s| s.connected);

        // Drop dismissed keys that the phone no longer reports, then hide dismissed ones.
        let notifications: Vec<Notification> = {
            let mut st = self.state.borrow_mut();
            let present: HashSet<&String> = raw.iter().map(|n| &n.key).collect();
            st.dismissed.retain(|k| present.contains(k));
            raw.iter().filter(|n| !st.dismissed.contains(&n.key)).cloned().collect()
        };

        let count = u32::try_from(notifications.len()).unwrap_or(u32::MAX);
        self.notif_page.set_badge_number(count);
        self.notif_page.set_needs_attention(count > 0);
        self.notifs.empty.set_visible(notifications.is_empty());
        self.notifs.today.set_visible(!notifications.is_empty());
        self.notifs.list.set_visible(!notifications.is_empty());

        // Rows are kept per key (and re-ordered) so a half-typed reply survives status updates.
        let mut st = self.state.borrow_mut();
        for n in notifications.iter().filter(|n| !n.pkg.is_empty() && !n.app.is_empty()) {
            st.app_names.insert(n.pkg.clone(), n.app.clone());
        }
        st.rows.retain(|key, row| {
            let keep = notifications.iter().any(|n| &n.key == key);
            if !keep {
                self.notifs.list.remove(&row.row);
            }
            keep
        });
        for n in &notifications {
            if !st.rows.contains_key(&n.key) {
                let row = self.notification_row(n);
                st.rows.insert(n.key.clone(), row);
            }
            let muted = st.muted.as_ref().is_some_and(|m| m.contains(&n.pkg));
            let row = st.rows.get_mut(&n.key).expect("row just inserted");
            if row.data != *n {
                row.update(n, &self.apps.icon_query(&n.pkg, &n.app));
            }
            row.send.set_sensitive(connected);
            row.entry.set_sensitive(connected);
            row.open_item.set_sensitive(connected);
            row.set_muted(muted);
        }
        // Re-insert in daemon order (newest first) only when the order actually changed.
        let mut child = self.notifs.list.first_child();
        let in_order = notifications.iter().all(|n| {
            let ok = child.as_ref() == Some(st.rows[&n.key].row.upcast_ref());
            child = child.as_ref().and_then(|c| c.next_sibling());
            ok
        });
        if !in_order {
            for n in &notifications {
                let row = &st.rows[&n.key].row;
                if row.parent().is_some() {
                    self.notifs.list.remove(row);
                }
            }
            for n in &notifications {
                self.notifs.list.append(&st.rows[&n.key].row);
            }
        }
    }

    fn notification_row(self: &Rc<Self>, n: &Notification) -> NotifRow {
        let icon = IconSlot::new(32);
        icon.widget().set_valign(gtk::Align::Start);
        let header = gtk::Label::builder().xalign(0.0).wrap(true).hexpand(true).css_classes(["heading"]).build();
        let sub = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["caption", "dim-label"]).build();
        let name_col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).hexpand(true).build();
        name_col.append(&header);
        name_col.append(&sub);

        // ⋯ menu: Open on phone / Dismiss / Mute <app>.
        let open_item = menu_item("Open on phone");
        let dismiss_item = menu_item("Dismiss");
        let mute_item = menu_item("Mute");
        let menu_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).build();
        menu_box.append(&open_item);
        menu_box.append(&dismiss_item);
        menu_box.append(&mute_item);
        let menu_pop = gtk::Popover::builder().child(&menu_box).width_request(180).build();
        let menu = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .valign(gtk::Align::Start)
            .css_classes(["flat", "circular"])
            .popover(&menu_pop)
            .build();

        let header_box = gtk::Box::builder().spacing(10).build();
        header_box.append(icon.widget());
        header_box.append(&name_col);
        header_box.append(&menu);

        let body = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .selectable(true)
            .build();
        let entry = gtk::Entry::builder().hexpand(true).placeholder_text("Reply…").build();
        let send = gtk::Button::builder().icon_name("mail-send-symbolic").tooltip_text("Send reply").css_classes(["suggested-action"]).build();
        let reply_box = gtk::Box::builder().spacing(6).margin_top(6).css_classes(["linked"]).build();
        reply_box.append(&entry);
        reply_box.append(&send);
        let column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .css_classes(["sc-notif"])
            .build();
        column.append(&header_box);
        column.append(&body);
        column.append(&reply_box);
        let row = gtk::ListBoxRow::builder().activatable(false).child(&column).build();

        // ── ⋯ menu handlers (read the current notification from state by key) ──
        let key = n.key.clone();
        let weak = Rc::downgrade(self);
        let pop = menu_pop.downgrade();
        open_item.connect_clicked(move |_| {
            if let Some(p) = pop.upgrade() {
                p.popdown();
            }
            let Some(ui) = weak.upgrade() else { return };
            let data = ui.state.borrow().rows.get(&key).map(|r| r.data.clone());
            if let Some(n) = data {
                ui.open_on_phone(&n);
            }
        });
        let key = n.key.clone();
        let weak = Rc::downgrade(self);
        let pop = menu_pop.downgrade();
        dismiss_item.connect_clicked(move |_| {
            if let Some(p) = pop.upgrade() {
                p.popdown();
            }
            if let Some(ui) = weak.upgrade() {
                ui.dismiss_notification(&key);
            }
        });
        let key = n.key.clone();
        let weak = Rc::downgrade(self);
        let pop = menu_pop.downgrade();
        mute_item.connect_clicked(move |_| {
            if let Some(p) = pop.upgrade() {
                p.popdown();
            }
            let Some(ui) = weak.upgrade() else { return };
            let data = ui.state.borrow().rows.get(&key).map(|r| r.data.clone());
            if let Some(n) = data.filter(|n| !n.pkg.is_empty()) {
                let muted = ui.state.borrow().muted.as_ref().is_some_and(|m| m.contains(&n.pkg));
                ui.set_app_muted(&n.pkg, !muted);
            }
        });

        // ── Reply ──
        let key = n.key.clone();
        let weak = Rc::downgrade(self);
        let entry_ref = entry.downgrade();
        let send_reply = move || {
            let (Some(ui), Some(entry)) = (weak.upgrade(), entry_ref.upgrade()) else { return };
            let text = entry.text().trim().to_owned();
            if text.is_empty() {
                return;
            }
            entry.set_sensitive(false);
            let entry_weak = entry.downgrade();
            ui.command(daemon::reply_line(&key, &text), move |ui, result| {
                if let Some(entry) = entry_weak.upgrade() {
                    entry.set_sensitive(true);
                    if matches!(result, Ok(Reply { ok: true, .. })) {
                        entry.set_text("");
                    }
                }
                ui.toast_reply(&result);
            });
        };
        let send_reply = Rc::new(send_reply);
        let f = send_reply.clone();
        entry.connect_activate(move |_| f());
        send.connect_clicked(move |_| send_reply());

        let mut r = NotifRow { row, icon, header, sub, body, reply_box, entry, send, menu, open_item, mute_item, data: Notification::default() };
        r.update(n, &self.apps.icon_query(&n.pkg, &n.app));
        r.set_muted(false);
        r
    }

    /// "Open on phone": forward the notification's own click to the phone.
    fn open_on_phone(self: &Rc<Self>, n: &Notification) {
        if !self.state.borrow().status.as_ref().is_some_and(|s| s.connected) {
            self.toast("The phone is not connected");
            return;
        }
        self.command(daemon::notif_open_line(&n.key, n.service), |ui, result| ui.toast_reply(&result));
    }

    /// Clears one notification from this PC's list (keeps it on the phone).
    fn dismiss_notification(self: &Rc<Self>, key: &str) {
        self.state.borrow_mut().dismissed.insert(key.to_owned());
        self.refresh_notifications();
    }

    /// Clears every currently shown notification from this PC's list.
    fn clear_all_notifications(self: &Rc<Self>) {
        {
            let mut st = self.state.borrow_mut();
            let keys: Vec<String> = match &st.status {
                Some(s) => s.notifications.iter().map(|n| n.key.clone()).collect(),
                None => return,
            };
            st.dismissed.extend(keys);
        }
        self.refresh_notifications();
    }

    // ── Per-app mute ────────────────────────────────────────────────────────

    /// Asks the daemon for the muted packages (once per daemon connection, and when the page shows).
    fn load_muted(self: &Rc<Self>) {
        {
            let mut st = self.state.borrow_mut();
            if st.muted_loading || st.status.is_none() {
                return;
            }
            st.muted_loading = true;
        }
        self.command(daemon::mute_line(None), |ui, result| {
            ui.state.borrow_mut().muted_loading = false;
            let list = result.as_ref().ok().and_then(Reply::muted).unwrap_or_else(|| {
                // An older daemon: don't ask again on every status push (showing the page still does).
                eprintln!("smartconnect-gui: no muted-app list from smartconnectd");
                Vec::new()
            });
            ui.set_muted_list(list);
        });
    }

    fn app_name(&self, pkg: &str) -> String {
        let known = self.state.borrow().app_names.get(pkg).cloned();
        known.or_else(|| self.apps.app_name(pkg)).unwrap_or_else(|| pkg.to_owned())
    }

    /// Mutes or unmutes the desktop popups of one phone app (the daemon keeps the list).
    fn set_app_muted(self: &Rc<Self>, pkg: &str, mute: bool) {
        let name = self.app_name(pkg);
        self.command(daemon::mute_line(Some((pkg, mute))), move |ui, result| match &result {
            Ok(reply) if reply.ok => {
                if let Some(list) = reply.muted() {
                    ui.set_muted_list(list);
                }
                ui.toast(&if mute { format!("{name} muted: no more desktop popups from it") } else { format!("{name} unmuted") });
            }
            _ => {
                ui.toast_reply(&result);
                ui.refresh_notifications(); // put the toggle back to the daemon's state
            }
        });
    }

    fn set_muted_list(self: &Rc<Self>, mut list: Vec<String>) {
        list.sort();
        if self.state.borrow().muted.as_ref() == Some(&list) {
            self.refresh_notifications(); // a toggle clicked without effect goes back
            return;
        }
        let old_rows = std::mem::take(&mut self.state.borrow_mut().muted_rows);
        for row in old_rows {
            self.notifs.muted.remove(&row);
        }
        let mut rows = Vec::new();
        for pkg in &list {
            let name = self.app_name(pkg);
            let row = adw::ActionRow::builder().title(&name).use_markup(false).build();
            if name != *pkg {
                row.set_subtitle(pkg);
            }
            let unmute = gtk::Button::builder().label("Unmute").valign(gtk::Align::Center).build();
            let weak = Rc::downgrade(self);
            let pkg = pkg.clone();
            unmute.connect_clicked(move |b| {
                b.set_sensitive(false);
                if let Some(ui) = weak.upgrade() {
                    ui.set_app_muted(&pkg, false);
                }
            });
            row.add_suffix(&unmute);
            self.notifs.muted.add_row(&row);
            rows.push(row);
        }
        self.notifs.muted.set_subtitle(&match list.len() {
            1 => "1 app shows no desktop popups".to_owned(),
            n => format!("{n} apps show no desktop popups"),
        });
        self.notifs.muted_group.set_visible(!list.is_empty());
        {
            let mut st = self.state.borrow_mut();
            st.muted_rows = rows;
            st.muted = Some(list);
        }
        self.refresh_notifications();
    }

    // ── Pair dialog ─────────────────────────────────────────────────────────

    /// Opens the pairing dialog and asks for a fresh QR.
    fn open_pair_dialog(self: &Rc<Self>) {
        self.pair.dialog.present(Some(&self.window));
        self.request_pairing();
    }

    fn request_pairing(self: &Rc<Self>) {
        self.pair.button.set_sensitive(false);
        self.command("pair".into(), |ui, result| {
            ui.pair.button.set_sensitive(true);
            if let Ok(reply) = &result {
                if reply.ok {
                    let mut st = ui.state.borrow_mut();
                    st.show_qr = true;
                    st.qr_path = reply
                        .raw
                        .get("qr")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| st.status.as_ref().and_then(|s| s.pairing_qr.clone()));
                    drop(st);
                    // Force a reload: the daemon may rewrite the same SVG path.
                    ui.pair.qr.set_filename(None::<&std::path::Path>);
                } else {
                    ui.toast_reply(&result);
                }
            } else {
                ui.toast_reply(&result);
            }
            ui.refresh_pair();
        });
    }

    fn refresh_pair(&self) {
        let st = self.state.borrow();
        let path = st.qr_path.as_deref().filter(|_| st.show_qr).map(|p| p.split('?').next().unwrap_or(p));
        match path {
            Some(path) => {
                let current = self.pair.qr.file().and_then(|f| f.path());
                if current.as_deref() != Some(std::path::Path::new(path)) {
                    self.pair.qr.set_filename(Some(path));
                }
                self.pair.qr_box.set_visible(true);
                self.pair.spinner.set_visible(true);
                self.pair.status.set_label("Waiting for phone… the phone connects automatically once pairing finishes.");
            }
            None => {
                self.pair.qr_box.set_visible(false);
                self.pair.spinner.set_visible(true);
                self.pair.status.set_label("Requesting a pairing code…");
            }
        }
    }
}

impl NotifRow {
    /// `icon` is the app's icon query (from the app list when the app is in it; see `AppsPage::icon_query`).
    fn update(&mut self, n: &Notification, icon: &Query) {
        self.icon.set(icon);
        // The sender is the notification title; the app name is the second line.
        let (header, sub) = match (n.title.is_empty(), n.app.is_empty()) {
            (false, _) => (n.title.clone(), n.app.clone()),
            (true, false) => (n.app.clone(), String::new()),
            (true, true) => (String::new(), String::new()),
        };
        self.header.set_label(&header);
        self.sub.set_label(&sub);
        self.sub.set_visible(!sub.is_empty());
        self.body.set_label(&n.body);
        self.body.set_visible(!n.body.is_empty());
        self.reply_box.set_visible(n.replyable);
        self.mute_item.set_visible(!n.pkg.is_empty());
        self.data = n.clone();
    }

    /// Shows the daemon's mute state on the ⋯ menu item (a click asks for the opposite).
    fn set_muted(&self, muted: bool) {
        let app = if self.data.app.is_empty() { "this app" } else { self.data.app.as_str() };
        if let Some(label) = self.mute_item.child().and_downcast::<gtk::Label>() {
            label.set_label(&if muted { format!("Unmute {app}") } else { format!("Mute {app}") });
        }
        self.menu.set_tooltip_text(Some(if muted { "Muted · more actions" } else { "More actions" }));
    }
}
