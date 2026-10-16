//! Preferences → Servers: Everything's ETP/FTP and HTTP servers, and the
//! index server other EverythingOS computers copy this index from (the
//! counterpart of the Everything Server). All of them are read-only.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use evos_proto::{IndexUser, ServerSettings};
use gtk::glib;

use crate::backend::Backend;

/// A password the server may require: off means none, an empty field
/// keeps the stored one.
struct PasswordField {
    require: adw::SwitchRow,
    entry: adw::PasswordEntryRow,
    was_set: Rc<Cell<bool>>,
}

impl PasswordField {
    fn new(group: &adw::PreferencesGroup) -> Self {
        let f = PasswordField {
            require: adw::SwitchRow::builder()
                .title("Require a password")
                .build(),
            entry: adw::PasswordEntryRow::builder().title("Password").build(),
            was_set: Rc::default(),
        };
        group.add(&f.require);
        group.add(&f.entry);
        let entry = f.entry.clone();
        f.require
            .connect_active_notify(move |r| entry.set_visible(r.is_active()));
        f.entry.set_visible(false);
        f
    }

    fn set(&self, password_set: bool) {
        self.was_set.set(password_set);
        self.require.set_active(password_set);
        self.entry.set_visible(password_set);
        self.entry.set_text("");
        self.entry.set_title(if password_set {
            "New password (empty keeps the current one)"
        } else {
            "Password"
        });
    }

    /// `None` keeps the stored password, `Some("")` removes it.
    fn read(&self, what: &str) -> Result<Option<String>, String> {
        let text = self.entry.text().to_string();
        match (
            self.require.is_active(),
            text.is_empty(),
            self.was_set.get(),
        ) {
            (false, _, _) => Ok(Some(String::new())),
            (true, false, _) => Ok(Some(text)),
            (true, true, true) => Ok(None),
            (true, true, false) => Err(format!("Enter a password for the {what}")),
        }
    }
}

/// The settings every server has.
struct Common {
    group: adw::PreferencesGroup,
    enabled: adw::SwitchRow,
    bind: adw::EntryRow,
    port: adw::SpinRow,
    downloads: adw::SwitchRow,
    run_as: adw::EntryRow,
}

impl Common {
    fn new(title: &str, description: &str, enable: &str) -> Self {
        let c = Common {
            // Descriptions are markup.
            group: adw::PreferencesGroup::builder()
                .title(title)
                .description(glib::markup_escape_text(description))
                .build(),
            enabled: adw::SwitchRow::builder().title(enable).build(),
            bind: adw::EntryRow::builder()
                .title("Listen on (addresses separated by commas; empty: all)")
                .build(),
            port: adw::SpinRow::builder()
                .title("Port")
                .adjustment(&gtk::Adjustment::new(1.0, 1.0, 65535.0, 1.0, 10.0, 0.0))
                .build(),
            downloads: adw::SwitchRow::builder()
                .title("Allow file downloads")
                .build(),
            run_as: adw::EntryRow::builder()
                .title("Show what this Linux account may see")
                .build(),
        };
        c.group.add(&c.enabled);
        c
    }

    /// Adds the remaining rows below the server-specific ones.
    fn finish(&self) {
        self.group.add(&self.bind);
        self.group.add(&self.port);
        self.group.add(&self.downloads);
        self.group.add(&self.run_as);
    }

    fn set(&self, enabled: bool, bind: &[String], port: u16, downloads: bool, run_as: &str) {
        self.enabled.set_active(enabled);
        self.bind.set_text(&bind.join(", "));
        self.port.set_value(port as f64);
        self.downloads.set_active(downloads);
        self.run_as.set_text(run_as);
    }

    fn bind(&self) -> Vec<String> {
        self.bind
            .text()
            .split(',')
            .map(|b| b.trim().to_owned())
            .filter(|b| !b.is_empty())
            .collect()
    }

    fn run_as(&self) -> String {
        let r = self.run_as.text().trim().to_owned();
        if r.is_empty() {
            "nobody".into()
        } else {
            r
        }
    }
}

struct ServersPage {
    dialog: adw::PreferencesDialog,
    backend: Backend,
    etp: Common,
    etp_user: adw::EntryRow,
    etp_password: PasswordField,
    welcome: adw::EntryRow,
    http: Common,
    http_user: adw::EntryRow,
    http_password: PasswordField,
    index: Common,
    users: adw::PreferencesGroup,
    user_rows: RefCell<Vec<gtk::Widget>>,
    /// Accounts of the index server as edited; passwords `None` keep the
    /// stored ones.
    accounts: RefCell<Vec<IndexUser>>,
    loaded: RefCell<Option<ServerSettings>>,
}

fn host_name() -> String {
    glib::host_name().to_string()
}

/// Builds the page; settings are loaded from the daemon.
pub fn page(dialog: &adw::PreferencesDialog, backend: &Backend) -> adw::PreferencesPage {
    let host = host_name();
    let etp = Common::new(
        "ETP/FTP Server",
        &format!(
            "Everything on other computers searches this index with File → Connect to ETP Server ({host}), and FTP clients browse and download. Nothing can be changed remotely."
        ),
        "Enable ETP/FTP server",
    );
    let etp_user = adw::EntryRow::builder()
        .title("User name (empty: any)")
        .build();
    etp.group.add(&etp_user);
    let etp_password = PasswordField::new(&etp.group);
    let welcome = adw::EntryRow::builder()
        .title("Welcome message (%v: version, %n: new line)")
        .build();
    etp.group.add(&welcome);
    etp.finish();

    let http = Common::new(
        "HTTP Server",
        &format!(
            "Search and download from any web browser at http://{host}/, and the ?search=…&json=1 interface scripts use."
        ),
        "Enable HTTP server",
    );
    let http_user = adw::EntryRow::builder()
        .title("User name (empty: no login)")
        .build();
    http.group.add(&http_user);
    let http_password = PasswordField::new(&http.group);
    http.finish();

    let index = Common::new(
        "Index Server",
        "Other EverythingOS computers add this one as a remote index and keep a live copy of it, like the Everything Server. Each account sees only what the account below may see.",
        "Enable index server",
    );
    index.finish();
    let users = adw::PreferencesGroup::builder()
        .title("Index Server Accounts")
        .description("The logins other computers use; each can be limited to some folders.")
        .build();
    let add = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Add Account")
        .css_classes(["flat"])
        .build();
    add.update_property(&[gtk::accessible::Property::Label("Add Account")]);
    users.set_header_suffix(Some(&add));

    let apply = gtk::Button::builder()
        .label("_Apply")
        .use_underline(true)
        .halign(gtk::Align::End)
        .css_classes(["suggested-action"])
        .build();
    let note = gtk::Label::builder()
        .label("Changes require administrator rights. Servers whose address is unchanged keep their connections.")
        .wrap(true)
        .xalign(0.0)
        .hexpand(true)
        .css_classes(["dim-label"])
        .build();
    let bar = gtk::Box::builder().spacing(12).build();
    bar.append(&note);
    bar.append(&apply);
    let actions = adw::PreferencesGroup::new();
    actions.add(&bar);

    let page = adw::PreferencesPage::builder()
        .title("Servers")
        .icon_name("network-server-symbolic")
        .build();
    page.add(&etp.group);
    page.add(&http.group);
    page.add(&index.group);
    page.add(&users);
    page.add(&actions);

    let p = Rc::new(ServersPage {
        dialog: dialog.clone(),
        backend: backend.clone(),
        etp,
        etp_user,
        etp_password,
        welcome,
        http,
        http_user,
        http_password,
        index,
        users,
        user_rows: RefCell::default(),
        accounts: RefCell::default(),
        loaded: RefCell::default(),
    });
    let pp = p.clone();
    add.connect_clicked(move |_| pp.edit_account(None));
    let pp = p.clone();
    apply.connect_clicked(move |b| pp.apply(b));
    p.load();
    page
}

impl ServersPage {
    fn toast(&self, text: &str) {
        self.dialog.add_toast(
            adw::Toast::builder()
                .title(glib::markup_escape_text(text))
                .timeout(5)
                .build(),
        );
    }

    fn load(self: &Rc<Self>) {
        let p = self.clone();
        self.backend.call(
            |c| c.servers(),
            move |r| match r {
                Ok(s) => p.fill(s),
                Err(e) => p.toast(&format!("Cannot read the server settings: {e}")),
            },
        );
    }

    fn fill(self: &Rc<Self>, s: ServerSettings) {
        let e = &s.etp;
        self.etp
            .set(e.enabled, &e.bind, e.port, e.allow_file_download, &e.run_as);
        self.etp_user.set_text(&e.username);
        self.etp_password.set(e.password_set);
        self.welcome.set_text(&e.welcome_message);
        let h = &s.http;
        self.http
            .set(h.enabled, &h.bind, h.port, h.allow_file_download, &h.run_as);
        self.http_user.set_text(&h.username);
        self.http_password.set(h.password_set);
        let i = &s.index;
        self.index
            .set(i.enabled, &i.bind, i.port, i.allow_file_download, &i.run_as);
        *self.accounts.borrow_mut() = i
            .users
            .iter()
            .map(|u| IndexUser {
                password: None,
                ..u.clone()
            })
            .collect();
        *self.loaded.borrow_mut() = Some(s);
        self.show_accounts();
    }

    fn show_accounts(self: &Rc<Self>) {
        for row in self.user_rows.borrow_mut().drain(..) {
            self.users.remove(&row);
        }
        let accounts = self.accounts.borrow().clone();
        let mut rows: Vec<gtk::Widget> = Vec::new();
        if accounts.is_empty() {
            rows.push(
                adw::ActionRow::builder()
                    .title("No accounts yet")
                    .subtitle("The index server needs at least one; use + to add it")
                    .build()
                    .upcast(),
            );
        }
        for (i, u) in accounts.iter().enumerate() {
            let folders = u
                .include_only
                .iter()
                .filter(|f| !f.trim().is_empty())
                .cloned()
                .collect::<Vec<_>>();
            let mut subtitle = if folders.is_empty() {
                "All folders".to_owned()
            } else {
                format!("Only {}", folders.join(", "))
            };
            let has_password = u
                .password
                .as_deref()
                .map_or(u.password_set, |p| !p.is_empty());
            subtitle.push_str(if has_password {
                " · password"
            } else {
                " · no password"
            });
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&u.name))
                .subtitle(glib::markup_escape_text(&subtitle))
                .build();
            let edit = gtk::Button::builder()
                .icon_name("document-edit-symbolic")
                .tooltip_text("Edit")
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            edit.update_property(&[gtk::accessible::Property::Label("Edit")]);
            let p = self.clone();
            edit.connect_clicked(move |_| p.edit_account(Some(i)));
            row.add_suffix(&edit);
            let remove = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text("Remove")
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            remove.update_property(&[gtk::accessible::Property::Label("Remove")]);
            let p = self.clone();
            remove.connect_clicked(move |_| {
                p.accounts.borrow_mut().remove(i);
                p.show_accounts();
            });
            row.add_suffix(&remove);
            rows.push(row.upcast());
        }
        for row in &rows {
            self.users.add(row);
        }
        *self.user_rows.borrow_mut() = rows;
    }

    /// Edits an account (by position) or adds one, as a subpage.
    fn edit_account(self: &Rc<Self>, at: Option<usize>) {
        let existing = at.and_then(|i| self.accounts.borrow().get(i).cloned());
        let group = adw::PreferencesGroup::builder()
            .title("Account")
            .description("Other computers log in with this name and password.")
            .build();
        let name = adw::EntryRow::builder().title("Name").build();
        group.add(&name);
        let password = PasswordField::new(&group);
        let folders = adw::EntryRow::builder()
            .title("Only these folders (separated by ;)")
            .build();
        group.add(&folders);
        if let Some(u) = &existing {
            name.set_text(&u.name);
            let has = u
                .password
                .as_deref()
                .map_or(u.password_set, |p| !p.is_empty());
            password.set(has);
            folders.set_text(&u.include_only.join(";"));
        } else {
            password.set(false);
            password.require.set_active(true);
        }
        let save = gtk::Button::builder()
            .label(if existing.is_some() { "_Done" } else { "_Add" })
            .use_underline(true)
            .halign(gtk::Align::End)
            .margin_top(12)
            .margin_bottom(12)
            .margin_end(12)
            .css_classes(["suggested-action"])
            .build();
        let content = adw::PreferencesPage::new();
        content.add(&group);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&content));
        toolbar.add_bottom_bar(&save);
        let page = adw::NavigationPage::builder()
            .title(if existing.is_some() {
                "Edit Account"
            } else {
                "Add Account"
            })
            .child(&toolbar)
            .build();
        let p = self.clone();
        save.connect_clicked(move |_| {
            let n = name.text().trim().to_owned();
            if n.is_empty() {
                return p.toast("Enter a name");
            }
            let taken = p
                .accounts
                .borrow()
                .iter()
                .enumerate()
                .any(|(i, u)| u.name == n && Some(i) != at);
            if taken {
                return p.toast(&format!("There is already an account named {n}"));
            }
            let pw = match password.read("account") {
                Ok(pw) => pw,
                Err(e) => return p.toast(&e),
            };
            let account = IndexUser {
                // A renamed account has no stored password to keep.
                password: match (&existing, pw) {
                    (Some(old), None) if old.name != n => {
                        return p.toast("Enter the password again for the new name");
                    }
                    (_, pw) => pw,
                },
                password_set: existing.as_ref().is_some_and(|e| e.password_set),
                name: n,
                include_only: folders
                    .text()
                    .split(';')
                    .map(|f| f.trim().to_owned())
                    .filter(|f| !f.is_empty())
                    .collect(),
            };
            match at {
                Some(i) => p.accounts.borrow_mut()[i] = account,
                None => p.accounts.borrow_mut().push(account),
            }
            p.show_accounts();
            p.dialog.pop_subpage();
        });
        self.dialog.push_subpage(&page);
    }

    /// The settings as edited.
    fn read(&self) -> Result<ServerSettings, String> {
        let mut s = self.loaded.borrow().clone().unwrap_or_default();
        s.etp.enabled = self.etp.enabled.is_active();
        s.etp.bind = self.etp.bind();
        s.etp.port = self.etp.port.value() as u16;
        s.etp.username = self.etp_user.text().trim().to_owned();
        s.etp.password = self.etp_password.read("ETP/FTP server")?;
        s.etp.welcome_message = self.welcome.text().to_string();
        s.etp.allow_file_download = self.etp.downloads.is_active();
        s.etp.run_as = self.etp.run_as();
        s.http.enabled = self.http.enabled.is_active();
        s.http.bind = self.http.bind();
        s.http.port = self.http.port.value() as u16;
        s.http.username = self.http_user.text().trim().to_owned();
        s.http.password = self.http_password.read("HTTP server")?;
        s.http.allow_file_download = self.http.downloads.is_active();
        s.http.run_as = self.http.run_as();
        s.index.enabled = self.index.enabled.is_active();
        s.index.bind = self.index.bind();
        s.index.port = self.index.port.value() as u16;
        s.index.allow_file_download = self.index.downloads.is_active();
        s.index.run_as = self.index.run_as();
        s.index.users = self.accounts.borrow().clone();
        Ok(s)
    }

    fn apply(self: &Rc<Self>, button: &gtk::Button) {
        let settings = match self.read() {
            Ok(s) => s,
            Err(e) => return self.toast(&e),
        };
        button.set_sensitive(false);
        let (p, b) = (self.clone(), button.clone());
        self.backend.call(
            move |c| c.set_servers(settings),
            move |r| {
                b.set_sensitive(true);
                match r {
                    Ok(()) => {
                        p.toast("Servers updated");
                        p.load();
                    }
                    Err(e) => p.toast(&e),
                }
            },
        );
    }
}
