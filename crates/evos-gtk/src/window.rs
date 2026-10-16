//! The search window.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use evos_proto::{ClientError, ErrorCode, RemoteSearch, Request, RunChange, SearchOptions, Sort};
use gtk::{gdk, gio, glib, graphene, pango};

use crate::backend::{cache_path, describe, Backend, Notice};
use crate::cmdline::{Focus, Launch};
use crate::columns::{self, ColumnDef, COLUMNS};
use crate::export::{Exporter, Format};
use crate::format;
use crate::model::ResultModel;
use crate::preview::Preview;
use crate::row_item::RowItem;
use crate::settings::{Bookmark, Settings, OPTIONS};

/// Type filters, as in Everything's filter drop-down.
pub const FILTERS: &[(&str, &str)] = &[
    ("Everything", ""),
    ("Audio", "audio:"),
    ("Video", "video:"),
    ("Pictures", "pic:"),
    ("Documents", "doc:"),
    ("Compressed", "zip:"),
    ("Executables", "exe:"),
    ("Folders", "folder:"),
];

/// Menu labels of the search options, by action name.
const OPTION_LABELS: [(&str, &str); 9] = [
    ("match-case", "Match _Case"),
    ("whole-word", "Match _Whole Words"),
    ("match-path", "Match _Path"),
    ("match-diacritics", "Match _Diacritics"),
    ("match-prefix", "Match P_refix"),
    ("match-suffix", "Match _Suffix"),
    ("ignore-punctuation", "Ignore Pu_nctuation"),
    ("ignore-white-space", "Ignore White Spac_e"),
    ("regex", "Enable Re_gex"),
];

/// Searches the user lingers on this long are added to the history.
const REMEMBER_AFTER: Duration = Duration::from_secs(2);

type UiAction = Box<dyn Fn(&Rc<Ui>)>;
type UiTextAction = Box<dyn Fn(&Rc<Ui>, String)>;
type UiToggle = Box<dyn Fn(&Rc<Ui>, bool)>;

#[derive(Default)]
struct State {
    options: SearchOptions,
    filter: usize,
    sort: Sort,
    /// When the current results were produced and how long that took.
    last_search: Option<(Instant, Duration)>,
    refresh_scheduled: bool,
    remember: Option<glib::SourceId>,
    /// Set while the header's sort indicator is changed from code.
    syncing_sort: bool,
}

pub struct Ui {
    pub window: adw::ApplicationWindow,
    entry: gtk::SearchEntry,
    filter: gtk::DropDown,
    model: ResultModel,
    selection: gtk::MultiSelection,
    view: gtk::ColumnView,
    columns: Vec<(&'static ColumnDef, gtk::ColumnViewColumn)>,
    count: gtk::Label,
    status: gtk::Label,
    status_bar: gtk::Box,
    /// Shows the ETP server a window searches, with a way back.
    server_button: gtk::MenuButton,
    banner: adw::Banner,
    toasts: adw::ToastOverlay,
    preview: Rc<Preview>,
    bookmarks_menu: gio::Menu,
    history_menu: gio::Menu,
    backend: Backend,
    settings: Rc<RefCell<Settings>>,
    state: RefCell<State>,
}

fn menus(columns_menu: &gio::Menu, history_menu: &gio::Menu) -> gio::Menu {
    let menu = gio::Menu::new();
    let file = gio::Menu::new();
    file.append(Some("_New Window"), Some("app.new-window"));
    file.append(Some("Connect to _ETP Server…"), Some("app.connect"));
    file.append(Some("_Disconnect"), Some("win.disconnect"));
    file.append(Some("_Export Results…"), Some("win.export"));
    menu.append_section(None, &file);

    let search = gio::Menu::new();
    for (name, label) in OPTION_LABELS {
        search.append(Some(label), Some(&format!("win.{name}")));
    }
    menu.append_submenu(Some("_Search"), &search);

    let view = gio::Menu::new();
    view.append(Some("_Preview"), Some("win.preview"));
    view.append(Some("_Status Bar"), Some("win.status-bar"));
    view.append_submenu(Some("_Columns"), columns_menu);
    let sort = gio::Menu::new();
    let keys = gio::Menu::new();
    for c in COLUMNS {
        keys.append(
            Some(c.title),
            Some(&format!("win.sort-by::{}", c.sort.name())),
        );
    }
    sort.append_section(None, &keys);
    let direction = gio::Menu::new();
    direction.append(Some("_Descending"), Some("win.sort-descending"));
    sort.append_section(None, &direction);
    view.append_submenu(Some("Sort _By"), &sort);
    menu.append_submenu(Some("_View"), &view);
    menu.append_submenu(Some("_Recent Searches"), history_menu);

    let tools = gio::Menu::new();
    tools.append(Some("_Statistics"), Some("win.statistics"));
    tools.append(Some("_Rescan All Volumes"), Some("win.reindex"));
    tools.append(Some("Save _Index Now"), Some("win.save-db"));
    menu.append_submenu(Some("_Tools"), &tools);

    let app = gio::Menu::new();
    app.append(Some("_Preferences"), Some("app.preferences"));
    app.append(Some("_About EverythingOS"), Some("app.about"));
    menu.append_section(None, &app);
    menu
}

/// Menu labels treat `_` as a mnemonic marker.
fn menu_label(text: &str) -> String {
    let text = text.replace('_', "__");
    if text.chars().count() > 60 {
        text.chars().take(59).collect::<String>() + "…"
    } else {
        text
    }
}

pub fn build(
    app: &adw::Application,
    backend: Backend,
    notices: async_channel::Receiver<Notice>,
    settings: Rc<RefCell<Settings>>,
) -> Rc<Ui> {
    let model = ResultModel::default();
    let selection = gtk::MultiSelection::new(Some(model.clone()));
    let view = gtk::ColumnView::new(Some(selection.clone()));
    view.set_show_column_separators(true);
    view.add_css_class("data-table");
    view.update_property(&[gtk::accessible::Property::Label("Results")]);

    let entry = gtk::SearchEntry::builder()
        .placeholder_text("Search everything")
        .hexpand(true)
        .search_delay(60)
        .width_request(360)
        .build();
    entry.update_property(&[gtk::accessible::Property::Label("Search")]);
    let filter = gtk::DropDown::from_strings(&FILTERS.iter().map(|(n, _)| *n).collect::<Vec<_>>());
    filter.set_tooltip_text(Some("Filter by type"));

    let columns_menu = gio::Menu::new();
    for c in COLUMNS {
        columns_menu.append(Some(c.title), Some(&format!("win.column-{}", c.id)));
    }
    let history_menu = gio::Menu::new();
    let menu_button = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menus(&columns_menu, &history_menu))
        .tooltip_text("Main Menu")
        .primary(true)
        .build();
    let bookmarks_root = gio::Menu::new();
    let manage = gio::Menu::new();
    manage.append(Some("_Add to Bookmarks…"), Some("win.add-bookmark-dialog"));
    manage.append(Some("_Organize Bookmarks…"), Some("win.organize-bookmarks"));
    bookmarks_root.append_section(None, &manage);
    let bookmarks_menu = gio::Menu::new();
    bookmarks_root.append_section(None, &bookmarks_menu);
    let bookmarks_button = gtk::MenuButton::builder()
        .icon_name("starred-symbolic")
        .menu_model(&bookmarks_root)
        .tooltip_text("Bookmarks")
        .build();

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(
        &adw::Clamp::builder()
            .maximum_size(720)
            .child(&entry)
            .build(),
    ));
    header.pack_start(&filter);
    let server_menu = gio::Menu::new();
    server_menu.append(Some("_Disconnect"), Some("win.disconnect"));
    let server_button = gtk::MenuButton::builder()
        .menu_model(&server_menu)
        .visible(false)
        .css_classes(["flat"])
        .build();
    header.pack_start(&server_button);
    header.pack_end(&menu_button);
    header.pack_end(&bookmarks_button);

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

    let preview = Preview::new(backend.clone());
    let scrolled = gtk::ScrolledWindow::builder()
        .child(&view)
        .vexpand(true)
        .hexpand(true)
        .build();
    let paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&scrolled)
        .end_child(&preview.widget)
        .resize_start_child(true)
        .resize_end_child(false)
        .shrink_end_child(false)
        .position(780)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&banner);
    content.append(&paned);
    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    content.append(&status_bar);
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&content));

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&toasts));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("EverythingOS")
        .default_width(1100)
        .default_height(700)
        .content(&toolbar)
        .build();

    let (options, sort, show_preview, show_status) = {
        let s = settings.borrow();
        (s.options.clone(), s.sort, s.preview, s.status_bar)
    };
    preview.widget.set_visible(show_preview);
    status_bar.set_visible(show_status);

    let mut ui = Ui {
        window,
        entry,
        filter,
        model,
        selection,
        view,
        columns: Vec::new(),
        count,
        status,
        status_bar,
        server_button,
        banner,
        toasts,
        preview,
        bookmarks_menu,
        history_menu,
        backend,
        settings,
        state: RefCell::new(State {
            options,
            sort,
            ..Default::default()
        }),
    };
    ui.columns = ui.make_columns(&columns_menu);
    let ui = Rc::new(ui);
    ui.connect_cells();
    ui.connect_fetcher();
    ui.add_actions(app);
    ui.connect_signals();
    ui.listen(notices);
    ui.show_sort_indicator(sort);
    ui.rebuild_bookmarks();
    ui.rebuild_history();
    ui.show_connection();
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

    pub fn backend(&self) -> &Backend {
        &self.backend
    }

    /// The ETP server this window searches, if any.
    pub fn remote(&self) -> Option<RemoteSearch> {
        self.backend.remote()
    }

    /// Shows which index the window searches: the title, the banner and
    /// the actions that only make sense for this computer's index.
    fn show_connection(&self) {
        let remote = self.remote();
        match &remote {
            Some(r) => {
                let who = describe(r);
                self.window
                    .set_title(Some(&format!("{who} — EverythingOS")));
                let content = adw::ButtonContent::builder()
                    .icon_name("network-server-symbolic")
                    .label(&who)
                    .build();
                self.server_button.set_child(Some(&content));
                let searching = format!("Searching {who}");
                self.server_button.set_tooltip_text(Some(&searching));
                self.server_button
                    .update_property(&[gtk::accessible::Property::Label(&searching)]);
                self.server_button.set_visible(true);
                self.banner.set_title(&format!("Cannot reach {who}"));
                self.entry
                    .set_placeholder_text(Some(&format!("Search {}", r.host)));
            }
            None => {
                self.server_button.set_visible(false);
                self.window.set_title(Some("EverythingOS"));
                self.banner
                    .set_title("Cannot reach the EverythingOS service");
                self.entry.set_placeholder_text(Some("Search everything"));
            }
        }
        for name in ["reindex", "save-db", "statistics"] {
            if let Some(a) = self.action(name) {
                a.set_enabled(remote.is_none());
            }
        }
        if let Some(a) = self.action("disconnect") {
            a.set_enabled(remote.is_some());
        }
    }

    /// Switches the window to an ETP server, or back to this computer.
    pub fn connect_to(self: &Rc<Self>, target: Option<RemoteSearch>) {
        let ui = self.clone();
        self.backend.set_remote(target, move |r| {
            if let Err(e) = r {
                ui.toast(&e);
            }
            ui.show_connection();
            ui.search();
            ui.refresh_status();
        });
        self.show_connection();
    }

    fn toast(&self, text: &str) {
        self.toasts.add_toast(adw::Toast::new(text));
    }

    fn save_settings(&self) {
        self.settings.borrow().save();
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
            move |c| c.search(&query, options, sort),
            move |result| match result {
                Ok(found) => {
                    ui.state.borrow_mut().last_search = Some((Instant::now(), started.elapsed()));
                    ui.show_results(found.id, found.total, found.highlight);
                }
                Err(e) => {
                    ui.count.set_text(&e);
                    ui.model.reset(0, 0);
                }
            },
        );
        self.remember_later();
    }

    fn show_results(&self, result: u32, total: u64, terms: Vec<String>) {
        let old = self.model.result();
        self.model.set_terms(terms);
        self.model.reset(result, total);
        self.update_count();
        // The selection went with the old results.
        if self.preview.widget.is_visible() {
            self.preview.show(None);
        }
        if old != 0 && old != result {
            self.backend.call(move |c| c.release(old), |_| {});
        }
    }

    fn update_count(&self) {
        let total = self.model.total() as u64;
        let noun = if total == 1 { "object" } else { "objects" };
        let mut text = format!("{} {noun}", format::count(total));
        let selected = self.selection.selection().size();
        if selected > 0 {
            text.push_str(&format!(" · {} selected", format::count(selected)));
        }
        self.count.set_text(&text);
    }

    /// Adds the search to the history once the user stays on it.
    fn remember_later(self: &Rc<Self>) {
        if let Some(id) = self.state.borrow_mut().remember.take() {
            id.remove();
        }
        let ui = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(REMEMBER_AFTER, move || {
            let Some(ui) = ui.upgrade() else { return };
            ui.state.borrow_mut().remember = None;
            ui.remember_now();
        });
        self.state.borrow_mut().remember = Some(id);
    }

    fn remember_now(&self) {
        let text = self.entry.text();
        if text.trim().is_empty() {
            return;
        }
        let changed = {
            let mut s = self.settings.borrow_mut();
            let before = s.history.first().cloned();
            s.remember(&text);
            before.as_deref() != Some(text.trim())
        };
        if changed {
            self.save_settings();
            self.rebuild_history();
        }
    }

    fn rebuild_history(&self) {
        self.history_menu.remove_all();
        let recent = gio::Menu::new();
        for h in self.settings.borrow().history.iter().take(15) {
            let item = gio::MenuItem::new(Some(&menu_label(h)), None);
            item.set_action_and_target_value(Some("win.search-history"), Some(&h.to_variant()));
            recent.append_item(&item);
        }
        self.history_menu.append_section(None, &recent);
        let clear = gio::Menu::new();
        clear.append(Some("_Clear History"), Some("win.clear-history"));
        self.history_menu.append_section(None, &clear);
    }

    fn rebuild_bookmarks(&self) {
        self.bookmarks_menu.remove_all();
        for b in &self.settings.borrow().bookmarks {
            let item = gio::MenuItem::new(Some(&menu_label(&b.name)), None);
            item.set_action_and_target_value(Some("win.open-bookmark"), Some(&b.name.to_variant()));
            self.bookmarks_menu.append_item(&item);
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

    fn make_columns(
        &self,
        columns_menu: &gio::Menu,
    ) -> Vec<(&'static ColumnDef, gtk::ColumnViewColumn)> {
        let visible = self.settings.borrow().columns.clone();
        COLUMNS
            .iter()
            .map(|def| {
                let factory = gtk::SignalListItemFactory::new();
                factory.connect_setup(move |_, obj| {
                    let cell = obj.downcast_ref::<gtk::ListItem>().expect("ListItem");
                    let label = gtk::Label::builder()
                        .xalign(if def.numeric { 1.0 } else { 0.0 })
                        .use_markup(def.markup)
                        .ellipsize(if def.id == "path" {
                            pango::EllipsizeMode::Middle
                        } else {
                            pango::EllipsizeMode::End
                        })
                        .build();
                    if def.numeric || def.id.starts_with("date") {
                        label.add_css_class("numeric");
                    }
                    let item = cell.property_expression("item");
                    item.chain_property::<RowItem>(def.property).bind(
                        &label,
                        "label",
                        gtk::Widget::NONE,
                    );
                    let widget: gtk::Widget = if def.id == "name" {
                        let icon = gtk::Image::new();
                        item.chain_property::<RowItem>("icon").bind(
                            &icon,
                            "gicon",
                            gtk::Widget::NONE,
                        );
                        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                        row.append(&icon);
                        row.append(&label);
                        row.upcast()
                    } else {
                        label.upcast()
                    };
                    // Greyed out while loading.
                    item.chain_property::<RowItem>("loaded").bind(
                        &widget,
                        "sensitive",
                        gtk::Widget::NONE,
                    );
                    cell.set_child(Some(&widget));
                });
                let column = gtk::ColumnViewColumn::new(Some(def.title), Some(factory));
                column.set_id(Some(def.id));
                column.set_resizable(true);
                column.set_fixed_width(def.width);
                column.set_expand(def.id == "path");
                column.set_visible(visible.iter().any(|v| v == def.id));
                column.set_header_menu(Some(columns_menu));
                // A no-op sorter makes the header clickable; the daemon sorts.
                column.set_sorter(Some(&gtk::CustomSorter::new(|_, _| gtk::Ordering::Equal)));
                self.view.append_column(&column);
                (def, column)
            })
            .collect()
    }

    /// Right-click menus and dragging files out, for every cell.
    fn connect_cells(self: &Rc<Self>) {
        for (_, column) in &self.columns {
            let Some(factory) = column
                .factory()
                .and_downcast::<gtk::SignalListItemFactory>()
            else {
                continue;
            };
            let ui = Rc::downgrade(self);
            factory.connect_setup(move |_, obj| {
                let cell = obj.downcast_ref::<gtk::ListItem>().expect("ListItem");
                if let (Some(ui), Some(widget)) = (ui.upgrade(), cell.child()) {
                    ui.add_cell_gestures(cell, &widget);
                }
            });
        }
    }

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
                if item.row().remote {
                    // Not a file here: its path as text.
                    let path = String::from_utf8_lossy(&item.path()).into_owned();
                    return gdk::ContentProvider::for_value(&path.to_value());
                }
                let files = gdk::FileList::from_array(&[item.file()]);
                gdk::ContentProvider::for_value(&files.to_value())
            })
        });
        widget.add_controller(drag);
    }

    fn context_menu(&self, x: i32, y: i32) {
        let menu = gio::Menu::new();
        let open = gio::Menu::new();
        open.append(Some("_Open"), Some("win.open"));
        open.append(Some("Open _With…"), Some("win.open-with"));
        open.append(Some("Open _Containing Folder"), Some("win.open-folder"));
        if self.selected(1).first().is_some_and(|i| i.row().remote) {
            open.append(Some("Save a Cop_y…"), Some("win.save-copy"));
        }
        menu.append_section(None, &open);
        let copy = gio::Menu::new();
        copy.append(Some("_Copy"), Some("win.copy"));
        copy.append(Some("Copy Full _Path"), Some("win.copy-path"));
        copy.append(Some("Copy _Name"), Some("win.copy-name"));
        copy.append(Some("Copy _Folder Path"), Some("win.copy-folder"));
        menu.append_section(None, &copy);
        let props = gio::Menu::new();
        props.append(Some("P_roperties"), Some("win.properties"));
        menu.append_section(None, &props);
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

    fn open(self: &Rc<Self>, item: &RowItem, how: Open) {
        if item.row().remote {
            return self.open_remote(item, how);
        }
        self.launch(item, &item.file(), how);
        // Tell the daemon the user cares about this folder.
        let mut folder = item.path();
        if let Some(cut) = folder.iter().rposition(|&b| b == b'/') {
            folder.truncate(cut.max(1));
        }
        self.backend
            .call(move |c| c.expect_ok(Request::Hint { path: folder }), |_| {});
    }

    /// Opens `file`, the item's file or a copy of it.
    fn launch(self: &Rc<Self>, item: &RowItem, file: &gio::File, how: Open) {
        let launcher = gtk::FileLauncher::new(Some(file));
        launcher.set_always_ask(how == Open::With);
        let window = self.window.clone();
        let report = move |r: Result<(), glib::Error>| {
            if let Err(e) = r {
                if !e.matches(gtk::DialogError::Dismissed) {
                    let text = format!("Cannot open: {}", e.message());
                    let dialog = adw::AlertDialog::new(Some("Cannot open"), Some(&text));
                    dialog.add_response("ok", "_OK");
                    dialog.present(Some(&window));
                }
            }
        };
        if how == Open::Folder {
            launcher.open_containing_folder(Some(&self.window), gio::Cancellable::NONE, report);
        } else {
            launcher.launch(Some(&self.window), gio::Cancellable::NONE, report);
            // Everything's run history: Run Count and Date Run.
            let path = item.path();
            self.backend
                .call(move |c| c.run_count(&path, RunChange::Opened), |_| {});
        }
        self.remember_now();
    }

    /// Results on another computer: folders are listed in the window, as
    /// a search; files are copied here and the copy is opened.
    fn open_remote(self: &Rc<Self>, item: &RowItem, how: Open) {
        let row = item.row();
        let folder = match how {
            Open::Folder => Some(row.folder.clone()).filter(|f| !f.is_empty()),
            _ if row.is_dir => Some(item.path()),
            _ => None,
        };
        if let Some(folder) = folder {
            let folder = String::from_utf8_lossy(&folder).into_owned();
            self.entry.set_text(&format!("parent:\"{folder}\""));
            self.entry.set_position(-1);
            return;
        }
        let path = item.path();
        let dest = cache_path(&path, &item.name());
        let ui = self.clone();
        let item = item.clone();
        self.download(path, dest, move |copy| {
            ui.launch(&item, &gio::File::for_path(copy), how);
        });
    }

    /// Copies a file of another computer to `dest`, showing the progress.
    fn download(
        self: &Rc<Self>,
        path: Vec<u8>,
        dest: PathBuf,
        then: impl FnOnce(PathBuf) + 'static,
    ) {
        let name = dest
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let toast = adw::Toast::builder()
            .title(glib::markup_escape_text(&format!("Downloading {name}…")))
            .timeout(0)
            .build();
        self.toasts.add_toast(toast.clone());
        let (t, n) = (toast.clone(), name.clone());
        let ui = self.clone();
        self.backend.download(
            path,
            dest,
            move |bytes| {
                t.set_title(&glib::markup_escape_text(&format!(
                    "Downloading {n}… {}",
                    format::size(bytes)
                )))
            },
            move |r| {
                toast.dismiss();
                match r {
                    Ok(copy) => then(copy),
                    Err(e) => ui.toast(&format!("Cannot download {name}: {e}")),
                }
            },
        );
    }

    /// Saves a copy of the selected result on another computer.
    fn save_copy(self: &Rc<Self>) {
        let Some(item) = self.selected(1).into_iter().next() else {
            return;
        };
        if !item.row().remote || item.is_dir() {
            return;
        }
        let dialog = gtk::FileDialog::builder()
            .title("Save a Copy")
            .initial_name(item.name())
            .modal(true)
            .build();
        let ui = self.clone();
        dialog.save(Some(&self.window), gio::Cancellable::NONE, move |r| {
            let Some(dest) = r.ok().and_then(|f| f.path()) else {
                return;
            };
            let u = ui.clone();
            ui.download(item.path(), dest, move |copy| {
                u.toast(&format!("Saved {}", copy.display()));
            });
        });
    }

    fn copy_files(&self) {
        let items = self.selected(100_000);
        if items.is_empty() {
            return;
        }
        if items.iter().any(|i| i.row().remote) {
            // Files of other computers are not files here: copy paths.
            self.window.clipboard().set_text(&paths_text(&items));
            return;
        }
        let files: Vec<gio::File> = items.iter().map(|i| i.file()).collect();
        let text = paths_text(&items);
        let list = gdk::FileList::from_array(&files);
        let provider = gdk::ContentProvider::new_union(&[
            gdk::ContentProvider::for_value(&list.to_value()),
            gdk::ContentProvider::for_bytes(
                "text/plain;charset=utf-8",
                &glib::Bytes::from_owned(text.into_bytes()),
            ),
        ]);
        let _ = self.window.clipboard().set_content(Some(&provider));
    }

    fn export_dialog(self: &Rc<Self>) {
        let dialog = gtk::FileDialog::builder()
            .title("Export Results")
            .initial_name("results.efu")
            .modal(true)
            .build();
        let ui = self.clone();
        dialog.save(Some(&self.window), gio::Cancellable::NONE, move |r| {
            if let Some(path) = r.ok().and_then(|f| f.path()) {
                ui.export_to(path);
            }
        });
    }

    /// Writes all current results to `path`; the extension picks the
    /// format (efu, csv, tsv, m3u, m3u8, anything else: paths).
    fn export_to(self: &Rc<Self>, path: PathBuf) {
        let (result, total) = (self.model.result(), self.model.total());
        if result == 0 {
            return self.toast("Nothing to export");
        }
        let format = Format::for_path(&path);
        let columns: Vec<&'static ColumnDef> = self
            .columns
            .iter()
            .filter(|(_, c)| c.is_visible())
            .map(|(d, _)| *d)
            .collect();
        let ui = self.clone();
        let target = path.clone();
        self.backend.call(
            move |c| {
                let failed = |e: std::io::Error| ClientError::Server {
                    code: ErrorCode::Failed,
                    message: format!("{}: {e}", target.display()),
                };
                let file = std::fs::File::create(&target).map_err(failed)?;
                let mut out = Exporter::new(std::io::BufWriter::new(file), format, columns)
                    .map_err(failed)?;
                let mut offset = 0;
                while offset < total {
                    let rows = c.fetch(result, offset, 10_000)?;
                    if rows.is_empty() {
                        break;
                    }
                    for row in &rows {
                        out.row(row).map_err(failed)?;
                    }
                    offset += rows.len() as u32;
                }
                out.finish().map_err(failed)?;
                Ok(offset)
            },
            move |r| match r {
                Ok(n) => ui.toast(&format!(
                    "Exported {} results to {}",
                    format::count(n as u64),
                    path.display()
                )),
                Err(e) => ui.toast(&format!("Export failed: {e}")),
            },
        );
    }

    fn bookmark_here(&self, name: String) -> Bookmark {
        let s = self.state.borrow();
        Bookmark {
            name,
            search: self.entry.text().to_string(),
            filter: s.filter,
            options: s.options.clone(),
            sort: s.sort,
        }
    }

    fn add_bookmark(&self, name: String) {
        let bookmark = self.bookmark_here(name.clone());
        self.settings.borrow_mut().set_bookmark(bookmark);
        self.save_settings();
        self.rebuild_bookmarks();
        self.toast(&format!("Bookmarked “{name}”"));
    }

    fn remove_bookmark(&self, name: &str) {
        self.settings
            .borrow_mut()
            .bookmarks
            .retain(|b| b.name != name);
        self.save_settings();
        self.rebuild_bookmarks();
    }

    /// Restores a bookmarked search. Returns false if there is none.
    fn open_bookmark(self: &Rc<Self>, name: &str) -> bool {
        let Some(b) = self.settings.borrow().bookmark(name).cloned() else {
            return false;
        };
        let filter = b.filter.min(FILTERS.len() - 1);
        self.set_options(&b.options);
        self.state.borrow_mut().filter = filter;
        self.filter.set_selected(filter as u32);
        self.apply_sort(b.sort, false);
        self.entry.set_text(&b.search);
        self.search();
        true
    }

    fn action(&self, name: &str) -> Option<gio::SimpleAction> {
        self.window.lookup_action(name).and_downcast()
    }

    /// Sets all search options at once without searching for each.
    fn set_options(&self, options: &SearchOptions) {
        let mut o = options.clone();
        for (name, field) in OPTIONS {
            if let Some(a) = self.action(name) {
                a.set_state(&(*field(&mut o)).to_variant());
            }
        }
        self.state.borrow_mut().options = options.clone();
        self.settings.borrow_mut().options = options.clone();
        self.save_settings();
    }

    fn show_sort_indicator(&self, sort: Sort) {
        self.state.borrow_mut().syncing_sort = true;
        let column = self
            .columns
            .iter()
            .find(|(d, c)| d.sort == sort.key && c.is_visible())
            .map(|(_, c)| c);
        let order = if sort.descending {
            gtk::SortType::Descending
        } else {
            gtk::SortType::Ascending
        };
        self.view.sort_by_column(column, order);
        self.state.borrow_mut().syncing_sort = false;
    }

    /// Switches the sort; `resort` re-sorts the current results.
    fn apply_sort(self: &Rc<Self>, sort: Sort, resort: bool) {
        if self.state.borrow().sort == sort {
            return;
        }
        self.state.borrow_mut().sort = sort;
        self.settings.borrow_mut().sort = sort;
        self.save_settings();
        if let Some(a) = self.action("sort-by") {
            a.set_state(&sort.key.name().to_variant());
        }
        if let Some(a) = self.action("sort-descending") {
            a.set_state(&sort.descending.to_variant());
        }
        self.show_sort_indicator(sort);
        if !resort {
            return;
        }
        let (result, total) = (self.model.result(), self.model.total() as u64);
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

    fn header_sort_changed(self: &Rc<Self>, sorter: &gtk::ColumnViewSorter) {
        if self.state.borrow().syncing_sort {
            return;
        }
        let Some(def) = sorter
            .primary_sort_column()
            .and_then(|c| c.id())
            .and_then(|id| columns::by_id(&id))
        else {
            return;
        };
        let sort = Sort {
            key: def.sort,
            descending: sorter.primary_sort_order() == gtk::SortType::Descending,
        };
        self.apply_sort(sort, true);
    }

    fn set_column_visible(&self, id: &str, visible: bool) {
        for (def, column) in &self.columns {
            if def.id == id {
                column.set_visible(visible);
            }
        }
        self.settings.borrow_mut().columns = self
            .columns
            .iter()
            .filter(|(_, c)| c.is_visible())
            .map(|(d, _)| d.id.to_owned())
            .collect();
        self.save_settings();
    }

    fn selection_changed(self: &Rc<Self>) {
        self.update_count();
        if self.preview.widget.is_visible() {
            self.preview.show(self.selected(1).into_iter().next());
        }
    }

    fn statistics(self: &Rc<Self>) {
        let ui = self.clone();
        self.backend.call(
            |c| c.statistics(),
            move |r| match r {
                Ok(stats) => crate::dialogs::statistics(&ui.window, &stats),
                Err(e) => ui.toast(&e),
            },
        );
    }

    fn request(self: &Rc<Self>, request: Request, done: &'static str) {
        let ui = self.clone();
        self.backend.call(
            move |c| c.expect_ok(request),
            move |r| match r {
                Ok(()) => ui.toast(done),
                Err(e) => ui.toast(&e),
            },
        );
    }

    fn add_actions(self: &Rc<Self>, app: &adw::Application) {
        let window = &self.window;
        let stateful = |name: &str, initial: bool, f: UiToggle| {
            let action = gio::SimpleAction::new_stateful(name, None, &initial.to_variant());
            let ui = Rc::downgrade(self);
            action.connect_change_state(move |action, value| {
                let (Some(ui), Some(on)) = (ui.upgrade(), value.and_then(|v| v.get::<bool>()))
                else {
                    return;
                };
                action.set_state(&on.to_variant());
                f(&ui, on);
            });
            window.add_action(&action);
        };
        let options = self.state.borrow().options.clone();
        for (name, field) in OPTIONS {
            let mut o = options.clone();
            stateful(
                name,
                *field(&mut o),
                Box::new(move |ui, on| {
                    *field(&mut ui.state.borrow_mut().options) = on;
                    let options = ui.state.borrow().options.clone();
                    ui.settings.borrow_mut().options = options;
                    ui.save_settings();
                    ui.search();
                }),
            );
        }
        for (def, column) in &self.columns {
            let id = def.id;
            stateful(
                &format!("column-{id}"),
                column.is_visible(),
                Box::new(move |ui, on| ui.set_column_visible(id, on)),
            );
        }
        let (preview, status_bar) = {
            let s = self.settings.borrow();
            (s.preview, s.status_bar)
        };
        stateful(
            "preview",
            preview,
            Box::new(|ui, on| {
                ui.preview.widget.set_visible(on);
                ui.settings.borrow_mut().preview = on;
                ui.save_settings();
                ui.selection_changed();
            }),
        );
        stateful(
            "status-bar",
            status_bar,
            Box::new(|ui, on| {
                ui.status_bar.set_visible(on);
                ui.settings.borrow_mut().status_bar = on;
                ui.save_settings();
            }),
        );
        let sort = self.state.borrow().sort;
        stateful(
            "sort-descending",
            sort.descending,
            Box::new(|ui, on| {
                let key = ui.state.borrow().sort.key;
                ui.apply_sort(
                    Sort {
                        key,
                        descending: on,
                    },
                    true,
                );
            }),
        );
        let sort_by = gio::SimpleAction::new_stateful(
            "sort-by",
            Some(glib::VariantTy::STRING),
            &sort.key.name().to_variant(),
        );
        let ui = Rc::downgrade(self);
        sort_by.connect_change_state(move |_, value| {
            let Some(ui) = ui.upgrade() else { return };
            let Some(key) = value
                .and_then(|v| v.get::<String>())
                .and_then(|k| evos_proto::SortKey::parse(&k))
            else {
                return;
            };
            let descending = ui.state.borrow().sort.descending;
            ui.apply_sort(Sort { key, descending }, true);
        });
        window.add_action(&sort_by);

        let simple = |name: &str, f: UiAction| {
            let action = gio::SimpleAction::new(name, None);
            let ui = Rc::downgrade(self);
            action.connect_activate(move |_, _| {
                if let Some(ui) = ui.upgrade() {
                    f(&ui);
                }
            });
            window.add_action(&action);
        };
        simple(
            "open",
            Box::new(|ui| {
                for item in ui.selected(16) {
                    ui.open(&item, Open::Default);
                }
            }),
        );
        simple(
            "open-with",
            Box::new(|ui| {
                if let Some(item) = ui.selected(1).first() {
                    ui.open(item, Open::With);
                }
            }),
        );
        simple(
            "open-folder",
            Box::new(|ui| {
                if let Some(item) = ui.selected(1).first() {
                    ui.open(item, Open::Folder);
                }
            }),
        );
        simple("copy", Box::new(|ui| ui.copy_files()));
        simple("save-copy", Box::new(|ui| ui.save_copy()));
        simple("disconnect", Box::new(|ui| ui.connect_to(None)));
        simple(
            "copy-path",
            Box::new(|ui| {
                ui.window
                    .clipboard()
                    .set_text(&paths_text(&ui.selected(100_000)));
            }),
        );
        simple(
            "copy-name",
            Box::new(|ui| {
                let names: Vec<String> = ui.selected(100_000).iter().map(|i| i.name()).collect();
                ui.window.clipboard().set_text(&names.join("\n"));
            }),
        );
        simple(
            "copy-folder",
            Box::new(|ui| {
                let folders: Vec<String> =
                    ui.selected(100_000).iter().map(|i| i.folder()).collect();
                ui.window.clipboard().set_text(&folders.join("\n"));
            }),
        );
        simple(
            "properties",
            Box::new(|ui| {
                if let Some(item) = ui.selected(1).first() {
                    crate::dialogs::properties(&ui.window, &item.row());
                }
            }),
        );
        simple(
            "focus-search",
            Box::new(|ui| {
                ui.entry.grab_focus();
            }),
        );
        simple(
            "focus-results",
            Box::new(|ui| {
                ui.view.grab_focus();
            }),
        );
        simple("export", Box::new(|ui| ui.export_dialog()));
        simple("statistics", Box::new(|ui| ui.statistics()));
        simple(
            "reindex",
            Box::new(|ui| ui.request(Request::Rescan { path: None }, "Rescanning all volumes")),
        );
        simple(
            "save-db",
            Box::new(|ui| ui.request(Request::Save, "Index saved")),
        );
        simple("refresh", Box::new(|ui| ui.search()));
        simple(
            "home",
            Box::new(|ui| {
                ui.entry.set_text("");
            }),
        );
        simple(
            "close",
            Box::new(|ui| {
                ui.window.close();
            }),
        );
        simple(
            "add-bookmark-dialog",
            Box::new(|ui| {
                let suggestion = ui.entry.text().to_string();
                let weak = Rc::downgrade(ui);
                crate::dialogs::bookmark_name(&ui.window, &suggestion, move |name| {
                    if let Some(ui) = weak.upgrade() {
                        ui.add_bookmark(name);
                    }
                });
            }),
        );
        simple(
            "organize-bookmarks",
            Box::new(|ui| {
                let weak = Rc::downgrade(ui);
                let bookmarks = ui.settings.borrow().bookmarks.clone();
                crate::dialogs::organize_bookmarks(&ui.window, &bookmarks, move |name| {
                    if let Some(ui) = weak.upgrade() {
                        ui.remove_bookmark(&name);
                    }
                });
            }),
        );
        simple(
            "clear-history",
            Box::new(|ui| {
                ui.settings.borrow_mut().history.clear();
                ui.save_settings();
                ui.rebuild_history();
            }),
        );

        let with_text = |name: &str, f: UiTextAction| {
            let action = gio::SimpleAction::new(name, Some(glib::VariantTy::STRING));
            let ui = Rc::downgrade(self);
            action.connect_activate(move |_, value| {
                if let (Some(ui), Some(text)) =
                    (ui.upgrade(), value.and_then(|v| v.get::<String>()))
                {
                    f(&ui, text);
                }
            });
            window.add_action(&action);
        };
        with_text(
            "set-search",
            Box::new(|ui, text| {
                ui.entry.set_text(&text);
                ui.search();
            }),
        );
        with_text(
            "search-history",
            Box::new(|ui, text| {
                ui.entry.set_text(&text);
                ui.entry.set_position(-1);
            }),
        );
        with_text(
            "export-to",
            Box::new(|ui, path| ui.export_to(PathBuf::from(path))),
        );
        with_text("add-bookmark", Box::new(|ui, name| ui.add_bookmark(name)));
        with_text(
            "remove-bookmark",
            Box::new(|ui, name| ui.remove_bookmark(&name)),
        );
        with_text(
            "open-bookmark",
            Box::new(|ui, name| {
                if !ui.open_bookmark(&name) {
                    ui.toast(&format!("No bookmark named “{name}”"));
                }
            }),
        );

        for (action, accels) in [
            ("app.preferences", &["<Ctrl>comma"][..]),
            ("app.new-window", &["<Ctrl>n"]),
            ("app.quit", &["<Ctrl>q"]),
            ("win.focus-search", &["<Ctrl>f", "<Ctrl>l", "F3"]),
            ("win.copy-path", &["<Ctrl><Shift>c"]),
            ("win.open-folder", &["<Ctrl>Return"]),
            ("win.properties", &["<Alt>Return"]),
            ("win.preview", &["<Alt>p"]),
            ("win.export", &["<Ctrl>s"]),
            ("win.add-bookmark-dialog", &["<Ctrl>d"]),
            ("win.organize-bookmarks", &["<Ctrl><Shift>b"]),
            ("win.match-case", &["<Ctrl>i"]),
            ("win.whole-word", &["<Ctrl>b"]),
            ("win.match-path", &["<Ctrl>u"]),
            ("win.match-diacritics", &["<Ctrl>m"]),
            ("win.regex", &["<Ctrl>r"]),
            ("win.refresh", &["F5"]),
            ("win.close", &["<Ctrl>w"]),
        ] {
            app.set_accels_for_action(action, accels);
        }
        // Ctrl+C copies files only in the result list; in the search box it
        // copies text as usual.
        let shortcuts = gtk::ShortcutController::new();
        shortcuts.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("<Control>c"),
            Some(gtk::NamedAction::new("win.copy")),
        ));
        self.view.add_controller(shortcuts);
    }

    fn connect_signals(self: &Rc<Self>) {
        let ui = Rc::downgrade(self);
        self.entry.connect_search_changed(move |_| {
            if let Some(ui) = ui.upgrade() {
                ui.search();
            }
        });
        let ui = Rc::downgrade(self);
        self.entry.connect_stop_search(move |entry| {
            let Some(ui) = ui.upgrade() else { return };
            if entry.text().is_empty() {
                ui.window.close();
            } else {
                entry.set_text("");
            }
        });
        // Enter in the search box opens the first result.
        let ui = Rc::downgrade(self);
        self.entry.connect_activate(move |_| {
            let Some(ui) = ui.upgrade() else { return };
            if let Some(item) = ui.model.loaded_item(0) {
                ui.open(&item, Open::Default);
            }
        });
        let ui = Rc::downgrade(self);
        self.filter.connect_selected_notify(move |dd| {
            let Some(ui) = ui.upgrade() else { return };
            let filter = dd.selected() as usize;
            if ui.state.borrow().filter != filter {
                ui.state.borrow_mut().filter = filter;
                ui.search();
            }
        });
        let ui = Rc::downgrade(self);
        self.view.connect_activate(move |_, pos| {
            let Some(ui) = ui.upgrade() else { return };
            if let Some(item) = ui.model.loaded_item(pos) {
                ui.open(&item, Open::Default);
            }
        });
        let ui = Rc::downgrade(self);
        self.selection.connect_selection_changed(move |_, _, _| {
            if let Some(ui) = ui.upgrade() {
                ui.selection_changed();
            }
        });
        if let Some(sorter) = self.view.sorter().and_downcast::<gtk::ColumnViewSorter>() {
            let ui = Rc::downgrade(self);
            sorter.connect_changed(move |sorter, _| {
                if let Some(ui) = ui.upgrade() {
                    ui.header_sort_changed(sorter);
                }
            });
        }
        let ui = Rc::downgrade(self);
        self.banner.connect_button_clicked(move |_| {
            if let Some(ui) = ui.upgrade() {
                ui.search();
                ui.refresh_status();
            }
        });
    }

    /// Applies a command line (see [`crate::cmdline`]).
    pub fn apply(self: &Rc<Self>, l: &Launch) {
        if let Some(columns) = &l.columns {
            if !columns.is_empty() {
                for def in COLUMNS {
                    self.set_column_state(def.id, columns.contains(&def.id));
                }
            }
        }
        for id in &l.add_columns {
            self.set_column_state(id, true);
        }
        for id in &l.remove_columns {
            self.set_column_state(id, false);
        }
        if !l.options.is_empty() {
            let mut options = self.state.borrow().options.clone();
            for (name, on) in &l.options {
                if let Some((_, field)) = OPTIONS.iter().find(|(n, _)| n == name) {
                    *field(&mut options) = *on;
                }
            }
            self.set_options(&options);
        }
        if let Some(f) = l.filter {
            self.state.borrow_mut().filter = f;
            self.filter.set_selected(f as u32);
        }
        if l.sort.is_some() || l.descending.is_some() {
            let current = self.state.borrow().sort;
            // A new sort column starts ascending unless told otherwise;
            // -sort-descending alone flips the current one.
            let sort = Sort {
                key: l.sort.unwrap_or(current.key),
                descending: l
                    .descending
                    .unwrap_or(l.sort.is_none() && current.descending),
            };
            self.apply_sort(sort, false);
        }
        if let Some(on) = l.preview {
            self.change_state("preview", on);
        }
        if let Some(on) = l.status_bar {
            self.change_state("status-bar", on);
        }
        if let Some(name) = &l.bookmark {
            if !self.open_bookmark(name) {
                self.toast(&format!("No bookmark named “{name}”"));
            }
        }
        if l.home {
            self.entry.set_text("");
        }
        if l.search.is_some() || !l.scope.is_empty() {
            let mut parts: Vec<String> = l.search.iter().cloned().collect();
            parts.extend(l.scope.iter().cloned());
            self.entry.set_text(&parts.join(" "));
            self.entry.set_position(-1);
        }
        self.search();
        match l.focus {
            Some(Focus::Search) => {
                self.entry.grab_focus();
            }
            Some(Focus::Results) => {
                self.view.grab_focus();
            }
            None => {}
        }
        match l.maximize {
            Some(true) => self.window.maximize(),
            Some(false) => self.window.unmaximize(),
            None => {}
        }
        if l.minimize {
            self.window.minimize();
        }
        match l.fullscreen {
            Some(true) => self.window.fullscreen(),
            Some(false) => self.window.unfullscreen(),
            None => {}
        }
        if l.reindex {
            self.request(Request::Rescan { path: None }, "Rescanning all volumes");
        }
        if l.save_db {
            self.request(Request::Save, "Index saved");
        }
        if l.close {
            self.window.close();
        }
    }

    fn change_state(&self, action: &str, on: bool) {
        if let Some(a) = self.action(action) {
            a.change_state(&on.to_variant());
        }
    }

    fn set_column_state(&self, id: &str, on: bool) {
        self.change_state(&format!("column-{id}"), on);
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
                    // An ETP server sends no changes; F5 searches again.
                    Notice::IndexChanged if ui.remote().is_some() => {}
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
        let ui = Rc::downgrade(self);
        glib::timeout_add_local_once(delay, move || {
            let Some(ui) = ui.upgrade() else { return };
            ui.state.borrow_mut().refresh_scheduled = false;
            if ui.window.is_visible() {
                ui.search();
            }
        });
    }

    pub fn refresh_status(self: &Rc<Self>) {
        if let Some(r) = self.remote() {
            self.status.set_text(&format!("Searching {}", describe(&r)));
            return;
        }
        let ui = Rc::downgrade(self);
        self.backend.call(
            |c| c.status(),
            move |r| {
                let Some(ui) = ui.upgrade() else { return };
                match r {
                    Ok(status) => {
                        ui.banner.set_revealed(false);
                        ui.status.set_text(&summarize(&status));
                    }
                    Err(_) => ui.banner.set_revealed(true),
                }
            },
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Open {
    Default,
    With,
    Folder,
}

fn paths_text(items: &[RowItem]) -> String {
    items
        .iter()
        .map(|i| String::from_utf8_lossy(&i.path()).into_owned())
        .collect::<Vec<_>>()
        .join("\n")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_labels_escape_mnemonics() {
        assert_eq!(menu_label("a_b"), "a__b");
        assert_eq!(menu_label(&"x".repeat(80)).chars().count(), 60);
    }
}
