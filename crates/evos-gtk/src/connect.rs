//! File → Connect to ETP Server: searching another computer's index, such
//! as Everything on Windows or another EverythingOS, in a window of its
//! own. The login is checked before the window opens; recent servers are
//! remembered without their passwords.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use evos_proto::RemoteSearch;
use gtk::glib;

use crate::backend::{describe, Backend};
use crate::settings::Settings;

/// The form's fields as a connection.
fn target(host: &str, port: f64, username: &str, password: &str) -> Result<RemoteSearch, String> {
    let host = host.trim();
    if host.is_empty() {
        return Err("Enter the server's name or address".into());
    }
    // "user@host:port" typed into the host field works too.
    let mut t = RemoteSearch::parse(host).ok_or("Not a server name or address")?;
    if !host.contains('@') {
        t.username = username.trim().to_owned();
    }
    if t.password.is_empty() {
        t.password = password.to_owned();
    }
    let server = host.rsplit_once('@').map_or(host, |(_, h)| h);
    let has_port = server
        .rsplit_once(':')
        .is_some_and(|(h, p)| !h.contains(':') && p.parse::<u16>().is_ok());
    if !has_port {
        t.port = port as u16;
    }
    Ok(t)
}

/// Shows the dialog; `open` gets the connection once the server accepted
/// the login.
pub fn show(
    parent: &impl IsA<gtk::Widget>,
    backend: &Backend,
    settings: Rc<RefCell<Settings>>,
    open: impl Fn(RemoteSearch) + 'static,
) {
    let dialog = adw::Dialog::builder()
        .title("Connect to ETP Server")
        .content_width(460)
        .build();
    let server = adw::PreferencesGroup::builder()
        .description("Search the index of another computer running Everything or EverythingOS with its ETP server on.")
        .build();
    let host = adw::EntryRow::builder().title("Host").build();
    let port = adw::SpinRow::builder()
        .title("Port")
        .adjustment(&gtk::Adjustment::new(
            evos_proto::ETP_PORT as f64,
            1.0,
            65535.0,
            1.0,
            10.0,
            0.0,
        ))
        .build();
    let username = adw::EntryRow::builder().title("User name").build();
    let password = adw::PasswordEntryRow::builder().title("Password").build();
    for row in [
        host.upcast_ref::<gtk::Widget>(),
        port.upcast_ref(),
        username.upcast_ref(),
        password.upcast_ref(),
    ] {
        server.add(row);
    }
    let error = gtk::Label::builder()
        .wrap(true)
        .xalign(0.0)
        .visible(false)
        .css_classes(["error"])
        .build();
    error.update_property(&[gtk::accessible::Property::Label("Connection error")]);
    let page = adw::PreferencesPage::new();
    page.add(&server);

    let recent = settings.borrow().connections.clone();
    if !recent.is_empty() {
        let group = adw::PreferencesGroup::builder().title("Recent").build();
        for c in recent {
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&c))
                .activatable(true)
                .build();
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            let (h, p, u, pw) = (
                host.clone(),
                port.clone(),
                username.clone(),
                password.clone(),
            );
            row.connect_activated(move |_| {
                if let Some(t) = RemoteSearch::parse(&c) {
                    h.set_text(&t.host);
                    p.set_value(t.port as f64);
                    u.set_text(&t.username);
                    pw.set_text("");
                    pw.grab_focus();
                }
            });
            group.add(&row);
        }
        page.add(&group);
    }

    let cancel = gtk::Button::with_mnemonic("_Cancel");
    let connect = gtk::Button::builder()
        .label("C_onnect")
        .use_underline(true)
        .css_classes(["suggested-action"])
        .build();
    let spinner = gtk::Spinner::builder().visible(false).build();
    let buttons = gtk::Box::builder()
        .spacing(12)
        .halign(gtk::Align::End)
        .margin_top(12)
        .margin_bottom(12)
        .margin_end(12)
        .build();
    buttons.append(&spinner);
    buttons.append(&cancel);
    buttons.append(&connect);
    let bottom = gtk::Box::new(gtk::Orientation::Vertical, 6);
    error.set_margin_start(12);
    error.set_margin_end(12);
    bottom.append(&error);
    bottom.append(&buttons);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&page));
    toolbar.add_bottom_bar(&bottom);
    dialog.set_child(Some(&toolbar));
    dialog.set_default_widget(Some(&connect));

    let d = dialog.clone();
    cancel.connect_clicked(move |_| {
        d.close();
    });
    let open = Rc::new(open);
    let (d, b) = (dialog.clone(), backend.clone());
    let (h, p, u, pw) = (
        host.clone(),
        port.clone(),
        username.clone(),
        password.clone(),
    );
    let go = Rc::new(move |button: &gtk::Button| {
        let t = match target(&h.text(), p.value(), &u.text(), &pw.text()) {
            Ok(t) => t,
            Err(e) => {
                error.set_text(&e);
                error.set_visible(true);
                return;
            }
        };
        error.set_visible(false);
        button.set_sensitive(false);
        spinner.set_visible(true);
        spinner.start();
        let (d, button, spinner, error) =
            (d.clone(), button.clone(), spinner.clone(), error.clone());
        let (open, settings) = (open.clone(), settings.clone());
        b.probe(t.clone(), move |r| {
            button.set_sensitive(true);
            spinner.stop();
            spinner.set_visible(false);
            match r {
                Ok(_) => {
                    settings.borrow_mut().remember_connection(&describe(&t));
                    settings.borrow().save();
                    d.close();
                    open(t);
                }
                Err(e) => {
                    error.set_text(&e);
                    error.set_visible(true);
                }
            }
        });
    });
    let g = go.clone();
    connect.connect_clicked(move |b| g(b));
    for entry in [&host, &username] {
        let c = connect.clone();
        entry.connect_entry_activated(move |_| c.emit_clicked());
    }
    let c = connect.clone();
    password.connect_entry_activated(move |_| c.emit_clicked());
    dialog.present(Some(parent));
    host.grab_focus();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_fields_make_a_connection() {
        let t = target("pc", 2121.0, " me ", "pw").unwrap();
        assert_eq!((t.host.as_str(), t.port), ("pc", 2121));
        assert_eq!((t.username.as_str(), t.password.as_str()), ("me", "pw"));
        let t = target("you:secret@nas:21", 2121.0, "me", "").unwrap();
        assert_eq!(
            (t.username.as_str(), t.password.as_str(), t.port),
            ("you", "secret", 21)
        );
        assert!(target("  ", 21.0, "", "").is_err());
    }
}
