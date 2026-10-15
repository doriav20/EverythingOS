//! The search window.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use evos_proto::{SearchOptions, Sort, SortKey};
use gtk::{gdk, gio, glib, graphene, pango};

use crate::backend::{Backend, Notice};
use crate::format;
use crate::model::ResultModel;
use crate::row_item::RowItem;

/// Type filters, as in Everything's filter drop-down.
const FILTERS: &[(&str, &str)] = &[
    ("Everything", ""),
    ("Audio", "audio:"),
    ("Video", "video:"),
    ("Pictures", "pic:"),
    ("Documents", "doc:"),
    ("Compressed", "zip:"),
    ("Executables", "exe:"),
    ("Folders", "folder:"),
];

/// Accessor for one boolean search option.
type OptionField = fn(&mut SearchOptions) -> &mut bool;
/// A window action handler.
type UiAction = Box<dyn Fn(&Rc<Ui>)>;

#[derive(Default)]
struct State {
    options: SearchOptions,
    filter: usize,
    sort: Sort,
    /// When the current results were produced and how long that took.
    last_search: Option<(Instant, Duration)>,
    refresh_scheduled: bool,
}

pub struct Ui {
    pub window: adw::ApplicationWindow,
    entry: gtk::SearchEntry,
    model: ResultModel,
    selection: gtk::MultiSelection,
    view: gtk::ColumnView,
    count: gtk::Label,
    status: gtk::Label,
    banner: adw::Banner,
    backend: Backend,
    state: RefCell<State>,
}

pub fn build(
    app: &adw::Application,
    backend: Backend,
    notices: async_channel::Receiver<Notice>,
) -> Rc<Ui> {
    let model = ResultModel::default();
    let selection = gtk::MultiSelection::new(Some(model.clone()));
    let view = gtk::ColumnView::new(Some(selection.clone()));
    view.set_show_column_separators(true);
    view.add_css_class("data-table");

    let entry = gtk::SearchEntry::builder()
        .placeholder_text("Search everything")
        .hexpand(true)
        .search_delay(60)
        .width_request(360)
        .build();
    let filter = gtk::DropDown::from_strings(&FILTERS.iter().map(|(n, _)| *n).collect::<Vec<_>>());
    filter.set_tooltip_text(Some("Filter by type"));

    let menu = gio::Menu::new();
    let options = gio::Menu::new();
    options.append(Some("Match _Case"), Some("win.match-case"));
    options.append(Some("Match _Whole Words"), Some("win.whole-word"));
    options.append(Some("Match _Path"), Some("win.match-path"));
    options.append(Some("_Regular Expressions"), Some("win.regex"));
    menu.append_section(None, &options);
    let app_section = gio::Menu::new();
    app_section.append(Some("_Preferences"), Some("app.preferences"));
    app_section.append(Some("_About EverythingOS"), Some("app.about"));
    menu.append_section(None, &app_section);
    let menu_button = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .tooltip_text("Main Menu")
        .build();

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(
        &adw::Clamp::builder()
            .maximum_size(720)
            .child(&entry)
            .build(),
    ));
    header.pack_start(&filter);
    header.pack_end(&menu_button);

    let banner = adw::Banner::builder()
        .title("Cannot reach the EverythingOS service")
        .button_label("Retry")
        .build();

    let count = gtk::Label::builder().xalign(0.0).hexpand(true).build();
    let status = gtk::Label::builder()
        .xalign(1.0)
        .ellipsize(pango::EllipsizeMode::Start)
        .build();
    let status_bar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .margin_start(12)
        .margin_end(12)
        .margin_top(4)
        .margin_bottom(4)
        .spacing(12)
        .build();
    status_bar.append(&count);
    status_bar.append(&status);
    status_bar.add_css_class("dim-label");

    let scrolled = gtk::ScrolledWindow::builder()
        .child(&view)
        .vexpand(true)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&banner);
    content.append(&scrolled);
    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    content.append(&status_bar);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&content));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("EverythingOS")
        .default_width(1100)
        .default_height(700)
        .content(&toolbar)
        .build();

    let ui = Rc::new(Ui {
        window,
        entry,
        model,
        selection,
        view,
        count,
        status,
        banner,
        backend,
        state: RefCell::new(State::default()),
    });
    ui.add_columns();
    ui.connect_fetcher();
    ui.add_actions(app);
    ui.connect_signals(&filter);
    ui.listen(notices);
    ui.search();
    ui.refresh_status();
    ui
}

impl Ui {
    /// The query as sent to the daemon, including the type filter.
    fn query(&self) -> String {
        let text = self.entry.text().to_string();
        let state = self.state.borrow();
        let prefix = FILTERS[state.filter].1;
        if prefix.is_empty() || state.options.regex {
            text
        } else {
            format!("{prefix} {text}")
        }
    }

    pub fn search(self: &Rc<Self>) {
        let query = self.query();
        let (options, sort) = {
            let s = self.state.borrow();
            (s.options.clone(), s.sort)
        };
        let generation = self.backend.next_search();
        let started = Instant::now();
        let ui = self.clone();
        self.backend.call_search(
            generation,
            move |c| c.search(&query, options, sort).map(|r| (r.id, r.total)),
            move |result| match result {
                Ok((result, total)) => {
                    ui.state.borrow_mut().last_search = Some((Instant::now(), started.elapsed()));
                    ui.show_results(result, total);
                }
                Err(e) => {
                    ui.count.set_text(&e);
                    ui.model.reset(0, 0);
                }
            },
        );
    }

    fn show_results(&self, result: u32, total: u64) {
        let old = self.model.result();
        self.model.reset(result, total);
        let noun = if total == 1 { "object" } else { "objects" };
        self.count
            .set_text(&format!("{} {noun}", format::count(total)));
        if old != 0 && old != result {
            self.backend.call(move |c| c.release(old), |_| {});
        }
    }

    fn connect_fetcher(self: &Rc<Self>) {
        let backend = self.backend.clone();
        let model = self.model.downgrade();
        self.model.set_fetcher(move |result, offset, count| {
            let model = model.clone();
            backend.call(
                move |c| c.fetch(result, offset, count),
                move |rows| {
                    let Some(model) = model.upgrade() else { return };
                    match rows {
                        Ok(rows) => model.set_rows(result, offset, rows),
                        Err(_) => model.fetch_failed(offset),
                    }
                },
            );
        });
    }

    fn add_columns(self: &Rc<Self>) {
        let name = self.column("Name", 320, |cell| {
            let icon = gtk::Image::new();
            let label = gtk::Label::builder()
                .xalign(0.0)
                .ellipsize(pango::EllipsizeMode::End)
                .build();
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.append(&icon);
            row.append(&label);
            let item = cell.property_expression("item");
            item.chain_property::<RowItem>("icon")
                .bind(&icon, "gicon", gtk::Widget::NONE);
            item.chain_property::<RowItem>("name")
                .bind(&label, "label", gtk::Widget::NONE);
            row.upcast()
        });
        let path = self.column("Path", 420, |cell| {
            let label = gtk::Label::builder()
                .xalign(0.0)
                .ellipsize(pango::EllipsizeMode::Middle)
                .build();
            cell.property_expression("item")
                .chain_property::<RowItem>("folder")
                .bind(&label, "label", gtk::Widget::NONE);
            label.upcast()
        });
        path.set_expand(true);
        self.column("Size", 100, |cell| {
            let label = gtk::Label::builder().xalign(1.0).build();
            label.add_css_class("numeric");
            cell.property_expression("item")
                .chain_property::<RowItem>("size")
                .bind(&label, "label", gtk::Widget::NONE);
            label.upcast()
        });
        self.column("Date Modified", 150, |cell| {
            let label = gtk::Label::builder().xalign(0.0).build();
            label.add_css_class("numeric");
            cell.property_expression("item")
                .chain_property::<RowItem>("modified")
                .bind(&label, "label", gtk::Widget::NONE);
            label.upcast()
        });
        self.view
            .sort_by_column(Some(&name), gtk::SortType::Ascending);
    }

    /// Adds a sortable column whose cells are built by `make`.
    fn column(
        self: &Rc<Self>,
        title: &str,
        width: i32,
        make: impl Fn(&gtk::ListItem) -> gtk::Widget + 'static,
    ) -> gtk::ColumnViewColumn {
        let factory = gtk::SignalListItemFactory::new();
        let ui = Rc::downgrade(self);
        factory.connect_setup(move |_, obj| {
            let cell = obj.downcast_ref::<gtk::ListItem>().expect("ListItem");
            let widget = make(cell);
            if let Some(ui) = ui.upgrade() {
                ui.add_cell_gestures(cell, &widget);
            }
            // Greyed out while loading or when the share is offline.
            let item = cell.property_expression("item");
            item.chain_property::<RowItem>("loaded")
                .bind(&widget, "sensitive", gtk::Widget::NONE);
            cell.set_child(Some(&widget));
        });
        let column = gtk::ColumnViewColumn::new(Some(title), Some(factory));
        column.set_resizable(true);
        column.set_fixed_width(width);
        // A no-op sorter makes the header clickable; the daemon sorts.
        column.set_sorter(Some(&gtk::CustomSorter::new(|_, _| gtk::Ordering::Equal)));
        self.view.append_column(&column);
        column
    }

    /// Right-click menu and dragging files out, per cell.
    fn add_cell_gestures(self: &Rc<Self>, cell: &gtk::ListItem, widget: &gtk::Widget) {
        let click = gtk::GestureClick::builder()
            .button(gdk::BUTTON_SECONDARY)
            .build();
        let (ui, cell_ref) = (Rc::downgrade(self), cell.downgrade());
        click.connect_pressed(move |gesture, _, x, y| {
            let (Some(ui), Some(cell)) = (ui.upgrade(), cell_ref.upgrade()) else {
                return;
            };
            let pos = cell.position();
            if !ui.selection.is_selected(pos) {
                ui.selection.select_item(pos, true);
            }
            let Some(widget) = gesture.widget() else {
                return;
            };
            let point = widget
                .compute_point(&ui.view, &graphene::Point::new(x as f32, y as f32))
                .unwrap_or(graphene::Point::new(x as f32, y as f32));
            ui.context_menu(point.x() as i32, point.y() as i32);
        });
        widget.add_controller(click);

        let drag = gtk::DragSource::builder()
            .actions(gdk::DragAction::COPY)
            .build();
        let cell_ref = cell.downgrade();
        drag.connect_prepare(move |_, _, _| {
            let item = cell_ref.upgrade()?.item().and_downcast::<RowItem>()?;
            item.loaded().then(|| {
                let files = gdk::FileList::from_array(&[item.file()]);
                gdk::ContentProvider::for_value(&files.to_value())
            })
        });
        widget.add_controller(drag);
    }

    fn context_menu(&self, x: i32, y: i32) {
        let menu = gio::Menu::new();
        menu.append(Some("_Open"), Some("win.open"));
        menu.append(Some("Open _Containing Folder"), Some("win.open-folder"));
        let copy = gio::Menu::new();
        copy.append(Some("Copy _Path"), Some("win.copy-path"));
        copy.append(Some("Copy _Name"), Some("win.copy-name"));
        menu.append_section(None, &copy);
        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        popover.set_parent(&self.view);
        popover.set_has_arrow(false);
        popover.set_halign(gtk::Align::Start);
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x, y, 1, 1)));
        popover.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        popover.popup();
    }

    /// Loaded items in the selection (at most `limit`).
    fn selected(&self, limit: usize) -> Vec<RowItem> {
        let set = self.selection.selection();
        let mut items = Vec::new();
        if set.is_empty() {
            return items;
        }
        let (min, max) = (set.minimum(), set.maximum());
        for pos in min..=max {
            if items.len() >= limit {
                break;
            }
            if set.contains(pos) {
                if let Some(item) = self.model.loaded_item(pos) {
                    items.push(item);
                }
            }
        }
        items
    }

    fn open(self: &Rc<Self>, item: &RowItem, containing_folder: bool) {
        let launcher = gtk::FileLauncher::new(Some(&item.file()));
        let window = self.window.clone();
        let report = move |r: Result<(), glib::Error>| {
            if let Err(e) = r {
                if !e.matches(gtk::DialogError::Dismissed) {
                    let toast_text = format!("Cannot open: {}", e.message());
                    let dialog = adw::AlertDialog::new(Some("Cannot open"), Some(&toast_text));
                    dialog.add_response("ok", "_OK");
                    dialog.present(Some(&window));
                }
            }
        };
        if containing_folder {
            launcher.open_containing_folder(Some(&self.window), gio::Cancellable::NONE, report);
        } else {
            launcher.launch(Some(&self.window), gio::Cancellable::NONE, report);
        }
        // Tell the daemon the user cares about this folder.
        let mut folder = item.path();
        if let Some(cut) = folder.iter().rposition(|&b| b == b'/') {
            folder.truncate(cut.max(1));
        }
        self.backend.call(
            move |c| c.expect_ok(evos_proto::Request::Hint { path: folder }),
            |_| {},
        );
    }

    fn add_actions(self: &Rc<Self>, app: &adw::Application) {
        let toggles: [(&str, OptionField); 4] = [
            ("match-case", |o| &mut o.match_case),
            ("whole-word", |o| &mut o.whole_word),
            ("match-path", |o| &mut o.match_path),
            ("regex", |o| &mut o.regex),
        ];
        for (name, field) in toggles {
            let action = gio::SimpleAction::new_stateful(name, None, &false.to_variant());
            let ui = self.clone();
            action.connect_activate(move |action, _| {
                let on = !action
                    .state()
                    .and_then(|s| s.get::<bool>())
                    .unwrap_or(false);
                action.set_state(&on.to_variant());
                *field(&mut ui.state.borrow_mut().options) = on;
                ui.search();
            });
            self.window.add_action(&action);
        }

        let simple = |name: &str, f: UiAction| {
            let action = gio::SimpleAction::new(name, None);
            let ui = self.clone();
            action.connect_activate(move |_, _| f(&ui));
            self.window.add_action(&action);
        };
        simple(
            "open",
            Box::new(|ui| {
                for item in ui.selected(16) {
                    ui.open(&item, false);
                }
            }),
        );
        simple(
            "open-folder",
            Box::new(|ui| {
                if let Some(item) = ui.selected(1).first() {
                    ui.open(item, true);
                }
            }),
        );
        simple(
            "copy-path",
            Box::new(|ui| {
                let paths: Vec<String> = ui
                    .selected(10_000)
                    .iter()
                    .map(|i| String::from_utf8_lossy(&i.path()).into_owned())
                    .collect();
                ui.window.clipboard().set_text(&paths.join("\n"));
            }),
        );
        simple(
            "copy-name",
            Box::new(|ui| {
                let names: Vec<String> = ui.selected(10_000).iter().map(|i| i.name()).collect();
                ui.window.clipboard().set_text(&names.join("\n"));
            }),
        );
        simple(
            "focus-search",
            Box::new(|ui| {
                ui.entry.grab_focus();
            }),
        );
        let preferences = gio::SimpleAction::new("preferences", None);
        let ui = self.clone();
        preferences.connect_activate(move |_, _| crate::prefs::show(&ui.window, &ui.backend));
        app.add_action(&preferences);
        app.set_accels_for_action("app.preferences", &["<Ctrl>comma"]);
        app.set_accels_for_action("win.focus-search", &["<Ctrl>f", "<Ctrl>l"]);
        app.set_accels_for_action("win.copy-path", &["<Ctrl><Shift>c"]);
        app.set_accels_for_action("win.open-folder", &["<Ctrl>Return"]);
        app.set_accels_for_action("win.match-case", &["<Ctrl>i"]);
        app.set_accels_for_action("win.regex", &["<Ctrl>r"]);
    }

    fn connect_signals(self: &Rc<Self>, filter: &gtk::DropDown) {
        let ui = self.clone();
        self.entry.connect_search_changed(move |_| ui.search());
        let ui = self.clone();
        self.entry.connect_stop_search(move |entry| {
            if entry.text().is_empty() {
                ui.window.close();
            } else {
                entry.set_text("");
            }
        });
        // Enter in the search box opens the first result.
        let ui = self.clone();
        self.entry.connect_activate(move |_| {
            if let Some(item) = ui.model.loaded_item(0) {
                ui.open(&item, false);
            }
        });
        let ui = self.clone();
        filter.connect_selected_notify(move |dd| {
            ui.state.borrow_mut().filter = dd.selected() as usize;
            ui.search();
        });
        let ui = self.clone();
        self.view.connect_activate(move |_, pos| {
            if let Some(item) = ui.model.loaded_item(pos) {
                ui.open(&item, false);
            }
        });
        if let Some(sorter) = self.view.sorter().and_downcast::<gtk::ColumnViewSorter>() {
            let ui = self.clone();
            sorter.connect_changed(move |sorter, _| ui.sort_changed(sorter));
        }
        let ui = self.clone();
        self.banner.connect_button_clicked(move |_| {
            ui.search();
            ui.refresh_status();
        });
    }

    fn sort_changed(self: &Rc<Self>, sorter: &gtk::ColumnViewSorter) {
        let key = match sorter
            .primary_sort_column()
            .and_then(|c| c.title())
            .as_deref()
        {
            Some("Path") => SortKey::Path,
            Some("Size") => SortKey::Size,
            Some("Date Modified") => SortKey::Modified,
            _ => SortKey::Name,
        };
        let sort = Sort {
            key,
            descending: sorter.primary_sort_order() == gtk::SortType::Descending,
        };
        if self.state.borrow().sort == sort {
            return;
        }
        self.state.borrow_mut().sort = sort;
        let result = self.model.result();
        let total = self.model.total() as u64;
        if result == 0 {
            return self.search();
        }
        let ui = self.clone();
        self.backend.call(
            move |c| c.sort(result, sort),
            move |r| match r {
                Ok(()) => ui.model.reset(result, total),
                Err(_) => ui.search(),
            },
        );
    }

    /// Reacts to daemon notifications.
    fn listen(self: &Rc<Self>, notices: async_channel::Receiver<Notice>) {
        let ui = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(notice) = notices.recv().await {
                let Some(ui) = ui.upgrade() else { return };
                match notice {
                    Notice::Connected => {
                        ui.banner.set_revealed(false);
                        ui.search();
                        ui.refresh_status();
                    }
                    Notice::Disconnected => ui.banner.set_revealed(true),
                    Notice::IndexChanged => ui.schedule_refresh(),
                    Notice::StatusChanged => ui.refresh_status(),
                }
            }
        });
        // Keep the status line current while volumes are being scanned.
        let ui = Rc::downgrade(self);
        glib::timeout_add_seconds_local(3, move || match ui.upgrade() {
            Some(ui) => {
                if ui.window.is_visible() {
                    ui.refresh_status();
                }
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });
    }

    /// Re-runs the search after index changes, at most once per second, or
    /// less often when searches are slow (huge result sets).
    fn schedule_refresh(self: &Rc<Self>) {
        let delay = {
            let mut s = self.state.borrow_mut();
            if s.refresh_scheduled {
                return;
            }
            s.refresh_scheduled = true;
            let (when, took) = s.last_search.unwrap_or((Instant::now(), Duration::ZERO));
            let min_gap = (took * 10).clamp(Duration::from_secs(1), Duration::from_secs(10));
            min_gap.saturating_sub(when.elapsed())
        };
        let ui = self.clone();
        glib::timeout_add_local_once(delay, move || {
            ui.state.borrow_mut().refresh_scheduled = false;
            if ui.window.is_visible() {
                ui.search();
            }
        });
    }

    pub fn refresh_status(self: &Rc<Self>) {
        let ui = self.clone();
        self.backend.call(
            |c| c.status(),
            move |r| match r {
                Ok(status) => {
                    ui.banner.set_revealed(false);
                    ui.status.set_text(&summarize(&status));
                }
                Err(_) => ui.banner.set_revealed(true),
            },
        );
    }
}

/// One-line summary of the daemon's volumes.
fn summarize(status: &evos_proto::DaemonStatus) -> String {
    let name = |v: &evos_proto::VolumeInfo| String::from_utf8_lossy(&v.root).into_owned();
    let scanning: Vec<String> = status
        .volumes
        .iter()
        .filter(|v| v.phase == "scanning" || v.phase == "reconciling" || v.phase == "starting")
        .map(|v| format!("{} ({} folders)", name(v), format::count(v.dirs_listed)))
        .collect();
    let offline: Vec<String> = status
        .volumes
        .iter()
        .filter(|v| v.phase == "offline")
        .map(name)
        .collect();
    let mut parts = Vec::new();
    if !scanning.is_empty() {
        parts.push(format!("Indexing {}", scanning.join(", ")));
    }
    if !offline.is_empty() {
        parts.push(format!("Offline: {}", offline.join(", ")));
    }
    if parts.is_empty() {
        let n = status.volumes.len();
        parts.push(format!(
            "{n} {} up to date",
            if n == 1 { "volume" } else { "volumes" }
        ));
    }
    parts.join(" · ")
}
