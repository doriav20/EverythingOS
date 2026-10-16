//! Preferences → Remote Indexes: copies of other computers' indexes,
//! searchable like local disks, from another EverythingOS (its index
//! server) or from an ETP server such as Everything on Windows.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use evos_proto::{RemoteIndexInfo, RemoteIndexSpec, RemoteKind};
use gtk::glib;

use crate::backend::Backend;
use crate::format;

const KINDS: &[(&str, RemoteKind)] = &[
    ("EverythingOS", RemoteKind::Evos),
    ("Everything (ETP)", RemoteKind::Etp),
];

struct RemotePage {
    dialog: adw::PreferencesDialog,
    backend: Backend,
    group: adw::PreferencesGroup,
    rows: RefCell<Vec<gtk::Widget>>,
    /// What the rows show, to rebuild them only on change.
    shown: RefCell<Option<Vec<RemoteIndexInfo>>>,
}

pub fn page(dialog: &adw::PreferencesDialog, backend: &Backend) -> adw::PreferencesPage {
    let group = adw::PreferencesGroup::builder()
        .title("Remote Indexes")
        .description("Other computers' indexes, copied here and kept current, so their files are found like local ones even while they are offline. Changes require administrator rights.")
        .build();
    let add = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Add Remote Index")
        .css_classes(["flat"])
        .build();
    add.update_property(&[gtk::accessible::Property::Label("Add Remote Index")]);
    group.set_header_suffix(Some(&add));
    let page = adw::PreferencesPage::builder()
        .title("Remote Indexes")
        .icon_name("network-workgroup-symbolic")
        .build();
    page.add(&group);
    let p = Rc::new(RemotePage {
        dialog: dialog.clone(),
        backend: backend.clone(),
        group,
        rows: RefCell::default(),
        shown: RefCell::default(),
    });
    let pp = p.clone();
    add.connect_clicked(move |_| pp.edit(None));
    p.load();
    // Keep the status current while the preferences are open.
    let weak = Rc::downgrade(&p);
    let page_ref = page.downgrade();
    glib::timeout_add_local(Duration::from_secs(2), move || {
        match (weak.upgrade(), page_ref.upgrade()) {
            // Closed preferences have no window.
            (Some(p), Some(page)) if page.root().is_some() => {
                if page.is_mapped() {
                    p.load();
                }
                glib::ControlFlow::Continue
            }
            _ => glib::ControlFlow::Break,
        }
    });
    page
}

fn kind_label(kind: RemoteKind) -> &'static str {
    match kind {
        RemoteKind::Evos => "EverythingOS",
        RemoteKind::Etp => "ETP",
    }
}

/// "5 s ago", "3 min ago", ...
fn ago(unix: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let s = (now - unix).max(0);
    match s {
        0..=59 => format!("{s} s ago"),
        60..=3599 => format!("{} min ago", s / 60),
        3600..=86399 => format!("{} h ago", s / 3600),
        _ => format::date(unix * 1_000_000_000),
    }
}

/// The status line of a remote index.
pub fn summary(info: &RemoteIndexInfo) -> String {
    let s = &info.spec;
    let mut parts = vec![
        format!("{} {}:{}", kind_label(s.kind), s.host, s.port()),
        format!("shown under {}", s.mount_as()),
    ];
    parts.push(if s.enabled {
        info.phase.clone()
    } else {
        "disabled".into()
    });
    parts.push(format!("{} entries", format::count(info.entries)));
    if let Some(t) = info.last_sync {
        parts.push(format!("updated {}", ago(t)));
    }
    if let Some(e) = &info.error {
        parts.push(e.clone());
    }
    parts.join(" · ")
}

impl RemotePage {
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
            |c| c.remote_indexes(),
            move |r| match r {
                Ok(list) => p.show(Some(list)),
                Err(e) => {
                    p.shown.replace(None);
                    p.replace(vec![adw::ActionRow::builder()
                        .title("Daemon unavailable")
                        .subtitle(glib::markup_escape_text(&e))
                        .build()
                        .upcast()]);
                }
            },
        );
    }

    fn replace(&self, rows: Vec<gtk::Widget>) {
        for row in self.rows.borrow_mut().drain(..) {
            self.group.remove(&row);
        }
        for row in &rows {
            self.group.add(row);
        }
        *self.rows.borrow_mut() = rows;
    }

    fn show(self: &Rc<Self>, list: Option<Vec<RemoteIndexInfo>>) {
        if *self.shown.borrow() == list {
            return;
        }
        let rows = match &list {
            Some(l) if l.is_empty() => vec![adw::ActionRow::builder()
                .title("No remote indexes yet")
                .subtitle("Use + to add another computer running EverythingOS or Everything")
                .build()
                .upcast()],
            Some(l) => l.iter().map(|info| self.row(info)).collect(),
            None => Vec::new(),
        };
        self.replace(rows);
        self.shown.replace(list);
    }

    fn row(self: &Rc<Self>, info: &RemoteIndexInfo) -> gtk::Widget {
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&info.spec.name))
            .subtitle(glib::markup_escape_text(&summary(info)))
            .subtitle_lines(3)
            .build();
        let switch = gtk::Switch::builder()
            .active(info.spec.enabled)
            .valign(gtk::Align::Center)
            .tooltip_text("Enabled")
            .build();
        switch.update_property(&[gtk::accessible::Property::Label("Enabled")]);
        let (p, spec) = (self.clone(), info.spec.clone());
        switch.connect_state_set(move |sw, on| {
            if on == spec.enabled {
                return glib::Propagation::Proceed;
            }
            let spec = RemoteIndexSpec {
                enabled: on,
                ..spec.clone()
            };
            let (p2, sw) = (p.clone(), sw.clone());
            p.backend.call(
                move |c| c.remote_index_add(spec, None),
                move |r| {
                    if let Err(e) = r {
                        p2.toast(&e);
                        sw.set_active(!on);
                    }
                    p2.load();
                },
            );
            glib::Propagation::Proceed
        });
        row.add_suffix(&switch);
        let edit = gtk::Button::builder()
            .icon_name("document-edit-symbolic")
            .tooltip_text("Edit")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        edit.update_property(&[gtk::accessible::Property::Label("Edit")]);
        let (p, spec) = (self.clone(), info.spec.clone());
        edit.connect_clicked(move |_| p.edit(Some(spec.clone())));
        row.add_suffix(&edit);
        let remove = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Remove")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        remove.update_property(&[gtk::accessible::Property::Label("Remove")]);
        let (p, name) = (self.clone(), info.spec.name.clone());
        remove.connect_clicked(move |_| p.confirm_remove(&name));
        row.add_suffix(&remove);
        row.upcast()
    }

    fn confirm_remove(self: &Rc<Self>, name: &str) {
        let alert = adw::AlertDialog::new(
            Some(&format!("Remove {name}?")),
            Some("The copy of its index and its stored login are deleted here. Nothing changes on the other computer."),
        );
        alert.add_response("cancel", "_Cancel");
        alert.add_response("remove", "_Remove");
        alert.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
        let (p, name) = (self.clone(), name.to_owned());
        alert.connect_response(Some("remove"), move |_, _| {
            let (name, p2) = (name.clone(), p.clone());
            p.backend.call(
                move |c| c.remote_index_remove(&name),
                move |r| {
                    if let Err(e) = r {
                        p2.toast(&e);
                    }
                    p2.load();
                },
            );
        });
        alert.present(Some(&self.dialog));
    }

    /// The add/edit form, as a subpage.
    fn edit(self: &Rc<Self>, existing: Option<RemoteIndexSpec>) {
        let form = Rc::new(Form::new(existing.as_ref()));
        let content = adw::PreferencesPage::new();
        content.add(&form.server);
        content.add(&form.copy);
        let test = gtk::Button::with_mnemonic("_Test Connection");
        let save = gtk::Button::builder()
            .label(if existing.is_some() { "_Save" } else { "_Add" })
            .use_underline(true)
            .css_classes(["suggested-action"])
            .build();
        let buttons = gtk::Box::builder()
            .spacing(12)
            .halign(gtk::Align::End)
            .margin_top(12)
            .margin_bottom(12)
            .margin_end(12)
            .build();
        buttons.append(&test);
        buttons.append(&save);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&content));
        toolbar.add_bottom_bar(&buttons);
        let page = adw::NavigationPage::builder()
            .title(if existing.is_some() {
                "Edit Remote Index"
            } else {
                "Add Remote Index"
            })
            .child(&toolbar)
            .build();

        let (p, f) = (self.clone(), form.clone());
        test.connect_clicked(move |button| {
            let (spec, password) = match f.read() {
                Ok(v) => v,
                Err(e) => return p.toast(&e),
            };
            button.set_sensitive(false);
            p.toast("Connecting…");
            let (p2, b) = (p.clone(), button.clone());
            p.backend.call(
                move |c| c.remote_index_test(spec, password),
                move |r| {
                    b.set_sensitive(true);
                    p2.toast(&match r {
                        Ok(server) if server.trim().is_empty() => "Connection works".into(),
                        Ok(server) => format!("Connection works: {}", server.trim()),
                        Err(e) => format!("Test failed: {e}"),
                    });
                },
            );
        });
        let (p, f) = (self.clone(), form.clone());
        save.connect_clicked(move |button| {
            let (spec, password) = match f.read() {
                Ok(v) => v,
                Err(e) => return p.toast(&e),
            };
            button.set_sensitive(false);
            let (p2, b) = (p.clone(), button.clone());
            p.backend.call(
                move |c| c.remote_index_add(spec, password),
                move |r| {
                    b.set_sensitive(true);
                    match r {
                        Ok(()) => {
                            p2.dialog.pop_subpage();
                            p2.toast("Saved; copying its index");
                        }
                        Err(e) => p2.toast(&e),
                    }
                    p2.load();
                },
            );
        });
        self.dialog.push_subpage(&page);
    }
}

/// The add/edit form.
struct Form {
    server: adw::PreferencesGroup,
    copy: adw::PreferencesGroup,
    name: adw::EntryRow,
    kind: adw::ComboRow,
    host: adw::EntryRow,
    port: adw::SpinRow,
    username: adw::EntryRow,
    password: adw::PasswordEntryRow,
    mount_as: adw::EntryRow,
    include: adw::EntryRow,
    refresh: adw::SpinRow,
    enabled: adw::SwitchRow,
    editing: bool,
}

impl Form {
    fn new(existing: Option<&RemoteIndexSpec>) -> Self {
        let names: Vec<&str> = KINDS.iter().map(|(n, _)| *n).collect();
        let f = Form {
            server: adw::PreferencesGroup::builder().title("Server").build(),
            copy: adw::PreferencesGroup::builder().title("Copy").build(),
            name: adw::EntryRow::builder().title("Name").build(),
            kind: adw::ComboRow::builder()
                .title("Type")
                .model(&gtk::StringList::new(&names))
                .build(),
            host: adw::EntryRow::builder().title("Host").build(),
            port: adw::SpinRow::builder()
                .title("Port")
                .adjustment(&gtk::Adjustment::new(
                    RemoteKind::Evos.default_port() as f64,
                    1.0,
                    65535.0,
                    1.0,
                    10.0,
                    0.0,
                ))
                .build(),
            username: adw::EntryRow::builder().title("User name").build(),
            password: adw::PasswordEntryRow::builder()
                .title(if existing.is_some() {
                    "Password (empty keeps the stored one)"
                } else {
                    "Password"
                })
                .build(),
            mount_as: adw::EntryRow::builder()
                .title("Show its files under (empty: /net/name)")
                .build(),
            include: adw::EntryRow::builder()
                .title("Only these server folders (separated by ;)")
                .build(),
            refresh: adw::SpinRow::builder()
                .title("Check for changes every (seconds)")
                .adjustment(&gtk::Adjustment::new(10.0, 1.0, 86_400.0, 1.0, 60.0, 0.0))
                .build(),
            enabled: adw::SwitchRow::builder()
                .title("Enabled")
                .active(true)
                .build(),
            editing: existing.is_some(),
        };
        for row in [
            f.name.upcast_ref::<gtk::Widget>(),
            f.kind.upcast_ref(),
            f.host.upcast_ref(),
            f.port.upcast_ref(),
            f.username.upcast_ref(),
            f.password.upcast_ref(),
        ] {
            f.server.add(row);
        }
        for row in [
            f.mount_as.upcast_ref::<gtk::Widget>(),
            f.include.upcast_ref(),
            f.refresh.upcast_ref(),
            f.enabled.upcast_ref(),
        ] {
            f.copy.add(row);
        }
        // Switching the type moves the port and the interval to that
        // type's defaults, unless they were changed.
        let (port, refresh, copy) = (f.port.clone(), f.refresh.clone(), f.copy.clone());
        f.kind.connect_selected_notify(move |k| {
            let kind = KINDS[k.selected() as usize].1;
            let other = if kind == RemoteKind::Evos {
                RemoteKind::Etp
            } else {
                RemoteKind::Evos
            };
            if port.value() as u16 == other.default_port() {
                port.set_value(kind.default_port() as f64);
            }
            let spec = |k| RemoteIndexSpec {
                kind: k,
                ..Default::default()
            };
            if refresh.value() as u64 == spec(other).refresh_secs() {
                refresh.set_value(spec(kind).refresh_secs() as f64);
            }
            copy.set_description(Some(if kind == RemoteKind::Etp {
                "ETP servers send no changes: the whole list is copied again at each check."
            } else {
                "Only changes are copied at each check."
            }));
        });
        f.copy
            .set_description(Some("Only changes are copied at each check."));
        if let Some(s) = existing {
            f.name.set_text(&s.name);
            f.name.set_editable(false);
            let kind = KINDS.iter().position(|(_, k)| *k == s.kind).unwrap_or(0);
            f.kind.set_selected(kind as u32);
            f.host.set_text(&s.host);
            f.port.set_value(s.port() as f64);
            f.username.set_text(&s.username);
            f.mount_as.set_text(s.mount_as.as_deref().unwrap_or(""));
            f.include.set_text(&s.include_only.join(";"));
            f.refresh.set_value(s.refresh_secs() as f64);
            f.enabled.set_active(s.enabled);
        }
        f
    }

    /// Reads and checks the form; a `None` password keeps the stored one.
    fn read(&self) -> Result<(RemoteIndexSpec, Option<String>), String> {
        let text = |row: &adw::EntryRow| row.text().trim().to_owned();
        let mount_as = text(&self.mount_as);
        let spec = RemoteIndexSpec {
            name: text(&self.name),
            kind: KINDS[self.kind.selected() as usize].1,
            host: text(&self.host),
            port: Some(self.port.value() as u16),
            username: text(&self.username),
            mount_as: (!mount_as.is_empty()).then_some(mount_as),
            include_only: text(&self.include)
                .split(';')
                .map(|f| f.trim().to_owned())
                .filter(|f| !f.is_empty())
                .collect(),
            refresh_secs: Some(self.refresh.value() as u64),
            enabled: self.enabled.is_active(),
        };
        spec.validate()?;
        let password = self.password.text().to_string();
        let password = (!(self.editing && password.is_empty())).then_some(password);
        Ok((spec, password))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries() {
        let info = RemoteIndexInfo {
            spec: RemoteIndexSpec {
                name: "office".into(),
                host: "pc".into(),
                ..Default::default()
            },
            phase: "live".into(),
            entries: 1234,
            last_sync: None,
            error: None,
        };
        let s = summary(&info);
        assert!(
            s.starts_with("EverythingOS pc:14630 · shown under /net/office · live"),
            "{s}"
        );
        assert!(s.contains("entries"));
    }
}
