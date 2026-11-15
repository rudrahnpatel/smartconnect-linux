//! Smart Connect for Linux: GTK4/libadwaita front-end for `smartconnectd`.

mod apps;
mod daemon;
mod crosscontrol;
mod display;
mod messages;
mod files;
mod icons;
mod lastdevice;
mod lifecycle;
mod mirror;
mod prefs;
mod setup;
mod share;
mod tray;
mod window;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, glib};

const APP_ID: &str = "dev.smartconnect.Linux";

// Structural CSS only — spacing and radius, never colours. Colours come from the user's theme via
// libadwaita style classes (.card, .dim-label, .error, …). The one exception is the QR code's white
// backdrop, which is functional: a QR must stay dark-on-light to scan under any theme.
const CSS: &str = "
picture.wallpaper { border-radius: 12px; }
picture.qr { background: white; border-radius: 12px; padding: 12px; }
.sc-card { padding: 16px; }
.sc-mode { padding: 16px 10px; }
.sc-tile { padding: 10px; }
.sc-chip { padding: 6px 10px; }
.sc-dimmed { opacity: 0.55; }
.sc-badge { padding: 1px 8px; border-radius: 9999px; min-height: 0; }
.sc-notif { padding: 12px; }
";

fn main() -> glib::ExitCode {
    // `--background` starts the app hidden (tray only), for the autostart entry. GApplication never
    // sees the flag: we strip it and forward the rest of argv.
    let mut forwarded: Vec<String> = Vec::new();
    let mut background = false;
    for arg in std::env::args() {
        if arg == "--background" {
            background = true;
        } else {
            forwarded.push(arg);
        }
    }

    let app = adw::Application::builder().application_id(APP_ID).build();
    let ui: Rc<RefCell<Option<Rc<window::Ui>>>> = Rc::default();
    // Only the first activation honours `--background`; launching the app again always shows it.
    let first_activation = Rc::new(Cell::new(true));

    app.connect_startup(|_| {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(CSS);
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        }
    });

    let slot = ui.clone();
    app.connect_activate(move |app| {
        let is_first = first_activation.replace(false);
        // Started from the AppImage file: set it up first (or open the installed copy).
        if is_first && slot.borrow().is_none() {
            if setup::installed_same_build() {
                setup::open_installed_and_quit(app);
                return;
            }
            if let Some((appimage, update)) = setup::pending_install() {
                let (slot, app_c) = (slot.clone(), app.clone());
                setup::welcome(app, appimage, update, move || {
                    let ui = window::Ui::new(&app_c);
                    ui.start();
                    ui.present();
                    *slot.borrow_mut() = Some(ui);
                });
                return;
            }
        }
        let mut slot = slot.borrow_mut();
        let ui = slot.get_or_insert_with(|| {
            let ui = window::Ui::new(app);
            ui.start();
            ui
        });
        // Stay hidden to the tray only when launched with `--background` on the very first
        // activation; any later launch (or a normal launch) presents the window.
        if !(is_first && background) {
            ui.present();
        }
    });

    let slot = ui.clone();
    app.connect_shutdown(move |_| {
        if let Some(ui) = slot.borrow().as_ref() {
            ui.shutdown();
        }
    });

    app.run_with_args(&forwarded)
}
