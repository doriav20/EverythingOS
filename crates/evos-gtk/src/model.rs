//! A virtual list model over a result set that lives in the daemon.
//!
//! `n_items` is the result count, which may be millions. Rows are fetched
//! in pages when GTK asks for an item that is not cached; until the page
//! arrives a placeholder item is returned and filled in place afterwards,
//! so bindings update without the list being rebuilt. Only a bounded number
//! of pages is kept.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};

use evos_proto::Row;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

use crate::row_item::RowItem;

/// Rows per fetch.
pub const PAGE: u32 = 256;
/// Pages kept in memory.
const MAX_PAGES: usize = 64;

type Fetcher = Box<dyn Fn(u32, u32, u32)>;

#[derive(Default)]
pub struct State {
    result: u32,
    total: u32,
    pages: HashMap<u32, Vec<Row>>,
    lru: VecDeque<u32>,
    requested: HashSet<u32>,
    /// Placeholders handed out for rows that are still loading.
    waiting: HashMap<u32, glib::WeakRef<RowItem>>,
    fetcher: Option<Fetcher>,
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct ResultModel {
        pub state: RefCell<State>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ResultModel {
        const NAME: &'static str = "EvosResultModel";
        type Type = super::ResultModel;
        type Interfaces = (gio::ListModel,);
    }

    impl ObjectImpl for ResultModel {}

    impl ListModelImpl for ResultModel {
        fn item_type(&self) -> glib::Type {
            RowItem::static_type()
        }

        fn n_items(&self) -> u32 {
            self.state.borrow().total
        }

        fn item(&self, position: u32) -> Option<glib::Object> {
            let mut state = self.state.borrow_mut();
            if position >= state.total {
                return None;
            }
            let page = position / PAGE;
            if let Some(rows) = state.pages.get(&page) {
                let row = rows.get((position % PAGE) as usize)?;
                return Some(RowItem::from_row(row).upcast());
            }
            let item = RowItem::loading();
            state.waiting.insert(position, item.downgrade());
            if state.requested.insert(page) {
                if let Some(fetch) = &state.fetcher {
                    fetch(state.result, page * PAGE, PAGE);
                }
            }
            Some(item.upcast())
        }
    }
}

glib::wrapper! {
    pub struct ResultModel(ObjectSubclass<imp::ResultModel>) @implements gio::ListModel;
}

impl Default for ResultModel {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl ResultModel {
    /// Sets how pages are requested: `fetch(result, offset, count)`. The
    /// answer must come back through [`ResultModel::set_rows`].
    pub fn set_fetcher(&self, fetch: impl Fn(u32, u32, u32) + 'static) {
        self.imp().state.borrow_mut().fetcher = Some(Box::new(fetch));
    }

    pub fn result(&self) -> u32 {
        self.imp().state.borrow().result
    }

    pub fn total(&self) -> u32 {
        self.imp().state.borrow().total
    }

    /// Shows a new result set (or the same one, re-sorted).
    pub fn reset(&self, result: u32, total: u64) {
        let old = {
            let mut s = self.imp().state.borrow_mut();
            let old = s.total;
            s.result = result;
            s.total = total.min(u32::MAX as u64) as u32;
            s.pages.clear();
            s.lru.clear();
            s.requested.clear();
            s.waiting.clear();
            old
        };
        self.items_changed(0, old, self.total());
    }

    /// Delivers a fetched page. Rows for an outdated result are ignored.
    pub fn set_rows(&self, result: u32, offset: u32, rows: Vec<Row>) {
        let page = offset / PAGE;
        let mut fill = Vec::new();
        {
            let mut s = self.imp().state.borrow_mut();
            if s.result != result {
                return;
            }
            s.requested.remove(&page);
            for i in 0..rows.len() {
                let pos = offset + i as u32;
                if let Some(item) = s.waiting.remove(&pos).and_then(|w| w.upgrade()) {
                    fill.push((item, i));
                }
            }
            s.pages.insert(page, rows);
            s.lru.push_back(page);
            while s.lru.len() > MAX_PAGES {
                if let Some(old) = s.lru.pop_front() {
                    if old != page {
                        s.pages.remove(&old);
                    }
                }
            }
        }
        // Fill outside the borrow: property notifications re-enter GTK.
        let s = self.imp().state.borrow();
        if let Some(rows) = s.pages.get(&page) {
            let rows: Vec<Row> = fill.iter().map(|(_, i)| rows[*i].clone()).collect();
            drop(s);
            for ((item, _), row) in fill.into_iter().zip(rows) {
                item.fill(&row);
            }
        }
    }

    /// A fetch failed; allow it to be retried.
    pub fn fetch_failed(&self, offset: u32) {
        self.imp()
            .state
            .borrow_mut()
            .requested
            .remove(&(offset / PAGE));
    }

    /// The loaded item at `position`, if its row is cached.
    pub fn loaded_item(&self, position: u32) -> Option<RowItem> {
        self.item(position)
            .and_downcast::<RowItem>()
            .filter(|i| i.loaded())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn row(i: u32) -> Row {
        Row {
            name: format!("f{i}").into_bytes(),
            folder: b"/d".to_vec(),
            is_dir: false,
            size: Some(i as u64),
            mtime: 0,
            offline: false,
        }
    }

    #[test]
    fn pages_are_fetched_on_demand_and_fill_placeholders() {
        let model = ResultModel::default();
        let calls = Rc::new(RefCell::new(Vec::new()));
        let c = calls.clone();
        model
            .set_fetcher(move |result, offset, count| c.borrow_mut().push((result, offset, count)));
        model.reset(7, 1000);
        assert_eq!(model.n_items(), 1000);

        let placeholder = model.item(300).and_downcast::<RowItem>().unwrap();
        assert!(!placeholder.loaded());
        let _ = model.item(301);
        assert_eq!(*calls.borrow(), vec![(7, 256, PAGE)], "one fetch per page");

        let rows: Vec<Row> = (256..512).map(row).collect();
        model.set_rows(7, 256, rows);
        assert!(placeholder.loaded(), "placeholder filled in place");
        assert_eq!(placeholder.name(), "f300");
        assert_eq!(placeholder.path(), b"/d/f300");
        assert_eq!(model.loaded_item(511).unwrap().name(), "f511");

        // Rows of an outdated result are ignored.
        model.reset(8, 10);
        model.set_rows(7, 0, (0..10).map(row).collect());
        assert!(model.loaded_item(0).is_none());
        assert!(model.item(10).is_none());
    }
}
