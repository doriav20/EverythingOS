//! One result row as a GObject, so list widgets can bind to it.

use std::cell::{Cell, RefCell};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

use evos_proto::Row;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

use crate::format;

mod imp {
    use super::*;

    #[derive(Default, glib::Properties)]
    #[properties(wrapper_type = super::RowItem)]
    pub struct RowItem {
        #[property(get, set)]
        name: RefCell<String>,
        /// The name with the search terms in bold (Pango markup).
        #[property(get, set)]
        name_markup: RefCell<String>,
        #[property(get, set)]
        folder: RefCell<String>,
        #[property(get, set)]
        folder_markup: RefCell<String>,
        #[property(get, set)]
        size: RefCell<String>,
        #[property(get, set)]
        extension: RefCell<String>,
        #[property(get, set)]
        type_name: RefCell<String>,
        #[property(get, set)]
        modified: RefCell<String>,
        #[property(get, set)]
        created: RefCell<String>,
        #[property(get, set)]
        accessed: RefCell<String>,
        #[property(get, set)]
        changed: RefCell<String>,
        #[property(get, set)]
        attributes: RefCell<String>,
        #[property(get, set)]
        run_count: RefCell<String>,
        #[property(get, set)]
        date_run: RefCell<String>,
        #[property(get, set, nullable)]
        icon: RefCell<Option<gio::Icon>>,
        #[property(get, set)]
        loaded: Cell<bool>,
        #[property(get, set)]
        offline: Cell<bool>,
        #[property(get, set)]
        is_dir: Cell<bool>,
        /// The row as the daemon sent it; raw path bytes need not be
        /// valid UTF-8.
        pub row: RefCell<Row>,
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

/// Escapes `text` for Pango markup with the search terms in bold.
pub fn highlight_markup(text: &str, terms: &[String]) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let mut at = 0;
    for range in evos_core::fold::highlight(text, terms) {
        out.push_str(&glib::markup_escape_text(&text[at..range.start]));
        out.push_str("<b>");
        out.push_str(&glib::markup_escape_text(&text[range.clone()]));
        out.push_str("</b>");
        at = range.end;
    }
    out.push_str(&glib::markup_escape_text(&text[at..]));
    out
}

impl RowItem {
    /// A placeholder shown until the row's page arrives.
    pub fn loading() -> Self {
        Self::default()
    }

    pub fn from_row(row: &Row, terms: &[String]) -> Self {
        let item = Self::default();
        item.fill(row, terms);
        item
    }

    /// Fills the item with a row's data, highlighting `terms`.
    pub fn fill(&self, row: &Row, terms: &[String]) {
        let name = String::from_utf8_lossy(&row.name).into_owned();
        let folder = String::from_utf8_lossy(&row.folder).into_owned();
        let icon = if row.is_dir {
            gio::ThemedIcon::new("folder").upcast::<gio::Icon>()
        } else {
            let (content_type, _) =
                gio::content_type_guess(Some(OsStr::from_bytes(&row.name)), None);
            gio::content_type_get_icon(&content_type)
        };
        let extension = match name.rfind('.') {
            Some(dot) if dot > 0 && !row.is_dir => name[dot + 1..].to_owned(),
            _ => String::new(),
        };
        self.set_name_markup(highlight_markup(&name, terms));
        self.set_folder_markup(highlight_markup(&folder, terms));
        self.set_name(name);
        self.set_folder(folder);
        self.set_extension(extension);
        self.set_size(row.size.map(format::size).unwrap_or_default());
        self.set_type_name(row.type_name.clone());
        self.set_modified(format::date(row.mtime));
        self.set_created(format::date(row.created));
        self.set_accessed(format::date(row.accessed));
        self.set_changed(format::date(row.changed));
        self.set_attributes(row.attributes.clone());
        self.set_run_count(if row.run_count == 0 {
            String::new()
        } else {
            row.run_count.to_string()
        });
        self.set_date_run(format::date(row.date_run));
        self.set_icon(Some(icon));
        self.set_is_dir(row.is_dir);
        self.set_offline(row.offline);
        self.imp().row.replace(row.clone());
        self.set_loaded(true);
    }

    pub fn row(&self) -> Row {
        self.imp().row.borrow().clone()
    }

    pub fn path(&self) -> Vec<u8> {
        self.imp().row.borrow().path()
    }

    pub fn file(&self) -> gio::File {
        gio::File::for_path(OsStr::from_bytes(&self.path()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_is_escaped_and_bold() {
        let terms = vec!["report".to_string()];
        assert_eq!(
            highlight_markup("Q1 <Report> & co", &terms),
            "Q1 &lt;<b>Report</b>&gt; &amp; co"
        );
        let terms = vec!["שלום".to_string()];
        assert_eq!(highlight_markup("שָׁלוֹם.txt", &terms), "<b>שָׁלוֹם</b>.txt");
    }
}
