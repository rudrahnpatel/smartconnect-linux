//! Messages page: reads the daemon's in-memory SMS cache and sends through service 204.
//! Nothing is written to disk; the phone remains source of truth.

use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use serde::Deserialize;
use serde_json::json;

use crate::daemon;

#[derive(Clone, Debug, Deserialize)]
struct Thread {
    thread: String,
    address: String,
    name: Option<String>,
    count: usize,
    unread: usize,
    last_body: String,
}

#[derive(Clone, Debug, Deserialize)]
struct Message {
    id: i64,
    message_type: i64,
    body: String,
    read: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct ThreadsReply {
    enabled: bool,
    threads: Vec<Thread>,
}

#[derive(Clone, Debug, Deserialize)]
struct ThreadReply {
    messages: Vec<Message>,
}

pub struct MessagesPage {
    pub refresh: gtk::Button,
    status: gtk::Label,
    list: gtk::ListBox,
}

impl MessagesPage {
    pub fn build() -> (Rc<Self>, gtk::Widget) {
        let refresh = gtk::Button::builder()
            .label("Refresh")
            .icon_name("view-refresh-symbolic")
            .build();
        let status = gtk::Label::builder()
            .label("Connect phone, then refresh Messages.")
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label"])
            .build();
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        let header = gtk::Box::builder().spacing(12).build();
        header.append(
            &gtk::Label::builder()
                .label("Messages")
                .xalign(0.0)
                .hexpand(true)
                .css_classes(["title-2"])
                .build(),
        );
        header.append(&refresh);
        let column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(24)
            .margin_bottom(24)
            .margin_start(12)
            .margin_end(12)
            .build();
        column.append(&header);
        column.append(&status);
        column.append(&list);
        let page = Rc::new(Self {
            refresh,
            status,
            list,
        });
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&column)
            .build();
        (page, scroll.upcast())
    }

    pub fn refresh(self: &Rc<Self>) {
        self.refresh.set_sensitive(false);
        self.status.set_label("Syncing messages from phone…");
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(|| daemon::command(r#"{"cmd":"sms_threads"}"#)).await;
            let Some(page) = weak.upgrade() else { return };
            page.refresh.set_sensitive(true);
            match result {
                Ok(Ok(reply)) if reply.ok => {
                    match serde_json::from_value::<ThreadsReply>(reply.raw) {
                        Ok(data) => page.show_threads(data),
                        Err(_) => page
                            .status
                            .set_label("Phone returned an invalid Messages response."),
                    }
                }
                Ok(Ok(reply)) => page.status.set_label(&reply.message),
                Ok(Err(e)) => page.status.set_label(&e),
                Err(_) => page.status.set_label("Messages request worker failed."),
            }
        });
    }

    fn show_threads(self: &Rc<Self>, data: ThreadsReply) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        if !data.enabled {
            self.status.set_label(
                "Enable Messages permission in Smart Connect on phone, then press Refresh.",
            );
        } else if data.threads.is_empty() {
            self.status
                .set_label("No recent SMS conversations on phone.");
        } else {
            self.status
                .set_label("Messages stay on phone. This view keeps no local history.");
        }
        for thread in data.threads {
            let title = thread
                .name
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| thread.address.clone());
            let detail = if thread.unread > 0 {
                format!(
                    "{} · {} unread · {} messages",
                    thread.last_body, thread.unread, thread.count
                )
            } else {
                format!("{} · {} messages", thread.last_body, thread.count)
            };
            let row = adw::ActionRow::builder()
                .title(title)
                .subtitle(detail)
                .activatable(true)
                .build();
            if thread.unread > 0 {
                row.add_suffix(
                    &gtk::Label::builder()
                        .label(&thread.unread.to_string())
                        .css_classes(["sc-badge", "accent"])
                        .build(),
                );
            }
            let key = thread.thread.clone();
            let address = thread.address.clone();
            let weak = Rc::downgrade(self);
            row.connect_activated(move |_| {
                if let Some(page) = weak.upgrade() {
                    page.open_thread(key.clone(), address.clone());
                }
            });
            self.list.append(&row);
        }
    }

    fn open_thread(self: &Rc<Self>, key: String, address: String) {
        let dialog = adw::Dialog::builder()
            .title(&address)
            .content_width(560)
            .content_height(620)
            .build();
        let messages = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&messages)
            .build();
        let entry = gtk::Entry::builder()
            .placeholder_text("Write SMS")
            .hexpand(true)
            .build();
        let send = gtk::Button::builder()
            .label("Send")
            .css_classes(["suggested-action"])
            .build();
        let compose = gtk::Box::builder()
            .spacing(8)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        compose.append(&entry);
        compose.append(&send);
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        content.append(&scroll);
        content.append(&compose);
        dialog.set_child(Some(&content));

        let weak = Rc::downgrade(self);
        let key_load = key.clone();
        glib::spawn_future_local(async move {
            let line = json!({ "cmd": "sms_thread", "thread": key_load }).to_string();
            let result = gio::spawn_blocking(move || daemon::command(&line)).await;
            let Some(page) = weak.upgrade() else { return };
            if let Ok(Ok(reply)) = result {
                if reply.ok {
                    if let Ok(data) = serde_json::from_value::<ThreadReply>(reply.raw) {
                        for message in data.messages {
                            let side = if message.message_type == 2 {
                                gtk::Align::End
                            } else {
                                gtk::Align::Start
                            };
                            let label = gtk::Label::builder()
                                .label(&message.body)
                                .wrap(true)
                                .max_width_chars(52)
                                .xalign(0.0)
                                .halign(side)
                                .css_classes(["card", "sc-chip"])
                                .build();
                            messages.append(&label);
                            if !message.read && message.message_type == 1 {
                                let line =
                                    json!({ "cmd": "sms_mark_read", "id": message.id }).to_string();
                                std::thread::spawn(move || {
                                    let _ = daemon::command(&line);
                                });
                            }
                        }
                    }
                }
            }
            page.refresh();
        });

        let entry_for_send = entry.clone();
        let send_for_action = send.clone();
        let key_for_send = key.clone();
        let send_action = Rc::new(move || {
            let text = entry_for_send.text().trim().to_owned();
            if text.is_empty() {
                return;
            }
            entry_for_send.set_text("");
            send_for_action.set_sensitive(false);
            let line =
                json!({ "cmd": "sms_send", "thread": key_for_send, "text": text }).to_string();
            let send_button = send_for_action.clone();
            glib::spawn_future_local(async move {
                let _ = gio::spawn_blocking(move || daemon::command(&line)).await;
                send_button.set_sensitive(true);
            });
        });
        let action = send_action.clone();
        send.connect_clicked(move |_| action());
        let action = send_action.clone();
        entry.connect_activate(move |_| action());
        dialog.present(None::<&gtk::Widget>);
    }
}

#[cfg(test)]
mod tests {
    use super::ThreadsReply;

    #[test]
    fn parses_daemon_thread_reply() {
        let reply: ThreadsReply = serde_json::from_str(
            r#"{"enabled":true,"threads":[{"thread":"5551234567","address":"+1 555 123 4567","name":null,"count":2,"unread":1,"last_body":"hello"}]}"#,
        )
        .unwrap();
        assert!(reply.enabled);
        assert_eq!(reply.threads[0].thread, "5551234567");
        assert_eq!(reply.threads[0].unread, 1);
    }
}
