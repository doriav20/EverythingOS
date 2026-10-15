//! Properties, statistics and bookmark dialogs.

use adw::prelude::*;
use evos_proto::{Row, VolumeStatistics};

use crate::format;

fn row(title: &str, value: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(if value.is_empty() { "—" } else { value })
        .subtitle_selectable(true)
        .build();
    row.add_css_class("property");
    row
}

fn date(ns: i64) -> String {
    if ns == 0 {
        String::new()
    } else {
        format::date(ns)
    }
}

/// Shows everything the index knows about one result.
pub fn properties(parent: &impl IsA<gtk::Widget>, r: &Row) {
    let dialog = adw::PreferencesDialog::new();
    dialog.set_title("Properties");
    dialog.set_content_width(520);
    let page = adw::PreferencesPage::new();
    let general = adw::PreferencesGroup::builder()
        .title(String::from_utf8_lossy(&r.name).as_ref())
        .build();
    general.add(&row("Location", &String::from_utf8_lossy(&r.folder)));
    general.add(&row("Type", &r.type_name));
    let size = r
        .size
        .map(|s| format!("{} ({} bytes)", format::size(s), format::count(s)))
        .unwrap_or_default();
    general.add(&row("Size", &size));
    general.add(&row("Attributes", &r.attributes));
    let dates = adw::PreferencesGroup::builder().title("Dates").build();
    dates.add(&row("Date Created", &date(r.created)));
    dates.add(&row("Date Modified", &date(r.mtime)));
    dates.add(&row("Date Accessed", &date(r.accessed)));
    dates.add(&row("Date Recently Changed", &date(r.changed)));
    let runs = adw::PreferencesGroup::builder().title("History").build();
    runs.add(&row("Run Count", &r.run_count.to_string()));
    runs.add(&row("Date Run", &date(r.date_run)));
    page.add(&general);
    page.add(&dates);
    page.add(&runs);
    dialog.add(&page);
    dialog.present(Some(parent));
}

/// Shows file, folder and size totals per volume.
pub fn statistics(parent: &impl IsA<gtk::Widget>, stats: &[VolumeStatistics]) {
    let dialog = adw::PreferencesDialog::new();
    dialog.set_title("Statistics");
    dialog.set_content_width(560);
    let page = adw::PreferencesPage::new();
    let totals = adw::PreferencesGroup::builder().title("Total").build();
    let sum = |f: fn(&VolumeStatistics) -> u64| stats.iter().map(f).sum::<u64>();
    totals.add(&row("Files", &format::count(sum(|v| v.files))));
    totals.add(&row("Folders", &format::count(sum(|v| v.folders))));
    totals.add(&row("Total Size", &format::size(sum(|v| v.total_size))));
    totals.add(&row("Index Memory", &format::size(sum(|v| v.index_bytes))));
    page.add(&totals);
    for v in stats {
        let kind = if v.network { "network share" } else { "local" };
        let group = adw::PreferencesGroup::builder()
            .title(String::from_utf8_lossy(&v.root).as_ref())
            .description(format!("{} · {kind}", v.fs_type))
            .build();
        group.add(&row("Files", &format::count(v.files)));
        group.add(&row("Folders", &format::count(v.folders)));
        group.add(&row("Total Size", &format::size(v.total_size)));
        group.add(&row("Index Memory", &format::size(v.index_bytes)));
        page.add(&group);
    }
    dialog.add(&page);
    dialog.present(Some(parent));
}

/// Asks for a bookmark name; `done` gets it.
pub fn bookmark_name(
    parent: &impl IsA<gtk::Widget>,
    suggestion: &str,
    done: impl Fn(String) + 'static,
) {
    let dialog = adw::AlertDialog::new(
        Some("Add to Bookmarks"),
        Some("The search, its filter, options and sort are saved."),
    );
    // Enter in the name field confirms.
    let entry = adw::EntryRow::builder()
        .title("Name")
        .text(suggestion)
        .activates_default(true)
        .build();
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    list.append(&entry);
    dialog.set_extra_child(Some(&list));
    dialog.add_responses(&[("cancel", "_Cancel"), ("add", "_Add")]);
    dialog.set_response_appearance("add", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("add"));
    let field = entry.clone();
    dialog.connect_response(None, move |_, response| {
        let name = entry.text().trim().to_string();
        if response == "add" && !name.is_empty() {
            done(name);
        }
    });
    dialog.present(Some(parent));
    // Type the name right away.
    field.grab_focus();
}

/// Lists bookmarks with a remove button each; `remove` gets the name.
pub fn organize_bookmarks(
    parent: &impl IsA<gtk::Widget>,
    bookmarks: &[crate::settings::Bookmark],
    remove: impl Fn(String) + Clone + 'static,
) {
    let dialog = adw::PreferencesDialog::new();
    dialog.set_title("Bookmarks");
    dialog.set_content_width(520);
    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::builder()
        .title("Bookmarks")
        .description("Open bookmarks from the bookmark menu or with evos-gtk -bookmark NAME.")
        .build();
    if bookmarks.is_empty() {
        group.add(&row("No bookmarks", "Add one with Ctrl+D."));
    }
    for b in bookmarks {
        let r = adw::ActionRow::builder()
            .title(b.name.as_str())
            .subtitle(b.search.as_str())
            .build();
        let button = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Remove")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        button.update_property(&[gtk::accessible::Property::Label("Remove")]);
        let (name, remove, r2, group2) = (b.name.clone(), remove.clone(), r.clone(), group.clone());
        button.connect_clicked(move |_| {
            remove(name.clone());
            group2.remove(&r2);
        });
        r.add_suffix(&button);
        group.add(&r);
    }
    page.add(&group);
    dialog.add(&page);
    dialog.present(Some(parent));
}
