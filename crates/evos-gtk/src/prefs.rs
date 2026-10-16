//! Preferences: indexed volumes, managed mounts, remote indexes and the
//! network servers.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use evos_proto::{Credentials, IndexSettings, MountInfo, MountKind, MountSpec, Request};
use gtk::glib;

use crate::backend::Backend;
use crate::format;

const KINDS: &[(&str, MountKind, &str)] = &[
    ("SMB / Windows share", MountKind::Smb, "//server/share"),
    ("NFS export", MountKind::Nfs, "server:/export"),
    ("SSHFS", MountKind::Sshfs, "user@host:/path"),
    (
        "Local folder (bind mount)",
        MountKind::Bind,
        "/path/to/folder",
    ),
    ("Existing path", MountKind::Existing, "/mnt/already-mounted"),
];

/// Rows of a group, so the group can be refilled.
type Rows = Rc<RefCell<Vec<gtk::Widget>>>;

struct Prefs {
    dialog: adw::PreferencesDialog,
    backend: Backend,
    volumes: adw::PreferencesGroup,
    volume_rows: Rows,
    mounts: adw::PreferencesGroup,
    mount_rows: Rows,
}

/// Opens the preferences dialog.
pub fn show(parent: &impl IsA<gtk::Widget>, backend: &Backend) {
    let dialog = adw::PreferencesDialog::new();
    dialog.set_title("Preferences");
    // Rows with wrapping subtitles have a tiny natural width; give the
    // dialog a real size instead of shrinking to it.
    dialog.set_content_width(800);
    dialog.set_content_height(560);

    let volumes = adw::PreferencesGroup::builder()
        .title("Indexed Volumes")
        .description("Local disks follow every change live; network shares are checked on an adaptive schedule.")
        .build();
    let volume_page = adw::PreferencesPage::builder()
        .title("Volumes")
        .icon_name("drive-harddisk-symbolic")
        .build();
    volume_page.add(&volumes);

    let mounts = adw::PreferencesGroup::builder()
        .title("Mounts")
        .description("Shares and folders EverythingOS mounts and indexes. Changes require administrator rights.")
        .build();
    let add = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Add Mount")
        .css_classes(["flat"])
        .build();
    add.update_property(&[gtk::accessible::Property::Label("Add Mount")]);
    mounts.set_header_suffix(Some(&add));
    let mount_page = adw::PreferencesPage::builder()
        .title("Mounts")
        .icon_name("folder-remote-symbolic")
        .build();
    mount_page.add(&mounts);

    dialog.add(&volume_page);
    dialog.add(&mount_page);
    dialog.add(&crate::prefs_remote::page(&dialog, backend));
    dialog.add(&crate::prefs_servers::page(&dialog, backend));

    let prefs = Rc::new(Prefs {
        dialog: dialog.clone(),
        backend: backend.clone(),
        volumes,
        volume_rows: Rows::default(),
        mounts,
        mount_rows: Rows::default(),
    });
    let p = prefs.clone();
    add.connect_clicked(move |_| p.edit_mount(None));
    prefs.load_volumes();
    prefs.load_mounts();
    dialog.present(Some(parent));
}

fn replace_rows(group: &adw::PreferencesGroup, rows: &Rows, new: Vec<gtk::Widget>) {
    for old in rows.borrow_mut().drain(..) {
        group.remove(&old);
    }
    for row in &new {
        group.add(row);
    }
    *rows.borrow_mut() = new;
}

fn kind_label(kind: MountKind) -> &'static str {
    match kind {
        MountKind::Smb => "SMB",
        MountKind::Nfs => "NFS",
        MountKind::Sshfs => "SSHFS",
        MountKind::Bind => "Folder",
        MountKind::Existing => "Existing",
    }
}

impl Prefs {
    fn toast(&self, text: &str) {
        self.dialog.add_toast(
            adw::Toast::builder()
                .title(glib::markup_escape_text(text))
                .timeout(5)
                .build(),
        );
    }

    fn load_volumes(self: &Rc<Self>) {
        let p = self.clone();
        self.backend.call(
            |c| c.status(),
            move |r| {
                let rows = match r {
                    Ok(status) => status
                        .volumes
                        .iter()
                        .map(|v| {
                            let root = String::from_utf8_lossy(&v.root).into_owned();
                            let mut subtitle = format!(
                                "{} · {} · {} · {} entries",
                                v.fs_type,
                                v.phase,
                                v.watch,
                                format::count(v.entries)
                            );
                            if let (Some(c), Some(ops)) = (v.concurrency, v.ops_per_sec) {
                                subtitle.push_str(&format!(
                                    " · {c} parallel, {ops:.0} ops/s, {} folders scheduled",
                                    format::count(v.tracked_dirs)
                                ));
                            }
                            if let Some(e) = &v.error {
                                subtitle.push_str(&format!(" · {e}"));
                            }
                            let row = adw::ActionRow::builder()
                                .title(glib::markup_escape_text(&root))
                                .subtitle(glib::markup_escape_text(&subtitle))
                                .build();
                            let rescan = gtk::Button::builder()
                                .icon_name("view-refresh-symbolic")
                                .tooltip_text("Re-check for changes")
                                .valign(gtk::Align::Center)
                                .css_classes(["flat"])
                                .build();
                            rescan.update_property(&[gtk::accessible::Property::Label(
                                "Re-check for changes",
                            )]);
                            let (pp, path) = (p.clone(), v.root.clone());
                            rescan.connect_clicked(move |_| {
                                let (path, pp2) = (path.clone(), pp.clone());
                                pp.backend.call(
                                    move |c| c.expect_ok(Request::Rescan { path: Some(path) }),
                                    move |r| {
                                        pp2.toast(&r.map_or_else(
                                            |e| e,
                                            |()| "Re-checking for changes".into(),
                                        ))
                                    },
                                );
                            });
                            row.add_suffix(&rescan);
                            row.upcast()
                        })
                        .collect(),
                    Err(e) => vec![adw::ActionRow::builder()
                        .title("Daemon unavailable")
                        .subtitle(glib::markup_escape_text(&e))
                        .build()
                        .upcast()],
                };
                replace_rows(&p.volumes, &p.volume_rows, rows);
            },
        );
    }

    fn load_mounts(self: &Rc<Self>) {
        let p = self.clone();
        self.backend.call(
            |c| c.mounts(),
            move |r| {
                let rows = match r {
                    Ok(mounts) if mounts.is_empty() => vec![adw::ActionRow::builder()
                        .title("No mounts yet")
                        .subtitle("Use + to mount an SMB, NFS or SSHFS share or a folder")
                        .build()
                        .upcast()],
                    Ok(mounts) => mounts.into_iter().map(|m| p.mount_row(m)).collect(),
                    Err(e) => vec![adw::ActionRow::builder()
                        .title("Daemon unavailable")
                        .subtitle(glib::markup_escape_text(&e))
                        .build()
                        .upcast()],
                };
                replace_rows(&p.mounts, &p.mount_rows, rows);
            },
        );
    }

    fn mount_row(self: &Rc<Self>, info: MountInfo) -> gtk::Widget {
        let spec = info.spec.clone();
        let mut subtitle = format!(
            "{} · {} → {}",
            kind_label(spec.kind),
            spec.source,
            spec.mountpoint()
        );
        if let Some(e) = &info.error {
            subtitle.push_str(&format!(" · {e}"));
        }
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&spec.name))
            .subtitle(glib::markup_escape_text(&subtitle))
            .subtitle_lines(2)
            .build();

        if spec.kind != MountKind::Existing {
            let switch = gtk::Switch::builder()
                .active(info.mounted)
                .valign(gtk::Align::Center)
                .tooltip_text("Mounted")
                .build();
            switch.update_property(&[gtk::accessible::Property::Label("Mounted")]);
            let (p, name) = (self.clone(), spec.name.clone());
            switch.connect_state_set(move |switch, on| {
                let (name, p2, sw) = (name.clone(), p.clone(), switch.clone());
                let request = if on {
                    Request::MountUp { name }
                } else {
                    Request::MountDown { name }
                };
                p.backend.call(
                    move |c| c.expect_ok(request),
                    move |r| {
                        if let Err(e) = r {
                            p2.toast(&e);
                            sw.set_active(!on);
                        }
                        p2.load_mounts();
                        p2.load_volumes();
                    },
                );
                glib::Propagation::Proceed
            });
            row.add_suffix(&switch);
        }

        let edit = gtk::Button::builder()
            .icon_name("document-edit-symbolic")
            .tooltip_text("Edit")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        edit.update_property(&[gtk::accessible::Property::Label("Edit")]);
        let (p, s) = (self.clone(), spec.clone());
        edit.connect_clicked(move |_| p.edit_mount(Some(s.clone())));
        row.add_suffix(&edit);

        let remove = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Remove")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        remove.update_property(&[gtk::accessible::Property::Label("Remove")]);
        let (p, name) = (self.clone(), spec.name.clone());
        remove.connect_clicked(move |_| p.confirm_remove(&name));
        row.add_suffix(&remove);
        row.upcast()
    }

    fn confirm_remove(self: &Rc<Self>, name: &str) {
        let alert = adw::AlertDialog::new(
            Some(&format!("Remove {name}?")),
            Some("The share or folder is unmounted and its definition and stored login are deleted. Files are not touched."),
        );
        alert.add_response("cancel", "_Cancel");
        alert.add_response("remove", "_Remove");
        alert.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
        let (p, name) = (self.clone(), name.to_owned());
        alert.connect_response(Some("remove"), move |_, _| {
            let (name, p2) = (name.clone(), p.clone());
            p.backend.call(
                move |c| c.expect_ok(Request::MountRemove { name }),
                move |r| {
                    if let Err(e) = r {
                        p2.toast(&e);
                    }
                    p2.load_mounts();
                    p2.load_volumes();
                },
            );
        });
        alert.present(Some(&self.dialog));
    }

    /// Shows the add/edit form as a subpage.
    fn edit_mount(self: &Rc<Self>, existing: Option<MountSpec>) {
        let form = Rc::new(MountForm::new(existing.as_ref()));
        let page_content = adw::PreferencesPage::new();
        page_content.add(&form.connection);
        page_content.add(&form.login);
        page_content.add(&form.indexing);

        let test = gtk::Button::builder()
            .label("_Test Connection")
            .use_underline(true)
            .build();
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
        toolbar.set_content(Some(&page_content));
        toolbar.add_bottom_bar(&buttons);
        let title = if existing.is_some() {
            "Edit Mount"
        } else {
            "Add Mount"
        };
        let page = adw::NavigationPage::builder()
            .title(title)
            .child(&toolbar)
            .build();

        let (p, f) = (self.clone(), form.clone());
        test.connect_clicked(move |button| {
            let (spec, creds) = match f.read() {
                Ok(v) => v,
                Err(e) => return p.toast(&e),
            };
            button.set_sensitive(false);
            let (p2, b) = (p.clone(), button.clone());
            p.toast("Testing…");
            p.backend.call(
                move |c| {
                    c.expect_ok(Request::MountTest {
                        spec,
                        credentials: creds,
                    })
                },
                move |r| {
                    b.set_sensitive(true);
                    p2.toast(&r.map_or_else(
                        |e| format!("Test failed: {e}"),
                        |()| "Connection works".into(),
                    ));
                },
            );
        });
        let (p, f) = (self.clone(), form.clone());
        save.connect_clicked(move |button| {
            let (spec, credentials) = match f.read() {
                Ok(v) => v,
                Err(e) => return p.toast(&e),
            };
            button.set_sensitive(false);
            let (p2, b) = (p.clone(), button.clone());
            p.backend.call(
                move |c| c.expect_ok(Request::MountAdd { spec, credentials }),
                move |r| {
                    b.set_sensitive(true);
                    match r {
                        Ok(()) => {
                            p2.dialog.pop_subpage();
                            p2.toast("Saved");
                        }
                        Err(e) => p2.toast(&e),
                    }
                    p2.load_mounts();
                    p2.load_volumes();
                },
            );
        });
        self.dialog.push_subpage(&page);
    }
}

/// The add/edit mount form.
struct MountForm {
    connection: adw::PreferencesGroup,
    login: adw::PreferencesGroup,
    indexing: adw::PreferencesGroup,
    name: adw::EntryRow,
    kind: adw::ComboRow,
    source: adw::EntryRow,
    mountpoint: adw::EntryRow,
    options: adw::EntryRow,
    username: adw::EntryRow,
    password: adw::PasswordEntryRow,
    domain: adw::EntryRow,
    auto_mount: adw::SwitchRow,
    index: adw::SwitchRow,
    max_ops: adw::SpinRow,
    recheck: adw::SpinRow,
    smb_notify: adw::SwitchRow,
    /// Settings the form does not show (exclusions, staleness, ...) are
    /// kept from the mount being edited.
    base: IndexSettings,
}

impl MountForm {
    fn new(existing: Option<&MountSpec>) -> Self {
        let names: Vec<&str> = KINDS.iter().map(|(n, _, _)| *n).collect();
        let f = MountForm {
            connection: adw::PreferencesGroup::builder().title("Connection").build(),
            login: adw::PreferencesGroup::builder()
                .title("Login")
                .description(
                    "Leave empty for guest access, or to keep the stored login when editing.",
                )
                .build(),
            indexing: adw::PreferencesGroup::builder().title("Indexing").build(),
            name: adw::EntryRow::builder().title("Name").build(),
            kind: adw::ComboRow::builder()
                .title("Type")
                .model(&gtk::StringList::new(&names))
                .build(),
            source: adw::EntryRow::builder().title("Source").build(),
            mountpoint: adw::EntryRow::builder()
                .title("Mount point (optional)")
                .build(),
            options: adw::EntryRow::builder()
                .title("Extra mount options (comma separated)")
                .build(),
            username: adw::EntryRow::builder().title("User name").build(),
            password: adw::PasswordEntryRow::builder().title("Password").build(),
            domain: adw::EntryRow::builder().title("Domain (optional)").build(),
            auto_mount: adw::SwitchRow::builder()
                .title("Mount at start-up")
                .active(true)
                .build(),
            index: adw::SwitchRow::builder()
                .title("Index this mount")
                .active(true)
                .build(),
            max_ops: adw::SpinRow::builder()
                .title("Operations per second limit")
                .subtitle("Protects busy servers; 0 means unlimited")
                .adjustment(&gtk::Adjustment::new(0.0, 0.0, 100_000.0, 50.0, 500.0, 0.0))
                .build(),
            recheck: adw::SpinRow::builder()
                .title("Re-check changed folders every (seconds)")
                .subtitle("Unchanged folders are checked less and less often, at least daily")
                .adjustment(&gtk::Adjustment::new(30.0, 5.0, 3600.0, 5.0, 60.0, 0.0))
                .build(),
            smb_notify: adw::SwitchRow::builder()
                .title("Use SMB change notifications")
                .active(true)
                .build(),
            base: existing.map(|s| s.index.clone()).unwrap_or_default(),
        };
        f.connection.add(&f.name);
        f.connection.add(&f.kind);
        f.connection.add(&f.source);
        f.connection.add(&f.mountpoint);
        f.connection.add(&f.options);
        f.login.add(&f.username);
        f.login.add(&f.password);
        f.login.add(&f.domain);
        for row in [&f.auto_mount, &f.index] {
            f.indexing.add(row);
        }
        f.indexing.add(&f.max_ops);
        f.indexing.add(&f.recheck);
        f.indexing.add(&f.smb_notify);

        let (login, notify, source) = (f.login.clone(), f.smb_notify.clone(), f.source.clone());
        let update = move |kind: &adw::ComboRow| {
            let (_, k, hint) = KINDS[kind.selected() as usize];
            login.set_visible(k == MountKind::Smb);
            notify.set_visible(k == MountKind::Smb);
            source.set_title(&format!("Source, e.g. {hint}"));
        };
        update(&f.kind);
        f.kind.connect_selected_notify(update);

        if let Some(spec) = existing {
            f.name.set_text(&spec.name);
            f.name.set_editable(false);
            let kind = KINDS
                .iter()
                .position(|(_, k, _)| *k == spec.kind)
                .unwrap_or(0);
            f.kind.set_selected(kind as u32);
            f.source.set_text(&spec.source);
            f.mountpoint
                .set_text(spec.mountpoint.as_deref().unwrap_or(""));
            f.options.set_text(&spec.options.join(","));
            f.auto_mount.set_active(spec.auto_mount);
            f.index.set_active(spec.index.enabled);
            f.max_ops
                .set_value(spec.index.max_ops_per_sec.unwrap_or(0.0));
            f.recheck
                .set_value(spec.index.min_recheck_secs.unwrap_or(30) as f64);
            f.smb_notify.set_active(spec.index.smb_notify);
        }
        f
    }

    /// Reads and validates the form.
    fn read(&self) -> Result<(MountSpec, Option<Credentials>), String> {
        let text = |row: &adw::EntryRow| row.text().trim().to_owned();
        let kind = KINDS[self.kind.selected() as usize].1;
        let mountpoint = text(&self.mountpoint);
        let max_ops = self.max_ops.value();
        let spec = MountSpec {
            name: text(&self.name),
            kind,
            source: text(&self.source),
            mountpoint: (!mountpoint.is_empty()).then_some(mountpoint),
            options: text(&self.options)
                .split(',')
                .map(|o| o.trim().to_owned())
                .filter(|o| !o.is_empty())
                .collect(),
            auto_mount: self.auto_mount.is_active(),
            index: IndexSettings {
                enabled: self.index.is_active(),
                max_ops_per_sec: (max_ops > 0.0).then_some(max_ops),
                min_recheck_secs: Some(self.recheck.value() as u64),
                smb_notify: self.smb_notify.is_active(),
                ..self.base.clone()
            },
        };
        spec.validate()?;
        let username = text(&self.username);
        let credentials = (kind == MountKind::Smb && !username.is_empty()).then(|| Credentials {
            username,
            password: self.password.text().to_string(),
            domain: Some(text(&self.domain)).filter(|d| !d.is_empty()),
        });
        Ok((spec, credentials))
    }
}
