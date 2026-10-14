//! One result row as a GObject, so list widgets can bind to it.

use std::cell::{Cell, RefCell};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

use evos_proto::Row;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

mod imp {
    use super::*;

    #[derive(Default, glib::Properties)]
    #[properties(wrapper_type = super::RowItem)]
    pub struct RowItem {
        #[property(get, set)]
        name: RefCell<String>,
        #[property(get, set)]
        folder: RefCell<String>,
        #[property(get, set)]
        size: RefCell<String>,
        #[property(get, set)]
        modified: RefCell<String>,
        #[property(get, set, nullable)]
        icon: RefCell<Option<gio::Icon>>,
        #[property(get, set)]
        loaded: Cell<bool>,
        #[property(get, set)]
        offline: Cell<bool>,
        #[property(get, set)]
        is_dir: Cell<bool>,
        /// Raw path bytes; Linux paths need not be valid UTF-8.
        pub path: RefCell<Vec<u8>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RowItem {
        const NAME: &'static str = "EvosRowItem";
        type Type = super::RowItem;
    }

    #[glib::derived_properties]
    impl ObjectImpl for RowItem {}
}

glib::wrapper! {
    pub struct RowItem(ObjectSubclass<imp::RowItem>);
}

impl Default for RowItem {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl RowItem {
    /// A placeholder shown until the row's page arrives.
    pub fn loading() -> Self {
        Self::default()
    }

    pub fn from_row(row: &Row) -> Self {
        let item = Self::default();
        item.fill(row);
        item
    }

    /// Fills the item with a row's data.
    pub fn fill(&self, row: &Row) {
        let name = String::from_utf8_lossy(&row.name).into_owned();
        let icon = if row.is_dir {
            gio::ThemedIcon::new("folder").upcast::<gio::Icon>()
        } else {
            let (content_type, _) =
                gio::content_type_guess(Some(OsStr::from_bytes(&row.name)), None);
            gio::content_type_get_icon(&content_type)
        };
        self.set_name(name);
        self.set_folder(String::from_utf8_lossy(&row.folder).into_owned());
        self.set_size(row.size.map(crate::format::size).unwrap_or_default());
        self.set_modified(crate::format::date(row.mtime));
        self.set_icon(Some(icon));
        self.set_is_dir(row.is_dir);
        self.set_offline(row.offline);
        self.imp().path.replace(row.path());
        self.set_loaded(true);
    }

    pub fn path(&self) -> Vec<u8> {
        self.imp().path.borrow().clone()
    }

    pub fn file(&self) -> gio::File {
        gio::File::for_path(OsStr::from_bytes(&self.path()))
    }
}
